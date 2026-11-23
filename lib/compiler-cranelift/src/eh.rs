//! Helpers for generating DWARF LSDA data for Cranelift-compiled functions.
//!
//! The structures and encoding implemented here mirror what LLVM produces for
//! Wasm exception handling so that Wasmer's libunwind personalities can parse
//! the tables without any runtime changes.

use cranelift_codegen::{ExceptionContextLoc, MachCallSiteItem, MachExceptionHandler};
use cranelift_entity::EntityRef;
use itertools::Itertools;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::convert::TryFrom;
use std::io::{Cursor, Write};

/// Relocation information for an LSDA entry that references a tag constant.
#[derive(Debug, Clone)]
pub struct TagRelocation {
    /// Offset within the LSDA blob where the relocation should be applied.
    pub offset: u32,
    /// The module-local exception tag value.
    pub tag: u32,
}

/// Fully encoded LSDA bytes for a single function, together with pending tag
/// relocations that will be resolved once the global tag section is built.
#[derive(Debug, Clone)]
pub struct FunctionLsdaData {
    pub bytes: Vec<u8>,
    pub relocations: Vec<TagRelocation>,
}

/// Build the LSDA for a single function given the finalized Cranelift
/// call-site metadata.
pub fn build_function_lsda<'a>(
    call_sites: impl Iterator<Item = MachCallSiteItem<'a>>,
    function_length: usize,
    pointer_bytes: u8,
    pcrel_type_table: bool,
) -> Option<FunctionLsdaData> {
    let mut sites = Vec::new();

    for site in call_sites {
        let mut catches = Vec::new();
        let mut landing_pad = None;

        // Our landing pads handle all the tags considered for a call instruction, thus
        // we use the latest landing pad.
        for handler in site.exception_handlers {
            match handler {
                MachExceptionHandler::Tag(tag, offset) => {
                    landing_pad = Some(landing_pad.unwrap_or(offset.as_offset()));
                    catches.push(ExceptionType::Tag {
                        tag: u32::try_from(tag.index()).expect("tag index fits in u32"),
                    });
                }
                MachExceptionHandler::Default(offset) => {
                    landing_pad = Some(landing_pad.unwrap_or(offset.as_offset()));
                    catches.push(ExceptionType::CatchAll);
                }
                MachExceptionHandler::Context(context) => {
                    // Context records are used by Cranelift to thread VMContext
                    // information through the landing pad. We emit the LSDA
                    // regardless of whether we see them; nothing to do here.
                    match context {
                        ExceptionContextLoc::SPOffset(_) | ExceptionContextLoc::GPR(_) => {}
                    }
                }
            }
        }

        if catches.is_empty() {
            continue;
        }

        let landing_pad = landing_pad.expect("landing pad offset set when catches exist");
        let cs_start = site.ret_addr.saturating_sub(1);

        sites.push(CallSiteDesc {
            start: cs_start,
            len: 1,
            landing_pad,
            actions: catches,
        });
    }

    if sites.is_empty() {
        return None;
    }

    // Ensure all instructions in the function are covered by filling gaps with
    // default unwinding behavior (no catch actions).
    let mut current_pos = 0u32;
    let mut filled_sites = Vec::new();

    for site in sites {
        if site.start > current_pos {
            // Gap found: add a default site that covers instructions with no handlers
            filled_sites.push(CallSiteDesc {
                start: current_pos,
                len: site.start - current_pos,
                landing_pad: 0,
                actions: Vec::new(),
            });
        }
        current_pos = site.start + site.len;
        filled_sites.push(site);
    }

    // Cover any remaining instructions at the end of the function
    if current_pos < function_length as u32 {
        filled_sites.push(CallSiteDesc {
            start: current_pos,
            len: function_length as u32 - current_pos,
            landing_pad: 0,
            actions: Vec::new(),
        });
    }

    let sites = filled_sites;

    let mut type_entries = TypeTable::new();
    let mut callsite_actions = Vec::with_capacity(sites.len());

    for site in &sites {
        #[cfg(debug_assertions)]
        {
            // CatchAll must always be the last item in the action list; otherwise, the tags that follow
            // it will be ignored.
            let catch_all_positions = site
                .actions
                .iter()
                .positions(|a| matches!(a, ExceptionType::CatchAll))
                .collect_vec();
            assert!(catch_all_positions.iter().at_most_one().is_ok());
            if let Some(&i) = catch_all_positions.first() {
                assert!(i == site.actions.len() - 1);
            }
        }

        let action_indices = site
            .actions
            .iter()
            // Reverse actions to ensure CatchAll is always last in the chain, since the action table
            // encoding uses back references and relies on this ordering.
            .rev()
            .map(|action| type_entries.get_or_insert(*action) as i32)
            .collect_vec();
        callsite_actions.push(action_indices);
    }

    let action_table = encode_action_table(&callsite_actions);
    let call_site_table = encode_call_site_table(&sites, &action_table);
    let (type_table_bytes, type_table_relocs) = if pcrel_type_table {
        type_entries.encode_relocated()
    } else {
        type_entries.encode(pointer_bytes)
    };

    let call_site_table_len = call_site_table.len() as u64;
    let mut writer = Cursor::new(Vec::new());
    writer
        .write_all(&cranelift_codegen::gimli::DW_EH_PE_omit.0.to_le_bytes())
        .unwrap(); // lpstart encoding omitted (relative to function start)

    if type_entries.is_empty() {
        writer
            .write_all(&cranelift_codegen::gimli::DW_EH_PE_omit.0.to_le_bytes())
            .unwrap();
    } else if pcrel_type_table {
        // PC-relative, 4-byte entries. This keeps the `.gcc_except_table`
        // section relocations position-independent (e.g. `R_X86_64_PC32`).
        writer
            .write_all(
                &(cranelift_codegen::gimli::DW_EH_PE_pcrel
                    | cranelift_codegen::gimli::DW_EH_PE_sdata4)
                    .0
                    .to_le_bytes(),
            )
            .unwrap();
    } else {
        writer
            .write_all(&cranelift_codegen::gimli::DW_EH_PE_absptr.0.to_le_bytes())
            .unwrap();
    }

    if !type_entries.is_empty() {
        let ttype_table_end = 1 // call-site encoding byte
            + uleb128_len(call_site_table_len)
            + call_site_table.len()
            + action_table.bytes.len()
            + type_table_bytes.len();
        leb128::write::unsigned(&mut writer, ttype_table_end as u64).unwrap();
    }

    writer
        .write_all(&cranelift_codegen::gimli::DW_EH_PE_udata4.0.to_le_bytes())
        .unwrap();
    leb128::write::unsigned(&mut writer, call_site_table_len).unwrap();
    writer.write_all(&call_site_table).unwrap();
    writer.write_all(&action_table.bytes).unwrap();

    let type_table_offset = writer.position() as u32;
    writer.write_all(&type_table_bytes).unwrap();

    let mut relocations = Vec::new();
    for reloc in type_table_relocs {
        relocations.push(TagRelocation {
            offset: type_table_offset + reloc.offset,
            tag: reloc.tag,
        });
    }

    Some(FunctionLsdaData {
        bytes: writer.into_inner(),
        relocations,
    })
}

#[derive(Debug)]
struct CallSiteDesc {
    start: u32,
    len: u32,
    landing_pad: u32,
    actions: Vec<ExceptionType>,
}

#[derive(Debug, Hash, PartialEq, Eq, Clone, Copy)]
enum ExceptionType {
    Tag { tag: u32 },
    CatchAll,
}

#[derive(Debug)]
struct TypeTable {
    entries: indexmap::IndexSet<ExceptionType>,
}

impl TypeTable {
    fn new() -> Self {
        Self {
            entries: indexmap::IndexSet::new(),
        }
    }

    fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn get_or_insert(&mut self, exception: ExceptionType) -> usize {
        self.entries.insert(exception);

        // The indices are one-based!
        self.entries
            .get_index_of(&exception)
            .expect("must be already inserted")
            + 1
    }

    fn encode(&self, pointer_bytes: u8) -> (Vec<u8>, Vec<TagRelocation>) {
        let mut bytes = Vec::with_capacity(self.entries.len() * pointer_bytes as usize);
        let mut relocations = Vec::new();

        // Note the exception types must be streamed in the reverse order!
        for entry in self.entries.iter().rev() {
            let offset = bytes.len() as u32;
            match entry {
                ExceptionType::Tag { tag } => {
                    bytes.extend(std::iter::repeat_n(0, pointer_bytes as usize));
                    relocations.push(TagRelocation { offset, tag: *tag });
                }
                ExceptionType::CatchAll => {
                    bytes.extend(std::iter::repeat_n(0, pointer_bytes as usize));
                }
            }
        }

        (bytes, relocations)
    }

    /// Encode the type table as PC-relative, 4-byte slots resolved through
    /// relocations against the per-object tag section. Used by the ELF
    /// artifact format, where each function's LSDA lives in its own object.
    fn encode_relocated(&self) -> (Vec<u8>, Vec<TagRelocation>) {
        const ENTRY_SIZE: usize = 4;
        let mut bytes = Vec::with_capacity(self.entries.len() * ENTRY_SIZE);
        let mut relocations = Vec::new();

        // Note the exception types must be streamed in the reverse order!
        for entry in self.entries.iter().rev() {
            let offset = bytes.len() as u32;
            match entry {
                ExceptionType::Tag { tag } => {
                    bytes.extend(std::iter::repeat_n(0, ENTRY_SIZE));
                    relocations.push(TagRelocation { offset, tag: *tag });
                }
                ExceptionType::CatchAll => {
                    bytes.extend(std::iter::repeat_n(0, ENTRY_SIZE));
                }
            }
        }

        (bytes, relocations)
    }
}

struct ActionTable {
    bytes: Vec<u8>,
    first_action_offsets: Vec<Option<u32>>,
}

fn encode_action_table(callsite_actions: &[Vec<i32>]) -> ActionTable {
    let mut writer = Cursor::new(Vec::new());
    let mut first_action_offsets = Vec::new();

    let mut cache = HashMap::new();

    for actions in callsite_actions {
        if actions.is_empty() {
            first_action_offsets.push(None);
        } else {
            match cache.entry(actions.clone()) {
                Entry::Occupied(entry) => {
                    first_action_offsets.push(Some(*entry.get()));
                }
                Entry::Vacant(entry) => {
                    let mut last_action_start = 0;
                    for (i, &ttype_index) in actions.iter().enumerate() {
                        let next_action_start = writer.position();
                        leb128::write::signed(&mut writer, ttype_index as i64)
                            .expect("leb128 write failed");

                        if i != 0 {
                            // Make a linked list to the previous action
                            let displacement = last_action_start - writer.position() as i64;
                            leb128::write::signed(&mut writer, displacement)
                                .expect("leb128 write failed");
                        } else {
                            leb128::write::signed(&mut writer, 0).expect("leb128 write failed");
                        }
                        last_action_start = next_action_start as i64;
                    }
                    let last_action_start = last_action_start as u32;
                    entry.insert(last_action_start);
                    first_action_offsets.push(Some(last_action_start));
                }
            }
        }
    }

    ActionTable {
        bytes: writer.into_inner(),
        first_action_offsets,
    }
}

fn encode_call_site_table(callsites: &[CallSiteDesc], action_table: &ActionTable) -> Vec<u8> {
    let mut writer = Cursor::new(Vec::new());
    for (idx, site) in callsites.iter().enumerate() {
        write_encoded_offset(site.start, &mut writer);
        write_encoded_offset(site.len, &mut writer);
        write_encoded_offset(site.landing_pad, &mut writer);

        let action = match action_table.first_action_offsets[idx] {
            Some(offset) => offset as u64 + 1,
            None => 0,
        };
        leb128::write::unsigned(&mut writer, action).expect("leb128 write failed");
    }
    writer.into_inner()
}

fn write_encoded_offset(val: u32, out: &mut impl Write) {
    // We use DW_EH_PE_udata4 for all offsets.
    out.write_all(&val.to_le_bytes())
        .expect("write to buffer failed")
}

fn uleb128_len(value: u64) -> usize {
    let mut cursor = Cursor::new([0u8; 10]);
    leb128::write::unsigned(&mut cursor, value).unwrap()
}

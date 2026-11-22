// This file contains code from external sources.
// Attributions: https://github.com/wasmerio/wasmer/blob/main/docs/ATTRIBUTIONS.md

//! A trampoline generator for calling dynamic host functions from Wasm.

use crate::{
    CraneliftCallbacks, abi,
    eh::compact_unwind_encoding_aarch64,
    translator::{
        CraneliftUnwindInfo, compiled_function_unwind_info, signature_to_cranelift_ir,
        type_to_irtype,
    },
};
use cranelift_codegen::{
    Context,
    gimli::write::{Address, FrameDescriptionEntry},
    ir::{self, Function, InstBuilder, StackSlotData, StackSlotKind, UserFuncName},
    isa::TargetIsa,
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use std::{cmp, mem};
use target_lexicon::Architecture;
use wasmer_compiler::{dwarf::WriterRelocate, misc::CompiledKind, types::function::Compilation};
use wasmer_types::{CompileError, FunctionType, VMOffsets};

/// A trampoline for calling a dynamic host function from Wasm.
pub struct DynamicFunctionTrampoline {
    pub body: Compilation,
    /// How to unwind its frame on a target that reads DWARF.
    pub fde: Option<FrameDescriptionEntry>,
    /// How to unwind its frame on a target that reads Mach-O compact unwind.
    pub compact_unwind_encoding: Option<u32>,
}

/// Create a trampoline for calling a dynamic host function from Wasm.
///
/// The host function throws the exception it returns from the trampoline's
/// callee, so the trampoline comes with the unwind information of its frame,
/// in the forms the module's functions use (`emit_dwarf_unwind`,
/// `emit_macho_compact_unwind`).
#[allow(clippy::too_many_arguments)]
pub fn make_trampoline_dynamic_function(
    callbacks: &Option<CraneliftCallbacks>,
    isa: &dyn TargetIsa,
    arch: Architecture,
    offsets: &VMOffsets,
    fn_builder_ctx: &mut FunctionBuilderContext,
    kind: &CompiledKind,
    func_type: &FunctionType,
    module_hash: &Option<String>,
    emit_dwarf_unwind: bool,
    emit_macho_compact_unwind: bool,
) -> Result<DynamicFunctionTrampoline, CompileError> {
    let pointer_type = isa.pointer_type();
    let frontend_config = isa.frontend_config();
    let signature = signature_to_cranelift_ir(func_type, frontend_config, arch);
    let mut stub_sig = ir::Signature::new(frontend_config.default_call_conv);
    // Add the caller `vmctx` parameter.
    stub_sig.params.push(ir::AbiParam::special(
        pointer_type,
        ir::ArgumentPurpose::VMContext,
    ));

    // Add the `values_vec` parameter.
    stub_sig.params.push(ir::AbiParam::new(pointer_type));

    // Compute the size of the values vector. The vmctx and caller vmctx are passed separately.
    let value_size = mem::size_of::<u128>();
    let values_vec_len =
        (value_size * cmp::max(func_type.params().len(), func_type.results().len())) as u32;

    let mut context = Context::new();
    context.func = Function::with_name_signature(UserFuncName::user(0, 0), signature.clone());

    let ss = context.func.create_sized_stack_slot(StackSlotData::new(
        StackSlotKind::ExplicitSlot,
        values_vec_len,
        0,
    ));

    {
        let mut builder = FunctionBuilder::new(&mut context.func, fn_builder_ctx);
        let block0 = builder.create_block();

        builder.append_block_params_for_function_params(block0);
        builder.switch_to_block(block0);
        builder.seal_block(block0);

        let values_vec_ptr_val = builder.ins().stack_addr(pointer_type, ss, 0);
        let mflags = ir::MemFlagsData::trusted();
        // Copy only normal WebAssembly arguments; special ABI arguments are separate.
        let mut wasm_param = 0usize;
        let mut vmctx_ptr_val = None;
        let mut sret_ptr = None;
        for (i, param) in signature.params.iter().enumerate() {
            let val = builder.func.dfg.block_params(block0)[i];
            match param.purpose {
                ir::ArgumentPurpose::Normal => {
                    builder.ins().store(
                        mflags,
                        val,
                        values_vec_ptr_val,
                        (wasm_param * value_size) as i32,
                    );
                    wasm_param += 1;
                }
                ir::ArgumentPurpose::VMContext => vmctx_ptr_val = Some(val),
                ir::ArgumentPurpose::StructReturn => sret_ptr = Some(val),
                _ => unreachable!("unexpected WebAssembly ABI parameter"),
            }
        }

        let vmctx_ptr_val = vmctx_ptr_val.expect("WebAssembly signature has vmctx");
        let callee_args = vec![vmctx_ptr_val, values_vec_ptr_val];

        let new_sig = builder.import_signature(stub_sig);

        let mem_flags = ir::MemFlagsData::trusted();
        let callee_value = builder.ins().load(
            pointer_type,
            mem_flags,
            vmctx_ptr_val,
            offsets.vmdynamicfunction_import_context_address() as i32,
        );

        builder
            .ins()
            .call_indirect(new_sig, callee_value, &callee_args);

        let mflags = ir::MemFlagsData::trusted();
        let mut results = Vec::new();
        for (i, &ty) in func_type.results().iter().enumerate() {
            let load = builder.ins().load(
                type_to_irtype(ty, frontend_config).unwrap(),
                mflags,
                values_vec_ptr_val,
                (i * value_size) as i32,
            );
            results.push(load);
        }
        let return_abi = abi::classify_returns(arch, func_type.results());
        match &return_abi {
            wasmer_compiler::abi::ReturnAbi::Sret(types) => {
                let layout = abi::return_area_layout(types);
                abi::store_sret(
                    &mut builder,
                    sret_ptr.expect("sret signature has return pointer"),
                    &layout,
                    &results,
                );
                builder.ins().return_(&[]);
            }
            _ => {
                let packed = abi::pack_register_returns(&mut builder, &return_abi, &results);
                builder.ins().return_(&packed);
            }
        }
        builder.finalize(frontend_config)
    }

    if let Some(callbacks) = callbacks.as_ref() {
        callbacks.preopt_ir(
            kind,
            module_hash,
            context.func.display().to_string().as_bytes(),
        );
    }

    let mut code_buf = Vec::new();
    let mut ctrl_plane = Default::default();
    let compiled = context
        .compile(isa, &mut ctrl_plane)
        .map_err(|error| CompileError::Codegen(error.inner.to_string()))?;
    code_buf.extend_from_slice(compiled.code_buffer());
    let compact_unwind_encoding = emit_macho_compact_unwind
        .then(|| compact_unwind_encoding_aarch64(&compiled.buffer.unwind_info))
        .transpose()
        .map_err(|error| {
            CompileError::Codegen(format!(
                "failed to encode aarch64 Mach-O compact unwind for a dynamic function \
                 trampoline: {error}"
            ))
        })?;
    let fde = match compiled_function_unwind_info(isa, &context)? {
        CraneliftUnwindInfo::Fde(fde) if emit_dwarf_unwind => Some(fde.to_fde(Address::Symbol {
            symbol: WriterRelocate::FUNCTION_SYMBOL,
            addend: 0,
        })),
        _ => None,
    };

    if let Some(callbacks) = callbacks.as_ref() {
        callbacks.obj_memory_buffer(kind, module_hash, &code_buf);
        callbacks.asm_memory_buffer(kind, module_hash, arch, &code_buf)?;
    }

    Ok(DynamicFunctionTrampoline {
        body: code_buf,
        fde,
        compact_unwind_encoding,
    })
}

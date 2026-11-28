use super::{WasmExternType, wasm_externtype_t};
use wasmer_api::{ExternType, MemoryType, Pages};

#[derive(Debug, Clone)]
pub(crate) struct WasmMemoryType {
    pub(crate) memory_type: MemoryType,
    limits: wasm_limits_t,
}

impl WasmMemoryType {
    pub(crate) fn new(memory_type: MemoryType) -> Self {
        let limits = wasm_limits_t {
            min: memory_type.minimum.pages(),
            max: memory_type
                .maximum
                .map_or(LIMITS_MAX_SENTINEL, Pages::pages),
        };

        Self {
            memory_type,
            limits,
        }
    }
}

#[allow(non_camel_case_types)]
#[derive(Debug, Clone)]
#[repr(transparent)]
pub struct wasm_memorytype_t {
    pub(crate) extern_type: wasm_externtype_t,
}

impl wasm_memorytype_t {
    pub(crate) fn new(memory_type: MemoryType) -> Self {
        Self {
            extern_type: wasm_externtype_t::new(ExternType::Memory(memory_type)),
        }
    }

    pub(crate) fn inner(&self) -> &WasmMemoryType {
        match &self.extern_type.inner {
            WasmExternType::Memory(wasm_memory_type) => wasm_memory_type,
            _ => {
                unreachable!("Data corruption: `wasm_memorytype_t` does not contain a memory type")
            }
        }
    }
}

wasm_declare_boxed_vec!(memorytype);

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_memorytype_new(
    limits: &wasm_limits_t,
) -> Option<Box<wasm_memorytype_t>> {
    let min_pages = c_try!(Pages::try_from_pages(limits.min));
    let max_pages = if limits.max == LIMITS_MAX_SENTINEL {
        None
    } else {
        Some(c_try!(Pages::try_from_pages(limits.max)))
    };

    Some(Box::new(wasm_memorytype_t::new(MemoryType::new(
        min_pages, max_pages, false,
    ))))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_memorytype_delete(_memory_type: Option<Box<wasm_memorytype_t>>) {}

#[allow(non_camel_case_types)]
#[derive(Copy, Clone, Debug)]
#[repr(C)]
pub struct wasm_limits_t {
    pub min: u32,
    pub max: u32,
}

const LIMITS_MAX_SENTINEL: u32 = u32::MAX;

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_memorytype_limits(memory_type: &wasm_memorytype_t) -> &wasm_limits_t {
    &memory_type.inner().limits
}

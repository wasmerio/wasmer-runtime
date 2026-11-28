use crate::js::utils::js_handle::JsHandle;
use js_sys::WebAssembly::Memory as JsMemory;
use tracing::trace;
use wasm_bindgen::{JsCast, JsValue};
use wasmer_types::{MemoryError, MemoryType, Pages, WASM_PAGE_SIZE};

/// Represents linear memory that is managed by the javascript runtime
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VMMemory {
    pub(crate) memory: JsHandle<JsMemory>,
    pub(crate) ty: MemoryType,
}

unsafe impl Send for VMMemory {}
unsafe impl Sync for VMMemory {}

#[derive(serde::Serialize, serde::Deserialize)]
struct DummyBuffer {
    #[serde(rename = "byteLength")]
    byte_length: u32,
}

impl VMMemory {
    /// Creates a new memory directly from a WebAssembly javascript object
    pub fn new(memory: JsMemory, ty: MemoryType) -> Self {
        Self {
            memory: JsHandle::new(memory),
            ty,
        }
    }

    /// Returns the size of the memory buffer in pages
    pub fn get_runtime_size(&self) -> u32 {
        let dummy: DummyBuffer = match serde_wasm_bindgen::from_value(self.memory.buffer()) {
            Ok(o) => o,
            Err(_) => return 0,
        };
        if dummy.byte_length == 0 {
            return 0;
        }
        dummy.byte_length / WASM_PAGE_SIZE as u32
    }

    /// Attempts to clone this memory (if its cloneable)
    pub(crate) fn try_clone(&self) -> Result<Self, MemoryError> {
        Ok(self.clone())
    }

    /// Copies this memory to a new memory
    pub fn copy(&self) -> Result<Self, wasmer_types::MemoryError> {
        let new_memory = crate::js::memory::Memory::js_memory_from_type(&self.ty)?;

        let src = crate::js::memory::MemoryView::new_raw(&self.memory);
        let amount = src.data_size();

        trace!(%amount, "memory copy started");

        let mut dst = crate::js::memory::MemoryView::new_raw(&new_memory);
        let dst_size = dst.data_size();

        if amount > dst_size {
            let pages = Pages::from_bytes_rounded_up(amount - dst_size)
                .map_err(|err| MemoryError::Generic(err.to_string()))?;

            let our_js_memory: &crate::js::memory::JSMemory =
                JsCast::unchecked_from_js_ref(&new_memory);
            our_js_memory.grow(pages.pages()).map_err(|err| {
                if err.is_instance_of::<js_sys::RangeError>() {
                    MemoryError::CouldNotGrow {
                        current: dst.size(),
                        attempted_delta: pages,
                    }
                } else {
                    MemoryError::Generic(err.as_string().unwrap_or_else(|| format!("{err:?}")))
                }
            })?;

            dst = crate::js::memory::MemoryView::new_raw(&new_memory);
        }

        src.copy_to_memory(amount, &dst).map_err(|err| {
            wasmer_types::MemoryError::Generic(format!("failed to copy the memory - {err}"))
        })?;

        trace!("memory copy finished (size={})", dst.data_size());

        Ok(Self {
            memory: JsHandle::new(new_memory),
            ty: self.ty,
        })
    }
}

impl From<VMMemory> for JsValue {
    fn from(value: VMMemory) -> Self {
        Self::from(value.memory)
    }
}

impl From<VMMemory> for (JsValue, MemoryType) {
    fn from(value: VMMemory) -> Self {
        (JsValue::from(value.memory), value.ty)
    }
}

/// Detached shared memory contains no worker-local JavaScript handle.
#[derive(Clone, Debug)]
pub struct VMSharedMemory {
    memory: crate::js::utils::shared_handle::SharedJsHandle,
    ty: MemoryType,
}

impl TryFrom<VMMemory> for VMSharedMemory {
    type Error = MemoryError;

    fn try_from(memory: VMMemory) -> Result<Self, Self::Error> {
        if !memory.ty.shared
            || !memory
                .memory
                .buffer()
                .is_instance_of::<js_sys::SharedArrayBuffer>()
        {
            return Err(MemoryError::MemoryNotShared);
        }
        Ok(Self {
            memory: crate::js::utils::shared_handle::SharedJsHandle::new(
                memory.memory.into_inner(),
            ),
            ty: memory.ty,
        })
    }
}
impl VMSharedMemory {
    /// The memory on the current worker, or `None` if it was never delivered
    /// there.
    pub fn local_memory(&self) -> Option<JsMemory> {
        self.memory.get()
    }

    /// Fails if the memory was never delivered to the current worker.
    pub fn try_attach(self) -> Result<VMMemory, MemoryError> {
        let memory = self.memory.get().ok_or_else(|| {
            MemoryError::Generic(
                "shared memory is unavailable in this worker: deliver it with \
                 wasmer::js::prepare_shared_object_message and call \
                 receive_shared_object_message before attaching it (or use \
                 export_shared_objects/import_shared_objects)"
                    .into(),
            )
        })?;
        Ok(VMMemory::new(memory, self.ty))
    }
}

#[cfg(test)]
mod tests {
    use super::{VMMemory, VMSharedMemory};
    use crate::js::JsSharedMemoryExt;
    use crate::{Memory, MemoryError, MemoryType, Pages, Store};
    use wasm_bindgen_test::wasm_bindgen_test;

    fn memory_type(shared: bool) -> MemoryType {
        MemoryType::new(
            Pages::ONE,
            Some(Pages::try_from_pages(4).unwrap()),
            shared,
        )
    }

    #[wasm_bindgen_test]
    fn nonshared_memory_cannot_be_detached() {
        let mut store = Store::default();
        let ty = memory_type(false);
        let source = Memory::new(&mut store, ty).unwrap();
        assert!(matches!(
            source.copy(&store),
            Err(MemoryError::MemoryNotShared)
        ));
        assert!(source.as_shared(&store).is_none());
        let js = crate::js::memory::Memory::js_memory_from_type(&ty).unwrap();
        let vm = VMMemory::new(js.clone(), ty);
        assert!(matches!(
            crate::vm::VMMemory::Js(vm).as_shared(),
            Err(MemoryError::MemoryNotShared)
        ));
        // Incorrect metadata must not turn a non-shared JS object into a shared handle.
        let mislabeled = VMMemory::new(js, memory_type(true));
        assert!(matches!(
            VMSharedMemory::try_from(mislabeled),
            Err(MemoryError::MemoryNotShared)
        ));
    }

    #[wasm_bindgen_test]
    fn undelivered_shared_memory_fails_to_attach() {
        let mut store = Store::default();
        let source = Memory::new(&mut store, memory_type(true)).unwrap();
        let shared = source.as_shared(&store).unwrap();
        assert!(shared.local_js_memory().is_some());
        crate::js::utils::shared_handle::forget_local_objects();
        assert!(shared.local_js_memory().is_none());
        assert!(matches!(
            shared.try_attach(&mut store),
            Err(MemoryError::Generic(_))
        ));
    }

    #[wasm_bindgen_test]
    fn shared_copy_allocates_independent_shared_memory() {
        let mut store = Store::default();
        let source = Memory::new(&mut store, memory_type(true)).unwrap();
        source.grow(&mut store, Pages::ONE).unwrap();
        source.view(&store).write(0, &[41]).unwrap();
        let copy = source.copy(&store).unwrap().attach(&mut store);
        assert!(copy.ty(&store).shared);
        assert_eq!(copy.size(&store).pages(), 2);
        source.view(&store).write(0, &[99]).unwrap();
        let mut byte = [0];
        copy.view(&store).read(0, &mut byte).unwrap();
        assert_eq!(byte, [41]);
    }
}

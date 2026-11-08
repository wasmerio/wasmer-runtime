//! Data types, functions and traits for `v8` runtime's `Function` implementation.
use std::sync::Arc;

use crate::{
    AsStoreMut, AsStoreRef, Exports, Extern, Imports, InstantiationError, Module,
    backend::v8::bindings::*, v8::error::Trap, vm::VMExtern,
};

use super::check_isolate;
use crate::backend::v8::origin::{Handle, Origin};

#[derive(PartialEq, Eq)]
pub(crate) struct InstanceHandle(pub(crate) *mut wasm_instance_t);

unsafe impl Send for InstanceHandle {}
unsafe impl Sync for InstanceHandle {}

impl InstanceHandle {
    #[allow(clippy::result_large_err, clippy::unnecessary_mut_passed)]
    fn new(
        store: *mut wasm_store_t,
        module: *mut wasm_shared_module_t,
        mut externs: Vec<VMExtern>,
    ) -> Result<Self, InstantiationError> {
        let mut trap: *mut wasm_trap_t = std::ptr::null_mut() as _;

        let externs: Vec<_> = externs.into_iter().map(|v| v.unwrap_v_8().raw()).collect();

        let instance = unsafe {
            let mut imports = unsafe {
                let mut vec = Default::default();
                wasm_extern_vec_new(&mut vec, externs.len(), externs.as_ptr());
                vec
            };

            std::mem::forget(externs);

            wasm_instance_new(
                store,
                wasm_module_obtain(store, module),
                &mut imports,
                &mut trap,
            )
        };

        if instance.is_null() {
            let trap = Trap::from(trap);
            return Err(InstantiationError::Start(trap.into()));
        }

        Ok(Self(instance))
    }

    fn get_exports(&self, mut store: &mut impl AsStoreMut, module: &Module) -> Exports {
        check_isolate(store);
        let mut exports = unsafe {
            let mut vec = Default::default();
            wasm_instance_exports(self.0, &mut vec);
            vec
        };

        let wasm_exports: &[*mut wasm_extern_t] = if exports.size == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(exports.data, exports.size) }
        };

        let exports_ty = module.exports().collect::<Vec<_>>();
        exports_ty
            .iter()
            .zip(wasm_exports.iter())
            .map(|(export_type, wasm_export)| {
                let name = export_type.name();
                let mut store = store.as_store_mut();
                let _extern_type = export_type.ty();
                // Annotation is here to prevent spurious IDE warnings.

                let origin = Origin::of(&store).expect("actual V8 instance Store");
                let extern_ = Extern::from_vm_extern(
                    &mut store,
                    VMExtern::V8(Handle::new(*wasm_export, origin)),
                );
                (name.to_string(), extern_)
            })
            .collect::<Exports>()
    }
}
impl Drop for InstanceHandle {
    fn drop(&mut self) {
        unsafe { wasm_instance_delete(self.0) }
    }
}

#[derive(Clone, PartialEq, Eq)]
/// A WebAssembly `instance` in the `v8` runtime.
pub struct Instance {
    pub(crate) handle: Arc<InstanceHandle>,
}

impl Instance {
    #[allow(clippy::result_large_err)]
    pub(crate) fn new(
        store: &mut impl AsStoreMut,
        module: &Module,
        imports: &Imports,
    ) -> Result<(Self, Exports), InstantiationError> {
        check_isolate(store);
        let mut store = store.as_store_mut();

        let externs = imports
            .imports_for_module(module)
            .map_err(InstantiationError::Link)?;

        Self::new_by_index(&mut store, module, &externs)
    }

    #[allow(clippy::result_large_err)]
    pub(crate) fn new_by_index(
        store: &mut impl AsStoreMut,
        module: &Module,
        externs: &[Extern],
    ) -> Result<(Self, Exports), InstantiationError> {
        check_isolate(store);
        let store_ref = store.as_store_ref();
        if externs.iter().any(|value| {
            // Sys handles may share a numeric StoreId; that is not V8 origin.
            let is_v8 = match value {
                Extern::Function(value) => matches!(&value.0, crate::BackendFunction::V8(_)),
                Extern::Memory(value) => matches!(&value.0, crate::BackendMemory::V8(_)),
                Extern::Table(value) => matches!(&value.0, crate::BackendTable::V8(_)),
                Extern::Global(value) => matches!(&value.0, crate::BackendGlobal::V8(_)),
                Extern::Tag(value) => matches!(&value.0, crate::BackendTag::V8(_)),
            };
            !is_v8 || !value.is_from_store(store)
        }) {
            return Err(InstantiationError::DifferentStores);
        }
        let externs: Vec<VMExtern> = externs
            .iter()
            .map(|extern_| extern_.to_vm_extern())
            .collect::<Vec<_>>();
        let instance = InstanceHandle::new(
            store_ref.inner.store.as_v8().inner,
            module.as_v8().handle.v8_shared_module_handle,
            externs,
        )?;
        let exports = instance.get_exports(store, module);

        Ok((
            Self {
                handle: Arc::new(instance),
            },
            exports,
        ))
    }
}

impl crate::BackendInstance {
    /// Consume [`self`] into a [`crate::backend::v8::instance::Instance`].
    pub(crate) fn into_v8(self) -> crate::backend::v8::instance::Instance {
        match self {
            Self::V8(s) => s,
            _ => panic!("Not a `v8` instance"),
        }
    }
}

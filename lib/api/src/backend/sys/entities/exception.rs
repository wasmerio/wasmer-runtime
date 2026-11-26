//! Data types, functions and traits for `sys` runtime's `Exception` implementation.

use wasmer_vm::StoreHandle;

use crate::{AsStoreMut, AsStoreRef, RuntimeError, Value};

#[derive(Debug, Clone, PartialEq, Eq)]
/// A WebAssembly `exnref` in the `sys` runtime.
pub(crate) struct Exception {
    exnref: wasmer_vm::VMExceptionRef,
}

impl Exception {
    /// Creates a new exception with the given tag and payload, and also creates
    /// a reference to it, returning the reference.
    pub fn new(
        store: &mut impl AsStoreMut,
        tag: &crate::sys::tag::Tag,
        payload: &[Value],
    ) -> Result<Self, RuntimeError> {
        if !tag.is_from_store(store) {
            return Err(RuntimeError::new(
                "the tag of an exception must come from its store",
            ));
        }

        let store_objects = store.as_store_ref().objects().as_sys();
        let tag_ty = tag.handle.get(store_objects).signature.params();
        if tag_ty.len() != payload.len() {
            return Err(RuntimeError::new(format!(
                "the tag takes {} values, but the payload has {}",
                tag_ty.len(),
                payload.len()
            )));
        }
        for (i, (value, ty)) in payload.iter().zip(tag_ty).enumerate() {
            if value.ty() != *ty {
                return Err(RuntimeError::new(format!(
                    "payload value {i} is of type {}, but the tag takes {ty}",
                    value.ty()
                )));
            }
            // A reference is stored raw, so it must be one of this store.
            if !value.is_from_store(store) {
                return Err(RuntimeError::new(format!(
                    "payload value {i} comes from another store"
                )));
            }
            if !value.is_funcref_usable(store) {
                return Err(crate::entities::function::dynamic_funcref_error());
            }
        }

        let values = payload
            .iter()
            .map(|value| value.as_raw(store))
            .collect::<Vec<_>>()
            .into_boxed_slice();

        let ctx = store.objects_mut().as_sys_mut();
        let exception = wasmer_vm::VMExceptionObj::new(tag.handle.internal_handle(), values);
        let exn_handle = wasmer_vm::StoreHandle::new(ctx, exception);
        let exnref = wasmer_vm::VMExceptionRef(exn_handle);

        Ok(Self { exnref })
    }

    pub fn exnref(&self) -> wasmer_vm::VMExceptionRef {
        self.exnref.clone()
    }

    pub fn from_exnref(exnref: wasmer_vm::VMExceptionRef) -> Self {
        Self { exnref }
    }

    pub fn is_from_store(&self, store: &impl AsStoreRef) -> bool {
        self.exnref.0.store_id() == store.as_store_ref().objects().id()
    }

    pub fn tag(&self, store: &impl AsStoreRef) -> crate::sys::tag::Tag {
        if !self.is_from_store(store) {
            panic!("Exception is from another Store");
        }
        let ctx = store.as_store_ref().objects().as_sys();
        let exception = self.exnref.0.get(ctx);
        let tag_handle = exception.tag();
        crate::sys::tag::Tag {
            handle: unsafe { StoreHandle::from_internal(ctx.id(), tag_handle) },
        }
    }

    pub fn payload(&self, store: &mut impl AsStoreMut) -> Vec<Value> {
        if !self.is_from_store(store) {
            panic!("Exception is from another Store");
        }
        let ctx = store.as_store_ref().objects().as_sys();
        let exception = self.exnref.0.get(ctx);
        let params_ty = exception.tag().get(ctx).signature.params().to_vec();
        let payload_ptr = exception.payload();

        assert_eq!(params_ty.len(), payload_ptr.len());

        params_ty
            .iter()
            .zip(unsafe { payload_ptr.as_ref().iter() })
            .map(|(ty, raw)| unsafe { Value::from_raw(store, *ty, *raw) })
            .collect()
    }
}

#[cfg(feature = "artifact-size")]
impl loupe::MemoryUsage for Exception {
    fn size_of_val(&self, tracker: &mut dyn loupe::MemoryUsageTracker) -> usize {
        std::mem::size_of::<Self>()
    }
}

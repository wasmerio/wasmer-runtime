//! Data types, functions and traits for `v8` runtime's `Tag` implementation.
use wasmer_types::{TagType, Type, StoreId};

use crate::{
    AsStoreMut, AsStoreRef, RuntimeError,
    v8::{
        bindings::*,
        utils::convert::{IntoCApiType, IntoWasmerType},
        vm::VMTag,
    },
    vm::{VMExtern, VMExternTag},
};

use super::{check_isolate, check_owner};

#[derive(Debug, Clone, PartialEq, Eq)]
/// A WebAssembly `tag` in the `v8` runtime.
pub(crate) struct Tag {
    pub(crate) handle: VMTag,
    /// The store it belongs to: V8 does not know.
    pub(crate) store_id: StoreId,
}

unsafe impl Send for Tag {}
unsafe impl Sync for Tag {}

impl Tag {
    pub fn new<P: Into<Box<[Type]>>>(
        store: &mut impl AsStoreMut,
        params: P,
    ) -> Result<Self, RuntimeError> {
        check_isolate(store);
        let store_mut = store.as_store_mut();
        let v8_store = store_mut.inner.store.as_v8();

        let params = Into::<Box<[Type]>>::into(params).into_vec();
        let params = params
            .into_iter()
            .map(|param| {
                let kind = param.into_ct();
                unsafe { wasm_valtype_new(kind) }
            })
            .collect::<Vec<_>>();

        let mut params = unsafe {
            let mut vec: wasm_valtype_vec_t = Default::default();
            wasm_valtype_vec_new(&mut vec, params.len(), params.as_ptr());
            vec
        };

        let tag_type = unsafe { wasm_tagtype_new(&mut params) };
        if tag_type.is_null() {
            return Err(RuntimeError::new("v8 could not make the type of the tag"));
        }

        let handle = unsafe { wasm_tag_new(v8_store.inner, tag_type) };
        if handle.is_null() {
            return Err(RuntimeError::new("v8 could not make the tag"));
        }

        Ok(Self {
            handle,
            store_id: store_mut.inner.objects.id(),
        })
    }

    pub fn ty(&self, store: &impl AsStoreRef) -> TagType {
        check_owner(store, self.store_id);
        let type_ = unsafe { wasm_tag_type(self.handle) };
        let params: *const wasm_valtype_vec_t = unsafe { wasm_tagtype_params(type_) };

        let params: Vec<wasmer_types::Type> = unsafe {
            let mut res = vec![];
            for i in 0..(*params).size {
                res.push((*(*params).data.wrapping_add(i)).into_wt());
            }
            res
        };

        TagType::new(wasmer_types::TagKind::Exception, params)
    }

    pub(crate) fn from_vm_extern(store: &mut impl AsStoreMut, vm_tag: VMExternTag) -> Self {
        check_isolate(store);
        Self {
            handle: vm_tag.unwrap_v_8(),
            store_id: store.objects_mut().id(),
        }
    }

    pub fn is_from_store(&self, store: &impl AsStoreRef) -> bool {
        check_isolate(store);
        self.store_id == store.as_store_ref().objects().id()
    }

    pub(crate) fn to_vm_extern(&self) -> VMExtern {
        let extern_ = unsafe { wasm_tag_as_extern(self.handle) };
        assert!(
            !extern_.is_null(),
            "Returned null Tag extern from wasm-c-api"
        );

        VMExtern::V8(extern_)
    }
}

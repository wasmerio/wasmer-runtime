use wasmer_types::{TagType, Type};

use crate::{
    AsStoreMut, AsStoreRef, ExportError, Exportable, Extern, RuntimeError,
    macros::backend::{gen_rt_ty, match_rt},
    vm::{VMExtern, VMExternTag},
};

// The tag of each backend, behind [`crate::Tag`].
gen_rt_ty! {
    #[cfg_attr(feature = "artifact-size", derive(loupe::MemoryUsage))]
    #[derive(Debug, Clone, PartialEq, Eq, derive_more::From)]
    pub(crate) BackendTag(entities::tag::Tag);
}

impl BackendTag {
    /// Create a new tag with event of type P -> [], that is a function that takes parameters `P`
    /// and has no return value.
    #[inline]
    pub fn new<P: Into<Box<[Type]>>>(
        store: &mut impl AsStoreMut,
        params: P,
    ) -> Result<Self, RuntimeError> {
        match &store.as_store_mut().inner.store {
            #[cfg(feature = "sys")]
            crate::BackendStore::Sys(_) => {
                Ok(Self::Sys(crate::backend::sys::tag::Tag::new(store, params)))
            }
            #[cfg(feature = "v8")]
            crate::BackendStore::V8(_) => {
                crate::backend::v8::entities::tag::Tag::new(store, params).map(Self::V8)
            }
            #[cfg(feature = "js")]
            crate::BackendStore::Js(_) => {
                crate::backend::js::tag::Tag::new(store, params).map(Self::Js)
            }
        }
    }

    /// Returns the [`TagType`] of the tag.
    #[inline]
    pub fn ty(&self, store: &impl AsStoreRef) -> TagType {
        match_rt!(on self => f {
            f.ty(store)
        })
    }

    #[inline]
    pub(crate) fn from_vm_extern(store: &mut impl AsStoreMut, vm_extern: VMExternTag) -> Self {
        match &store.as_store_mut().inner.store {
            #[cfg(feature = "sys")]
            crate::BackendStore::Sys(_) => Self::Sys(
                crate::backend::sys::tag::Tag::from_vm_extern(store, vm_extern),
            ),
            #[cfg(feature = "v8")]
            crate::BackendStore::V8(_) => Self::V8(
                crate::backend::v8::entities::tag::Tag::from_vm_extern(store, vm_extern),
            ),
            #[cfg(feature = "js")]
            crate::BackendStore::Js(_) => Self::Js(
                crate::backend::js::entities::tag::Tag::from_vm_extern(store, vm_extern),
            ),
        }
    }

    /// Checks whether this tag can be used with the given context.
    #[inline]
    pub fn is_from_store(&self, store: &impl AsStoreRef) -> bool {
        match_rt!(on self => f {
            f.is_from_store(store)
        })
    }

    /// Create a [`VMExtern`] from self.
    #[inline]
    pub(crate) fn to_vm_extern(&self) -> VMExtern {
        match_rt!(on self => f {
            f.to_vm_extern()
        })
    }
}

impl<'a> Exportable<'a> for BackendTag {
    fn get_self_from_extern(_extern: &'a Extern) -> Result<&'a Self, ExportError> {
        match _extern {
            Extern::Tag(func) => Ok(&func.0),
            _ => Err(ExportError::IncompatibleType),
        }
    }
}

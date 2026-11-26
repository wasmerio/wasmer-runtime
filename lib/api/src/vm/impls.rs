use crate::{AsStoreMut, macros::backend::match_rt};

use super::*;

impl VMExternToExtern for VMExtern {
    fn to_extern(self, store: &mut impl crate::AsStoreMut) -> crate::Extern {
        match_rt!(on self => s {
            s.to_extern(store)
        })
    }
}

impl VMFuncRef {
    /// Converts the `VMFuncRef` into a `RawValue`.
    pub fn into_raw(self) -> RawValue {
        match_rt!(on self => s {
            s.into_raw()
        })
    }
}

impl VMExternRef {
    /// Converts the `VMExternRef` into a `RawValue`.
    pub fn into_raw(self) -> RawValue {
        match_rt!(on self => s {
            s.into_raw()
        })
    }
}

impl VMMemory {
    /// Attempts to share this memory and return a shared detached memory.
    pub(crate) fn as_shared(&self) -> Result<VMSharedMemory, wasmer_types::MemoryError> {
        match self {
            #[cfg(feature = "sys")]
            Self::Sys(s) => s.0.as_shared().map(VMSharedMemory::Sys),
            #[cfg(feature = "v8")]
            Self::V8(s) => s.as_shared().map(VMSharedMemory::V8),
            #[cfg(feature = "js")]
            Self::Js(s) => s.try_clone()?.try_into().map(VMSharedMemory::Js),
        }
    }
}

impl VMSharedMemory {
    /// Clones this shared memory handle.
    pub(crate) fn clone(&self) -> Self {
        match self {
            #[cfg(feature = "sys")]
            Self::Sys(s) => Self::Sys(s.clone()),
            #[cfg(feature = "v8")]
            Self::V8(s) => Self::V8(s.clone()),
            #[cfg(feature = "js")]
            Self::Js(s) => Self::Js(s.clone()),
        }
    }

    #[cfg_attr(not(feature = "v8"), allow(unused_variables))]
    pub(crate) fn try_into_vm_memory(
        self,
        store: &mut impl AsStoreMut,
    ) -> Result<VMMemory, wasmer_types::MemoryError> {
        Ok(match self {
            #[cfg(feature = "sys")]
            Self::Sys(s) => VMMemory::Sys(s.into()),
            #[cfg(feature = "v8")]
            Self::V8(s) => {
                let store = store.as_store_mut();
                VMMemory::V8(s.into_vm_memory(store.inner.store.as_v8_mut()))
            }
            #[cfg(feature = "js")]
            Self::Js(s) => VMMemory::Js(s.try_attach()?),
        })
    }
}

impl VMExceptionRef {
    /// Converts the `VMExternRef` into a `RawValue`.
    pub fn into_raw(self) -> RawValue {
        match self {
            #[cfg(feature = "sys")]
            Self::Sys(s) => s.into_raw(),

            _ => unimplemented!("VMExceptionRef::into_raw is only implemented for the sys backend"),
        }
    }
}

use crate::{
    AsStoreMut, AsStoreRef, FunctionEnv, FunctionEnvMut, StoreMut, StoreRef,
    macros::backend::match_rt,
};
use std::any::Any;

#[derive(Debug, derive_more::From)]
/// An opaque reference to a function environment.
/// The function environment data is owned by the `Store`.
pub enum BackendFunctionEnv<T> {
    #[cfg(feature = "sys")]
    /// The function environment for the `sys` runtime.
    Sys(crate::backend::sys::function::env::FunctionEnv<T>),
    #[cfg(feature = "v8")]
    /// The function environment for the `v8` runtime.
    V8(crate::backend::v8::function::env::FunctionEnv<T>),
    #[cfg(feature = "js")]
    /// The function environment for the `js` runtime.
    Js(crate::backend::js::function::env::FunctionEnv<T>),
}

impl<T> Clone for BackendFunctionEnv<T> {
    fn clone(&self) -> Self {
        match self {
            #[cfg(feature = "sys")]
            Self::Sys(s) => Self::Sys(s.clone()),
            #[cfg(feature = "v8")]
            Self::V8(s) => Self::V8(s.clone()),
            #[cfg(feature = "js")]
            Self::Js(s) => Self::Js(s.clone()),
        }
    }
}

impl<T> BackendFunctionEnv<T> {
    /// Make a new FunctionEnv
    pub fn new(store: &mut impl AsStoreMut, value: T) -> Self
    where
        T: Any + Send + 'static + Sized,
    {
        match store.as_store_mut().inner.store {
            #[cfg(feature = "sys")]
            crate::BackendStore::Sys(_) => Self::Sys(
                crate::backend::sys::function::env::FunctionEnv::new(store, value),
            ),

            #[cfg(feature = "v8")]
            crate::BackendStore::V8(_) => Self::V8(
                crate::backend::v8::function::env::FunctionEnv::new(store, value),
            ),

            #[cfg(feature = "js")]
            crate::BackendStore::Js(_) => Self::Js(
                crate::backend::js::function::env::FunctionEnv::new(store, value),
            ),
        }
    }

    /// Get the data as reference
    pub fn as_ref<'a>(&self, store: &'a impl AsStoreMut) -> &'a T
    where
        T: Any + Send + 'static + Sized,
    {
        match_rt!(on self => f {
            f.as_ref(store)
        })
    }

    /// Get the data as mutable
    pub fn as_mut<'a>(&self, store: &'a mut impl AsStoreMut) -> &'a mut T
    where
        T: Any + Send + 'static + Sized,
    {
        match_rt!(on self => s {
            s.as_mut(store)
        })
    }

    /// Convert it into a `FunctionEnvMut`
    pub fn into_mut(self, store: &mut impl AsStoreMut) -> FunctionEnvMut<'_, T>
    where
        T: Any + Send + 'static + Sized,
    {
        match_rt!(on self => f {
            f.into_mut(store).into()
        })
    }

    /// The store this environment is in.
    #[cfg(feature = "async")]
    pub(crate) fn store_id(&self) -> wasmer_types::StoreId {
        match_rt!(on self => f {
            f.handle.store_id()
        })
    }

    /// A `FunctionEnvMut` of this environment on `store_mut`.
    #[cfg(feature = "async")]
    pub(crate) fn on_store<'s>(&self, store_mut: StoreMut<'s>) -> FunctionEnvMut<'s, T> {
        match self {
            #[cfg(feature = "sys")]
            Self::Sys(func_env) => crate::backend::sys::function::env::FunctionEnvMut {
                store_mut,
                func_env: func_env.clone(),
            }
            .into(),
            #[cfg(feature = "v8")]
            Self::V8(func_env) => crate::backend::v8::function::env::FunctionEnvMut {
                store_mut,
                func_env: func_env.clone(),
            }
            .into(),
            #[cfg(feature = "js")]
            Self::Js(func_env) => crate::backend::js::function::env::FunctionEnvMut {
                store_mut,
                func_env: func_env.clone(),
            }
            .into(),
        }
    }
}

/// A temporary handle to a [`FunctionEnv`].
#[derive(derive_more::From)]
pub enum BackendFunctionEnvMut<'a, T: 'a> {
    #[cfg(feature = "sys")]
    /// The function environment for the `sys` runtime.
    Sys(crate::backend::sys::function::env::FunctionEnvMut<'a, T>),

    #[cfg(feature = "v8")]
    /// The function environment for the `v8` runtime.
    V8(crate::backend::v8::function::env::FunctionEnvMut<'a, T>),

    #[cfg(feature = "js")]
    /// The function environment for the `js` runtime.
    Js(crate::backend::js::function::env::FunctionEnvMut<'a, T>),
}

impl<T: Send + 'static> BackendFunctionEnvMut<'_, T> {
    /// Returns a reference to the host state in this function environment.
    pub fn data(&self) -> &T {
        match_rt!(on self => f {
            f.data()
        })
    }

    /// Returns a mutable- reference to the host state in this function environment.
    pub fn data_mut(&mut self) -> &mut T {
        match_rt!(on self => f {
            f.data_mut()
        })
    }

    /// Borrows a new immmutable reference
    pub fn as_ref(&self) -> FunctionEnv<T> {
        match self {
            #[cfg(feature = "sys")]
            Self::Sys(f) => BackendFunctionEnv::Sys(f.as_ref()).into(),
            #[cfg(feature = "v8")]
            Self::V8(f) => BackendFunctionEnv::V8(f.as_ref()).into(),
            #[cfg(feature = "js")]
            Self::Js(f) => BackendFunctionEnv::Js(f.as_ref()).into(),
        }
    }

    /// Borrows a new mutable reference
    pub fn as_mut(&mut self) -> FunctionEnvMut<'_, T> {
        match self {
            #[cfg(feature = "sys")]
            Self::Sys(f) => BackendFunctionEnvMut::Sys(f.as_mut()).into(),
            #[cfg(feature = "v8")]
            Self::V8(f) => BackendFunctionEnvMut::V8(f.as_mut()).into(),
            #[cfg(feature = "js")]
            Self::Js(f) => BackendFunctionEnvMut::Js(f.as_mut()).into(),
        }
    }

    /// Borrows the host state mutably and the rest of the store immutably.
    pub fn data_and_store(&mut self) -> (&mut T, StoreRef<'_>) {
        match_rt!(on self => f {
            f.data_and_store()
        })
    }
}

impl<T> AsStoreRef for BackendFunctionEnvMut<'_, T> {
    fn as_store_ref(&self) -> StoreRef<'_> {
        match_rt!(on self => s {
            s.as_store_ref()
        })
    }
}

impl<T> AsStoreMut for BackendFunctionEnvMut<'_, T> {
    fn as_store_mut(&mut self) -> StoreMut<'_> {
        match_rt!(on self => s {
            s.as_store_mut()
        })
    }

    fn objects_mut(&mut self) -> &mut crate::StoreObjects {
        match_rt!(on self => s {
            s.objects_mut()
        })
    }
}

impl<T> std::fmt::Debug for BackendFunctionEnvMut<'_, T>
where
    T: Send + std::fmt::Debug + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match_rt!(on self => s {
            write!(f, "{s:?}")
        })
    }
}

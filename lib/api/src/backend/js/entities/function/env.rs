use std::{any::Any, fmt::Debug, marker::PhantomData};

use crate::{
    StoreMut,
    js::{store::StoreHandle, vm::VMFunctionEnvironment},
    store::{AsStoreMut, AsStoreRef, StoreRef},
};

#[derive(Debug)]
#[repr(transparent)]
/// An opaque reference to a function environment.
/// The function environment data is owned by the `Store`.
pub struct FunctionEnv<T> {
    pub(crate) handle: StoreHandle<VMFunctionEnvironment>,
    marker: PhantomData<T>,
}

impl<T> FunctionEnv<T> {
    /// Make a new FunctionEnv
    pub fn new(store: &mut impl AsStoreMut, value: T) -> Self
    where
        T: Any + Send + 'static + Sized,
    {
        Self {
            handle: StoreHandle::new(
                store.as_store_mut().objects_mut().as_js_mut(),
                VMFunctionEnvironment::new(value),
            ),
            marker: PhantomData,
        }
    }

    /// Get the data as reference
    ///
    /// This needs a mutable store view, because a [`StoreRef`] can coexist with
    /// the `&mut T` returned by [`FunctionEnvMut::data_and_store`].
    pub fn as_ref<'a>(&self, store: &'a impl AsStoreMut) -> &'a T
    where
        T: Any + 'static + Sized,
    {
        self.handle
            .get(store.as_store_ref().host_state_objects().as_js())
            .as_ref()
            .downcast_ref::<T>()
            .unwrap()
    }

    pub(crate) fn from_handle(handle: StoreHandle<VMFunctionEnvironment>) -> Self {
        Self {
            handle,
            marker: PhantomData,
        }
    }

    /// Get the data as mutable
    pub fn as_mut<'a>(&self, store: &'a mut impl AsStoreMut) -> &'a mut T
    where
        T: Any + 'static + Sized,
    {
        self.handle
            .get_mut(store.objects_mut().as_js_mut())
            .as_mut()
            .downcast_mut::<T>()
            .unwrap()
    }

    /// Convert it into a `FunctionEnvMut`
    pub fn into_mut(self, store: &mut impl AsStoreMut) -> FunctionEnvMut<'_, T>
    where
        T: Any + 'static + Sized,
    {
        FunctionEnvMut {
            store_mut: store.as_store_mut(),
            func_env: self,
        }
    }
}

impl<T> PartialEq for FunctionEnv<T> {
    fn eq(&self, other: &Self) -> bool {
        self.handle == other.handle
    }
}

impl<T> Eq for FunctionEnv<T> {}

impl<T> std::hash::Hash for FunctionEnv<T> {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.handle.hash(state);
        self.marker.hash(state);
    }
}

impl<T> Clone for FunctionEnv<T> {
    fn clone(&self) -> Self {
        Self {
            handle: self.handle.clone(),
            marker: self.marker,
        }
    }
}

/// A temporary handle to a [`FunctionEnv`].
pub struct FunctionEnvMut<'a, T: 'a> {
    pub(crate) store_mut: StoreMut<'a>,
    pub(crate) func_env: FunctionEnv<T>,
}

impl<T> Debug for FunctionEnvMut<'_, T>
where
    T: Send + Debug + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.func_env.as_ref(&self.store_mut).fmt(f)
    }
}

impl<T: Send + 'static> FunctionEnvMut<'_, T> {
    /// Returns a reference to the host state in this function environment.
    pub fn data(&self) -> &T {
        self.func_env.as_ref(&self.store_mut)
    }

    /// Returns a mutable- reference to the host state in this function environment.
    pub fn data_mut(&mut self) -> &mut T {
        self.func_env.as_mut(&mut self.store_mut)
    }

    /// Borrows a new immmutable reference
    pub fn as_ref(&self) -> FunctionEnv<T> {
        self.func_env.clone()
    }

    /// Borrows a new mutable reference
    pub fn as_mut(&mut self) -> FunctionEnvMut<'_, T> {
        FunctionEnvMut {
            store_mut: self.store_mut.as_store_mut(),
            func_env: self.func_env.clone(),
        }
    }

    /// Borrows the host state mutably and the rest of the store immutably.
    pub fn data_and_store(&mut self) -> (&mut T, StoreRef<'_>) {
        let data = self.func_env.as_mut(&mut self.store_mut) as *mut T;
        // SAFETY: the host state lives in its own heap allocation in the
        // store. A `StoreRef` cannot reach it, because `FunctionEnv::as_ref`
        // requires a mutable store view.
        let data = unsafe { &mut *data };
        (data, self.store_mut.as_store_ref().view_only())
    }
}

impl<T> AsStoreRef for FunctionEnvMut<'_, T> {
    fn as_store_ref(&self) -> StoreRef<'_> {
        self.store_mut.as_store_ref()
    }
}

impl<T> AsStoreMut for FunctionEnvMut<'_, T> {
    fn as_store_mut(&mut self) -> StoreMut<'_> {
        self.store_mut.as_store_mut()
    }

    fn objects_mut(&mut self) -> &mut crate::StoreObjects {
        self.store_mut.objects_mut()
    }
}

impl<T> crate::FunctionEnv<T> {
    /// Consume [`self`] into [`crate::backend::js::function::env::FunctionEnv`].
    pub fn into_js(self) -> FunctionEnv<T> {
        match self.0 {
            crate::BackendFunctionEnv::Js(s) => s,
            _ => panic!("Not a `js` function env!"),
        }
    }

    /// Convert a reference to [`self`] into a reference to [`crate::backend::js::function::env::FunctionEnv`].
    pub fn as_js(&self) -> &FunctionEnv<T> {
        match self.0 {
            crate::BackendFunctionEnv::Js(ref s) => s,
            _ => panic!("Not a `js` function env!"),
        }
    }

    /// Convert a mutable reference to [`self`] into a mutable reference [`crate::backend::js::function::env::FunctionEnv`].
    pub fn as_js_mut(&mut self) -> &mut FunctionEnv<T> {
        match self.0 {
            crate::BackendFunctionEnv::Js(ref mut s) => s,
            _ => panic!("Not a `js` function env!"),
        }
    }
}

impl<'a, T> From<FunctionEnvMut<'a, T>> for crate::FunctionEnvMut<'a, T> {
    fn from(value: FunctionEnvMut<'a, T>) -> Self {
        crate::FunctionEnvMut(crate::BackendFunctionEnvMut::Js(value))
    }
}

impl<T> From<FunctionEnv<T>> for crate::FunctionEnv<T> {
    fn from(value: FunctionEnv<T>) -> Self {
        Self(crate::BackendFunctionEnv::Js(value))
    }
}

pub(crate) mod inner;
pub(crate) use inner::*;

#[cfg(feature = "experimental-async")]
use crate::SharedStore;
use crate::{AsStoreMut, AsStoreRef, StoreMut, StoreRef};
use std::any::Any;

#[derive(Debug, derive_more::From)]
/// An opaque reference to a function environment.
/// The function environment data is owned by the `Store`.
pub struct FunctionEnv<T>(pub(crate) BackendFunctionEnv<T>);

impl<T> Clone for FunctionEnv<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> FunctionEnv<T> {
    /// Make a new FunctionEnv
    pub fn new(store: &mut impl AsStoreMut, value: T) -> Self
    where
        T: Any + Send + 'static + Sized,
    {
        Self(BackendFunctionEnv::new(store, value))
    }

    //#[allow(dead_code)] // This function is only used in js
    //pub(crate) fn from_handle(handle: StoreHandle<VMFunctionEnvironment>) -> Self {
    //    todo!()
    //}

    /// Get the data as reference.
    ///
    /// This needs a mutable store view: a [`StoreRef`] can coexist with the
    /// `&mut T` returned by [`FunctionEnvMut::data_and_store`], and must not
    /// reach `T`.
    pub fn as_ref<'a>(&self, store: &'a impl AsStoreMut) -> &'a T
    where
        T: Any + Send + 'static + Sized,
    {
        self.0.as_ref(store)
    }

    /// Get the data as mutable
    pub fn as_mut<'a>(&self, store: &'a mut impl AsStoreMut) -> &'a mut T
    where
        T: Any + Send + 'static + Sized,
    {
        self.0.as_mut(store)
    }

    /// Convert it into a `FunctionEnvMut`
    pub fn into_mut(self, store: &mut impl AsStoreMut) -> FunctionEnvMut<'_, T>
    where
        T: Any + Send + 'static + Sized,
    {
        self.0.into_mut(store)
    }
}

/// A temporary handle to a [`FunctionEnv`].
#[derive(derive_more::From)]
pub struct FunctionEnvMut<'a, T: 'a>(pub(crate) BackendFunctionEnvMut<'a, T>);

impl<T: Send + 'static> FunctionEnvMut<'_, T> {
    /// Returns a reference to the host state in this function environment.
    pub fn data(&self) -> &T {
        self.0.data()
    }

    /// Returns a mutable- reference to the host state in this function environment.
    pub fn data_mut(&mut self) -> &mut T {
        self.0.data_mut()
    }

    /// Borrows a new immmutable reference
    pub fn as_ref(&self) -> FunctionEnv<T> {
        self.0.as_ref()
    }

    /// Borrows a new mutable reference
    pub fn as_mut(&mut self) -> FunctionEnvMut<'_, T> {
        self.0.as_mut()
    }

    /// Borrows the host state mutably and the rest of the store immutably.
    ///
    /// The [`StoreRef`] can read memories, globals and tables, but cannot reach
    /// `T` or run a guest. Clone a handle out of [`Self::data`] first when an
    /// operation needs a mutable store.
    pub fn data_and_store(&mut self) -> (&mut T, StoreRef<'_>) {
        self.0.data_and_store()
    }

    /// A handle to this environment for a future to keep, as the one of
    /// [`Deferred::pending`](crate::Deferred::pending) does. It holds no
    /// borrow of the store, and reaches it with
    /// [`AsyncEnv::with`](crate::AsyncEnv::with) while its call is polled.
    #[cfg(feature = "experimental-async")]
    pub fn async_env(&self) -> crate::AsyncEnv<T> {
        crate::AsyncEnv::new(self.as_ref(), self.as_store_ref().objects().id())
    }

    /// The store's shared handle, if it has one: see [`Store::into_shared`](crate::Store::into_shared).
    #[cfg(feature = "experimental-async")]
    pub fn shared(&self) -> Option<SharedStore> {
        self.as_store_ref().inner.shared.upgrade().map(SharedStore)
    }

    /// Lets a [`SharedStore`] handle use the store while `f` runs.
    ///
    /// This borrow is unreachable until `f` returns, so the store can be lent
    /// to code that holds no reference to it. Inside `f`,
    /// [`SharedStore::try_with`] succeeds once at a time.
    #[cfg(feature = "experimental-async")]
    pub fn release<R>(&mut self, f: impl FnOnce() -> R) -> R {
        self.as_store_mut().parked(f)
    }
}

impl<T> AsStoreRef for FunctionEnvMut<'_, T> {
    fn as_store_ref(&self) -> StoreRef<'_> {
        self.0.as_store_ref()
    }
}

impl<T> AsStoreMut for FunctionEnvMut<'_, T> {
    fn as_store_mut(&mut self) -> StoreMut<'_> {
        self.0.as_store_mut()
    }

    fn objects_mut(&mut self) -> &mut crate::StoreObjects {
        self.0.objects_mut()
    }
}

impl<T> std::fmt::Debug for FunctionEnvMut<'_, T>
where
    T: Send + std::fmt::Debug + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

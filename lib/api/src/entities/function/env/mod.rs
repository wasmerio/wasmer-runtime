pub(crate) mod inner;
pub(crate) use inner::*;

use crate::{AsStoreMut, AsStoreRef, StoreMut, StoreRef, macros::backend::match_rt};
use std::{any::Any, fmt::Debug, marker::PhantomData};

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

    /// Get the data as reference
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

    /// Borrows the host state mutably and the store immutably.
    ///
    /// The store can inspect memories and function types, but cannot access
    /// another environment or call a guest while the host state is borrowed.
    /// Clone any handles needed to mutate the store before borrowing the state.
    ///
    /// A second mutable borrow of the same state is rejected:
    ///
    /// ```compile_fail,E0277
    /// use wasmer::{FunctionEnv, Store};
    /// let mut store = Store::default();
    /// let handle = FunctionEnv::new(&mut store, 0i32);
    /// let mut env = handle.clone().into_mut(&mut store);
    /// let (data, mut store) = env.data_and_store();
    /// let other = handle.as_mut(&mut store);
    /// *data += *other;
    /// ```
    ///
    /// The store cannot read the borrowed state either:
    ///
    /// ```compile_fail,E0277
    /// use wasmer::{FunctionEnv, Store};
    /// let mut store = Store::default();
    /// let handle = FunctionEnv::new(&mut store, 0i32);
    /// let mut env = handle.clone().into_mut(&mut store);
    /// let (data, store) = env.data_and_store();
    /// let other = handle.as_ref(&store);
    /// *data += *other;
    /// ```
    ///
    /// Guest calls, which could re-enter the environment, are rejected:
    ///
    /// ```compile_fail,E0277
    /// use wasmer::{Function, FunctionEnv, Store};
    /// let mut store = Store::default();
    /// let function = Function::new_typed(&mut store, || {});
    /// let handle = FunctionEnv::new(&mut store, 0i32);
    /// let mut env = handle.into_mut(&mut store);
    /// let (data, mut store) = env.data_and_store();
    /// function.call(&mut store, &[]).unwrap();
    /// *data += 1;
    /// ```
    pub fn data_and_store(&mut self) -> (&mut T, StoreRef<'_>) {
        self.0.data_and_store()
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

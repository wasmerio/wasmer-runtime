use std::{
    any::Any,
    fmt::Debug,
    marker::PhantomData,
    sync::{Arc, Weak},
};

#[cfg(feature = "experimental-async")]
use crate::{AsStoreAsync, StoreAsync, StoreAsyncReadLock, StoreAsyncWriteLock};
#[cfg(feature = "experimental-async")]
use wasmer_types::StoreId;

use crate::{
    StoreMut,
    store::{AsStoreMut, AsStoreRef, StoreRef},
    v8::{store::StoreHandle, vm::VMFunctionEnvironment},
};

#[derive(Debug)]
/// An opaque reference to a function environment.
/// The function environment data is owned by the `Store`.
pub struct FunctionEnv<T> {
    pub(crate) handle: StoreHandle<VMFunctionEnvironment>,
    pub(crate) identity: Weak<()>,
    marker: PhantomData<T>,
}

impl<T> FunctionEnv<T> {
    /// Make a new FunctionEnv
    pub fn new(store: &mut impl AsStoreMut, value: T) -> Self
    where
        T: Any + Send + 'static + Sized,
    {
        let owner = VMFunctionEnvironment::new(value);
        let identity = Arc::downgrade(&owner.identity);
        Self {
            handle: StoreHandle::new(store.objects_mut().as_v8_mut(), owner),
            identity,
            marker: PhantomData,
        }
    }

    /// Get the data as reference
    pub fn as_ref<'a>(&self, store: &'a impl AsStoreRef) -> &'a T
    where
        T: Any + 'static + Sized,
    {
        let owner = self.handle.get(store.as_store_ref().objects().as_v8());
        assert!(
            owner.matches(&self.identity),
            "V8 function environment was replaced"
        );
        owner.as_ref().downcast_ref::<T>().unwrap()
    }

    pub(crate) fn from_handle(
        handle: StoreHandle<VMFunctionEnvironment>,
        identity: Weak<()>,
    ) -> Self {
        Self {
            handle,
            identity,
            marker: PhantomData,
        }
    }

    /// Get the data as mutable.
    pub fn as_mut<'a>(&self, store: &'a mut impl AsStoreMut) -> &'a mut T
    where
        T: Any + 'static + Sized,
    {
        let owner = self.handle.get_mut(store.objects_mut().as_v8_mut());
        assert!(
            owner.matches(&self.identity),
            "V8 function environment was replaced"
        );
        owner.as_mut().downcast_mut::<T>().unwrap()
    }

    fn lend(&self, store: &mut StoreMut<'_>) -> Box<dyn Any + Send> {
        let owner = self.handle.get_mut(store.objects_mut().as_v8_mut());
        assert!(
            owner.matches(&self.identity),
            "V8 function environment was replaced"
        );
        owner
            .contents
            .take()
            .expect("V8 function environment is already lent")
    }

    fn restore(
        &self,
        store: &mut StoreMut<'_>,
        value: Box<dyn Any + Send>,
    ) -> Result<(), Box<dyn Any + Send>> {
        let crate::StoreObjects::V8(objects) = &mut store.inner.objects else {
            return Err(value);
        };
        if let Some(owner) = self.handle.try_get_mut(objects)
            && owner.matches(&self.identity)
            && owner.contents.is_none()
        {
            owner.contents = Some(value);
            return Ok(());
        }
        Err(value)
    }

    /// Convert it into a `FunctionEnvMut`
    pub fn into_mut<'a>(self, store: &'a mut impl AsStoreMut) -> FunctionEnvMut<'a, T>
    where
        T: Any + 'static + Sized,
    {
        FunctionEnvMut::new(store.as_store_mut(), self)
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
            identity: self.identity.clone(),
            marker: self.marker,
        }
    }
}

/// A temporary handle to a [`FunctionEnv`].
pub struct FunctionEnvMut<'a, T: 'a> {
    pub(crate) store_mut: Option<StoreMut<'a>>,
    pub(crate) func_env: FunctionEnv<T>,
    lease: Option<SplitLease<'a>>,
    #[cfg(feature = "experimental-async")]
    retirement: Option<crate::backend::v8::async_runtime::RetirementCap>,
}

enum SplitLease<'a> {
    Owned(Box<dyn Any + Send>),
    Borrowed(&'a mut Box<dyn Any + Send>),
}
impl SplitLease<'_> {
    fn value(&self) -> &(dyn Any + Send) {
        match self {
            Self::Owned(value) => &**value,
            Self::Borrowed(value) => &***value,
        }
    }
    fn value_mut(&mut self) -> &mut Box<dyn Any + Send> {
        match self {
            Self::Owned(value) => value,
            Self::Borrowed(value) => value,
        }
    }
}

impl<'a, T: 'static> FunctionEnvMut<'a, T> {
    fn new(store: StoreMut<'a>, func_env: FunctionEnv<T>) -> Self {
        #[cfg(feature = "experimental-async")]
        let retirement = crate::backend::v8::async_runtime::RetirementCap::capture(&store)
            .expect("V8 environment belongs to another write scope");
        Self {
            store_mut: Some(store),
            func_env,
            lease: None,
            #[cfg(feature = "experimental-async")]
            retirement,
        }
    }
    /// Returns the actual host data, including its exact separately lent Box.
    pub fn data(&self) -> &T {
        match &self.lease {
            Some(lease) => lease.value().downcast_ref().unwrap(),
            None => self.func_env.as_ref(self.store_mut.as_ref().unwrap()),
        }
    }
    /// Returns mutable host data.
    pub fn data_mut(&mut self) -> &mut T {
        match &mut self.lease {
            Some(lease) => lease.value_mut().downcast_mut().unwrap(),
            None => self.func_env.as_mut(self.store_mut.as_mut().unwrap()),
        }
    }
    /// Borrows the environment handle without gaining independent data access.
    pub fn as_ref(&self) -> FunctionEnv<T> {
        self.func_env.clone()
    }
    #[cfg(feature = "experimental-async")]
    pub(crate) fn checked_async(
        &self,
    ) -> Option<crate::backend::v8::function::env::AsyncFunctionEnvMut<T>> {
        let store = self.store_mut.as_ref()?;
        let crate::StoreObjects::V8(objects) = &store.inner.objects else {
            return None;
        };
        if self.func_env.handle.store_id() != objects.id() {
            return None;
        }
        let owner = self.func_env.handle.try_get(objects)?;
        if !owner.matches(&self.func_env.identity) {
            return None;
        }
        let cap = store.inner.store.as_v8().async_binding.store().ok()?;
        Some(AsyncFunctionEnvMut {
            store: cap,
            func_env: self.func_env.clone(),
            import_snapshot: None,
        })
    }

    /// Reborrow this frame and its SAME unique split lease/captured guard.
    pub fn as_mut(&mut self) -> FunctionEnvMut<'_, T> {
        FunctionEnvMut {
            store_mut: Some(self.store_mut.as_mut().unwrap().as_store_mut()),
            func_env: self.func_env.clone(),
            lease: self
                .lease
                .as_mut()
                .map(|lease| SplitLease::Borrowed(lease.value_mut())),
            #[cfg(feature = "experimental-async")]
            retirement: self.retirement.clone(),
        }
    }
    /// Lends the stable data Box independently from the mutable Store. The
    /// original slot remains None until this exact lease returns.
    pub fn data_and_store_mut(&mut self) -> (&mut T, StoreMut<'_>) {
        if self.lease.is_none() {
            self.lease = Some(SplitLease::Owned(
                self.func_env.lend(self.store_mut.as_mut().unwrap()),
            ));
        }
        let data = self
            .lease
            .as_mut()
            .unwrap()
            .value_mut()
            .downcast_mut()
            .unwrap();
        (data, self.store_mut.as_mut().unwrap().as_store_mut())
    }
}

impl<T> Drop for FunctionEnvMut<'_, T> {
    fn drop(&mut self) {
        let Some(SplitLease::Owned(value)) = self.lease.take() else {
            return;
        };
        let original = self
            .func_env
            .restore(self.store_mut.as_mut().unwrap(), value);
        if let Err(value) = original {
            // End this frame's real Store subborrow BEFORE any disposition.
            // Original write-cap transfer is metadata-only; it never resolves
            // a then-current/new guard or queries a dying Store's live token.
            drop(self.store_mut.take());
            #[cfg(feature = "experimental-async")]
            let value = match &self.retirement {
                Some(cap) => match cap.try_retain(value) {
                    Ok(()) => return,
                    Err(value) => value,
                },
                None => value,
            };
            dispose_lent(value);
        }
    }
}

fn dispose_lent(value: Box<dyn Any + Send>) {
    let unwinding = std::thread::panicking();
    if let Err(mut panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(value))) {
        if !unwinding {
            std::panic::resume_unwind(panic);
        }
        loop {
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(panic))) {
                Ok(()) => break,
                Err(next) => panic = next,
            }
        }
    }
}

impl<T: Debug + 'static> Debug for FunctionEnvMut<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.data().fmt(f)
    }
}

impl<T> AsStoreRef for FunctionEnvMut<'_, T> {
    fn as_store_ref(&self) -> StoreRef<'_> {
        StoreRef {
            inner: self.store_mut.as_ref().unwrap().inner,
        }
    }
}

impl<T> AsStoreMut for FunctionEnvMut<'_, T> {
    fn as_store_mut(&mut self) -> StoreMut<'_> {
        StoreMut {
            inner: self.store_mut.as_mut().unwrap().inner,
        }
    }

    fn objects_mut(&mut self) -> &mut crate::StoreObjects {
        self.store_mut.as_mut().unwrap().objects_mut()
    }
}

impl<T> crate::FunctionEnv<T> {
    /// Consume [`self`] into [`crate::backend::v8::function::env::FunctionEnv`].
    pub fn into_v8(self) -> FunctionEnv<T> {
        match self.0 {
            crate::BackendFunctionEnv::V8(s) => s,
            _ => panic!("Not a `v8` function env!"),
        }
    }

    /// Convert a reference to [`self`] into a reference to [`crate::backend::v8::function::env::FunctionEnv`].
    pub fn as_v8(&self) -> &FunctionEnv<T> {
        match self.0 {
            crate::BackendFunctionEnv::V8(ref s) => s,
            _ => panic!("Not a `v8` function env!"),
        }
    }

    /// Convert a mutable reference to [`self`] into a mutable reference [`crate::backend::v8::function::env::FunctionEnv`].
    pub fn as_v8_mut(&mut self) -> &mut FunctionEnv<T> {
        match self.0 {
            crate::BackendFunctionEnv::V8(ref mut s) => s,
            _ => panic!("Not a `v8` function env!"),
        }
    }
}

impl<'a, T> From<FunctionEnvMut<'a, T>> for crate::FunctionEnvMut<'a, T> {
    fn from(value: FunctionEnvMut<'a, T>) -> Self {
        crate::FunctionEnvMut(crate::BackendFunctionEnvMut::V8(value))
    }
}

impl<T> From<FunctionEnv<T>> for crate::FunctionEnv<T> {
    fn from(value: FunctionEnv<T>) -> Self {
        Self(crate::BackendFunctionEnv::V8(value))
    }
}

#[cfg(feature = "experimental-async")]
pub struct AsyncFunctionEnvMut<T> {
    pub(crate) store: StoreAsync,
    pub(crate) func_env: FunctionEnv<T>,
    pub(crate) import_snapshot: Option<std::sync::Arc<dyn Any + Send + Sync>>,
}

#[cfg(feature = "experimental-async")]
pub struct AsyncFunctionEnvHandle<T> {
    read_lock: StoreAsyncReadLock,
    pub(crate) func_env: FunctionEnv<T>,
}

#[cfg(feature = "experimental-async")]
pub struct AsyncFunctionEnvHandleMut<T> {
    write_lock: StoreAsyncWriteLock,
    pub(crate) func_env: FunctionEnv<T>,
    lease: Option<Box<dyn Any + Send>>,
    retirement: crate::backend::v8::async_runtime::RetirementCap,
}

#[cfg(feature = "experimental-async")]
impl<T> Clone for AsyncFunctionEnvMut<T> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.store(),
            func_env: self.func_env.clone(),
            import_snapshot: self.import_snapshot.clone(),
        }
    }
}

#[cfg(feature = "experimental-async")]
impl<T> Debug for AsyncFunctionEnvMut<T>
where
    T: Debug + 'static,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.store.inner.try_read() {
            Some(read_lock) => self.func_env.as_ref(&read_lock).fmt(f),
            None => write!(f, "AsyncFunctionEnvMut {{ <STORE LOCKED> }}"),
        }
    }
}

#[cfg(feature = "experimental-async")]
impl<T: 'static> AsyncFunctionEnvMut<T> {
    pub(crate) fn store_id(&self) -> StoreId {
        self.store.id
    }

    pub async fn read(&self) -> AsyncFunctionEnvHandle<T> {
        AsyncFunctionEnvHandle {
            read_lock: self.store.read_lock().await,
            func_env: self.func_env.clone(),
        }
    }

    pub async fn write(&self) -> AsyncFunctionEnvHandleMut<T> {
        AsyncFunctionEnvHandleMut::new(self.store.write_lock().await, self.func_env.clone())
    }

    pub fn try_read(&self) -> Option<AsyncFunctionEnvHandle<T>> {
        Some(AsyncFunctionEnvHandle {
            read_lock: StoreAsyncReadLock::try_acquire(&self.store)?,
            func_env: self.func_env.clone(),
        })
    }

    pub fn try_write(&self) -> Option<AsyncFunctionEnvHandleMut<T>> {
        Some(AsyncFunctionEnvHandleMut::new(
            StoreAsyncWriteLock::try_acquire(&self.store)?,
            self.func_env.clone(),
        ))
    }

    pub fn as_ref(&self) -> FunctionEnv<T> {
        self.func_env.clone()
    }

    pub fn as_mut(&mut self) -> Self {
        self.clone()
    }

    pub fn as_store_async(&self) -> StoreAsync {
        self.store.store()
    }
}

#[cfg(feature = "experimental-async")]
impl<T: 'static> AsyncFunctionEnvHandle<T> {
    pub fn data(&self) -> &T {
        self.func_env.as_ref(&self.read_lock)
    }

    pub fn data_and_store(&self) -> (&T, &StoreAsyncReadLock) {
        (self.data(), &self.read_lock)
    }
}

#[cfg(feature = "experimental-async")]
impl<T: 'static> AsStoreRef for AsyncFunctionEnvHandle<T> {
    fn as_store_ref(&self) -> StoreRef<'_> {
        self.read_lock.as_store_ref()
    }
}

#[cfg(feature = "experimental-async")]
impl<T: 'static> AsyncFunctionEnvHandleMut<T> {
    fn new(mut write_lock: StoreAsyncWriteLock, func_env: FunctionEnv<T>) -> Self {
        let retirement =
            crate::backend::v8::async_runtime::RetirementCap::capture(&write_lock.as_store_mut())
                .expect("V8 async environment has invalid write custody")
                .expect("V8 async environment requires its actual owned write guard");
        Self {
            write_lock,
            func_env,
            lease: None,
            retirement,
        }
    }
    pub fn data_mut(&mut self) -> &mut T {
        match &mut self.lease {
            Some(value) => value.downcast_mut().unwrap(),
            None => self.func_env.as_mut(&mut self.write_lock),
        }
    }
    pub fn data_and_store_mut(&mut self) -> (&mut T, &mut StoreAsyncWriteLock) {
        if self.lease.is_none() {
            self.lease = Some(self.func_env.lend(&mut self.write_lock.as_store_mut()));
        }
        (
            self.lease.as_mut().unwrap().downcast_mut().unwrap(),
            &mut self.write_lock,
        )
    }
    pub fn as_function_env_mut(&mut self) -> FunctionEnvMut<'_, T> {
        FunctionEnvMut {
            store_mut: Some(self.write_lock.as_store_mut()),
            func_env: self.func_env.clone(),
            lease: self.lease.as_mut().map(SplitLease::Borrowed),
            retirement: Some(self.retirement.clone()),
        }
    }
}

#[cfg(feature = "experimental-async")]
impl<T> Drop for AsyncFunctionEnvHandleMut<T> {
    fn drop(&mut self) {
        let Some(value) = self.lease.take() else {
            return;
        };
        if let Err(value) = self
            .func_env
            .restore(&mut self.write_lock.as_store_mut(), value)
        {
            // The exact still-live holder drains only after this field's real
            // write lock/context and all unlock notifications have retired.
            if let Err(value) = self.retirement.try_retain(value) {
                dispose_lent(value);
            }
        }
    }
}

#[cfg(feature = "experimental-async")]
impl<T: 'static> AsStoreRef for AsyncFunctionEnvHandleMut<T> {
    fn as_store_ref(&self) -> StoreRef<'_> {
        self.write_lock.as_store_ref()
    }
}

#[cfg(feature = "experimental-async")]
impl<T: 'static> AsStoreMut for AsyncFunctionEnvHandleMut<T> {
    fn as_store_mut(&mut self) -> StoreMut<'_> {
        self.write_lock.as_store_mut()
    }

    fn objects_mut(&mut self) -> &mut crate::StoreObjects {
        self.write_lock.objects_mut()
    }
}

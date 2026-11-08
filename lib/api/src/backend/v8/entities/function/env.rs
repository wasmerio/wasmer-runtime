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
        backing: &LentBacking,
        store: &mut StoreMut<'_>,
        value: Box<dyn Any + Send>,
    ) -> Result<(), Box<dyn Any + Send>> {
        if !backing.matches(store) {
            return Err(value);
        }
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

// This snapshot belongs to the original access, independently of movable
// StoreObjects. A replacement guard cannot adopt the original lent Box.
#[derive(Clone)]
struct LentBacking {
    address: usize,
    origin: crate::backend::v8::origin::Origin,
}
impl LentBacking {
    fn capture(store: &StoreMut<'_>) -> Self {
        Self {
            address: store.inner as *const crate::StoreInner as usize,
            origin: store.inner.store.as_v8().origin.capture(),
        }
    }
    fn matches(&self, store: &StoreMut<'_>) -> bool {
        store.inner as *const crate::StoreInner as usize == self.address
            && store.inner.store.is_v8()
            && self
                .origin
                .same(&store.inner.store.as_v8().origin.capture())
    }
}

/// A temporary handle to a [`FunctionEnv`].
pub struct FunctionEnvMut<'a, T: 'a> {
    pub(crate) store_mut: Option<StoreMut<'a>>,
    pub(crate) func_env: FunctionEnv<T>,
    lease: Option<SplitLease<'a>>,
    backing: LentBacking,
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
        let backing = LentBacking::capture(&store);
        Self {
            backing,
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
            backing: self.backing.clone(),
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
        let original =
            self.func_env
                .restore(&self.backing, self.store_mut.as_mut().unwrap(), value);
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
    write_lock: Option<StoreAsyncWriteLock>,
    pub(crate) func_env: FunctionEnv<T>,
    lease: Option<Box<dyn Any + Send>>,
    backing: LentBacking,
    retirement: crate::backend::v8::async_runtime::RetirementCap,
    failure: crate::backend::v8::async_runtime::CallPanicCap,
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
        let backing = LentBacking::capture(&write_lock.as_store_mut());
        let failure =
            crate::backend::v8::async_runtime::CallPanicCap::capture(&write_lock.as_store_mut());
        Self {
            write_lock: Some(write_lock),
            func_env,
            lease: None,
            backing,
            retirement,
            failure,
        }
    }
    pub fn data_mut(&mut self) -> &mut T {
        match &mut self.lease {
            Some(value) => value.downcast_mut().unwrap(),
            None => self.func_env.as_mut(self.write_lock.as_mut().unwrap()),
        }
    }
    pub fn data_and_store_mut(&mut self) -> (&mut T, &mut StoreAsyncWriteLock) {
        if self.lease.is_none() {
            self.lease = Some(
                self.func_env
                    .lend(&mut self.write_lock.as_mut().unwrap().as_store_mut()),
            );
        }
        (
            self.lease.as_mut().unwrap().downcast_mut().unwrap(),
            self.write_lock.as_mut().unwrap(),
        )
    }
    pub fn as_function_env_mut(&mut self) -> FunctionEnvMut<'_, T> {
        FunctionEnvMut {
            store_mut: Some(self.write_lock.as_mut().unwrap().as_store_mut()),
            backing: self.backing.clone(),
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
        if let Err(value) = self.func_env.restore(
            &self.backing,
            &mut self.write_lock.as_mut().unwrap().as_store_mut(),
            value,
        ) {
            // The exact still-live holder drains only after this field's real
            // write lock/context and all unlock notifications have retired.
            if let Err(value) = self.retirement.try_retain(value) {
                // The caller may have replaced A's guard with B. End that
                // CURRENT actual guard before any displaced T destructor.
                dispose_after_guard(&self.failure, self.write_lock.take(), value);
            }
        }
    }
}

#[cfg(feature = "experimental-async")]
impl<T: 'static> AsStoreRef for AsyncFunctionEnvHandleMut<T> {
    fn as_store_ref(&self) -> StoreRef<'_> {
        self.write_lock.as_ref().unwrap().as_store_ref()
    }
}

#[cfg(feature = "experimental-async")]
impl<T: 'static> AsStoreMut for AsyncFunctionEnvHandleMut<T> {
    fn as_store_mut(&mut self) -> StoreMut<'_> {
        self.write_lock.as_mut().unwrap().as_store_mut()
    }

    fn objects_mut(&mut self) -> &mut crate::StoreObjects {
        self.write_lock.as_mut().unwrap().objects_mut()
    }
}

#[cfg(feature = "experimental-async")]
fn dispose_after_guard(
    failure: &crate::backend::v8::async_runtime::CallPanicCap,
    guard: Option<StoreAsyncWriteLock>,
    value: Box<dyn Any + Send>,
) {
    use std::{
        cell::RefCell,
        panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    };
    let unwinding = std::thread::panicking();
    let local = RefCell::new(None);
    if let Err(panic) = catch_unwind(AssertUnwindSafe(|| drop(guard))) {
        // Publish P into the ORIGINAL live caller before T can reenter A/Q.
        failure.record(panic, unwinding, &local);
    }
    if let Err(panic) = catch_unwind(AssertUnwindSafe(|| drop(value))) {
        failure.record(panic, unwinding, &local);
    }
    if let Some(first) = local.into_inner() {
        resume_unwind(first);
    }
}

#[cfg(all(test, feature = "experimental-async"))]
mod tests {
    use super::*;
    use std::{
        future::Future,
        rc::Rc,
        sync::atomic::{AtomicBool, AtomicUsize, Ordering},
        task::{Context, Poll, Wake, Waker},
    };

    struct UnlockWake(Arc<AtomicBool>);
    impl Wake for UnlockWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    struct Displaced {
        unlocked: Arc<AtomicBool>,
        address: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
        reentered: Arc<AtomicBool>,
    }
    impl Drop for Displaced {
        fn drop(&mut self) {
            assert!(
                self.unlocked.load(Ordering::SeqCst),
                "replacement B guard must retire BEFORE T"
            );
            assert_eq!(
                self as *const Self as usize,
                self.address.load(Ordering::SeqCst)
            );
            self.drops.fetch_add(1, Ordering::SeqCst);
            // A genuine same-thread V8 reentry after B's real unlock, rather
            // than merely observing a private restoration flag.
            let mut store = crate::Store::new(crate::v8::V8::new());
            let function = crate::Function::new_typed_async(&mut store, || async { 37i32 });
            let store = store.into_async();
            let mut future = function.call_async(&store, vec![]);
            let result = future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()));
            assert!(matches!(result, Poll::Ready(Ok(values)) if values[0].i32() == Some(37)));
            self.reentered.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn split_subframes_restore_the_same_box_on_normal_return_and_unwind() {
        let mut store = crate::Store::new(crate::v8::V8::new());
        let environment = FunctionEnv::new(&mut store, 11usize);
        let address;
        {
            let mut frame = environment.clone().into_mut(&mut store);
            let (data, _) = frame.data_and_store_mut();
            address = data as *mut usize as usize;
            *data += 1;
            {
                let mut child = frame.as_mut();
                let (data, mut view) = child.data_and_store_mut();
                assert_eq!(data as *mut usize as usize, address);
                *data += 1;
                // The exact slot is absent while this Box is separately lent.
                assert!(
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        environment.as_mut(&mut view);
                    }))
                    .is_err()
                );
            }
            assert_eq!(*frame.data(), 13);
        }
        assert_eq!(environment.as_ref(&store) as *const usize as usize, address);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut frame = environment.clone().into_mut(&mut store);
            let (data, _) = frame.data_and_store_mut();
            *data += 1;
            std::panic::panic_any(71usize);
        }))
        .unwrap_err();
        assert_eq!(*panic.downcast::<usize>().unwrap(), 71);
        assert_eq!(environment.as_ref(&store) as *const usize as usize, address);
        assert_eq!(*environment.as_ref(&store), 14);
    }

    struct PanicValue {
        name: &'static str,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for PanicValue {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct PanicUnlockWake(std::sync::Mutex<Option<Box<PanicValue>>>);
    impl Wake for PanicUnlockWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            let panic = self.0.lock().unwrap().take();
            if let Some(panic) = panic {
                std::panic::resume_unwind(panic);
            }
        }
    }
    struct ReenterOriginal {
        function: crate::Function,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for ReenterOriginal {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            let values = crate::Store::with_current(|store| self.function.call(store, &[]))
                .expect("the original A is explicitly parked")
                .unwrap();
            assert_eq!(values[0].i32(), Some(21));
        }
    }

    #[test]
    fn replaced_guard_unlock_panic_is_published_before_t_drop_reenters_original_a() {
        use std::cell::Cell;
        let first_drops = Arc::new(AtomicUsize::new(0));
        let second_drops = Arc::new(AtomicUsize::new(0));
        let value_drops = Arc::new(AtomicUsize::new(0));
        let first = Box::new(PanicValue {
            name: "P",
            drops: first_drops.clone(),
        });
        let address = (&*first) as *const PanicValue as usize;
        let mut a = crate::Store::new(crate::v8::V8::new());
        let module = crate::Module::new(
            &a,
            wat::parse_str("(module (func (export \"ready\") (result i32) i32.const 21))").unwrap(),
        )
        .unwrap();
        let instance = crate::Instance::new(&mut a, &module, &crate::Imports::new()).unwrap();
        let environment = FunctionEnv::new(
            &mut a,
            ReenterOriginal {
                function: instance.exports.get_function("ready").unwrap().clone(),
                drops: value_drops.clone(),
            },
        );
        let binding = a.as_store_ref().inner.store.as_v8().async_binding.clone();
        let frame = binding.enter().unwrap();
        let saw_original = Rc::new(Cell::new(false));
        let observed = saw_original.clone();
        let second_count = second_drops.clone();
        let receiver = binding.clone();
        // Unit injection uses the existing canonical local handler type;
        // the public setter additionally requires Send+Sync for its clients.
        a.inner.on_called = Some(Box::new(move |_| {
            observed.set(receiver.current_has_panic());
            receiver.record(
                Box::new(PanicValue {
                    name: "Q",
                    drops: second_count,
                }),
                false,
            );
            Ok(wasmer_types::OnCalledAction::Finish)
        }));
        let a = a.into_async();
        let b = crate::Store::new(crate::v8::V8::new()).into_async();
        let environment = AsyncFunctionEnvMut {
            store: a.store(),
            func_env: environment,
            import_snapshot: None,
        };
        let mut handle = environment.try_write().unwrap();
        let replacement = StoreAsyncWriteLock::try_acquire(&b).unwrap();
        let mut waiting = Box::pin(b.write_lock());
        let waker = Waker::from(Arc::new(PanicUnlockWake(std::sync::Mutex::new(Some(
            first,
        )))));
        assert!(
            waiting
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        {
            let (_, current) = handle.data_and_store_mut();
            let mut original = std::mem::replace(current, replacement);
            std::mem::swap(current.objects_mut(), original.objects_mut());
            drop(original);
        }
        // A new real guard and explicit existing parked scope enable T's
        // public synchronous A reentry; the captured ORIGINAL holder has ended.
        let mut guard = StoreAsyncWriteLock::try_acquire(&a).unwrap();
        guard.as_store_mut().parked(|| drop(handle));
        drop(guard);
        assert!(
            saw_original.get(),
            "P must be in A before its on_called records Q"
        );
        assert_eq!(value_drops.load(Ordering::SeqCst), 1);
        assert_eq!(second_drops.load(Ordering::SeqCst), 1);
        let first = frame
            .take_panic()
            .unwrap()
            .downcast::<PanicValue>()
            .unwrap();
        assert_eq!((&*first) as *const PanicValue as usize, address);
        assert_eq!(first.name, "P");
        assert_eq!(first_drops.load(Ordering::SeqCst), 0);
        drop(first);
        assert_eq!(first_drops.load(Ordering::SeqCst), 1);
        drop(frame);
        assert!(
            waiting
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_ready()
        );
        drop(waiting);
        drop(environment);
        drop(instance);
        drop(b);
        drop(a);
    }

    #[test]
    fn replaced_owned_guard_cannot_restore_transplanted_slot_and_unlocks_before_disposal() {
        let unlocked = Arc::new(AtomicBool::new(false));
        let address = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let reentered = Arc::new(AtomicBool::new(false));
        let mut a = crate::Store::new(crate::v8::V8::new());
        let environment = FunctionEnv::new(
            &mut a,
            Displaced {
                unlocked: unlocked.clone(),
                address: address.clone(),
                drops: drops.clone(),
                reentered: reentered.clone(),
            },
        );
        let b = crate::Store::new(crate::v8::V8::new()).into_async();
        let a = a.into_async();
        let environment = AsyncFunctionEnvMut {
            store: a.store(),
            func_env: environment,
            import_snapshot: None,
        };
        let mut handle = environment.try_write().unwrap();
        let replacement = StoreAsyncWriteLock::try_acquire(&b).unwrap();
        let mut waiting = Box::pin(b.write_lock());
        let waker = Waker::from(Arc::new(UnlockWake(unlocked.clone())));
        assert!(
            waiting
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        {
            let (data, current) = handle.data_and_store_mut();
            address.store(data as *mut Displaced as usize, Ordering::SeqCst);
            let mut original = std::mem::replace(current, replacement);
            // Numeric handle, slot identity and StoreObjects all now match A,
            // but the actual current backing is B. Restore must still refuse.
            std::mem::swap(current.objects_mut(), original.objects_mut());
            drop(original);
        }
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert!(!unlocked.load(Ordering::SeqCst));
        drop(handle);
        assert!(unlocked.load(Ordering::SeqCst));
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert!(reentered.load(Ordering::SeqCst));
        assert!(
            waiting
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_ready()
        );
        drop(waiting);
        drop(environment);
        drop(b);
        drop(a);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }
}

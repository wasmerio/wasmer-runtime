//! Borrow-checked async calls and synchronous host access capabilities.
//!
//! A call borrows its store until completion or cancellation. Host futures only
//! access the store inside [`AsyncEnv::with`]. Interleaving guest stacks requires
//! an explicit [`SharedStore`]; such guests must manage their own shadow stacks.
//!
//! Public function methods dispatch through the sealed capability implementations
//! here; backend code never acquires an asynchronous store lock.

use std::{
    cell::RefCell,
    future::Future,
    marker::PhantomData,
    pin::Pin,
    rc::{Rc, Weak},
    task::{Context, Poll},
};

use crate::{
    AsStoreMut, AsStoreRef, BackendFunction, Function, FunctionEnv, FunctionEnvMut, FunctionType,
    IntoResult, RuntimeError, Store, StoreInner, StoreMut, TypedFunction, Value, WasmTypeList,
    entities::store::{AllocationInner, StoreAllocation},
};
use wasmer_types::StoreId;

pub(crate) type HostFuture = Pin<Box<dyn Future<Output = Result<Vec<Value>, RuntimeError>>>>;
type CallResult = Result<Box<[Value]>, RuntimeError>;
type CallFactory<R> = Box<dyn FnOnce(StoreAccess) -> Pin<Box<dyn Future<Output = R>>>>;

#[derive(Default)]
struct Scope {
    // A capability is published only while this call is being polled/dropped.
    access: RefCell<Option<StoreAccess>>,
}

thread_local! {
    static CURRENT: RefCell<Vec<Rc<Scope>>> = const { RefCell::new(Vec::new()) };
}

/// Private ownership and provenance of a call's store. The raw pointer is
/// derived once from AsStoreMut; no subsequent call to that public trait can
/// redirect the call to a different store.
#[derive(Clone)]
pub(crate) struct StoreAccess {
    allocation: Ownership,
    pub(crate) ptr: *mut StoreInner,
    scope: Rc<Scope>,
}

/// Exclusive native calls rely only on their Rust borrow. Engine-owned JSPI
/// stacks and shared calls retain allocation ownership independently.
#[derive(Clone)]
enum Ownership {
    Borrowed(*const AllocationInner),
    Owned(StoreAllocation),
}
impl std::ops::Deref for Ownership {
    type Target = AllocationInner;
    fn deref(&self) -> &AllocationInner {
        match self {
            // SAFETY: the enclosing call or synchronous entry keeps its store
            // loan alive; a host environment uses it only inside that scope.
            Self::Borrowed(ptr) => unsafe { &**ptr },
            Self::Owned(allocation) => &allocation.0,
        }
    }
}

impl StoreAccess {
    /// # Safety
    /// The caller keeps this borrow exclusive for the lifetime of the call, or
    /// holds a shared-store owner and uses the allocation's slice check.
    pub(crate) unsafe fn from_ptr(ptr: *mut StoreInner) -> Self {
        let weak = unsafe { &(*ptr).allocation };
        #[cfg(feature = "js")]
        let allocation = if matches!(unsafe { &(*ptr).store }, crate::BackendStore::Js(_)) {
            Ownership::Owned(StoreAllocation(weak.upgrade().expect("live store")))
        } else {
            Ownership::Borrowed(weak.as_ptr())
        };
        #[cfg(not(feature = "js"))]
        let allocation = Ownership::Borrowed(weak.as_ptr());
        Self {
            allocation,
            ptr,
            scope: Rc::new(Scope::default()),
        }
    }

    /// A synchronous scope cannot outlive the store loan that entered it.
    /// Unlike a JSPI activation, it must not retain an allocation owner on a
    /// Rust frame that a JavaScript exception could skip.
    ///
    /// # Safety
    /// The caller keeps the store loan alive until the synchronous scope exits.
    /// This access must never be retained by an engine-owned guest activation.
    pub(crate) unsafe fn borrowed_from_ptr(ptr: *mut StoreInner) -> Self {
        Self {
            allocation: Ownership::Borrowed(unsafe { (*ptr).allocation.as_ptr() }),
            ptr,
            scope: Rc::new(Scope::default()),
        }
    }

    /// Nonowning view used only while the parent call keeps the allocation
    /// alive. A JavaScript exception may skip a temporary slice or poison a
    /// host future without retaining an additional store owner. A backend
    /// retaining this in a host future must discard that future before its
    /// owning activation, and may never use it after that activation ends.
    ///
    /// # Safety
    /// The original capability or its enclosing store loan must remain live
    /// for every use of the returned nonowning view.
    pub(crate) unsafe fn borrowed(&self) -> Self {
        Self {
            allocation: Ownership::Borrowed(&*self.allocation),
            ptr: self.ptr,
            scope: self.scope.clone(),
        }
    }

    fn nested(&self) -> Self {
        Self {
            scope: Rc::new(Scope::default()),
            ..self.clone()
        }
    }

    #[cfg(feature = "js")]
    pub(crate) fn is_borrowed(&self) -> bool {
        matches!(self.allocation, Ownership::Borrowed(_))
    }

    #[cfg(feature = "js")]
    pub(crate) fn allocation(&self) -> StoreAllocation {
        match &self.allocation {
            Ownership::Owned(allocation) => allocation.clone(),
            Ownership::Borrowed(_) => panic!("native calls do not own their store"),
        }
    }

    pub(crate) fn store_id(&self) -> StoreId {
        self.allocation.id
    }

    pub(crate) fn is_shared(&self) -> bool {
        self.allocation.shared.get()
    }

    pub(crate) fn with<R>(&self, f: impl FnOnce(StoreMut<'_>) -> R) -> R {
        assert!(!self.allocation.busy.replace(true), "async store is busy");
        struct Leave<'a>(&'a AllocationInner);
        impl Drop for Leave<'_> {
            fn drop(&mut self) {
                self.0.busy.set(false);
            }
        }
        let _leave = Leave(&self.allocation);
        // SAFETY: the call's exclusive borrow or shared slice flag excludes
        // other access. The higher-ranked closure cannot return a store borrow.
        let lent = self.allocation.lent.get();
        let ptr = if self.is_shared() {
            if lent.is_null() {
                self.allocation.value.get()
            } else {
                lent
            }
        } else {
            self.ptr
        };
        f(StoreMut {
            inner: unsafe { &mut *ptr },
        })
    }
}

pub(crate) fn with_access<R>(access: &StoreAccess, f: impl FnOnce() -> R) -> R {
    struct Leave {
        scope: Rc<Scope>,
        previous: Option<StoreAccess>,
    }
    impl Drop for Leave {
        fn drop(&mut self) {
            CURRENT.with(|current| {
                current.borrow_mut().pop();
            });
            *self.scope.access.borrow_mut() = self.previous.take();
        }
    }
    let previous = access.scope.access.replace(Some(access.clone()));
    CURRENT.with(|current| current.borrow_mut().push(access.scope.clone()));
    let _leave = Leave {
        scope: access.scope.clone(),
        previous,
    };
    f()
}

/// Save the publication depth before entering a JavaScript host callback.
#[cfg(feature = "js")]
pub(crate) fn scope_checkpoint() -> usize {
    CURRENT.with(|current| current.borrow().len())
}

/// Revoke publications whose Rust guards a JavaScript exception skipped.
///
/// # Safety
/// All frames entered after this checkpoint have returned or been unwound.
/// Scopes below it still belong to live enclosing frames on this thread.
#[cfg(feature = "js")]
pub(crate) unsafe fn restore_scope_checkpoint(depth: usize) {
    let removed = CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        assert!(current.len() >= depth, "mismatched JavaScript call scope");
        current.split_off(depth).into_iter().filter(|scope| {
            // Nested publication of the same scope must not revoke the
            // capability still used by an enclosing frame.
            !current.iter().any(|outer| Rc::ptr_eq(outer, scope))
        }).collect::<Vec<_>>()
    });
    // Clear every publication before dropping any owners. An active scope
    // contains a StoreAccess pointing back to it, so truncation alone leaks a
    // cycle and leaves a raw store capability usable after its loan has ended.
    let revoked = removed.iter().map(|scope| scope.access.take()).collect::<Vec<_>>();
    drop(revoked);
}

fn current_access(id: StoreId) -> StoreAccess {
    CURRENT.with(|current| {
        current
            .borrow()
            .iter()
            .rev()
            .find_map(|scope| {
                scope
                    .access
                    .borrow()
                    .as_ref()
                    .filter(|a| a.store_id() == id)
                    .cloned()
            })
            .expect("async environment used outside its call's poll")
    })
}

pub(crate) fn current_store(id: StoreId) -> *mut StoreInner {
    current_access(id).ptr
}

pub(crate) fn with_current_store<R>(id: StoreId, f: impl FnOnce(*mut StoreInner) -> R) -> R {
    current_access(id).with(|store| f(store.inner as *mut StoreInner))
}

/// # Safety
/// `ptr` is the current synchronous host function's exclusively lent store.
pub(crate) unsafe fn with_sync_store<R>(ptr: *mut StoreInner, f: impl FnOnce() -> R) -> R {
    let access = unsafe { StoreAccess::borrowed_from_ptr(ptr) };
    struct Restore {
        allocation: Ownership,
        busy: bool,
        lent: *mut StoreInner,
    }
    impl Drop for Restore {
        fn drop(&mut self) {
            self.allocation.busy.set(self.busy);
            self.allocation.lent.set(self.lent);
        }
    }
    // A synchronous import inherits the active borrow from its trampoline.
    // Reborrows must derive from that pointer, including shared callbacks.
    let _restore = Restore {
        busy: access.allocation.busy.replace(false),
        lent: access.allocation.lent.replace(ptr),
        allocation: access.allocation.clone(),
    };
    with_access(&access, f)
}

/// Host state capability held across waits. It contains no live store borrow.
///
/// Store references cannot escape a synchronous slice:
///
/// ```compile_fail
/// use wasmer::AsyncEnv;
/// async fn bad(cx: &mut AsyncEnv<u32>) {
///     let data = cx.with(|env| env.data());
///     std::future::ready(()).await;
///     println!("{data}");
/// }
/// ```
pub struct AsyncEnv<T> {
    env: FunctionEnv<T>,
    scope: Rc<Scope>,
}

impl<T: Send + 'static> AsyncEnv<T> {
    pub(crate) fn new(env: FunctionEnv<T>) -> Self {
        let scope = CURRENT.with(|current| {
            current
                .borrow()
                .last()
                .cloned()
                .expect("async host function has no call scope")
        });
        Self { env, scope }
    }

    /// Borrow the environment and store for one synchronous slice. No borrow
    /// into either can survive this closure, and the closure cannot suspend.
    ///
    /// ```compile_fail,E0728
    /// use wasmer::AsyncEnv;
    /// async fn bad(cx: &mut AsyncEnv<()>) {
    ///     cx.with(|_| std::future::ready(()).await);
    /// }
    /// ```
    ///
    /// ```compile_fail
    /// use wasmer::AsyncEnv;
    /// async fn bad(cx: &mut AsyncEnv<()>) {
    ///     cx.with(|_| cx.with(|_| ()));
    /// }
    /// ```
    ///
    /// ```compile_fail
    /// use wasmer::{Function, AsyncEnv};
    /// async fn bad(cx: &mut AsyncEnv<()>, f: &Function) {
    ///     let a = f.call_async(cx, &[]);
    ///     let b = f.call_async(cx, &[]);
    ///     drop((a, b));
    /// }
    /// ```
    pub fn with<R>(&mut self, f: impl FnOnce(FunctionEnvMut<'_, T>) -> R) -> R {
        let access = {
            let published = self.scope.access.borrow();
            let access = published
                .as_ref()
                .expect("async environment used outside its call's poll");
            // SAFETY: the enclosing poll/sync entry retains this publication
            // and its store loan until the synchronous slice has returned.
            unsafe { access.borrowed() }
        };
        access.with(|mut store| f(self.env.clone().into_mut(&mut store)))
    }
}

/// Future holding the exclusive borrow of a call, or an owned shared handle.
///
/// The store stays inaccessible until the call is completed or dropped:
///
/// ```compile_fail,E0499
/// use wasmer::{Function, Store};
/// let mut store = Store::default();
/// let f = Function::new_typed(&mut store, || {});
/// let call = f.call_async(&mut store, &[]);
/// f.call(&mut store, &[]).unwrap();
/// drop(call);
/// ```
///
/// The call cannot outlive its store:
///
/// ```compile_fail,E0505
/// use wasmer::{Function, Store};
/// let mut store = Store::default();
/// let f = Function::new_typed(&mut store, || {});
/// let call = f.call_async(&mut store, &[]);
/// drop(store);
/// drop(call);
/// ```
///
/// Calls remain on the store's thread:
///
/// ```compile_fail,E0277
/// use wasmer::{Function, Store};
/// fn needs_send(_: impl Send) {}
/// let mut store = Store::default();
/// let f = Function::new_typed(&mut store, || {});
/// needs_send(f.call_async(&mut store, &[]));
/// ```
/// It is deliberately `!Send`, as is its store.
#[must_use = "an async call must be awaited or polled"]
pub struct AsyncCall<'a, R> {
    future: Option<Pin<Box<dyn Future<Output = R>>>>,
    access: StoreAccess,
    borrow: PhantomData<&'a mut Store>,
}

impl<R> Unpin for AsyncCall<'_, R> {}

impl<R> Future for AsyncCall<'_, R> {
    type Output = R;
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<R> {
        let access = self.access.clone();
        assert!(
            !access.allocation.busy.get(),
            "cannot poll a call inside a store slice"
        );
        with_access(&access, || {
            self.future
                .as_mut()
                .expect("call polled after completion")
                .as_mut()
                .poll(cx)
        })
    }
}

impl<R> Drop for AsyncCall<'_, R> {
    fn drop(&mut self) {
        if CURRENT.try_with(|_| ()).is_err() {
            // Thread-local destruction may already have removed the scope
            // stack. As with native coroutine teardown, leak rather than run
            // a host destructor without the context it depends on.
            std::mem::forget(self.future.take());
            return;
        }
        let access = self.access.clone();
        // Drop the host future while its own scope still exists, then let the
        // backend discard a stack containing only engine and guest frames.
        with_access(&access, || drop(self.future.take()));
    }
}

mod sealed {
    pub trait Sealed {}
}

/// Sealed store capability accepted by both typed and dynamic async calls.
#[allow(private_interfaces)]
pub trait AsyncCallStore: sealed::Sealed {
    /// An exclusive call borrows its capability; a shared call owns a handle.
    type Call<'a, R: 'static>: Future<Output = R>
    where
        Self: 'a;
    #[doc(hidden)]
    fn build<'a, R: 'static>(&'a mut self, build: CallFactory<R>) -> Self::Call<'a, R>;
}

impl<S: AsStoreMut> sealed::Sealed for S {}
#[allow(private_interfaces)]
impl<S: AsStoreMut> AsyncCallStore for S {
    type Call<'a, R: 'static>
        = AsyncCall<'a, R>
    where
        Self: 'a;
    fn build<'a, R: 'static>(&'a mut self, build: CallFactory<R>) -> AsyncCall<'a, R> {
        let store = self.as_store_mut();
        // The only derivation from the public, potentially stateful trait.
        let access = unsafe { StoreAccess::from_ptr(store.inner as *mut StoreInner) };
        let future = build(access.clone());
        AsyncCall {
            future: Some(future),
            access,
            borrow: PhantomData,
        }
    }
}

impl<T: Send + 'static> sealed::Sealed for AsyncEnv<T> {}
#[allow(private_interfaces)]
impl<T: Send + 'static> AsyncCallStore for AsyncEnv<T> {
    type Call<'a, R: 'static>
        = AsyncCall<'a, R>
    where
        Self: 'a;
    fn build<'a, R: 'static>(&'a mut self, build: CallFactory<R>) -> AsyncCall<'a, R> {
        let access = self
            .scope
            .access
            .borrow()
            .as_ref()
            .expect("nested call outside its parent poll")
            .nested();
        let future = build(access.clone());
        AsyncCall {
            future: Some(future),
            access,
            borrow: PhantomData,
        }
    }
}

/// A store already has a running synchronous slice. There is no waiter queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StoreBusy;
impl std::fmt::Display for StoreBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("store is busy")
    }
}
impl std::error::Error for StoreBusy {}

/// Explicitly permits several guest stacks to interleave on one store.
/// The guest must support this; ordinary C and Rust shadow stacks do not.
///
/// Shared calls own their inputs and can be spawned on a local executor:
///
/// ```
/// use wasmer::{Function, Store};
/// fn needs_static(_: impl std::future::Future + 'static) {}
/// let mut store = Store::default();
/// let f = Function::new_typed(&mut store, || {});
/// let mut shared = store.into_shared();
/// needs_static(f.call_async(&mut shared, &[]));
/// ```
///
/// The shared handle also remains on one thread:
///
/// ```compile_fail,E0277
/// use wasmer::Store;
/// fn needs_send(_: impl Send) {}
/// needs_send(Store::default().into_shared());
/// ```
#[derive(Clone)]
pub struct SharedStore {
    allocation: StoreAllocation,
}

impl std::fmt::Debug for SharedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedStore")
            .field("id", &self.allocation.0.id)
            .finish()
    }
}

/// Non-owning shared store handle, suitable for persistent host callbacks.
#[derive(Clone)]
pub struct WeakSharedStore {
    allocation: Weak<AllocationInner>,
}
impl WeakSharedStore {
    /// Return a strong handle while the store is still alive.
    pub fn upgrade(&self) -> Option<SharedStore> {
        self.allocation
            .upgrade()
            .filter(|a| a.shared.get())
            .map(|a| SharedStore {
                allocation: StoreAllocation(a),
            })
    }
}
impl Store {
    /// Opt in to interleaving several guest stacks on this store.
    pub fn into_shared(self) -> SharedStore {
        #[cfg(feature = "js")]
        self.inner.0.guest.assert_access();
        self.inner.0.shared.set(true);
        SharedStore {
            allocation: self.inner,
        }
    }
}
impl SharedStore {
    /// Take a synchronous slice, or fail immediately if another slice runs.
    pub fn try_with<R>(&self, f: impl FnOnce(StoreMut<'_>) -> R) -> Result<R, StoreBusy> {
        if self.allocation.0.busy.get() {
            return Err(StoreBusy);
        }
        #[cfg(feature = "js")]
        if self.allocation.0.guest.is_running() {
            return Err(StoreBusy);
        }
        let lent = self.allocation.0.lent.get();
        let ptr = if lent.is_null() {
            self.allocation.0.value.get()
        } else {
            lent
        };
        let access = StoreAccess {
            allocation: Ownership::Owned(self.allocation.clone()),
            ptr,
            scope: Rc::new(Scope::default()),
        };
        Ok(access.with(f))
    }

    /// Recover the store when no other owner or guest task remains.
    pub fn into_store(self) -> Result<Store, Self> {
        if Rc::strong_count(&self.allocation.0) != 1 || self.allocation.0.busy.get() {
            return Err(self);
        }
        #[cfg(feature = "js")]
        self.allocation.0.guest.assert_access();
        self.allocation.0.shared.set(false);
        Ok(Store {
            inner: self.allocation,
        })
    }

    /// Make a handle that does not keep the store alive.
    pub fn downgrade(&self) -> WeakSharedStore {
        WeakSharedStore {
            allocation: Rc::downgrade(&self.allocation.0),
        }
    }
}
impl sealed::Sealed for SharedStore {}
#[allow(private_interfaces)]
impl AsyncCallStore for SharedStore {
    type Call<'a, R: 'static> = AsyncCall<'static, R>;
    fn build<R: 'static>(&mut self, build: CallFactory<R>) -> AsyncCall<'static, R> {
        // Copy ownership and values now: the returned task borrows nothing.
        let access = StoreAccess {
            allocation: Ownership::Owned(self.allocation.clone()),
            ptr: self.allocation.0.value.get(),
            scope: Rc::new(Scope::default()),
        };
        let future = build(access.clone());
        AsyncCall {
            future: Some(future),
            access,
            borrow: PhantomData,
        }
    }
}

impl<T: Send + 'static> FunctionEnvMut<'_, T> {
    /// The explicitly shared handle of this store, if it has one.
    pub fn shared(&self) -> Option<SharedStore> {
        let allocation = self.as_store_ref().inner.allocation.upgrade()?;
        allocation.shared.get().then_some(SharedStore {
            allocation: StoreAllocation(allocation),
        })
    }

    /// Lend this borrow to shared callbacks while the synchronous closure runs.
    /// The closure cannot retain a borrow of this environment.
    pub fn release<R>(&mut self, f: impl FnOnce() -> R) -> R {
        self.as_store_mut().parked(f)
    }

}

/// Call with exclusive borrowing, or with an explicitly shared store.
///
/// Two exclusive calls cannot overlap:
///
/// ```compile_fail,E0499
/// use wasmer::{Function, Store};
/// let mut store = Store::default();
/// let f = Function::new_typed(&mut store, || {});
/// let a = f.call_async(&mut store, &[]);
/// let b = f.call_async(&mut store, &[]);
/// drop((a, b));
/// ```
pub(crate) fn call_async<'a, S: AsyncCallStore>(
    function: &'a Function,
    store: &'a mut S,
    params: &'a [Value],
) -> S::Call<'a, CallResult> {
    let function = function.clone();
    let params = params.to_vec();
    AsyncCallStore::build(
        store,
        Box::new(move |access| backend_call(function, access, params)),
    )
}

/// Typed counterpart of [`call_async`]. Arguments use the same tuple type as
/// `TypedFunction`; public methods unpack their arguments into this tuple.
pub(crate) fn call_typed_async<'a, S, Args, Rets>(
    function: &'a TypedFunction<Args, Rets>,
    store: &'a mut S,
    args: Args,
) -> S::Call<'a, Result<Rets, RuntimeError>>
where
    S: AsyncCallStore,
    Args: WasmTypeList + 'static,
    Rets: WasmTypeList + 'static,
{
    let function = function.func.clone();
    AsyncCallStore::build(
        store,
        Box::new(move |access| {
            Box::pin(async move {
                let params = access.with(|mut store| to_values(&mut store, args));
                let values = backend_call(function, access.clone(), params).await?;
                access.with(|mut store| from_values(&mut store, &values))
            })
        }),
    )
}

fn backend_call(
    function: Function,
    access: StoreAccess,
    params: Vec<Value>,
) -> Pin<Box<dyn Future<Output = CallResult>>> {
    match function.0 {
        #[cfg(feature = "sys")]
        BackendFunction::Sys(f) => {
            Box::pin(crate::backend::sys::async_runtime::call_function_async(
                f,
                access.store_id(),
                params,
            ))
        }
        #[cfg(feature = "js")]
        BackendFunction::Js(f) => Box::pin(crate::backend::js::async_runtime::call(f, access, params)),
        #[cfg(feature = "v8")]
        BackendFunction::V8(f) => Box::pin(async move {
            crate::backend::v8::async_runtime::with_async_entry(access.store_id(), || {
                access.with(|mut store| f.call(&mut store, &params))
            })
        }),
    }
}

fn from_values<T: WasmTypeList>(
    store: &mut impl AsStoreMut,
    values: &[Value],
) -> Result<T, RuntimeError> {
    if values.len() != T::wasm_types().len()
        || values
            .iter()
            .zip(T::wasm_types())
            .any(|(v, t)| v.ty() != *t)
    {
        return Err(RuntimeError::new("async function signature mismatch"));
    }
    let mut raw = T::empty_array();
    for (slot, value) in raw.as_mut().iter_mut().zip(values) {
        *slot = value.as_raw(store);
    }
    // SAFETY: both the length and every type were checked above.
    Ok(unsafe { T::from_array(store, raw) })
}
fn to_values<T: WasmTypeList>(store: &mut impl AsStoreMut, value: T) -> Vec<Value> {
    let mut raw = unsafe { value.into_array(store) };
    raw.as_mut()
        .iter()
        .zip(T::wasm_types())
        .map(|(raw, ty)| unsafe { Value::from_raw(store, *ty, *raw) })
        .collect()
}

mod host_sealed {
    pub trait Sealed<T, Args, Rets> {}
}

/// Sealed adapter for async functions borrowing an [`AsyncEnv`].
pub trait AsyncHostFunction<T, Args, Rets>: host_sealed::Sealed<T, Args, Rets> + 'static {
    #[doc(hidden)]
    fn invoke<'a>(
        &'a self,
        env: &'a mut AsyncEnv<T>,
        args: Args,
    ) -> Pin<Box<dyn Future<Output = Result<Rets, RuntimeError>> + 'a>>;
}

macro_rules! host {
    ($($arg:ident),*) => {
        impl<T, F, Rets, Out, $($arg,)*> host_sealed::Sealed<T, ($($arg),*), Rets> for F
        where
            T: Send + 'static,
            F: for<'e> AsyncFn(&'e mut AsyncEnv<T>, $($arg),*) -> Out + 'static,
            Out: IntoResult<Rets>, Rets: WasmTypeList,
            $($arg: crate::FromToNativeWasmType + 'static,)*
        {}
        impl<T, F, Rets, Out, $($arg,)*> AsyncHostFunction<T, ($($arg),*), Rets> for F
        where
            T: Send + 'static,
            F: for<'e> AsyncFn(&'e mut AsyncEnv<T>, $($arg),*) -> Out + 'static,
            Out: IntoResult<Rets>, Rets: WasmTypeList,
            $($arg: crate::FromToNativeWasmType + 'static,)*
        {
            fn invoke<'a>(&'a self, env: &'a mut AsyncEnv<T>, args: ($($arg),*))
                -> Pin<Box<dyn Future<Output = Result<Rets, RuntimeError>> + 'a>> {
                #[allow(non_snake_case)]
                let ($($arg),*) = args;
                Box::pin(async move {
                    self(env, $($arg),*).await.into_result()
                        .map_err(|e| RuntimeError::from_dyn(Box::new(e)))
                })
            }
        }
    }
}
host!();
host!(A1);
host!(A1, A2);
host!(A1, A2, A3);
host!(A1, A2, A3, A4);
host!(A1, A2, A3, A4, A5);
host!(A1, A2, A3, A4, A5, A6);
host!(A1, A2, A3, A4, A5, A6, A7);
host!(A1, A2, A3, A4, A5, A6, A7, A8);
host!(A1, A2, A3, A4, A5, A6, A7, A8, A9);
host!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10);
host!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11);
host!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12);
host!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13);
host!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14);
host!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15
);
host!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16
);
host!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17
);
host!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18
);
host!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19
);
host!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20
);

/// Create an async typed host function. Its environment cannot escape an import.
pub(crate) fn new_typed_async<T, Args, Rets, F>(
    store: &mut impl AsStoreMut,
    env: &FunctionEnv<T>,
    func: F,
) -> Function
where
    T: Send + 'static,
    Args: WasmTypeList + 'static,
    Rets: WasmTypeList + 'static,
    F: AsyncHostFunction<T, Args, Rets>,
{
    let env = env.clone();
    let func = Rc::new(func);
    let factory = Rc::new(move |values: Vec<Value>| -> HostFuture {
        let mut env = AsyncEnv::new(env.clone());
        let func = func.clone();
        Box::pin(async move {
            let args = env.with(|mut env| from_values::<Args>(&mut env, &values))?;
            let rets = func.invoke(&mut env, args).await?;
            Ok(env.with(|mut env| to_values(&mut env, rets)))
        })
    });
    new_boxed(
        store,
        FunctionType::new(Args::wasm_types().to_vec(), Rets::wasm_types().to_vec()),
        factory,
    )
}

/// Create an async import without host state. The argument values are owned
/// by the host future so they remain valid across suspension.
pub(crate) fn new_async<F, Fut>(
    store: &mut impl AsStoreMut,
    ty: impl Into<FunctionType>,
    func: F,
) -> Function
where
    F: Fn(&[Value]) -> Fut + 'static,
    Fut: Future<Output = Result<Vec<Value>, RuntimeError>> + 'static,
{
    new_boxed(
        store,
        ty.into(),
        Rc::new(move |values| Box::pin(func(&values))),
    )
}

/// Create a dynamic async import using owned arguments and a borrowed capability.
pub(crate) fn new_with_env_async<T, F>(
    store: &mut impl AsStoreMut,
    env: &FunctionEnv<T>,
    ty: impl Into<FunctionType>,
    func: F,
) -> Function
where
    T: Send + 'static,
    F: for<'e> AsyncFn(&'e mut AsyncEnv<T>, Vec<Value>) -> Result<Vec<Value>, RuntimeError>
        + 'static,
{
    let env = env.clone();
    let func = Rc::new(func);
    new_boxed(
        store,
        ty.into(),
        Rc::new(move |values| {
            let mut env = AsyncEnv::new(env.clone());
            let func = func.clone();
            Box::pin(async move { func(&mut env, values).await })
        }),
    )
}

fn new_boxed(
    store: &mut impl AsStoreMut,
    ty: FunctionType,
    factory: Rc<dyn Fn(Vec<Value>) -> HostFuture>,
) -> Function {
    let mut store = store.as_store_mut();
    match &store.inner.store {
        #[cfg(feature = "sys")]
        crate::BackendStore::Sys(_) => Function(BackendFunction::Sys(
            crate::backend::sys::entities::function::Function::new_async_boxed(
                &mut store,
                ty,
                move |v| factory(v),
            ),
        )),
        #[cfg(feature = "js")]
        crate::BackendStore::Js(_) => {
            crate::backend::js::async_runtime::new_function(&mut store, ty, factory)
        }
        #[cfg(feature = "v8")]
        crate::BackendStore::V8(_) => Function(BackendFunction::V8(
            crate::backend::v8::entities::function::Function::new_async_boxed(
                &mut store, ty, factory,
            ),
        )),
    }
}

#[cfg(all(test, feature = "sys"))]
mod provenance {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    fn store() -> Store {
        Store::new(crate::backend::sys::entities::engine::default_engine())
    }

    #[test]
    fn shared_release_reborrows_the_lending_environment() {
        let mut store = store();
        let env = FunctionEnv::new(&mut store, 1usize);
        let shared = store.into_shared();
        shared
            .try_with(|mut store| {
                let mut outer = env.clone().into_mut(&mut store);
                *outer.data_mut() += 1;
                outer.release(|| {
                    shared
                        .try_with(|mut store| {
                            *env.as_mut(&mut store) += 3;
                        })
                        .unwrap()
                });
                *outer.data_mut() += 5;
                assert_eq!(*outer.data(), 10);
            })
            .unwrap();
        assert_eq!(shared.try_with(|store| *env.as_ref(&store)).unwrap(), 10);
    }

    #[test]
    fn a_poll_scope_restores_after_panic_and_can_be_used_again() {
        let mut store = store();
        let env = FunctionEnv::new(&mut store, 0usize);
        let ptr = store.as_store_mut().inner as *mut StoreInner;
        let access = unsafe { StoreAccess::from_ptr(ptr) };
        let mut cx = with_access(&access, || AsyncEnv::new(env.clone()));
        assert!(
            catch_unwind(AssertUnwindSafe(|| with_access(&access, || {
                cx.with(|mut env| {
                    *env.data_mut() = 1;
                    panic!("slice panic");
                });
            })))
            .is_err()
        );
        assert!(CURRENT.with(|current| current.borrow().is_empty()));
        assert!(cx.scope.access.borrow().is_none());
        with_access(&access, || cx.with(|mut env| *env.data_mut() += 2));
        drop(cx);
        drop(access);
        assert_eq!(*env.as_ref(&store), 3);
    }

    #[test]
    fn shared_slices_restore_busy_and_provenance_after_unwind() {
        let mut store = store();
        let env = FunctionEnv::new(&mut store, 0usize);
        let shared = store.into_shared();
        let weak = shared.downgrade();
        shared
            .try_with(|mut store| {
                let mut outer = env.clone().into_mut(&mut store);
                assert!(
                    catch_unwind(AssertUnwindSafe(|| outer.release(|| {
                        shared
                            .try_with(|mut store| {
                                *env.as_mut(&mut store) = 7;
                                panic!("callback panic");
                            })
                            .unwrap();
                    })))
                    .is_err()
                );
                assert!(shared.try_with(|_| ()).is_err());
                *outer.data_mut() += 1;
            })
            .unwrap();
        let store = shared.into_store().unwrap();
        assert!(weak.upgrade().is_none());
        assert_eq!(*env.as_ref(&store), 8);
    }
}

#[cfg(all(test, feature = "sys"))]
mod capability_lifecycle {
    use super::*;

    fn store() -> Store {
        Store::new(crate::backend::sys::entities::engine::default_engine())
    }

    #[test]
    fn a_forgotten_host_future_has_no_active_store_capability() {
        let mut store = store();
        let env = FunctionEnv::new(&mut store, 0usize);
        let handle = env.clone();
        let mut call = AsyncCallStore::build(
            &mut store,
            Box::new(move |_access| {
                Box::pin(async move {
                    let mut cx = AsyncEnv::new(handle);
                    cx.with(|mut env| *env.data_mut() += 1);
                    std::future::pending::<()>().await;
                })
            }),
        );
        assert!(
            Pin::new(&mut call)
                .poll(&mut Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        std::mem::forget(call);
        *env.as_mut(&mut store) += 1;
        assert_eq!(*env.as_ref(&store), 2);
        assert!(CURRENT.with(|current| current.borrow().is_empty()));
        drop(store);
    }

    #[test]
    fn forgetting_an_exclusive_call_does_not_retain_the_store_allocation() {
        use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
        struct State(Arc<AtomicUsize>);
        impl Drop for State {
            fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut store = store();
        let env = FunctionEnv::new(&mut store, State(dropped.clone()));
        let mut call = AsyncCallStore::build(&mut store, Box::new(move |_access| Box::pin(async move {
            let _cx = AsyncEnv::new(env);
            std::future::pending::<()>().await;
        })));
        assert!(Pin::new(&mut call).poll(&mut Context::from_waker(std::task::Waker::noop())).is_pending());
        std::mem::forget(call);
        drop(store);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn an_inline_waker_cannot_poll_another_call_inside_a_shared_slice() {
        use std::{
            cell::Cell,
            panic::{AssertUnwindSafe, catch_unwind},
            sync::Arc,
            task::{Wake, Waker},
        };
        thread_local! {
            static CALL: RefCell<Option<AsyncCall<'static, ()>>> = const { RefCell::new(None) };
            static REFUSED: Cell<bool> = const { Cell::new(false) };
        }
        struct InlineWake;
        impl Wake for InlineWake {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                // No thread-local data is carried by this Send + Sync waker.
                // A wake on another thread simply has no task to poll.
                CALL.with(|slot| {
                    let mut slot = slot.borrow_mut();
                    if let Some(call) = slot.as_mut() {
                        REFUSED.set(catch_unwind(AssertUnwindSafe(|| {
                            Pin::new(call).poll(&mut Context::from_waker(Waker::noop()))
                        })).is_err());
                    }
                });
            }
        }
        let mut store = store();
        let env = FunctionEnv::new(&mut store, 0usize);
        let mut shared = store.into_shared();
        let handle = env.clone();
        let call = AsyncCallStore::build(&mut shared, Box::new(move |_access| {
            Box::pin(async move {
                AsyncEnv::new(handle).with(|mut env| *env.data_mut() += 10);
            })
        }));
        CALL.with(|slot| *slot.borrow_mut() = Some(call));
        let waker = Waker::from(Arc::new(InlineWake));
        shared.try_with(|mut store| {
            let state = env.as_mut(&mut store);
            *state += 1;
            waker.wake_by_ref();
            assert!(REFUSED.get());
            assert_eq!(*state, 1, "the rejected inline poll cannot touch the live borrow");
        }).unwrap();
        let mut call = CALL.with(|slot| slot.borrow_mut().take().unwrap());
        assert!(Pin::new(&mut call).poll(&mut Context::from_waker(Waker::noop())).is_ready());
        drop(call);
        assert_eq!(shared.try_with(|store| *env.as_ref(&store)).unwrap(), 11);
        assert!(shared.into_store().is_ok());
    }

    #[test]
    fn host_destructor_has_its_poll_scope_until_cancelled() {
        struct OnDrop(AsyncEnv<usize>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.0.with(|mut env| *env.data_mut() += 10);
            }
        }
        let mut store = store();
        let env = FunctionEnv::new(&mut store, 0usize);
        let handle = env.clone();
        let mut call = AsyncCallStore::build(
            &mut store,
            Box::new(move |_access| {
                Box::pin(async move {
                    let _on_drop = OnDrop(AsyncEnv::new(handle));
                    std::future::pending::<()>().await;
                })
            }),
        );
        assert!(
            Pin::new(&mut call)
                .poll(&mut Context::from_waker(std::task::Waker::noop()))
                .is_pending()
        );
        drop(call);
        assert_eq!(*env.as_ref(&store), 10);
    }
}

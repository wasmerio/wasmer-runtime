//! Native async calls: one `corosensei` coroutine per `Function::call_async`.
//!
//! The call future owns the coroutine, and lends it the store for one slice at
//! a time. When the guest waits in an async import, the coroutine suspends
//! with only engine and guest frames on its stack, and the future polls the
//! host future on the executor's stack.
//!
//! Everything a call leaves on the thread moves with its stack, at the single
//! switch point in [`AsyncCallFuture::switch_to_coroutine`].

#[cfg(feature = "experimental-host-interrupt")]
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, atomic::AtomicU64},
};
use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    ptr,
    rc::Rc,
    task::{Context, Poll, Waker},
};

use corosensei::{Coroutine, CoroutineResult, Yielder, stack::DefaultStack};
#[cfg(feature = "experimental-host-interrupt")]
use dashmap::DashMap;

use crate::{
    AsStoreMut, AsStoreRef, AsyncCallRequest, CallStore, PollScope, PollStore, RuntimeError,
    StoreContext, StoreInner, StoreMut, StoreRef, Value,
    entities::function::async_host::HostFuture,
};
use wasmer_types::StoreId;
use wasmer_vm::ActivationState;

/// Why a coroutine suspended: for the poller to drive this host future. See
/// [`CoroutineContext::block_on_future`].
struct AsyncYield(HostFuture);

enum AsyncResume {
    Start,
    HostFutureReady(Result<Vec<Value>, RuntimeError>),
}

#[cfg(feature = "experimental-host-interrupt")]
type AsyncCallFutureId = u64;

#[cfg(feature = "experimental-host-interrupt")]
static NEXT_ASYNC_CALL_FUTURE_ID: AtomicU64 = AtomicU64::new(0);

/// The wakers of the calls that are in progress, by store, for
/// [`notify_pending_futures_of_interrupt`]: a host interrupt is the only
/// thing that wakes a call from outside. Everything else a call waits for
/// is a host future, which has the waker itself.
///
/// A waker's `clone`, `wake` and `drop` are user code, so they never run under
/// the map's locks: the map holds `Arc`s, which are cloned and dropped there.
#[cfg(feature = "experimental-host-interrupt")]
static ASYNC_CALL_FUTURE_WAKERS: LazyLock<
    DashMap<StoreId, HashMap<AsyncCallFutureId, Arc<Waker>>>,
> = LazyLock::new(DashMap::new);

/// Releases what forgotten calls left registered for a store, when it drops.
#[cfg(feature = "experimental-host-interrupt")]
pub(crate) struct StoreTeardown(pub(crate) StoreId);

#[cfg(feature = "experimental-host-interrupt")]
impl Drop for StoreTeardown {
    fn drop(&mut self) {
        // Dropped after the map's lock is released.
        let wakers = ASYNC_CALL_FUTURE_WAKERS.remove(&self.0);
        drop(wakers);
        super::vm::interrupt_registry::release_store(self.0);
    }
}

/// The future of one call. `'a` is its borrow of the store, if it has one.
pub(crate) struct AsyncCallFuture<'a, X: 'static> {
    #[cfg(feature = "experimental-host-interrupt")]
    id: AsyncCallFutureId,

    coroutine: Option<Coroutine<AsyncResume, AsyncYield, Result<X, RuntimeError>>>,
    pending_future: Option<HostFuture>,
    next_resume: Option<AsyncResume>,
    result: Option<Result<X, RuntimeError>>,

    store: CallStore<'a>,
    /// Where the coroutine finds the store for the slice it runs in.
    current: Rc<Cell<*mut StoreInner>>,

    activation: Activation,
}

// Nothing here relies on being pinned: the coroutine's frames are on its own
// stack, and the host future is boxed.
impl<X> Unpin for AsyncCallFuture<'_, X> {}

/// How many coroutine stacks a thread keeps for its next `call_async`s.
const STACK_POOL_CAPACITY: usize = 16;

thread_local! {
    /// Stacks of `call_async` coroutines that ran to completion on this
    /// thread, for its next calls. A stack is a 1 MiB mapping with a guard
    /// page; mapping and unmapping one costs several times what the rest of a
    /// call does.
    ///
    /// This is not `wasmer_vm`'s cache of Wasm stacks: that one keeps a
    /// single stack per thread, which the Wasm frames of a call already take,
    /// sends the rest through a queue shared by all threads, and discards
    /// stacks smaller than the configured Wasm stack size.
    static STACK_POOL: RefCell<Vec<DefaultStack>> = const { RefCell::new(Vec::new()) };
}

/// A stack for a new coroutine: a pooled one if there is any.
fn take_stack() -> DefaultStack {
    // `try_with`: a call can be made while the thread's locals are destroyed.
    STACK_POOL
        .try_with(|pool| pool.borrow_mut().pop())
        .ok()
        .flatten()
        .unwrap_or_default()
}

/// Keeps the stack of a coroutine that returned for the thread's next call,
/// or unmaps it if the thread has enough of them.
///
/// Only a coroutine whose function returned gives its stack back. One that
/// was unwound, by a host panic or because its call was dropped while
/// suspended, is dropped with its stack, which is unmapped as before. Those
/// exits are rare and cost far more than a mapping, so reuse is kept to the
/// one exit after which every frame on the stack has returned.
fn give_stack_back(stack: DefaultStack) {
    let _ = STACK_POOL.try_with(|pool| {
        let mut pool = pool.borrow_mut();
        if pool.len() < STACK_POOL_CAPACITY {
            pool.push(stack);
        }
    });
}

thread_local! {
    /// The `call_async` coroutine this thread is running on, if any.
    static CURRENT_COROUTINE: Cell<*const CoroutineContext> = const { Cell::new(ptr::null()) };
    /// Where the store context entries of that coroutine start.
    static COROUTINE_BASE: Cell<usize> = const { Cell::new(0) };
}

/// A [`CURRENT_COROUTINE`] while it is off the thread: that of a coroutine
/// that is not running, kept for when it runs again, or the thread's own,
/// kept while the coroutine runs. Empty at first: a coroutine that has not
/// started runs on no `call_async` coroutine, whatever the thread that starts
/// it runs on, and a host function on its stack must not suspend that one
/// from there.
struct CoroutineSlot(*const CoroutineContext);

impl Default for CoroutineSlot {
    fn default() -> Self {
        Self(ptr::null())
    }
}

impl CoroutineSlot {
    /// Exchanges the slot with the thread's [`CURRENT_COROUTINE`]: once when
    /// the thread switches to the coroutine the slot belongs to, and again
    /// when it switches back.
    fn swap(&mut self) {
        CURRENT_COROUTINE.with(|cell| self.0 = cell.replace(self.0));
    }
}

/// The thread-local state that belongs to one `call_async`: kept here while
/// its coroutine is suspended, and exchanged with the thread's by
/// [`AsyncCallFuture::switch_to_coroutine`], the only place where the thread
/// enters or leaves the coroutine.
///
/// Suspended calls can be resumed in any order, so none of this may be left
/// on the thread while the coroutine is not running: another call would run
/// with it, or put its own on top and have it popped out of order.
struct Activation {
    /// The trap handler the signal handler consults and the interrupt
    /// registry's active stores.
    vm: ActivationState,
    /// The store context entries of the calls on the coroutine's stack.
    entries: Vec<StoreContext>,
    /// [`CURRENT_COROUTINE`].
    coroutine: CoroutineSlot,
}

/// The store of a call, as the code on its coroutine reaches it. It is not
/// one of the `AsStoreMut` types because the pointer is refreshed for every
/// slice. References it returns must not be held across a yield, and
/// `Function::call` holds none.
struct AsyncCallStoreMut {
    current: Rc<Cell<*mut StoreInner>>,
}

impl AsStoreRef for AsyncCallStoreMut {
    fn as_store_ref(&self) -> StoreRef<'_> {
        // Safety: the call lends this pointer to its coroutine for the slice
        // that is running.
        StoreRef::from_slice(unsafe { &*self.current.get() })
    }
}

impl AsStoreMut for AsyncCallStoreMut {
    fn as_store_mut(&mut self) -> StoreMut<'_> {
        // Safety: as for `as_store_ref`; `&mut self` is the only way to it.
        unsafe { StoreMut::from_raw(self.current.get()) }
    }

    fn objects_mut(&mut self) -> &mut crate::StoreObjects {
        // Safety: as for `as_store_mut`.
        unsafe { &mut (*self.current.get()).objects }
    }
}

impl<'a, X: 'static> AsyncCallFuture<'a, X> {
    pub(crate) fn new(store: CallStore<'a>, request: AsyncCallRequest<X>) -> Self {
        let current = Rc::new(Cell::new(ptr::null_mut()));
        let coroutine_store = Rc::clone(&current);
        let coroutine = Coroutine::with_stack(
            take_stack(),
            move |yielder: &Yielder<AsyncResume, AsyncYield>, resume| {
                assert!(matches!(resume, AsyncResume::Start));

                let ctx_state = CoroutineContext { yielder };
                CURRENT_COROUTINE.with(|cell| cell.set(&ctx_state));
                let mut store = AsyncCallStoreMut {
                    current: coroutine_store,
                };
                let AsyncCallRequest {
                    function,
                    params,
                    finish,
                } = request;
                let params = params(&mut store.as_store_mut())?;
                let results = function.call(&mut store, &params)?;
                finish(&mut store.as_store_mut(), &results)
            },
        );

        Self {
            #[cfg(feature = "experimental-host-interrupt")]
            id: NEXT_ASYNC_CALL_FUTURE_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
            coroutine: Some(coroutine),
            pending_future: None,
            next_resume: Some(AsyncResume::Start),
            result: None,
            store,
            current,
            activation: Activation {
                vm: ActivationState::new(),
                entries: Vec::new(),
                coroutine: CoroutineSlot::default(),
            },
        }
    }

    /// Runs the coroutine until it suspends or finishes; or, given no
    /// `resume`, unwinds a suspended coroutine (see `Drop`).
    ///
    /// The coroutine's [`Activation`] is on the thread for exactly as long as
    /// the thread is on the coroutine's stack, and is taken back however the
    /// thread leaves it: by suspending, by returning, or by a host panic
    /// propagating out of `resume`.
    fn switch_to_coroutine(
        &mut self,
        resume: Option<AsyncResume>,
        store: Option<*mut StoreInner>,
    ) -> Option<CoroutineResult<AsyncYield, Result<X, RuntimeError>>> {
        // The store is published for host futures only: a sync host function
        // of this guest holds it, and must not find it published as well.
        let _hidden = PollScope::enter(None);

        struct SwitchBack<'s> {
            activation: &'s mut Activation,
            base: usize,
            outer_base: usize,
            suspended: bool,
        }
        impl Drop for SwitchBack<'_> {
            fn drop(&mut self) {
                // Safety: the second half of the pair below; the thread is
                // back on the stack it switched from.
                unsafe { self.activation.vm.swap() };
                self.activation.coroutine.swap();
                COROUTINE_BASE.set(self.outer_base);
                StoreContext::suspend_entries(&mut self.activation.entries, self.base);
                if !self.suspended {
                    // The coroutine returned or was unwound: nothing may be
                    // left pointing into a stack that is gone.
                    self.activation.coroutine = CoroutineSlot::default();
                    self.activation.entries.clear();
                    debug_assert!(self.activation.vm.is_empty());
                }
            }
        }

        let base = StoreContext::resume_entries(&mut self.activation.entries, store);
        let outer_base = COROUTINE_BASE.replace(base);
        // Safety: host code is about to switch to the coroutine that owns the
        // activation, on the thread that runs it; `SwitchBack` swaps again as
        // soon as control is back, also when it comes back by unwinding. The
        // state is not used again once the coroutine is gone.
        unsafe { self.activation.vm.swap() };
        self.activation.coroutine.swap();
        let mut switch_back = SwitchBack {
            activation: &mut self.activation,
            base,
            outer_base,
            suspended: false,
        };
        let result = match resume {
            Some(resume_arg) => Some(self.coroutine.as_mut().unwrap().resume(resume_arg)),
            None => {
                self.coroutine = None;
                None
            }
        };
        switch_back.suspended = matches!(result, Some(CoroutineResult::Yield(_)));
        drop(switch_back);
        result
    }

    #[cfg(not(feature = "experimental-host-interrupt"))]
    fn remove_from_wakers_list(&self) {}

    #[cfg(feature = "experimental-host-interrupt")]
    fn remove_from_wakers_list(&self) {
        let removed = match ASYNC_CALL_FUTURE_WAKERS.entry(self.store.id()) {
            dashmap::Entry::Occupied(mut wakers) => {
                let removed = wakers.get_mut().remove(&self.id);
                if wakers.get().is_empty() {
                    wakers.remove();
                }
                removed
            }
            dashmap::Entry::Vacant(_) => None,
        };
        // After the map's lock is released.
        drop(removed);
    }

    /// Records the waker of the latest poll, which an interrupt has to wake.
    #[cfg(feature = "experimental-host-interrupt")]
    fn register_waker(&self, waker: &Waker) {
        let store_id = self.store.id();
        let current = ASYNC_CALL_FUTURE_WAKERS
            .get(&store_id)
            .is_some_and(|wakers| wakers.get(&self.id).is_some_and(|w| w.will_wake(waker)));
        if current {
            return;
        }
        let waker = Arc::new(waker.clone());
        let previous = ASYNC_CALL_FUTURE_WAKERS
            .entry(store_id)
            .or_default()
            .insert(self.id, waker);
        // After the map's lock is released.
        drop(previous);
    }

    /// Drops a host future the call is waiting on, while it can still reach
    /// the store if its destructor asks.
    fn drop_pending_future(&mut self) {
        if let Some(future) = self.pending_future.take() {
            let _scope = PollScope::enter(self.store.try_resolve());
            drop(future);
        }
    }

    /// Ends a call whose coroutine will not be resumed. A suspended coroutine
    /// is unwound: guest frames are discarded without running, and the
    /// frames of the call between them run their destructors. The unwind
    /// happens on the coroutine, so it needs the coroutine's state like any
    /// other resumption. It needs no store: a coroutine suspends only where
    /// no host function holds a borrow of its store (see
    /// `block_on_host_future`), so no frame on its stack uses one.
    fn cancel(&mut self) {
        if self
            .coroutine
            .as_ref()
            .is_some_and(|c| c.started() && !c.done())
        {
            if !StoreContext::thread_can_unwind() {
                // What the unwind needs of the thread is gone: the coroutine
                // is forgotten with its stack.
                std::mem::forget(self.coroutine.take());
                self.activation.vm.abandon();
                return;
            }
            self.switch_to_coroutine(None, None);
        }
        self.coroutine = None;
    }
}

impl<X: 'static> Future for AsyncCallFuture<'_, X> {
    type Output = Result<X, RuntimeError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        // Before the interrupt is checked, so that one arriving after the
        // check wakes this poll's waker.
        #[cfg(feature = "experimental-host-interrupt")]
        if this.coroutine.is_some() {
            this.register_waker(cx.waker());
        }
        loop {
            #[cfg(feature = "experimental-host-interrupt")]
            {
                let store_id = this.store.id();
                if super::vm::interrupt_registry::is_interrupted(store_id) {
                    this.remove_from_wakers_list();
                    // The call ends here, so its coroutine goes now, and with
                    // it the installations that keep the store interrupted.
                    this.drop_pending_future();
                    this.cancel();
                    return Poll::Ready(Err(super::vm::Trap::lib(
                        super::vm::TrapCode::HostInterrupt,
                    )
                    .into()));
                }
            }

            // If we're ready, return early
            if this.coroutine.is_none() {
                this.remove_from_wakers_list();
                return Poll::Ready(this.result.take().expect("polled after completion"));
            }

            let store: PollStore = this.store.resolve();

            if let Some(future) = this.pending_future.as_mut() {
                let result = {
                    let _scope = PollScope::enter(Some(store));
                    let result = future.as_mut().poll(cx);
                    if result.is_ready() {
                        // Its destructors run with the store in reach too.
                        this.pending_future = None;
                    }
                    result
                };
                match result {
                    Poll::Ready(result) => {
                        this.next_resume = Some(AsyncResume::HostFutureReady(result));
                    }
                    Poll::Pending => return Poll::Pending,
                }
            }

            // The store is taken for as long as the guest runs, and given up
            // as soon as it suspends or returns.
            let slice = store.enter_slice();
            this.current.set(slice.ptr());
            let resume_arg = this.next_resume.take().expect("no resume arg available");
            match this.switch_to_coroutine(Some(resume_arg), Some(slice.ptr())) {
                Some(CoroutineResult::Yield(AsyncYield(future))) => {
                    this.pending_future = Some(future);
                }
                Some(CoroutineResult::Return(result)) => {
                    give_stack_back(this.coroutine.take().unwrap().into_stack());
                    this.result = Some(result);
                }
                None => unreachable!("resumed with an argument"),
            }
            drop(slice);
        }
    }
}

impl<X: 'static> Drop for AsyncCallFuture<'_, X> {
    fn drop(&mut self) {
        // Runs even if a host future's destructor panics: a coroutine must
        // never be dropped without its state on the thread.
        struct Cancel<'c, 'a, X: 'static>(&'c mut AsyncCallFuture<'a, X>);
        impl<X: 'static> Drop for Cancel<'_, '_, X> {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }

        self.remove_from_wakers_list();
        let cancel = Cancel(self);
        cancel.0.drop_pending_future();
    }
}

pub enum AsyncRuntimeError {
    YieldOutsideAsyncContext,
    RuntimeError(RuntimeError),
}

pub(crate) fn block_on_host_future(
    store_id: StoreId,
    future: HostFuture,
) -> Result<Vec<Value>, AsyncRuntimeError> {
    match CoroutineContext::get_current() {
        // A store entered synchronously from inside a coroutine is a sync
        // context too: the host function that entered it holds a borrow of
        // its store across the call, which must not be parked.
        Some(context) if StoreContext::can_suspend(COROUTINE_BASE.get()) => {
            unsafe { context.as_ref().expect("valid context pointer") }
                .block_on_future(future)
                .map_err(AsyncRuntimeError::RuntimeError)
        }
        _ => run_immediate(store_id, future),
    }
}

// This function will only *wake* pending futures belonging to the given store.
// It is expected that the interrupt happens before this function is called, so
// that the futures can catch and react to `interrupt_registry::is_interrupted`
// correctly.
#[cfg(feature = "experimental-host-interrupt")]
pub(crate) fn notify_pending_futures_of_interrupt(store_id: StoreId) {
    // Woken once the map is unlocked: a waker may poll its call at once,
    // which takes the map's lock again.
    let wakers: Vec<Arc<Waker>> = match ASYNC_CALL_FUTURE_WAKERS.get(&store_id) {
        Some(wakers) => wakers.values().cloned().collect(),
        None => return,
    };
    for waker in wakers {
        waker.wake_by_ref();
    }
}

/// What host code needs to suspend the coroutine it is running on. It lives
/// on the coroutine's own stack.
struct CoroutineContext {
    yielder: *const Yielder<AsyncResume, AsyncYield>,
}

impl CoroutineContext {
    fn get_current() -> Option<*const Self> {
        let current = CURRENT_COROUTINE.with(|cell| cell.get());
        (!current.is_null()).then_some(current)
    }

    /// Waits for a host future on behalf of the guest, which called an async
    /// import: the coroutine suspends, and the poller polls the future, on the
    /// executor's stack, until it is ready.
    fn block_on_future(&self, future: HostFuture) -> Result<Vec<Value>, RuntimeError> {
        // The poller takes this coroutine's state off the thread and puts it
        // back before resuming us; see `AsyncCallFuture::switch_to_coroutine`.
        let yielder = unsafe { self.yielder.as_ref().expect("yielder pointer valid") };
        match yielder.suspend(AsyncYield(future)) {
            AsyncResume::HostFutureReady(result) => result,
            AsyncResume::Start => unreachable!("coroutine resumed without start"),
        }
    }
}

/// Polls a host future once, where the guest cannot suspend. The store is
/// reachable through the entry of the synchronous call beneath, whose borrow
/// this holds so that nothing else can take it meanwhile.
fn run_immediate(store_id: StoreId, future: HostFuture) -> Result<Vec<Value>, AsyncRuntimeError> {
    // Safety: the entry's pointer is used only for the duration of this poll,
    // in the slices the future takes, while the wrapper's borrow stands.
    let wrapper = unsafe { StoreContext::get_current(store_id) };
    let _scope = PollScope::enter(Some(PollStore::exclusive(store_id, wrapper.as_ptr())));
    // Declared after the scope, so that its destructor runs with the store of
    // this entry, not whatever was published before.
    let mut future = future;
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(result) => result.map_err(AsyncRuntimeError::RuntimeError),
        Poll::Pending => Err(AsyncRuntimeError::YieldOutsideAsyncContext),
    }
}

/// A store of this backend, whichever backend `Store::default` picks: the
/// Windows test lane makes V8 the default, which has no async host functions.
#[cfg(test)]
fn sys_store() -> crate::Store {
    crate::Store::new(crate::backend::sys::entities::engine::default_engine())
}

#[cfg(test)]
mod stack_pool {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use corosensei::stack::Stack;

    use super::*;
    use crate::{Function, FunctionType};

    type CallResult = Result<Box<[Value]>, RuntimeError>;
    type Call = Pin<Box<dyn Future<Output = CallResult>>>;

    struct OnOneThread<T>(T);
    unsafe impl<T> Send for OnOneThread<T> {}
    unsafe impl<T> Sync for OnOneThread<T> {}
    impl<T> OnOneThread<T> {
        fn get(&self) -> &T {
            &self.0
        }
    }

    /// Pending until its flag is set.
    struct Gate(Rc<Cell<bool>>);
    impl Future for Gate {
        type Output = ();
        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
            if self.0.get() {
                Poll::Ready(())
            } else {
                Poll::Pending
            }
        }
    }

    struct Host {
        store: crate::SharedStore,
        /// Waits for the gate, then returns.
        wait: Function,
        /// Notes where on the stack it runs.
        on_stack: Function,
        /// Panics.
        panic: Function,
        gate: Rc<Cell<bool>>,
        /// Where on the stack the last `wait` ran.
        ran_at: Rc<Cell<usize>>,
    }

    impl Host {
        fn new() -> Self {
            // Tests can share a thread (`--test-threads=1`).
            STACK_POOL.with(|pool| pool.borrow_mut().clear());
            let mut store = sys_store();
            let gate = Rc::new(Cell::new(false));
            let ran_at = Rc::new(Cell::new(0));
            let ty = FunctionType::new(vec![], vec![]);
            let env = crate::FunctionEnv::new(&mut store, ());
            let on_stack = crate::Function::new(&mut store, ty.clone(), {
                let ran_at = OnOneThread(ran_at.clone());
                move |_| {
                    let local = 0u8;
                    ran_at.get().set(ptr::from_ref(&local) as usize);
                    Ok(vec![])
                }
            });
            let wait = Function::new_async(&mut store, &env, ty.clone(), {
                let gate = gate.clone();
                async move |cx: &mut crate::AsyncEnv<()>, _: &[Value]| {
                    Gate(gate.clone()).await;
                    let _ = cx;
                    Ok(vec![])
                }
            });
            let panic = Function::new(&mut store, ty, |_| panic!("host function panicked"));
            Self {
                store: store.into_shared(),
                wait,
                on_stack,
                panic,
                gate,
                ran_at,
            }
        }

        fn call(&self, function: &Function) -> Call {
            Box::pin(function.call_async(&mut self.store.clone(), &[]))
        }
    }

    fn poll(call: &mut Call) -> Poll<CallResult> {
        call.as_mut()
            .poll(&mut Context::from_waker(&futures::task::noop_waker()))
    }

    /// The address range of each pooled stack.
    fn pooled() -> Vec<std::ops::Range<usize>> {
        STACK_POOL.with(|pool| {
            pool.borrow()
                .iter()
                .map(|stack| stack.limit().get()..stack.base().get())
                .collect()
        })
    }

    #[test]
    fn a_finished_call_leaves_its_stack_to_the_next_one() {
        let host = Host::new();
        assert!(matches!(poll(&mut host.call(&host.on_stack)), Poll::Ready(Ok(_))));
        let first = pooled();
        assert_eq!(first.len(), 1, "the stack of the finished call is kept");
        assert!(first[0].contains(&host.ran_at.get()));

        host.ran_at.set(0);
        let mut call = host.call(&host.wait);
        assert!(pooled().is_empty(), "the next call took it");
        assert!(poll(&mut call).is_pending());
        // A call that overlaps it gets a stack of its own.
        let mut overlapping = host.call(&host.on_stack);
        assert!(matches!(poll(&mut overlapping), Poll::Ready(Ok(_))));
        assert!(!first[0].contains(&host.ran_at.get()));

        host.gate.set(true);
        assert!(matches!(poll(&mut call), Poll::Ready(Ok(_))));
        assert_eq!(pooled().len(), 2);
        assert!(pooled().contains(&first[0]));
    }

    #[test]
    fn the_pool_is_bounded() {
        let host = Host::new();
        let mut calls: Vec<Call> = (0..STACK_POOL_CAPACITY + 4)
            .map(|_| host.call(&host.wait))
            .collect();
        for call in &mut calls {
            assert!(poll(call).is_pending());
        }
        assert!(pooled().is_empty());
        host.gate.set(true);
        for call in &mut calls {
            assert!(matches!(poll(call), Poll::Ready(Ok(_))));
        }
        assert_eq!(pooled().len(), STACK_POOL_CAPACITY);
    }

    /// A guest's sync host function holds the store, so it must not also find
    /// it published for a host future, even under an outer poll.
    #[test]
    fn a_running_guest_hides_the_published_store() {
        let mut store = sys_store();
        let seen = Rc::new(Cell::new(None));
        let ty = FunctionType::new(vec![], vec![]);
        let inner = Function::new(&mut store, ty.clone(), {
            let seen = OnOneThread(seen.clone());
            move |_| {
                seen.get().set(Some(crate::entities::store::is_published()));
                Ok(vec![])
            }
        });
        let env = crate::FunctionEnv::new(&mut store, ());
        let outer = Function::new_async(&mut store, &env, ty, {
            async move |cx: &mut crate::AsyncEnv<()>, _: &[Value]| {
                assert!(crate::entities::store::is_published());
                inner.call_async(cx, &[]).await.map(|_| vec![])
            }
        });
        let mut call = Box::pin(outer.call_async(&mut store, &[]));
        assert!(matches!(
            call.as_mut()
                .poll(&mut Context::from_waker(&futures::task::noop_waker())),
            Poll::Ready(Ok(_))
        ));
        assert_eq!(seen.get(), Some(false));
    }

    #[test]
    fn an_unwound_coroutine_does_not_give_its_stack_back() {
        let host = Host::new();

        // Dropped while suspended.
        let mut call = host.call(&host.wait);
        assert!(poll(&mut call).is_pending());
        drop(call);
        assert!(pooled().is_empty(), "dropped while suspended");

        // Unwound by a host panic.
        let mut call = host.call(&host.panic);
        assert!(catch_unwind(AssertUnwindSafe(|| poll(&mut call))).is_err());
        drop(call);
        assert!(pooled().is_empty(), "unwound by a host panic");

        // Never polled.
        drop(host.call(&host.wait));
        assert!(pooled().is_empty(), "never started");

        // The same with a stack that had been pooled: it is not put back.
        host.gate.set(true);
        assert!(matches!(poll(&mut host.call(&host.wait)), Poll::Ready(Ok(_))));
        assert_eq!(pooled().len(), 1);
        host.gate.set(false);
        let mut call = host.call(&host.wait);
        assert!(poll(&mut call).is_pending());
        drop(call);
        let mut call = host.call(&host.panic);
        assert!(catch_unwind(AssertUnwindSafe(|| poll(&mut call))).is_err());
        drop(call);
        assert!(pooled().is_empty());

        // And calls keep working.
        host.gate.set(true);
        assert!(matches!(poll(&mut host.call(&host.wait)), Poll::Ready(Ok(_))));
        assert_eq!(pooled().len(), 1);
    }
}

/// These compile a module: not on a build without a compiler, such as the
/// Windows test lane, whose `sys` backend is there beside V8's.
#[cfg(all(
    test,
    feature = "wat",
    any(feature = "cranelift", feature = "llvm", feature = "singlepass")
))]
mod instantiation {
    use std::{
        future::Future,
        pin::Pin,
        task::{Context, Poll},
    };

    use super::*;
    use crate::{Function, FunctionEnv, FunctionEnvMut, FunctionType, Imports, Instance, Module};

    /// The instances the store owns.
    fn instances(store: &crate::Store) -> usize {
        let objects = store.as_store_ref().objects().as_sys();
        <wasmer_vm::VMInstance as wasmer_vm::StoreObject>::list(objects).len()
    }

    /// A start function that `Instance::new` runs from a host function cannot
    /// suspend its call: the host function holds a borrow of the store across
    /// it. An async import it waits in fails, and so does the instantiation;
    /// the instance is the store's already, which frees it with itself.
    #[test]
    fn a_start_function_run_by_a_host_function_cannot_suspend() {
        let mut store = sys_store();
        let module = Module::new(
            &store,
            r#"(module
              (import "h" "wait" (func $wait))
              (func $start (call $wait))
              (start $start))"#,
        )
        .unwrap();
        let unit = FunctionType::new(vec![], vec![]);
        let env = FunctionEnv::new(&mut store, ());
        let wait = Function::new_async(
            &mut store,
            &env,
            unit.clone(),
            async |_: &mut crate::AsyncEnv<()>, _: &[Value]| std::future::pending().await,
        );
        struct OnOneThread<T>(T);
        unsafe impl<T> Send for OnOneThread<T> {}
        unsafe impl<T> Sync for OnOneThread<T> {}
        impl<T> OnOneThread<T> {
            fn get(&self) -> &T {
                &self.0
            }
        }
        let imported = OnOneThread((wait, module));
        let instantiate = Function::new_with_env(&mut store, &env, unit, {
            move |mut env: FunctionEnvMut<()>, _: &[Value]| {
                let (wait, module) = imported.get();
                let mut imports = Imports::new();
                imports.define("h", "wait", wait.clone());
                match Instance::new(&mut env, module, &imports) {
                    Ok(_) => Ok(vec![]),
                    Err(crate::InstantiationError::Start(error)) => Err(error),
                    Err(error) => Err(RuntimeError::new(error.to_string())),
                }
            }
        });
        let before = instances(&store);
        let mut call = Box::pin(instantiate.call_async(&mut store, &[]));
        let waker = futures::task::noop_waker();
        let Poll::Ready(Err(error)) = call.as_mut().poll(&mut Context::from_waker(&waker)) else {
            panic!("the start function suspended the call");
        };
        assert_eq!(
            error.to_trap(),
            Some(crate::sys::vm::TrapCode::YieldOutsideAsyncContext)
        );
        drop(call);
        assert_eq!(instances(&store), before + 1, "the instance is not the store's");
    }
}

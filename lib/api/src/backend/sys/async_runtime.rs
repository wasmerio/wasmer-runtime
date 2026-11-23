//! Native async calls: `Function::call_async` runs its guest on a Wasm stack
//! of its own, which an async import suspends.
//!
//! The stack is the one the VM runs every Wasm call on (see
//! [`wasmer_vm::SuspendableCall`]). Host functions run on the stack of whoever
//! polls the call, as in a synchronous call. When the guest calls an async
//! import, the import polls its host future there once. If the future is not
//! ready, the import suspends the guest's stack, which then holds guest frames
//! only, and the future of the call polls the host future until it is ready,
//! and resumes the guest with what it returned.
//!
//! Nothing of a call stays on the thread while its guest waits: each
//! resumption installs the store and the trap handler, and takes them off
//! when control comes back. Dropping the call drops the host future and
//! resets the stack: no guest or host code runs.

#[cfg(feature = "experimental-host-interrupt")]
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, atomic::AtomicU64},
};
use std::{
    cell::Cell,
    future::Future,
    pin::Pin,
    ptr,
    task::{Context, Poll, Waker},
};

#[cfg(feature = "experimental-host-interrupt")]
use dashmap::DashMap;

use crate::{
    AsStoreRef, AsyncCallRequest, CallStore, PollScope, PollStore, RuntimeError, StoreContext,
    StoreMut, StorePtrWrapper, Value,
    backend::sys::entities::{
        engine::NativeEngineExt,
        function::{Function, RawEntry, RawHostFuture, RawResults, RawValues},
    },
    entities::function::async_call::CallFinish,
};
use wasmer_types::{RawValue, StoreId, Type};
use wasmer_vm::{CallStarted, CallStep, CallValues, INLINE_CALL_VALUES, SuspendableCall};
use std::ptr::NonNull;

#[cfg(feature = "experimental-host-interrupt")]
type AsyncCallFutureId = u64;

#[cfg(feature = "experimental-host-interrupt")]
static NEXT_ASYNC_CALL_FUTURE_ID: AtomicU64 = AtomicU64::new(0);

/// The wakers of the calls that wait, by store, for
/// [`notify_pending_futures_of_interrupt`]: a host interrupt is the only
/// thing that wakes a call from outside. Everything else a call waits for
/// is a host future, which has the waker itself. A call that never waits
/// has no waker here: an interrupt finds its guest running.
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
        let wakers = ASYNC_CALL_FUTURE_WAKERS.remove(&self.0);
        super::vm::interrupt_registry::release_store(self.0);
        // Last, and after the map's lock is released: dropping a waker is
        // user code.
        drop(wakers);
    }
}

/// What the call that is being resumed on a thread shares with the async
/// import its guest waits in.
struct Waiting {
    /// The context of the poll that resumed the guest, for the first poll of
    /// a host future.
    cx: *mut Context<'static>,
    /// The host future the guest waits for, once its first poll was pending.
    pending: Option<RawHostFuture>,
    /// What that future returned, for the import to take when the guest is
    /// resumed.
    ready: Option<Result<RawResults, RuntimeError>>,
}

thread_local! {
    /// The [`Waiting`] of the call being resumed on this thread, if one is.
    static WAITING: Cell<*mut Waiting> = const { Cell::new(ptr::null_mut()) };
}

/// The guest of a call.
pub(crate) struct Guest {
    wasm: SuspendableCall,
    /// The store's interrupt registration, which follows the guest: it is on
    /// the thread while the guest runs, and kept here while it waits, so that
    /// an interrupt still finds the store.
    #[cfg(feature = "experimental-host-interrupt")]
    interrupts: Interrupts,
}

#[cfg(feature = "experimental-host-interrupt")]
struct Interrupts {
    state: wasmer_vm::ActivationState,
    installed: Option<super::vm::interrupt_registry::InterruptInstallGuard>,
}

#[cfg(feature = "experimental-host-interrupt")]
impl Interrupts {
    /// Puts the registration on the thread, where the guest is about to
    /// run, until the returned guard is dropped. The first time, the store
    /// is registered: that fails if it is interrupted already.
    fn switch_in(&mut self, store_id: StoreId) -> Result<impl Drop + '_, RuntimeError> {
        struct SwitchBack<'s>(&'s mut wasmer_vm::ActivationState);
        impl Drop for SwitchBack<'_> {
            fn drop(&mut self) {
                // Safety: the second half of the pair below.
                unsafe { self.0.swap() };
            }
        }
        // Safety: host code, about to run the guest's stack on its thread;
        // swapped back as soon as control is back.
        unsafe { self.state.swap() };
        let switch_back = SwitchBack(&mut self.state);
        if self.installed.is_none() {
            use super::vm::interrupt_registry::{InstallError, install};
            match install(store_id) {
                Ok(installed) => self.installed = Some(installed),
                Err(InstallError::AlreadyInterrupted) => {
                    return Err(super::vm::Trap::lib(super::vm::TrapCode::HostInterrupt).into());
                }
            }
        }
        Ok(switch_back)
    }
}

#[cfg(feature = "experimental-host-interrupt")]
impl Drop for Interrupts {
    fn drop(&mut self) {
        let Some(installed) = self.installed.take() else {
            return;
        };
        if !wasmer_vm::ActivationState::thread_is_usable() {
            // What uninstalling needs of the thread is gone.
            std::mem::forget(installed);
            self.state.abandon();
            return;
        }
        // Safety: host code, with the guest's stack not running.
        unsafe { self.state.swap() };
        drop(installed);
        unsafe { self.state.swap() };
    }
}

/// What differs from one kind of call to another: how its guest is made, and
/// what is made of its results.
pub(crate) trait Entry {
    /// What the call returns.
    type Output;

    /// What to call, in the first slice of the call: the function, and its
    /// arguments with room for its results. They stay where they are until
    /// [`Self::finish`]: the guest reads them from there, and keeps there
    /// those that are too many for its own stack.
    fn start(
        &mut self,
        store: &mut StoreMut<'_>,
    ) -> Result<(&Function, &mut RawValues), RuntimeError>;

    /// What the call returns: of the values of a guest that returned, the
    /// results first, in the slice in which it did, or of what ended the call
    /// before.
    fn finish(
        &mut self,
        outcome: Result<(&mut StoreMut<'_>, &[RawValue]), RuntimeError>,
    ) -> Self::Output;
}

/// The future of one call. `'a` is its borrow of the store, if it has one.
///
/// It holds what every call needs: its store and what it calls. What only a
/// call whose guest waits needs is made when the guest first waits.
pub(crate) struct AsyncCallFuture<'a, E: Entry> {
    store: CallStore<'a>,
    entry: E,
    suspended: Option<Box<Suspended>>,
}

/// What a call keeps from the moment its guest waits.
struct Suspended {
    /// The key of the call's waker among those of its store.
    #[cfg(feature = "experimental-host-interrupt")]
    waker_id: Option<AsyncCallFutureId>,
    /// The values of the call, where the entry keeps them on the heap: too
    /// many for the guest's own stack.
    spilled: Option<NonNull<[RawValue]>>,
    guest: Guest,
    waiting: Waiting,
}

// Nothing here relies on being pinned: the guest's frames, and the values it
// was called with, are on its own stack or the heap, as is what the call
// keeps while the guest waits.
impl<E: Entry> Unpin for AsyncCallFuture<'_, E> {}

impl<'a, E: Entry> AsyncCallFuture<'a, E> {
    pub(crate) fn new(store: CallStore<'a>, entry: E) -> Self {
        Self {
            store,
            entry,
            suspended: None,
        }
    }

    /// Takes the call's waker out of the wakers of its store. It is returned
    /// rather than dropped: dropping a waker is user code.
    #[cfg(feature = "experimental-host-interrupt")]
    fn take_waker(&mut self) -> Option<Arc<Waker>> {
        let id = self.suspended.as_mut()?.waker_id.take()?;
        match ASYNC_CALL_FUTURE_WAKERS.entry(self.store.id()) {
            dashmap::Entry::Occupied(mut wakers) => {
                let removed = wakers.get_mut().remove(&id);
                if wakers.get().is_empty() {
                    wakers.remove();
                }
                removed
            }
            dashmap::Entry::Vacant(_) => None,
        }
    }

    /// There is none to take: without host interrupts, nothing wakes a call
    /// from outside.
    #[cfg(not(feature = "experimental-host-interrupt"))]
    fn take_waker(&mut self) -> Option<std::sync::Arc<Waker>> {
        None
    }

    /// Records the waker of the latest poll, which an interrupt has to wake.
    #[cfg(feature = "experimental-host-interrupt")]
    fn register_waker(&mut self, waker: &Waker) {
        let store_id = self.store.id();
        let Some(suspended) = self.suspended.as_mut() else {
            return;
        };
        let id = match suspended.waker_id {
            Some(id) => {
                let current = ASYNC_CALL_FUTURE_WAKERS
                    .get(&store_id)
                    .is_some_and(|wakers| wakers.get(&id).is_some_and(|w| w.will_wake(waker)));
                if current {
                    return;
                }
                id
            }
            None => {
                let id =
                    NEXT_ASYNC_CALL_FUTURE_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                suspended.waker_id = Some(id);
                id
            }
        };
        let waker = Arc::new(waker.clone());
        let previous = ASYNC_CALL_FUTURE_WAKERS
            .entry(store_id)
            .or_default()
            .insert(id, waker);
        // After the map's lock is released.
        drop(previous);
    }

    /// Whether the store was interrupted where this call can see it: on the
    /// thread, or in its own guest while that waits.
    #[cfg(feature = "experimental-host-interrupt")]
    fn interrupted(&self) -> bool {
        let store_id = self.store.id();
        super::vm::interrupt_registry::is_interrupted(store_id)
            || self
                .suspended
                .as_ref()
                .is_some_and(|suspended| suspended.guest.interrupts.state.is_interrupted(store_id))
    }

    /// Ends the call as an interrupt of its store does.
    #[cfg(feature = "experimental-host-interrupt")]
    fn interrupt(&mut self) -> E::Output {
        self.cancel();
        let interrupt = super::vm::Trap::lib(super::vm::TrapCode::HostInterrupt);
        self.entry.finish(Err(interrupt.into()))
    }

    /// The call waits, with nothing of its store held. Only now does an
    /// interrupt have to wake it: until here its guest ran, or was about to.
    fn wait(&mut self, cx: &mut Context<'_>) -> Poll<E::Output> {
        #[cfg(feature = "experimental-host-interrupt")]
        {
            // Before the interrupt is checked, so that one arriving after
            // the check wakes this poll's waker.
            self.register_waker(cx.waker());
            if self.interrupted() {
                return Poll::Ready(self.interrupt());
            }
        }
        #[cfg(not(feature = "experimental-host-interrupt"))]
        let _ = cx;
        Poll::Pending
    }

    /// Ends the call before its guest returned: the host future it waits for
    /// is dropped, with the store in reach of its destructor, and the guest's
    /// stack is reset. What the caller owns is dropped last.
    fn cancel(&mut self) {
        let _waker = self.take_waker();
        let Some(mut suspended) = self.suspended.take() else {
            return;
        };
        if let Some(future) = suspended.waiting.pending.take() {
            let _scope = PollScope::enter(self.store.try_resolve());
            drop(future);
        }
    }
}

impl<E: Entry> Future for AsyncCallFuture<'_, E> {
    type Output = E::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        #[cfg(feature = "experimental-host-interrupt")]
        if this.interrupted() {
            return Poll::Ready(this.interrupt());
        }

        // The guest waits for a host future: it is polled here, on this
        // stack, with the store in reach.
        if let Some(suspended) = this.suspended.as_mut()
            && let Some(future) = suspended.waiting.pending.as_mut()
        {
            let polled = {
                let _scope = PollScope::enter(Some(this.store.resolve()));
                let polled = future.as_mut().poll(cx);
                if polled.is_ready() {
                    // Its destructors run with the store in reach too.
                    suspended.waiting.pending = None;
                }
                polled
            };
            match polled {
                Poll::Pending => return this.wait(cx),
                Poll::Ready(result) => suspended.waiting.ready = Some(result),
            }
        }

        // The store is taken for as long as the guest runs, and given up as
        // soon as it waits or returns.
        let slice = this.store.resolve().enter_slice();
        // A call that starts in this poll keeps what it shares with an async
        // import on this frame, and makes nothing to keep unless its guest
        // waits: most return at once.
        let mut first = Waiting {
            cx: ptr::null_mut(),
            pending: None,
            ready: None,
        };
        let mut spilled = None;
        let starting = if this.suspended.is_none() {
            // Safety: the slice is the only holder of the store.
            let mut store = unsafe { StoreMut::from_raw(slice.ptr()) };
            match this.entry.start(&mut store) {
                Ok((function, values)) => {
                    let call_values = if values.spilled() {
                        spilled = Some(NonNull::from(&mut values[..]));
                        CallValues::At(values.as_mut_ptr())
                    } else {
                        CallValues::Inline(values.as_ptr(), values.len())
                    };
                    Some(Starting::new(&store, function, call_values))
                }
                Err(error) => return Poll::Ready(this.entry.finish(Err(error))),
            }
        } else {
            None
        };
        // Safety: as above; the handle is gone before the guest runs.
        let trap_handler = unsafe { StoreMut::from_raw(slice.ptr()) }
            .as_store_ref()
            .signal_handler();
        let store_id = this.store.id();
        // Where the guest's values go when it returns, unless they are too
        // many: the results first.
        let mut results = [RawValue { i32: 0 }; INLINE_CALL_VALUES];
        let (guest, waiting) = match this.suspended.as_deref_mut() {
            Some(suspended) => {
                spilled = suspended.spilled;
                (Some(&mut suspended.guest), &mut suspended.waiting)
            }
            None => (None, &mut first),
        };
        waiting.cx = ptr::from_mut(cx).cast();
        let step = {
            // The store is published for host futures only: a sync host
            // function of this guest holds it, and must not find it
            // published as well.
            let _hidden = PollScope::enter(None);
            // Safety: the entry is taken off before the slice ends.
            let entry = unsafe { StoreContext::new(slice.ptr()) };
            let _entered = unsafe { entry.install() };
            let _waiting = Resuming::enter(waiting);
            match (starting, guest) {
                (Some(starting), _) => starting.run(store_id, &mut results, trap_handler),
                (None, Some(guest)) => guest
                    .resume(store_id, &mut results, trap_handler)
                    .map(|step| match step {
                        CallStep::Suspended => Step::Suspended(None),
                        CallStep::Finished(returned) => Step::Finished(returned),
                    }),
                (None, None) => unreachable!("a call that neither starts nor waits"),
            }
        };
        waiting.cx = ptr::null_mut();
        let returned = match step {
            Ok(Step::Suspended(guest)) => {
                if let Some(guest) = guest {
                    // The guest waits for the first time: from here on the
                    // call keeps it, and what it waits for.
                    this.suspended = Some(Box::new(Suspended {
                        #[cfg(feature = "experimental-host-interrupt")]
                        waker_id: None,
                        spilled,
                        guest,
                        waiting: first,
                    }));
                }
                drop(slice);
                return this.wait(cx);
            }
            Ok(Step::Finished(returned)) => returned.map_err(RuntimeError::from),
            Err(error) => Err(error),
        };
        if this.suspended.is_some() {
            let _waker = this.take_waker();
            this.suspended = None;
        }
        // Safety: the slice is the only holder of the store.
        let mut store = unsafe { StoreMut::from_raw(slice.ptr()) };
        let values = match spilled {
            // Safety: the entry keeps them until `finish` is done with them.
            Some(spilled) => unsafe { spilled.as_ref() },
            None => &results[..],
        };
        let outcome = returned.map(|()| (&mut store, values));
        Poll::Ready(this.entry.finish(outcome))
    }
}

impl<E: Entry> Drop for AsyncCallFuture<'_, E> {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// What a poll of a call did with its guest.
enum Step {
    /// The guest waits. With the guest, if it started in this poll.
    Suspended(Option<Guest>),
    /// The guest is over.
    Finished(Result<(), wasmer_vm::Trap>),
}

/// A call that is about to start: what [`SuspendableCall::start`] takes.
struct Starting {
    entry: RawEntry,
    config: wasmer_vm::VMConfig,
    values: CallValues,
}

impl Starting {
    fn new(store: &StoreMut<'_>, function: &Function, values: CallValues) -> Self {
        let entry = function.raw_entry(store);
        let store = store.as_store_ref();
        let wasm_stack_size = store.engine().tunables().vmconfig().wasm_stack_size;
        Self {
            entry,
            config: wasmer_vm::VMConfig { wasm_stack_size },
            values,
        }
    }

    /// Runs the guest until it waits or is over.
    fn run(
        self,
        store_id: StoreId,
        results: &mut wasmer_vm::CallResults,
        trap_handler: Option<*const wasmer_vm::TrapHandlerFn<'static>>,
    ) -> Result<Step, RuntimeError> {
        let RawEntry {
            vmctx,
            trampoline,
            callee,
        } = self.entry;
        #[cfg(feature = "experimental-host-interrupt")]
        let mut interrupts = Interrupts {
            state: wasmer_vm::ActivationState::new(),
            installed: None,
        };
        let started = {
            #[cfg(feature = "experimental-host-interrupt")]
            let _switch_back = interrupts.switch_in(store_id)?;
            #[cfg(not(feature = "experimental-host-interrupt"))]
            let _ = store_id;
            // Safety: the function belongs to the store, which outlives the
            // call, and its entry keeps the values until the call is over.
            unsafe {
                SuspendableCall::start(
                    &self.config,
                    vmctx,
                    trampoline,
                    callee,
                    self.values,
                    results,
                    trap_handler,
                )
            }
        };
        Ok(match started {
            CallStarted::Finished(returned) => Step::Finished(returned),
            CallStarted::Suspended(wasm) => Step::Suspended(Some(Guest {
                wasm,
                #[cfg(feature = "experimental-host-interrupt")]
                interrupts,
            })),
        })
    }
}

impl Guest {
    /// Runs a guest that waits until it waits again or is over.
    fn resume(
        &mut self,
        store_id: StoreId,
        results: &mut wasmer_vm::CallResults,
        trap_handler: Option<*const wasmer_vm::TrapHandlerFn<'static>>,
    ) -> Result<CallStep, RuntimeError> {
        #[cfg(feature = "experimental-host-interrupt")]
        let _switch_back = self.interrupts.switch_in(store_id)?;
        #[cfg(not(feature = "experimental-host-interrupt"))]
        let _ = store_id;
        // Safety: on the thread of the call, which is not over.
        Ok(unsafe { self.wasm.resume(results, trap_handler) })
    }
}

/// A call whose arguments and results are [`Value`]s, converted in slices of
/// the call.
pub(crate) struct Converted<X> {
    /// The call, until its first poll starts it.
    request: Option<AsyncCallRequest<X>>,
    /// Once it started: the types of its results, and what makes `X` of them.
    results: Option<(Vec<Type>, CallFinish<X>)>,
    /// Once it started: what it calls, until it is over.
    started: Option<(Function, RawValues)>,
}

impl<X> Converted<X> {
    pub(crate) fn new(request: AsyncCallRequest<X>) -> Self {
        Self {
            request: Some(request),
            results: None,
            started: None,
        }
    }
}

impl<X> Entry for Converted<X> {
    type Output = Result<X, RuntimeError>;

    fn start(
        &mut self,
        store: &mut StoreMut<'_>,
    ) -> Result<(&Function, &mut RawValues), RuntimeError> {
        let request = self.request.take().expect("polled after completion");
        let params = (request.params)(store)?;
        let function = request.function.as_sys();
        let (values, results) = function.raw_params(store, &params)?;
        self.results = Some((results, request.finish));
        let (function, values) = self.started.insert((function.clone(), values));
        Ok((function, values))
    }

    fn finish(
        &mut self,
        outcome: Result<(&mut StoreMut<'_>, &[RawValue]), RuntimeError>,
    ) -> Self::Output {
        // Dropped with the call, if it ended before it started.
        self.request = None;
        let (store, values) = outcome?;
        let (types, finish) = self.results.take().expect("a call that started");
        let results = types.iter().zip(values);
        let results: Vec<Value> = results
            .map(|(ty, raw)| unsafe { Value::from_raw(store, *ty, *raw) })
            .collect();
        // The guest's values were read.
        self.started = None;
        finish(store, &results)
    }
}

/// A typed call: its arguments were made raw values when it was made, and
/// `finish` reads its results, so that nothing of it is on the heap.
pub(crate) struct Typed<R> {
    function: Function,
    /// The arguments, with room for the results, until the call starts.
    values: Option<Result<RawValues, RuntimeError>>,
    finish: TypedFinish<R>,
}

/// What a typed call returns; see [`Entry::finish`].
pub(crate) type TypedFinish<R> = fn(Result<(&mut StoreMut<'_>, &[RawValue]), RuntimeError>) -> R;

impl<R> Typed<R> {
    pub(crate) fn new(
        function: Function,
        values: Result<RawValues, RuntimeError>,
        finish: TypedFinish<R>,
    ) -> Self {
        Self {
            function,
            values: Some(values),
            finish,
        }
    }
}

impl<R> Entry for Typed<R> {
    type Output = R;

    fn start(
        &mut self,
        _: &mut StoreMut<'_>,
    ) -> Result<(&Function, &mut RawValues), RuntimeError> {
        if matches!(self.values, Some(Err(_))) {
            return Err(self.values.take().and_then(Result::err).expect("the error"));
        }
        let values = self.values.as_mut().expect("polled after completion");
        let values = values.as_mut().expect("values that are not an error");
        Ok((&self.function, values))
    }

    fn finish(
        &mut self,
        outcome: Result<(&mut StoreMut<'_>, &[RawValue]), RuntimeError>,
    ) -> R {
        let finished = (self.finish)(outcome);
        // The guest's values, or those of a call that ended before it
        // started.
        self.values = None;
        finished
    }
}

/// Publishes the [`Waiting`] of a call for as long as its guest runs.
struct Resuming(*mut Waiting);

impl Resuming {
    fn enter(waiting: &mut Waiting) -> Self {
        Self(WAITING.replace(waiting))
    }
}

impl Drop for Resuming {
    fn drop(&mut self) {
        WAITING.set(self.0);
    }
}

/// What an async import does with its host future: see
/// [`wait_for_host_future`].
pub(crate) enum HostWait<R> {
    /// The future was ready.
    Ready(R),
    /// The guest is to be suspended: its call has the future.
    Suspend,
    /// The future was not ready, and the guest cannot be suspended.
    CannotSuspend,
}

/// Polls the host future of an async import once, on behalf of the guest
/// that called it, on the host stack. The store is reachable through the
/// entry of the call beneath: `store` is the borrow of it that the import
/// read its arguments with, kept for the poll so that nothing else can take
/// it meanwhile.
///
/// With `suspendable`, the guest runs on the stack of a `call_async`, which
/// takes a future that is not ready, as `into_pending` makes it: with raw
/// results. Otherwise the guest was entered synchronously, by a host function
/// that holds a borrow of its store across the call: it cannot wait.
pub(crate) fn wait_for_host_future<Fut: Future + ?Sized>(
    store: StorePtrWrapper,
    future: Pin<Box<Fut>>,
    suspendable: bool,
    into_pending: impl FnOnce(Pin<Box<Fut>>) -> RawHostFuture,
) -> HostWait<Fut::Output> {
    // The entry's pointer is used only for the duration of this poll, in the
    // slices the future takes, while the borrow in `store` stands.
    let _scope = PollScope::enter(Some(PollStore::exclusive(store.id(), store.as_ptr())));
    // Declared after the scope, so that its destructor runs with the store of
    // this entry, not whatever was published before.
    let mut future = future;
    let waiting = WAITING.get();
    if suspendable && !waiting.is_null() {
        // Safety: the call that is being resumed published it, and its poll
        // is beneath on this stack.
        let waiting = unsafe { &mut *waiting };
        let cx = unsafe { &mut *waiting.cx.cast::<Context<'_>>() };
        return match future.as_mut().poll(cx) {
            Poll::Ready(result) => HostWait::Ready(result),
            Poll::Pending => {
                waiting.pending = Some(into_pending(future));
                HostWait::Suspend
            }
        };
    }
    match future.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(result) => HostWait::Ready(result),
        Poll::Pending => HostWait::CannotSuspend,
    }
}

/// What the host future of the import returned, for the guest that was
/// resumed with it.
pub(crate) fn take_host_result() -> Result<RawResults, RuntimeError> {
    let waiting = WAITING.get();
    assert!(!waiting.is_null(), "a guest was resumed by no call");
    // Safety: as in `wait_for_host_future`.
    let ready = unsafe { &mut *waiting }.ready.take();
    ready.expect("a guest was resumed before its host future was ready")
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

/// A store of this backend, whichever backend `Store::default` picks: the
/// Windows test lane makes V8 the default, which has no async host functions.
#[cfg(test)]
fn sys_store() -> crate::Store {
    crate::Store::new(crate::backend::sys::entities::engine::default_engine())
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

#[cfg(test)]
mod published_store {
    use std::rc::Rc;

    use super::*;
    use crate::{Function, FunctionType};

    /// The tests are single-threaded; a host function has to be `Send`.
    struct OnOneThread<T>(T);
    unsafe impl<T> Send for OnOneThread<T> {}
    unsafe impl<T> Sync for OnOneThread<T> {}

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
                let seen = &seen;
                seen.0.set(Some(crate::entities::store::is_published()));
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
            call.as_mut().poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Ok(_))
        ));
        assert_eq!(seen.get(), Some(false));
    }
}

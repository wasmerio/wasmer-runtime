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
    AsStoreMut, AsStoreRef, AsyncCallRequest, CallStore, PollScope, PollStore, RuntimeError,
    StoreContext, StoreMut, StorePtrWrapper, Value,
    backend::sys::entities::{
        engine::NativeEngineExt,
        function::{RawEntry, RawHostFuture, RawResults},
    },
    entities::function::async_call::CallFinish,
};
use wasmer_types::{RawValue, StoreId, Type};
use wasmer_vm::{CallStep, SuspendableCall};

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

/// The guest of a call, once its arguments are converted.
struct Guest {
    wasm: SuspendableCall,
    /// The arguments, which the trampoline overwrites with the results.
    values: Vec<RawValue>,
    /// The types of the results.
    results: Vec<Type>,
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

/// The future of one call. `'a` is its borrow of the store, if it has one.
pub(crate) struct AsyncCallFuture<'a, X: 'static> {
    #[cfg(feature = "experimental-host-interrupt")]
    id: AsyncCallFutureId,
    store: CallStore<'a>,
    /// The call, until its first poll starts it.
    request: Option<AsyncCallRequest<X>>,
    finish: Option<CallFinish<X>>,
    guest: Option<Guest>,
    waiting: Waiting,
}

// Nothing here relies on being pinned: the guest's frames are on its own
// stack, and the host future is boxed.
impl<X> Unpin for AsyncCallFuture<'_, X> {}

impl<'a, X: 'static> AsyncCallFuture<'a, X> {
    pub(crate) fn new(store: CallStore<'a>, request: AsyncCallRequest<X>) -> Self {
        Self {
            #[cfg(feature = "experimental-host-interrupt")]
            id: NEXT_ASYNC_CALL_FUTURE_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
            store,
            request: Some(request),
            finish: None,
            guest: None,
            waiting: Waiting {
                cx: ptr::null_mut(),
                pending: None,
                ready: None,
            },
        }
    }

    /// Takes the call's waker out of the wakers of its store. It is returned
    /// rather than dropped: dropping a waker is user code.
    #[cfg(feature = "experimental-host-interrupt")]
    fn take_waker(&self) -> Option<Arc<Waker>> {
        match ASYNC_CALL_FUTURE_WAKERS.entry(self.store.id()) {
            dashmap::Entry::Occupied(mut wakers) => {
                let removed = wakers.get_mut().remove(&self.id);
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
    fn take_waker(&self) -> Option<std::sync::Arc<Waker>> {
        None
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

    /// Ends the call before its guest returned: the host future it waits for
    /// is dropped, with the store in reach of its destructor, and the guest's
    /// stack is reset. What the caller owns is dropped last.
    fn cancel(&mut self) {
        let _waker = self.take_waker();
        self.request = None;
        if let Some(future) = self.waiting.pending.take() {
            let _scope = PollScope::enter(self.store.try_resolve());
            drop(future);
        }
        self.guest = None;
    }
}

impl<X: 'static> Future for AsyncCallFuture<'_, X> {
    type Output = Result<X, RuntimeError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;
        // Before the interrupt is checked, so that one arriving after the
        // check wakes this poll's waker.
        #[cfg(feature = "experimental-host-interrupt")]
        {
            this.register_waker(cx.waker());
            let store_id = this.store.id();
            let interrupted = super::vm::interrupt_registry::is_interrupted(store_id)
                || this
                    .guest
                    .as_ref()
                    .is_some_and(|guest| guest.interrupts.state.is_interrupted(store_id));
            if interrupted {
                this.cancel();
                return Poll::Ready(Err(super::vm::Trap::lib(
                    super::vm::TrapCode::HostInterrupt,
                )
                .into()));
            }
        }

        // The guest waits for a host future: it is polled here, on this
        // stack, with the store in reach.
        if let Some(future) = this.waiting.pending.as_mut() {
            let _scope = PollScope::enter(Some(this.store.resolve()));
            match future.as_mut().poll(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(result) => {
                    // Its destructors run with the store in reach too.
                    this.waiting.pending = None;
                    this.waiting.ready = Some(result);
                }
            }
        }

        // The store is taken for as long as the guest runs, and given up as
        // soon as it waits or returns.
        let slice = this.store.resolve().enter_slice();
        if let Some(request) = this.request.take() {
            // Safety: the slice is the only holder of the store.
            let mut store = unsafe { StoreMut::from_raw(slice.ptr()) };
            match Guest::new(&mut store, request.function, request.params) {
                Ok(guest) => {
                    this.guest = Some(guest);
                    this.finish = Some(request.finish);
                }
                Err(error) => {
                    let _waker = this.take_waker();
                    return Poll::Ready(Err(error));
                }
            }
        }
        let guest = this.guest.as_mut().expect("polled after completion");
        // Safety: as above; the handle is gone before the guest runs.
        let trap_handler = unsafe { StoreMut::from_raw(slice.ptr()) }
            .as_store_ref()
            .signal_handler();
        this.waiting.cx = ptr::from_mut(cx).cast();
        let step = {
            // The store is published for host futures only: a sync host
            // function of this guest holds it, and must not find it
            // published as well.
            let _hidden = PollScope::enter(None);
            // Safety: the entry is taken off before the slice ends.
            let _entered = unsafe { StoreContext::install(slice.ptr()) };
            let _waiting = Resuming::enter(&mut this.waiting);
            guest.resume(this.store.id(), trap_handler)
        };
        this.waiting.cx = ptr::null_mut();
        let result = match step {
            Ok(CallStep::Suspended) => return Poll::Pending,
            Ok(CallStep::Finished(result)) => result.map_err(RuntimeError::from),
            Err(error) => Err(error),
        };
        let guest = this.guest.take().expect("the guest that ran");
        let _waker = this.take_waker();
        let finish = this.finish.take().expect("a call that started");
        Poll::Ready(result.and_then(|()| {
            // Safety: the slice is the only holder of the store.
            let mut store = unsafe { StoreMut::from_raw(slice.ptr()) };
            let results = guest.results(&mut store);
            finish(&mut store, &results)
        }))
    }
}

impl<X: 'static> Drop for AsyncCallFuture<'_, X> {
    fn drop(&mut self) {
        self.cancel();
    }
}

impl Guest {
    fn new(
        store: &mut StoreMut<'_>,
        function: crate::Function,
        params: crate::entities::function::async_call::CallParams,
    ) -> Result<Self, RuntimeError> {
        let params = params(store)?;
        let RawEntry {
            vmctx,
            trampoline,
            callee,
            mut values,
            results,
        } = function.as_sys().raw_entry(store, &params)?;
        let store = store.as_store_ref();
        let config = store.engine().tunables().vmconfig();
        // Safety: the function belongs to the store, which outlives the
        // call, and the values are kept with it.
        let wasm = unsafe {
            SuspendableCall::new(config, vmctx, trampoline, callee, values.as_mut_ptr().cast())
        }
        .map_err(RuntimeError::from)?;
        Ok(Self {
            wasm,
            values,
            results,
            #[cfg(feature = "experimental-host-interrupt")]
            interrupts: Interrupts {
                state: wasmer_vm::ActivationState::new(),
                installed: None,
            },
        })
    }

    /// Runs the guest until it waits or is over.
    fn resume(
        &mut self,
        store_id: StoreId,
        trap_handler: Option<*const wasmer_vm::TrapHandlerFn<'static>>,
    ) -> Result<CallStep, RuntimeError> {
        #[cfg(feature = "experimental-host-interrupt")]
        let _switch_back = self.interrupts.switch_in(store_id)?;
        #[cfg(not(feature = "experimental-host-interrupt"))]
        let _ = store_id;
        // Safety: on the thread of the call, which is not over.
        Ok(unsafe { self.wasm.resume(trap_handler) })
    }

    /// The results of a guest that returned.
    fn results(&self, store: &mut StoreMut<'_>) -> Vec<Value> {
        let results = self.results.iter().zip(&self.values);
        results
            .map(|(ty, raw)| unsafe { Value::from_raw(store, *ty, *raw) })
            .collect()
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

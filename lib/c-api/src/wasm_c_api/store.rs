use super::engine::wasm_engine_t;
use std::cell::RefCell;
use std::rc::{Rc, Weak};
use wasmer_api::{
    AsStoreMut, Extern, Function, Instance, InstantiationError, Module, RuntimeError, Store,
    StoreMut, Value,
};

#[derive(Clone)]
pub struct StoreRef {
    inner: Rc<StoreState>,
}

struct StoreState {
    id: wasmer_types::StoreId,
    mode: StoreMode,
}

#[cfg(all(feature = "v8", feature = "extensions"))]
pub mod v8;

enum StoreMode {
    Plain(PlainStore),
    #[cfg(feature = "sys")]
    Native(ManagedStore),
}

// Keep the existing outer mode matches (including frozen native controls).
// Only the actual V8 path uses this owned backing; other plain Stores keep
// their existing RefCell borrow behavior.
enum PlainStore {
    Borrowed(RefCell<Store>),
    #[cfg(all(feature = "v8", feature = "extensions"))]
    V8(v8::ManagedStore),
}
impl PlainStore {
    fn new(store: Store) -> Self {
        #[cfg(all(feature = "v8", feature = "extensions"))]
        if store.engine().is_v8() {
            return Self::V8(v8::ManagedStore::new(store));
        }
        Self::Borrowed(RefCell::new(store))
    }
    fn try_borrow_mut(&self) -> Result<std::cell::RefMut<'_, Store>, RuntimeError> {
        match self {
            Self::Borrowed(store) => store
                .try_borrow_mut()
                .map_err(|_| RuntimeError::new("Store is already borrowed")),
            #[cfg(all(feature = "v8", feature = "extensions"))]
            Self::V8(_) => Err(RuntimeError::new(
                "V8 Store requires its actual owner guard",
            )),
        }
    }
}

#[cfg(feature = "sys")]
struct ManagedStore {
    store: Option<wasmer_api::StoreAsync>,
    generation: Option<wasmer_api::sys::NativeInvocationScope>,
    owner: wasmer_api::sys::NativeCleanupOwner,
    driver: RefCell<Option<Rc<Driver>>>,
    ledger: Rc<RetirementLedger>,
    scopes: Rc<DriverScopes>,
    cleanup_failure: Rc<RetirementFailure>,
    failures_receiver: wasmer_api::sys::NativeCleanupFailures,
    seal: Option<wasmer_api::sys::NativeCleanupSeal>,
    reservation: Option<wasmer_api::sys::NativeStoreDestructionReservation>,
    finalization: RefCell<Weak<NativeStoreFinalizationHandoff>>,
    #[cfg(feature = "extensions")]
    endpoints: Box<StoreEndpoints>,
}

/// Extension endpoints of one native Store.
#[cfg(all(feature = "sys", feature = "extensions"))]
#[derive(Default)]
struct StoreEndpoints {
    registered: RefCell<Vec<Weak<dyn ContinuationEndpoint>>>,
    // Outlives its endpoint: a handoff error can surface after the endpoint dropped.
    handoff: std::cell::Cell<Option<HandoffClassifier>>,
}

#[cfg(feature = "sys")]
pub struct Driver {
    runtime: std::sync::Arc<tokio::runtime::Runtime>,
    local: tokio::task::LocalSet,
    active: std::cell::Cell<bool>,
    scopes: Rc<DriverScopes>,
    owner: wasmer_api::sys::NativeCleanupOwner,
    ledger: Rc<RetirementLedger>,
    cleanup_failure: Weak<RetirementFailure>,
    wake: std::sync::Arc<DriverWake>,
}

// Metadata only. Counts exact lexical receipts, never Rc::strong_count policy.
// It outlives each actual Rc/entered guard so the release wake cannot precede
// destruction of a wrapper's real Driver reference.
#[cfg(feature = "sys")]
#[derive(Default)]
struct DriverScopes {
    entered: std::cell::Cell<usize>,
    users: std::cell::Cell<usize>,
    common_collector: RefCell<Weak<CommonCollectorState>>,
    finalization: RefCell<Weak<NativeStoreFinalizationHandoff>>,
}
#[cfg(feature = "sys")]
impl DriverScopes {
    fn canonical_allowed(&self, owner: Option<&CommonCollectorLease>) -> bool {
        let current = self.common_collector.borrow().upgrade();
        current.is_none_or(|current| {
            current.released.get() || owner.is_some_and(|owner| Rc::ptr_eq(&current, &owner.state))
        })
    }
    fn released(&self) {
        if self.entered.get() == 0 && self.users.get() == 0 {
            let handoff = self.finalization.borrow().upgrade();
            if let Some(handoff) = handoff
                && handoff.blocked.replace(false)
            {
                handoff.notify();
            }
        }
    }
}
// Exclusive CLAIM identity of one actual common drive, never an admission or
// progress lock. Other tails continue their own outcomes without these events.
#[cfg(feature = "sys")]
struct CommonCollectorState {
    scopes: Rc<DriverScopes>,
    canonical: Weak<RetirementFailure>,
    wake: std::sync::Weak<DriverWake>,
    service_claim: RefCell<Option<ServiceClaimLease>>,
    released: std::cell::Cell<bool>,
}
#[cfg(feature = "sys")]
struct CommonCollectorLease {
    state: Rc<CommonCollectorState>,
}
#[cfg(feature = "sys")]
impl CommonCollectorLease {
    fn wake_while_owned(&self) {
        let canonical = self
            .state
            .canonical
            .upgrade()
            .expect("active common collector lost canonical custody");
        if let Some(wake) = self.state.wake.upgrade() {
            // Snapshot ONLY metadata under the mutex. Catch/stamp each actual
            // subscriber callback at its ORIGINAL catch, not after the fanout.
            let subscribers = wake
                .state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .subscribers
                .values()
                .filter_map(Clone::clone)
                .collect::<Vec<_>>();
            for subscriber in subscribers {
                canonical.attempt(|| subscriber.wake_by_ref());
                canonical.attempt(|| drop(subscriber));
            }
            canonical.attempt(|| drop(wake));
        }
    }
    fn clear_without_callbacks(&self) {
        if self.state.released.replace(true) {
            return;
        }
        {
            let mut current = self.state.scopes.common_collector.borrow_mut();
            if current.ptr_eq(&Rc::downgrade(&self.state)) {
                *current = Weak::new();
            }
        }
        let claim = self.state.service_claim.borrow_mut().take();
        drop(claim);
    }
    fn release_without_callbacks(self) {
        self.clear_without_callbacks();
    }
}
#[cfg(feature = "sys")]
impl Drop for CommonCollectorLease {
    fn drop(&mut self) {
        if !self.state.released.get() {
            // Cancellation: actual future/output fields were already disposed.
            // Wake/reconcile while identity and canonical first remain owned.
            self.wake_while_owned();
            self.clear_without_callbacks();
        }
    }
}

#[cfg(feature = "sys")]
struct DriveState<'a, F: std::future::Future> {
    driver: &'a Driver,
    future: Option<std::pin::Pin<Box<F>>>,
    output: Option<F::Output>,
    main_ready: bool,
    collector: Option<CommonCollectorLease>, // LAST through callback-bearing fields
}
#[cfg(feature = "sys")]
impl<F: std::future::Future> DriveState<'_, F> {
    fn dispose_completed_frame(&mut self) {
        let future = self.future.take();
        self.driver.canonical_failure().attempt(|| drop(future));
    }
    fn dispose_output(&mut self) {
        let output = self.output.take();
        self.driver.canonical_failure().attempt(|| drop(output));
    }
}
#[cfg(feature = "sys")]
impl<F: std::future::Future> Drop for DriveState<'_, F> {
    fn drop(&mut self) {
        {
            let _entered = self.driver.entered();
            let _runtime = self.driver.runtime.enter();
            let _local = self.driver.local.enter();
            self.dispose_completed_frame();
            self.dispose_output();
        }
        // No extra entered/runtime/local guard or user field remains when the
        // release wake can reenter. Canonical first stays in its original slot.
        drop(self.collector.take());
    }
}

#[cfg(feature = "sys")]
struct DriverUseReceipt(Rc<DriverScopes>);
#[cfg(feature = "sys")]
impl DriverUseReceipt {
    fn new(scopes: Rc<DriverScopes>) -> Self {
        scopes.users.set(scopes.users.get() + 1);
        Self(scopes)
    }
}
#[cfg(feature = "sys")]
impl Drop for DriverUseReceipt {
    fn drop(&mut self) {
        self.0.users.set(self.0.users.get() - 1);
        self.0.released();
    }
}
#[cfg(feature = "sys")]
struct DriverUse {
    driver: Option<Rc<Driver>>,
    lease: Option<DriverUseReceipt>,
    exit: Option<wasmer_api::sys::NativeCleanupExitBoundary>,
}
#[cfg(feature = "sys")]
impl DriverUse {
    fn acquire(native: &ManagedStore) -> Result<Self, RuntimeError> {
        let exit = native.owner.enter_exit_boundary();
        let lease = DriverUseReceipt::new(native.scopes.clone());
        let driver = native.driver()?;
        Ok(Self {
            driver: Some(driver),
            lease: Some(lease),
            exit: Some(exit),
        })
    }
    #[cfg(feature = "extensions")]
    fn capture(owner: &CleanupRetirement) -> Self {
        let exit = owner.owner.enter_exit_boundary();
        let lease = DriverUseReceipt::new(owner.driver.scopes.clone());
        Self {
            driver: Some(owner.driver.clone()),
            lease: Some(lease),
            exit: Some(exit),
        }
    }
    fn driver(&self) -> &Rc<Driver> {
        self.driver.as_ref().unwrap()
    }
}
#[cfg(feature = "sys")]
impl std::ops::Deref for DriverUse {
    type Target = Rc<Driver>;
    fn deref(&self) -> &Self::Target {
        self.driver()
    }
}
#[cfg(feature = "sys")]
impl Drop for DriverUse {
    fn drop(&mut self) {
        let unwinding = std::thread::panicking();
        let mut first = None;
        attempt_retirement(&mut first, || drop(self.driver.take()));
        attempt_retirement(&mut first, || drop(self.lease.take()));
        attempt_retirement(&mut first, || drop(self.exit.take()));
        if let Some(first) = first {
            if unwinding {
                discard_retirement_panic(first);
            } else {
                std::panic::resume_unwind(first);
            }
        }
    }
}
#[cfg(feature = "sys")]
struct DriverEntered<'a>(&'a DriverScopes);
#[cfg(feature = "sys")]
impl Drop for DriverEntered<'_> {
    fn drop(&mut self) {
        self.0.entered.set(self.0.entered.get() - 1);
        self.0.released();
    }
}

// The shared wake path carries only Send metadata. All local executor and
// native-owner polling stays in the subscribing caller on its original thread.
#[cfg(feature = "sys")]
#[derive(Default)]
struct DriverWake {
    state: std::sync::Mutex<DriverWakeState>,
}
#[cfg(feature = "sys")]
#[derive(Default)]
struct DriverWakeState {
    next: u64,
    subscribers: std::collections::HashMap<u64, Option<std::sync::Arc<std::task::Waker>>>,
}
#[cfg(feature = "sys")]
impl std::task::Wake for DriverWake {
    fn wake(self: std::sync::Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &std::sync::Arc<Self>) {
        let subscribers = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .subscribers
            .values()
            .filter_map(Clone::clone)
            .collect::<Vec<_>>();
        let mut panic = None;
        for subscriber in subscribers {
            attempt_retirement(&mut panic, || subscriber.wake_by_ref());
            attempt_retirement(&mut panic, || drop(subscriber));
        }
        if let Some(panic) = panic {
            std::panic::resume_unwind(panic);
        }
    }
}

#[cfg(feature = "sys")]
impl DriverWake {
    fn with_real_context<R>(
        &self,
        token: u64,
        fallback: &mut std::task::Context<'_>,
        poll: impl FnOnce(&mut std::task::Context<'_>) -> R,
    ) -> R {
        let real = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .subscribers
            .get(&token)
            .and_then(Clone::clone);
        match real {
            Some(real) => poll(&mut std::task::Context::from_waker(&real)),
            // A directly nested internal poll may already carry our stable
            // observer. It creates no new consumer or substitute caller.
            None => poll(fallback),
        }
    }

    fn with_context<R>(
        self: &std::sync::Arc<Self>,
        token: u64,
        cx: &mut std::task::Context<'_>,
        poll: impl FnOnce(&mut std::task::Context<'_>) -> R,
    ) -> R {
        let waker = std::task::Waker::from(self.clone());
        // Nested polling can already carry this exact stable observer. It must
        // never be stored as its own consumer (self-wake and an Arc cycle).
        if !waker.will_wake(cx.waker()) {
            let next = std::sync::Arc::new(cx.waker().clone());
            let previous = {
                let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                let slot = state.subscribers.get_mut(&token).unwrap();
                if slot.as_ref().is_some_and(|old| old.will_wake(&next)) {
                    Some(next)
                } else {
                    slot.replace(next)
                }
            };
            drop(previous);
        }
        poll(&mut std::task::Context::from_waker(&waker))
    }
}

#[cfg(feature = "sys")]
pub struct DriverSubscription {
    wake: std::sync::Arc<DriverWake>,
    owner: wasmer_api::sys::NativeCleanupOwner,
    token: u64,
}
#[cfg(feature = "sys")]
impl DriverSubscription {
    pub fn with_real_context<R>(
        &self,
        fallback: &mut std::task::Context<'_>,
        poll: impl FnOnce(&mut std::task::Context<'_>) -> R,
    ) -> R {
        self.wake.with_real_context(self.token, fallback, poll)
    }

    pub fn with_context<R>(
        &self,
        cx: &mut std::task::Context<'_>,
        poll: impl FnOnce(&mut std::task::Context<'_>) -> R,
    ) -> R {
        self.wake.with_context(self.token, cx, poll)
    }
}
#[cfg(feature = "sys")]
impl Drop for DriverSubscription {
    fn drop(&mut self) {
        let unwinding = std::thread::panicking();
        let previous = self
            .wake
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .subscribers
            .remove(&self.token);
        let mut panic = None;
        attempt_retirement(&mut panic, || drop(previous));
        // Dropping a user Waker may admit a new subscriber. Check LAST only
        // after its destructor has run, and never hold metadata during clear.
        let empty = self
            .wake
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .subscribers
            .is_empty();
        if empty {
            attempt_retirement(&mut panic, || self.owner.clear_idle_waker());
        }
        if let Some(panic) = panic {
            if unwinding {
                discard_retirement_panic(panic);
            } else {
                std::panic::resume_unwind(panic);
            }
        }
    }
}

#[cfg(all(feature = "sys", feature = "extensions"))]
#[derive(Debug)]
pub struct CompletedHandoff(RuntimeError);
#[cfg(all(feature = "sys", feature = "extensions"))]
impl std::fmt::Display for CompletedHandoff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.message())
    }
}
#[cfg(all(feature = "sys", feature = "extensions"))]
impl std::error::Error for CompletedHandoff {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}
/// Marks `error` as the result of a handoff that already completed.
#[cfg(all(feature = "sys", feature = "extensions"))]
pub fn completed_handoff(error: RuntimeError) -> RuntimeError {
    RuntimeError::user(Box::new(CompletedHandoff(error)))
}

/// The future of one guest call made inside an [`EntryScope`].
#[cfg(all(feature = "sys", feature = "extensions"))]
pub type EntryFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Box<[Value]>, RuntimeError>>>>;

/// Recognizes the call handoff errors of one kind of [`ContinuationEndpoint`].
#[cfg(all(feature = "sys", feature = "extensions"))]
pub type HandoffClassifier = fn(&RuntimeError) -> bool;

/// A future borrowing an [`EntryScope`].
#[cfg(all(feature = "sys", feature = "extensions"))]
pub type ScopeFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + 'a>>;

/// Host state an extension keeps for one native call through an initialized start.
///
/// Re-entrant imports of the scope call [`invoke_initialized_entry`] and
/// [`authorize_initialized_entry`], passing back the opaque `token`.
#[cfg(all(feature = "sys", feature = "extensions"))]
pub trait EntryScope {
    fn call<'a>(
        &'a self,
        function: &'a Function,
        args: &'a [Value],
        owner: Weak<dyn std::any::Any>,
    ) -> ScopeFuture<'a, Result<Box<[Value]>, RuntimeError>>;
    fn contains(&self, token: &dyn std::any::Any, row: usize) -> bool;
    fn accepts<'a>(
        &'a self,
        token: &'a dyn std::any::Any,
        row: usize,
        store: &'a wasmer_api::StoreAsync,
    ) -> ScopeFuture<'a, bool>;
    fn finish<'a>(&'a self, dispose: &'a mut dyn FnMut(Box<dyn FnOnce()>)) -> ScopeFuture<'a, ()>;
    /// Maps the error a call inside this scope ended with, such as a thread exit.
    fn map_error(&self, error: RuntimeError) -> RuntimeError;
}

/// Host state a Store extension, such as WASI, registers with
/// [`StoreRef::register_endpoint`].
#[cfg(all(feature = "sys", feature = "extensions"))]
pub trait ContinuationEndpoint {
    /// Returns opaque state when `actual` is this endpoint's initialized start.
    fn initialized_start(
        &self,
        _store: &mut StoreMut<'_>,
        _actual: &Function,
    ) -> Option<Box<dyn std::any::Any>> {
        None
    }
    /// Opens the scope of one call over every endpoint's
    /// [`Self::initialized_start`] state; called on the first endpoint that had one.
    fn open_entry_scope(
        &self,
        _starts: Vec<Box<dyn std::any::Any>>,
        _store: wasmer_api::StoreAsync,
    ) -> Result<Box<dyn EntryScope>, RuntimeError> {
        Err(RuntimeError::new(
            "continuation endpoint has no entry scope",
        ))
    }
    /// Recognizes the call handoffs endpoints of this kind produce.
    fn handoff_classifier(&self) -> HandoffClassifier {
        |_| false
    }
    fn matches(&self, store: &mut StoreMut<'_>, error: &RuntimeError) -> bool;
    fn prepare(
        self: Rc<Self>,
        store: &StoreRef,
        error: &RuntimeError,
    ) -> Result<LocalContinuation, RuntimeError>;
    fn cancel_unclaimed(
        self: Rc<Self>,
        store: &StoreRef,
        error: &RuntimeError,
    ) -> Result<(), RuntimeError>;
}

#[cfg(all(feature = "sys", feature = "extensions"))]
pub type ContinuationFuture =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), RuntimeError>>>>;
#[cfg(all(feature = "sys", feature = "extensions"))]
pub type ContinuationEntry = Box<dyn FnOnce() -> ContinuationFuture>;

#[cfg(all(feature = "sys", feature = "extensions"))]
pub struct LocalContinuation {
    pub store: StoreRef,
    pub run: Option<ContinuationEntry>,
    pub cancel: Option<Box<dyn FnOnce()>>,
}
#[cfg(all(feature = "sys", feature = "extensions"))]
impl Drop for LocalContinuation {
    fn drop(&mut self) {
        if let Some(cancel) = self.cancel.take() {
            cancel();
        }
    }
}

/// Captured before admission; owns only the existing local executor and cleanup pump.
#[cfg(all(feature = "sys", feature = "extensions"))]
pub struct CleanupRetirement {
    driver: Rc<Driver>, // drops BEFORE its final lexical lease
    owner: wasmer_api::sys::NativeCleanupOwner,
    _use_receipt: DriverUseReceipt,
}
#[cfg(all(feature = "sys", feature = "extensions"))]
impl Clone for CleanupRetirement {
    fn clone(&self) -> Self {
        let lease = DriverUseReceipt::new(self.driver.scopes.clone());
        Self {
            driver: self.driver.clone(),
            owner: self.owner.clone(),
            _use_receipt: lease,
        }
    }
}
#[cfg(all(feature = "sys", feature = "extensions"))]
struct RetainedDriverFuture {
    future: std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>,
    receipt: CleanupRetirement, // future and actual Driver Rc drop before lease
}
#[cfg(all(feature = "sys", feature = "extensions"))]
impl std::future::Future for RetainedDriverFuture {
    type Output = ();
    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        let this = &mut *self;
        let _entered = this.receipt.driver.entered();
        let _runtime = this.receipt.driver.runtime.enter();
        let _local = this.receipt.driver.local.enter();
        this.future.as_mut().poll(cx)
    }
}
#[cfg(all(feature = "sys", feature = "extensions"))]
impl CleanupRetirement {
    #[cfg(feature = "extensions")]
    pub fn subscribe(&self) -> DriverSubscription {
        self.driver.subscribe()
    }
    #[cfg(all(test, feature = "extensions"))]
    pub(crate) fn notify_completion(&self) {
        let wake = std::task::Waker::from(self.driver.wake.clone());
        // The terminal flag/result are already published. Dispatch after the
        // native pump releases its running guard, outside every metadata lock.
        self.owner
            .after_pump_existing(Box::new(move || wake.wake()));
    }
    #[cfg(feature = "extensions")]
    pub fn notify_retirement(&self, failure: RetirementFailure) {
        let wake = std::task::Waker::from(self.driver.wake.clone());
        self.owner.after_pump_existing(Box::new(move || {
            failure.attempt(|| wake.wake_by_ref());
            failure.attempt(|| drop(wake));
        }));
    }
    pub fn retain(&self, future: std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>) {
        self.owner.transfer_existing(RetainedDriverFuture {
            future,
            receipt: self.clone(),
        });
    }
    #[cfg(feature = "extensions")]
    pub fn retain_receipt(self, future: std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>) {
        self.owner.clone().transfer_existing(RetainedDriverFuture {
            future,
            receipt: self,
        });
    }
}

#[cfg(feature = "sys")]
type RetirementClock = Rc<std::cell::Cell<u64>>;

#[cfg(feature = "sys")]
struct RetirementEvent {
    order: Option<u64>,
    clock: RetirementClock,
    payload: RetirementPanic,
}
#[cfg(feature = "sys")]
struct RetirementClockBinding {
    clock: RetirementClock,
    bound: bool,
    history: bool,
}
#[cfg(feature = "sys")]
#[derive(Clone)]
pub struct RetirementFailure(
    Rc<RefCell<Option<RetirementEvent>>>,
    #[cfg(feature = "extensions")] Rc<std::cell::Cell<bool>>,
    Rc<RefCell<RetirementClockBinding>>,
);
#[cfg(feature = "sys")]
impl Default for RetirementFailure {
    fn default() -> Self {
        Self(
            Rc::default(),
            #[cfg(feature = "extensions")]
            Rc::default(),
            Rc::new(RefCell::new(RetirementClockBinding {
                clock: Rc::default(),
                bound: false,
                history: false,
            })),
        )
    }
}
#[cfg(feature = "sys")]
impl RetirementFailure {
    fn binding(&self) -> &RefCell<RetirementClockBinding> {
        #[cfg(feature = "extensions")]
        {
            &self.2
        }
        #[cfg(not(feature = "extensions"))]
        {
            &self.1
        }
    }
    fn bind_clock(&self, clock: &RetirementClock) -> Result<(), RuntimeError> {
        let mut binding = self.binding().borrow_mut();
        if binding.bound {
            return if Rc::ptr_eq(&binding.clock, clock) {
                Ok(())
            } else {
                Err(RuntimeError::new(
                    "retirement carrier belongs to another Store",
                ))
            };
        }
        if binding.history {
            return Err(RuntimeError::new(
                "retirement carrier recorded before Store binding",
            ));
        }
        binding.clock = clock.clone();
        binding.bound = true;
        Ok(())
    }
    fn stamp(&self, payload: RetirementPanic) -> RetirementEvent {
        let clock = self.binding().borrow().clock.clone();
        let order = clock.get().checked_add(1);
        if let Some(order) = order {
            clock.set(order);
        }
        RetirementEvent {
            order,
            clock,
            payload,
        }
    }
    pub fn record(&self, payload: RetirementPanic) {
        let event = self.stamp(payload);
        let secondary = match self.publish_event(event) {
            Ok(secondary) => secondary,
            // A following raw event in THIS carrier cannot displace its already
            // owned unstamped first. Preserve it, but catch actual secondary Drop.
            Err(secondary) => Some(secondary),
        };
        self.dispose_secondary(secondary);
    }
    fn accept_event(&self, event: RetirementEvent) -> Result<(), RetirementEvent> {
        let secondary = self.publish_event(event)?;
        self.dispose_secondary(secondary);
        Ok(())
    }
    // Metadata-only commit. No user callback/payload Drop under these borrows.
    fn publish_event(
        &self,
        event: RetirementEvent,
    ) -> Result<Option<RetirementEvent>, RetirementEvent> {
        {
            let mut binding = self.binding().borrow_mut();
            assert!(
                Rc::ptr_eq(&binding.clock, &event.clock),
                "retirement event clock changed"
            );
            binding.history = true;
        }
        #[cfg(feature = "extensions")]
        self.1.set(false);
        let previous = self.0.borrow_mut().take();
        let (first, secondary) = match previous {
            Some(previous) if previous.order.is_none() || event.order.is_none() => {
                *self.0.borrow_mut() = Some(previous);
                return Err(event);
            }
            Some(previous) if previous.order <= event.order => (previous, Some(event)),
            Some(previous) => (event, Some(previous)),
            None => (event, None),
        };
        *self.0.borrow_mut() = Some(first);
        Ok(secondary)
    }
    fn dispose_secondary(&self, mut secondary: Option<RetirementEvent>) {
        while let Some(event) = secondary.take() {
            let unwinding = std::thread::panicking();
            let Err(panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(event)))
            else {
                return;
            };
            if unwinding {
                discard_retirement_panic(panic);
                return;
            }
            // Reentry may have CLAIMED the published first value. A newly
            // caught N then belongs to this SAME carrier and must reopen it
            // before another callback/destructor, never be silently discarded.
            let event = self.stamp(panic);
            secondary = match self.publish_event(event) {
                Ok(secondary) => secondary,
                Err(secondary) => Some(secondary),
            };
        }
    }
    fn has_unstamped_event(&self) -> bool {
        self.0
            .borrow()
            .as_ref()
            .is_some_and(|event| event.order.is_none())
    }
    pub fn record_caught(&self, panic: RetirementPanic, entry_unwinding: bool) {
        if entry_unwinding {
            discard_retirement_panic(panic);
        } else {
            self.record(panic);
        }
    }
    pub fn attempt<R>(&self, action: impl FnOnce() -> R) -> Option<R> {
        let unwinding = std::thread::panicking();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(action)) {
            Ok(value) => Some(value),
            Err(panic) => {
                self.record_caught(panic, unwinding);
                None
            }
        }
    }
    fn take_event(&self) -> Option<RetirementEvent> {
        let mut event = self.0.borrow_mut();
        if event.as_ref().is_some_and(|event| event.order.is_none()) {
            return None;
        }
        event.take()
    }
    fn take(&self) -> Option<RetirementPanic> {
        self.take_event().map(|event| event.payload)
    }
    // Exceptional delivery at the LAST owned, physically complete Rust
    // boundary only. This neither orders nor acknowledges an unstamped event;
    // ordinary callers must continue using take_event/move_into and stay Pending.
    fn take_terminal_payload(&self) -> Option<RetirementPanic> {
        self.0.borrow_mut().take().map(|event| event.payload)
    }
    // Used by sys-only physical finalization as well as WASI delivery.
    pub fn is_empty(&self) -> bool {
        self.0.borrow().is_none()
    }
    #[cfg(all(any(test, feature = "test-support"), feature = "extensions"))]
    pub fn inspect_message(&self) -> Option<String> {
        self.0.borrow().as_ref().and_then(|event| {
            event
                .payload
                .downcast_ref::<&str>()
                .map(|message| (*message).to_owned())
                .or_else(|| event.payload.downcast_ref::<String>().cloned())
        })
    }
}

#[cfg(feature = "sys")]
pub struct RetirementLedger {
    // The SAME weak enrollment collection formerly stored in Driver.
    enrolled: RefCell<Vec<Weak<NativeRetirementState>>>,
    clock: RetirementClock,
    final_done: std::cell::Cell<bool>,
    finalization_started: std::cell::Cell<bool>,
    service_claims: std::cell::Cell<usize>,
}
#[cfg(all(
    any(test, feature = "test-support"),
    feature = "sys",
    feature = "extensions"
))]
impl RetirementLedger {
    pub fn is_empty_for_test(&self) -> bool {
        self.snapshot().is_empty()
    }
    pub fn final_done_for_test(&self) -> bool {
        self.final_done.get()
    }
    pub fn service_claims_for_test(&self) -> usize {
        self.service_claims.get()
    }
}
#[cfg(feature = "sys")]
impl RetirementLedger {
    fn new() -> Rc<Self> {
        Rc::new(Self {
            enrolled: RefCell::default(),
            clock: Rc::default(),
            final_done: std::cell::Cell::new(false),
            finalization_started: std::cell::Cell::new(false),
            service_claims: std::cell::Cell::new(0),
        })
    }
    fn snapshot(&self) -> Vec<Rc<NativeRetirementState>> {
        self.enrolled
            .borrow()
            .clone()
            .into_iter()
            .map(|entry| entry.upgrade().expect("native retirement custody vanished"))
            .collect()
    }
    #[cfg(feature = "extensions")]
    fn enroll(&self, state: &Rc<NativeRetirementState>) {
        let identity = Rc::downgrade(state);
        let mut entries = self.enrolled.borrow_mut();
        if !entries.iter().any(|entry| entry.ptr_eq(&identity)) {
            entries.push(identity);
        }
    }
    #[cfg(feature = "extensions")]
    fn remove(&self, state: &Rc<NativeRetirementState>) {
        self.enrolled
            .borrow_mut()
            .retain(|entry| !entry.ptr_eq(&Rc::downgrade(state)));
    }
}

// ERROR-claim custody only: never an entered-depth/Rc/resource fence.
#[cfg(feature = "sys")]
struct ServiceClaimLease(Rc<RetirementLedger>);
#[cfg(feature = "sys")]
impl ServiceClaimLease {
    fn new(ledger: Rc<RetirementLedger>) -> Self {
        ledger.service_claims.set(ledger.service_claims.get() + 1);
        Self(ledger)
    }
}
#[cfg(feature = "sys")]
impl Drop for ServiceClaimLease {
    fn drop(&mut self) {
        self.0.service_claims.set(self.0.service_claims.get() - 1);
    }
}

#[cfg(feature = "sys")]
#[derive(Clone, Copy, PartialEq, Eq)]
#[cfg_attr(
    not(feature = "extensions"),
    allow(
        dead_code,
        reason = "Shared sys drain predicates read this metadata, but only WASI enrollment constructs invocation dispositions"
    )
)]
enum DeliveryDisposition {
    LiveTail,
    AdmittedContinuation,
    Cancelled,
    Relinquished,
    Delivered,
}

// Installed inside registration, before any callback can observe membership.
#[cfg(all(feature = "sys", feature = "extensions"))]
pub struct RetirementRegistration {
    state: Rc<NativeRetirementState>,
    armed: bool,
}
#[cfg(all(feature = "sys", feature = "extensions"))]
impl RetirementRegistration {
    pub fn state(&self) -> Rc<NativeRetirementState> {
        self.state.clone()
    }
    pub fn commit(&self) {
        self.state.committed.set(true);
    }
    pub fn relinquish(mut self) {
        self.state
            .disposition
            .set(DeliveryDisposition::Relinquished);
        self.armed = false;
        self.state.release_empty_service_claim();
    }
    fn delivered(mut self) {
        self.state
            .disposition
            .set(if self.state.ledger.final_done.get() {
                DeliveryDisposition::Delivered
            } else {
                // Publication can still record a following event after a live
                // public Store call returns; its common boundary owns that event.
                DeliveryDisposition::Relinquished
            });
        self.armed = false;
        self.state.release_empty_service_claim();
    }
}
#[cfg(all(feature = "sys", feature = "extensions"))]
impl Drop for RetirementRegistration {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if self.state.committed.get() {
            self.state.disposition.set(DeliveryDisposition::Cancelled);
            // Successful terminal publication can retire an empty receipt
            // before its result consumer is polled. This guard may then own
            // the LAST strong state: re-enrolling just its Weak would leave a
            // dead ledger row when the guard's fields finish dropping.
            if self.state.physically_ready()
                && !self.state.publishing.get()
                && !self.state.receipt_queued.get()
                && self.state.delivery_ack.get()
                && self.state.delivery_empty()
            {
                self.state.ledger.remove(&self.state);
                self.state.release_service_claim();
            } else {
                // A queued receipt, active publisher or unfinished physical
                // owner still retains the SAME exact state until settlement.
                self.state.ledger.enroll(&self.state);
            }
        } else {
            self.state.ledger.remove(&self.state);
            self.state.release_service_claim();
        }
    }
}

#[cfg(feature = "sys")]
pub struct NativeRetirementState {
    #[cfg(feature = "extensions")]
    ledger: Rc<RetirementLedger>,
    disposition: std::cell::Cell<DeliveryDisposition>,
    #[cfg(feature = "extensions")]
    committed: std::cell::Cell<bool>,
    terminal_done: Rc<std::cell::Cell<bool>>,
    tail_receipt_released: std::cell::Cell<bool>,
    delivery_ack: Rc<std::cell::Cell<bool>>,
    #[cfg(feature = "extensions")]
    publishing: std::cell::Cell<bool>,
    #[cfg(feature = "extensions")]
    receipt_queued: std::cell::Cell<bool>,
    pub failure: RetirementFailure,
    // At most ONE return from this state's unique live tail; the SAME receipt
    // retains both slots if either event cannot be ordered after exhaustion.
    returned: RefCell<Option<RetirementEvent>>,
    #[cfg(feature = "extensions")]
    release_waiter: RefCell<Option<std::sync::Arc<std::task::Waker>>>,
    #[cfg(feature = "extensions")]
    service_claim: RefCell<Option<ServiceClaimLease>>, // LAST, through event transfer/cancel
}
#[cfg(all(
    any(test, feature = "test-support"),
    feature = "sys",
    feature = "extensions"
))]
impl NativeRetirementState {
    /// Whether the tail finished, released its receipt and had its delivery acknowledged.
    pub fn is_settled_for_test(&self) -> bool {
        self.terminal_done.get() && self.tail_receipt_released.get() && self.delivery_ack.get()
    }
}

#[cfg(feature = "sys")]
impl NativeRetirementState {
    fn release_service_claim(&self) {
        #[cfg(feature = "extensions")]
        {
            let lease = self.service_claim.borrow_mut().take();
            drop(lease);
        }
    }
    #[cfg(feature = "extensions")]
    fn release_empty_service_claim(&self) {
        if self.physically_ready()
            && !self.publishing.get()
            && !self.receipt_queued.get()
            && self.delivery_ack.get()
            && self.delivery_empty()
        {
            self.release_service_claim();
        }
    }
    fn physically_ready(&self) -> bool {
        self.terminal_done.get() && self.tail_receipt_released.get()
    }
    fn delivery_empty(&self) -> bool {
        self.failure.is_empty() && self.returned.borrow().is_none()
    }
    fn has_unstamped_event(&self) -> bool {
        self.failure.has_unstamped_event()
            || self
                .returned
                .borrow()
                .as_ref()
                .is_some_and(|event| event.order.is_none())
    }
    fn move_delivery_into(&self, destination: &RetirementFailure) -> bool {
        loop {
            if self.has_unstamped_event() || destination.has_unstamped_event() {
                return false;
            }
            if !self.failure.move_into(destination) {
                return false;
            }
            let returned = self.returned.borrow_mut().take();
            if let Some(event) = returned
                && let Err(event) = destination.accept_event(event)
            {
                let previous = self.returned.borrow_mut().replace(event);
                assert!(previous.is_none(), "one return per unique native tail");
                return false;
            }
            if self.delivery_empty() {
                return true;
            }
        }
    }
    #[cfg(all(test, feature = "extensions"))]
    fn claim(&self, first: &mut Option<RetirementPanic>) -> bool {
        let owned = RetirementFailure::default();
        owned
            .bind_clock(&self.ledger.clock)
            .expect("fresh claim carrier");
        if !self.move_delivery_into(&owned) {
            return false;
        }
        if let Some(panic) = owned.take() {
            remember_retirement_panic(first, panic);
        }
        self.delivery_ack.set(true);
        if !self.publishing.get() {
            self.release_service_claim();
        }
        true
    }
    #[cfg(feature = "extensions")]
    fn remove_enrollment(self: &Rc<Self>) {
        self.ledger.remove(self);
    }
    #[cfg(feature = "extensions")]
    fn register_release_waker(&self, waker: &std::task::Waker) -> bool {
        let next = self.failure.attempt(|| std::sync::Arc::new(waker.clone()));
        if let Some(next) = next {
            let old = self.release_waiter.borrow_mut().replace(next);
            self.failure.attempt(|| drop(old));
            true
        } else {
            false
        }
    }
    #[cfg(feature = "extensions")]
    fn clear_release_waker(&self) {
        let old = self.release_waiter.borrow_mut().take();
        self.failure.attempt(|| drop(old));
    }
}

// This receipt holds only exact metadata until an explicit consumer acknowledges
// the owned value. It has no Driver/Store/root and is not a poll producer.
#[cfg(all(feature = "sys", feature = "extensions"))]
struct DeliveryReceipt {
    state: Rc<NativeRetirementState>,
    ledger: Rc<RetirementLedger>,
}
#[cfg(all(feature = "sys", feature = "extensions"))]
impl std::future::Future for DeliveryReceipt {
    type Output = ();
    fn poll(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        if self.state.has_unstamped_event() {
            return std::task::Poll::Pending;
        }
        if !self.state.physically_ready() {
            return std::task::Poll::Pending;
        }
        if self.state.delivery_empty() {
            self.state.delivery_ack.set(true);
        }
        if !self.state.delivery_ack.get() {
            return std::task::Poll::Pending;
        }
        self.state.receipt_queued.set(false);
        // The task may retire here, but the publisher retains exact enrollment
        // and the carrier until ALL fallible notification has finished.
        if !self.state.publishing.get() {
            self.ledger.remove(&self.state);
        }
        std::task::Poll::Ready(())
    }
}

#[cfg(all(feature = "sys", feature = "extensions"))]
pub struct TailRelease {
    state: Rc<NativeRetirementState>,
    ledger: Rc<RetirementLedger>,
    owner: wasmer_api::sys::NativeCleanupOwner,
    wake: std::sync::Arc<DriverWake>,
}
#[cfg(all(feature = "sys", feature = "extensions"))]
impl TailRelease {
    pub fn release(self) {
        self.owner.clone().after_pump_existing(Box::new(move || {
            assert!(
                self.state.terminal_done.get(),
                "unfinished native retirement"
            );
            self.state.publishing.set(true);
            // Publish actual readiness BEFORE callbacks may reenter and poll
            // the live tail. They must never observe a false bit and strand a
            // replacement waiter with no later wake.
            self.state.tail_receipt_released.set(true);
            let waiter = self.state.release_waiter.borrow_mut().take();
            if let Some(waiter) = waiter {
                self.state.failure.attempt(|| waiter.wake_by_ref());
                self.state.failure.attempt(|| drop(waiter));
            }
            let wake = std::task::Waker::from(self.wake);
            self.state.failure.attempt(|| wake.wake_by_ref());
            self.state.failure.attempt(|| drop(wake));
            // This can retire an empty receipt and THEN panic in the idle
            // waker. Publication still owns carrier+enrollment throughout it.
            self.state.failure.attempt(|| self.owner.notify());
            // retain() synchronously notifies too. Keep publication leased
            // across EVERY such admission: a reentrant consumer can ack its
            // value and retire that task before a later notify callback fails.
            // Recheck the exact state after each finite synchronous admission.
            while !self.state.receipt_queued.get() {
                if self.state.delivery_empty() && self.state.delivery_ack.get() {
                    break;
                }
                self.state.receipt_queued.set(true);
                self.state.failure.attempt(|| {
                    self.owner.transfer_existing(Box::pin(DeliveryReceipt {
                        state: self.state.clone(),
                        ledger: self.ledger.clone(),
                    }))
                });
            }
            // No fallible publication remains after releasing this lease.
            // A queued receipt owns any unacknowledged late value; otherwise
            // exact empty/acknowledged enrollment can now be removed.
            self.state.publishing.set(false);
            if !self.state.receipt_queued.get() {
                self.state.remove_enrollment();
            }
        }));
    }
}

#[cfg(all(feature = "sys", feature = "extensions"))]
struct NativeRetirementTail {
    subscription: Option<DriverSubscription>,
    _store: WeakStoreRef,
    base: Option<wasmer_api::StoreAsync>,
    owner: Option<CleanupRetirement>,
    cleanup: Option<wasmer_api::sys::NativeCleanupOwner>,
    release: Option<TailRelease>,
    state: Rc<NativeRetirementState>,
    registration: Option<RetirementRegistration>,
    first: RetirementFailure, // invocation-local; NEVER the shared cleanup carrier
    finalization_subscription: Option<DriverSubscription>,
    finalization: Rc<NativeStoreFinalizationHandoff>,
    exit: Option<wasmer_api::sys::NativeCleanupExitBoundary>, // LAST incl implicit fields
}
#[cfg(all(feature = "sys", feature = "extensions"))]
impl NativeRetirementTail {
    fn release_receipts(&mut self) {
        self.state
            .failure
            .attempt(|| drop(self.subscription.take()));
        self.state.failure.attempt(|| drop(self.base.take()));
        self.state.failure.attempt(|| drop(self.owner.take()));
        if let Some(release) = self.release.take() {
            self.state.failure.attempt(|| release.release());
        }
    }
    fn subscribe_finalization(&mut self, cx: &mut std::task::Context<'_>) {
        let subscription = self.finalization_subscription.get_or_insert_with(|| {
            DriverSubscription::new(
                self.finalization.wake.clone(),
                self.finalization.owner.clone(),
            )
        });
        // Preterminal observer errors belong to THIS invocation even if the
        // public target is still alive and no Store finalizer is published.
        self.state
            .failure
            .attempt(|| subscription.with_context(cx, |_| ()));
    }
    fn claim_own(&self) -> bool {
        if !self.state.move_delivery_into(&self.first) {
            return false;
        }
        self.state.delivery_ack.set(true); // exact value owned by THIS tail first
        true
    }
    fn dispose_terminal_cancel(&mut self) {
        assert!(self.finalization.ledger.final_done.get() && self.state.physically_ready());
        let state = self.state.clone();
        let owned = [
            self.first.0.borrow_mut().take(),
            state.returned.borrow_mut().take(),
            state.failure.0.borrow_mut().take(),
        ];
        let mut secondary = Vec::new(); // bounded by the THREE existing slots
        let mut unordered = false;
        for event in owned.into_iter().flatten() {
            if self.first.is_empty() {
                unordered |= event.order.is_none();
                *self.first.0.borrow_mut() = Some(event);
            } else if self.first.has_unstamped_event() || event.order.is_none() {
                // Exceptional OWNED Rust Drop, not an invented numeric order.
                unordered = true;
                secondary.push(event.payload);
            } else if let Some(event) = self
                .first
                .publish_event(event)
                .unwrap_or_else(|_| unreachable!("ordered terminal own event"))
            {
                secondary.push(event.payload);
            }
        }
        if self.finalization.scopes.canonical_allowed(None)
            && !self.first.has_unstamped_event()
            && !self.finalization.failure.has_unstamped_event()
        {
            if let Some(event) = self.finalization.failure.take_event()
                && let Some(event) = self
                    .first
                    .publish_event(event)
                    .unwrap_or_else(|_| unreachable!("same-clock terminal cleanup event"))
            {
                secondary.push(event.payload);
            }
        } else {
            unordered = true; // canonical unstamped stays SAME receiver-owned
        }
        let first = self.first.take_terminal_payload();
        let claim = state.service_claim.borrow_mut().take();
        state.delivery_ack.set(!unordered);
        state.ledger.remove(&state);
        if let Some(registration) = self.registration.take() {
            registration.delivered();
        }
        self.exit.as_ref().unwrap().defer(Box::new(move || {
            // Claim lease survives all finite callback-bearing secondary disposal.
            let _claim = claim;
            for exact in secondary {
                discard_retirement_panic(exact);
            }
            loop {
                let event = state.failure.0.borrow_mut().take();
                let Some(event) = event else {
                    break;
                };
                discard_retirement_panic(event.payload);
                state.delivery_ack.set(false);
            }
            first // existing owned Rust transport; NEVER restamp as a raw error
        }));
    }
    fn return_cancelled(&mut self) {
        let state = self.state.clone();
        let owner = self.finalization.owner.clone();
        if self.finalization.ledger.final_done.get() {
            self.dispose_terminal_cancel();
            return;
        }
        // There is no owner-thread callback/disposal between valid seal and
        // final_done; runtime panic payload disposal is deferred until Done.
        assert!(
            !owner.is_sealed(),
            "cancellation entered the sealed runtime transition"
        );
        drop(self.registration.take()); // Cancelled BEFORE every callback below
        let publishing = state.publishing.replace(true);
        loop {
            let need_receipt =
                !state.delivery_empty() || !state.delivery_ack.get() || !state.physically_ready();
            if need_receipt && !state.receipt_queued.get() {
                state.ledger.enroll(&state); // this frame already owns exact strong custody
                state.receipt_queued.set(true);
                let receipt = DeliveryReceipt {
                    state: state.clone(),
                    ledger: state.ledger.clone(),
                };
                state.failure.attempt(|| owner.transfer_existing(receipt));
            }
            // Real surviving original caller(s) are the producer after Driver
            // extraction; notification still uses the SAME existing fanout.
            let wake = std::task::Waker::from(self.finalization.wake.clone());
            state.failure.attempt(|| wake.wake_by_ref());
            state.failure.attempt(|| drop(wake));
            if state.receipt_queued.get() || (state.delivery_empty() && state.delivery_ack.get()) {
                break;
            }
        }
        state.publishing.set(publishing);
        if !state.receipt_queued.get() && state.delivery_empty() && state.delivery_ack.get() {
            state.ledger.remove(&state);
        }
        // This last wake follows our OWN publication lease. A wake failure can
        // reopen exact delivery, so repair that same receipt before field Drop.
        if !publishing {
            let wake = std::task::Waker::from(self.finalization.wake.clone());
            state.failure.attempt(|| wake.wake_by_ref());
            state.failure.attempt(|| drop(wake));
            if self.finalization.ledger.final_done.get() {
                self.dispose_terminal_cancel();
            } else if !state.delivery_empty() && !state.receipt_queued.get() {
                state.ledger.enroll(&state);
                state.receipt_queued.set(true);
                state.failure.attempt(|| {
                    owner.transfer_existing(DeliveryReceipt {
                        state: state.clone(),
                        ledger: state.ledger.clone(),
                    })
                });
            }
        }
    }
}
#[cfg(all(feature = "sys", feature = "extensions"))]
impl std::future::Future for NativeRetirementTail {
    type Output = ();
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        let this = self.get_mut();
        this.subscribe_finalization(cx);
        if let Some(owner) = &this.owner {
            let driver = DriverUse::capture(owner);
            if this
                .state
                .failure
                .attempt(|| {
                    this.subscription
                        .as_ref()
                        .unwrap()
                        .with_context(cx, |cx| driver.poll_retirement(cx))
                })
                .is_none()
            {
                this.state.failure.attempt(|| cx.waker().wake_by_ref());
            }
        }
        if !this.state.terminal_done.get() {
            return std::task::Poll::Pending;
        }
        if !this.state.tail_receipt_released.get() {
            if !this.state.register_release_waker(cx.waker()) {
                this.state.failure.attempt(|| cx.waker().wake_by_ref());
            }
            this.release_receipts();
        }
        // Actual publication/pump callbacks must finish before resource teardown.
        if !this.state.physically_ready() || this.state.publishing.get() {
            return std::task::Poll::Pending;
        }
        this.state.clear_release_waker();
        if !this.claim_own() {
            return std::task::Poll::Pending;
        }
        this.state
            .failure
            .attempt(|| this.finalization.owner.notify());
        if !this.claim_own() {
            return std::task::Poll::Pending;
        }
        let poll = this
            .finalization_subscription
            .as_ref()
            .unwrap()
            .with_context(cx, |cx| this.finalization.poll(cx));
        if poll.is_pending() {
            return poll;
        }
        // A caller Waker Drop may publish the last Store finalizer. Consume the
        // actual metadata first, then recheck publication and keep a real wake.
        this.state
            .failure
            .attempt(|| drop(this.finalization_subscription.take()));
        this.state.failure.attempt(|| drop(this.cleanup.take()));
        if !this.claim_own() {
            return std::task::Poll::Pending;
        }
        if !this.finalization.complete.get()
            && (this.finalization.pending.borrow().is_some() || this.finalization.polling.get())
        {
            this.subscribe_finalization(cx);
            this.state.failure.attempt(|| cx.waker().wake_by_ref());
            return std::task::Poll::Pending;
        }
        // Only here does this invocation legitimately own the Store-cleanup
        // outcome. Other live invocation carriers are never stolen into it.
        if this.finalization.scopes.canonical_allowed(None)
            && !this.finalization.failure.move_into(&this.first)
        {
            return std::task::Poll::Pending;
        }
        // Different common A owns those events. B finishes its OWN outcome;
        // ownership alone never fabricates Pending or adopts A's capability.
        let first = this.first.take(); // exact ordered ownership BEFORE release
        this.state.release_service_claim();
        if let Some(registration) = this.registration.take() {
            registration.delivered();
        }
        if let Some(first) = first {
            std::panic::resume_unwind(first);
        }
        std::task::Poll::Ready(())
    }
}
#[cfg(all(feature = "sys", feature = "extensions"))]
impl Drop for NativeRetirementTail {
    fn drop(&mut self) {
        self.exit = Some(self.finalization.owner.enter_exit_boundary());
        self.state.disposition.set(DeliveryDisposition::Cancelled);
        let own = self.first.0.borrow_mut().take();
        if let Some(own) = own {
            let previous = self.state.returned.borrow_mut().replace(own);
            assert!(previous.is_none(), "one return per unique native tail");
            self.state.delivery_ack.set(false);
        }
        self.state.clear_release_waker();
        self.state
            .failure
            .attempt(|| drop(self.subscription.take()));
        self.state
            .failure
            .attempt(|| drop(self.finalization_subscription.take()));
        if self.state.terminal_done.get() {
            self.release_receipts();
        } else if let Some(owner) = self.owner.take() {
            let state = self.state.clone();
            let mut base = self.base.take();
            let mut release = self.release.take();
            owner.retain_receipt(Box::pin(std::future::poll_fn(move |_| {
                if !state.terminal_done.get() {
                    return std::task::Poll::Pending;
                }
                state.failure.attempt(|| drop(base.take()));
                if let Some(release) = release.take() {
                    state.failure.attempt(|| release.release());
                }
                std::task::Poll::Ready(())
            })));
        }
        self.state.failure.attempt(|| drop(self.cleanup.take()));
        self.return_cancelled();
    }
}

#[cfg(feature = "sys")]
type NativeCallFuture<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Box<[Value]>, RuntimeError>> + 'a>>;

#[cfg(feature = "sys")]
struct CallOwner {
    driver: Rc<Driver>,
    #[cfg(feature = "extensions")]
    entry: RefCell<Option<Rc<CallEntry>>>,
    #[cfg(feature = "extensions")]
    pending: RefCell<std::collections::VecDeque<LocalContinuation>>,
}

#[cfg(all(feature = "sys", feature = "extensions"))]
pub struct CallEntry {
    scope: Box<dyn EntryScope>,
    cleanup: wasmer_api::sys::NativeCleanupOwner,
    failure: Rc<RetirementFailure>,
    observers: RefCell<Vec<Rc<CallEntryObserver>>>,
    done: std::cell::Cell<bool>,
}

#[cfg(all(feature = "sys", feature = "extensions"))]
struct CallEntryObserver {
    observer: RefCell<Option<wasmer_api::sys::NativeAncestrySettlement>>,
    done: std::cell::Cell<bool>,
}

#[cfg(all(feature = "sys", feature = "extensions"))]
impl CallEntryObserver {
    fn poll(&self, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        use std::future::Future;
        if self.done.get() {
            return std::task::Poll::Ready(());
        }
        // RawWaker callbacks run outside this metadata borrow.
        let Some(mut observer) = self.observer.borrow_mut().take() else {
            return std::task::Poll::Pending;
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            std::pin::Pin::new(&mut observer).poll(cx)
        }));
        let result = match result {
            Ok(result) => result,
            Err(exact) => {
                // Keep the SAME physical certificate on notification panic.
                *self.observer.borrow_mut() = Some(observer);
                std::panic::resume_unwind(exact);
            }
        };
        if result.is_ready() {
            self.done.set(true);
            drop(observer);
        } else {
            *self.observer.borrow_mut() = Some(observer);
        }
        result
    }
}

/// The entry of the native call whose local owner is `owner`.
#[cfg(all(
    any(test, feature = "test-support"),
    feature = "sys",
    feature = "extensions"
))]
pub fn call_entry_for_test(owner: Rc<dyn std::any::Any>) -> Option<Rc<CallEntry>> {
    let owner = owner.downcast::<CallOwner>().ok()?;
    owner.entry.borrow().clone()
}

#[cfg(all(
    any(test, feature = "test-support"),
    feature = "sys",
    feature = "extensions"
))]
impl CallEntry {
    pub fn is_done_for_test(&self) -> bool {
        self.done.get()
    }
}

#[cfg(all(feature = "sys", feature = "extensions"))]
impl CallEntry {
    fn observe_ready(&self, cx: &mut std::task::Context<'_>) {
        let observers = self.observers.borrow().clone();
        for observer in observers {
            if observer.poll(cx).is_ready() {
                self.observers
                    .borrow_mut()
                    .retain(|old| !Rc::ptr_eq(old, &observer));
            }
            drop(observer);
        }
    }

    async fn finish(&self) {
        loop {
            let observers = self.observers.borrow().clone();
            if observers.is_empty() {
                break;
            }
            for observer in observers {
                std::future::poll_fn(|cx| observer.poll(cx)).await;
                self.observers
                    .borrow_mut()
                    .retain(|old| !Rc::ptr_eq(old, &observer));
                drop(observer);
            }
        }
        self.scope
            .finish(&mut |exact| {
                self.failure.attempt(exact);
            })
            .await;
        self.done.set(true);
    }
}

#[cfg(all(feature = "sys", feature = "extensions"))]
struct ObservedEntryCall {
    // The actual future is destroyed before any observer can certify it.
    future: Option<EntryFuture>,
    result: Option<Result<Box<[Value]>, RuntimeError>>,
    entry: Rc<CallEntry>,
}

#[cfg(all(feature = "sys", feature = "extensions"))]
impl std::future::Future for ObservedEntryCall {
    type Output = Result<Box<[Value]>, RuntimeError>;
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        let this = self.get_mut();
        this.entry.observe_ready(cx);
        if let Some(future) = &mut this.future {
            match future.as_mut().poll(cx) {
                std::task::Poll::Pending => return std::task::Poll::Pending,
                std::task::Poll::Ready(result) => this.result = Some(result),
            }
            drop(this.future.take());
        }
        // A borrowed helper can require its parent's next base unlock epoch.
        // Return the engine outcome to that parent; only final entry reset may
        // await ALL certificates outside the actual guest/helper frames.
        this.entry.observe_ready(cx);
        std::task::Poll::Ready(this.result.take().expect("one actual guest result"))
    }
}

/// Checks that an [`EntryScope`] import still belongs to the live call that opened it.
#[cfg(all(feature = "sys", feature = "extensions"))]
pub fn authorize_initialized_entry(
    captured: Option<Weak<dyn std::any::Any>>,
    token: &dyn std::any::Any,
    row: usize,
) -> Result<(), RuntimeError> {
    let owner = captured
        .and_then(|weak| weak.upgrade())
        .and_then(|owner| owner.downcast::<CallOwner>().ok())
        .ok_or_else(|| RuntimeError::new("initialized entry import has no live matching owner"))?;
    let entry = owner
        .entry
        .borrow()
        .clone()
        .ok_or_else(|| RuntimeError::new("initialized entry owner has no active scope"))?;
    // Both RefCell borrows end before either local Rc can run a destructor.
    let active = !entry.done.get() && entry.scope.contains(token, row);
    drop(entry);
    drop(owner);
    if active {
        Ok(())
    } else {
        Err(RuntimeError::new(
            "initialized entry import has no matching active scope",
        ))
    }
}

/// Calls `function` for an [`EntryScope`] import. The scope stores this behind a
/// plain fn pointer: it never captures a local owner, and never falls back when
/// that scope's owner is stale.
#[cfg(all(feature = "sys", feature = "extensions"))]
pub fn invoke_initialized_entry(
    function: Function,
    store: wasmer_api::StoreAsync,
    args: Vec<Value>,
    captured: Option<Weak<dyn std::any::Any>>,
    token: Box<dyn std::any::Any>,
    row: usize,
) -> EntryFuture {
    Box::pin(async move {
        let weak = captured
            .ok_or_else(|| RuntimeError::new("initialized entry import has no captured owner"))?;
        let owner = weak
            .upgrade()
            .and_then(|owner| owner.downcast::<CallOwner>().ok())
            .ok_or_else(|| RuntimeError::new("initialized entry import owner is no longer live"))?;
        let entry = owner
            .entry
            .borrow()
            .clone()
            .ok_or_else(|| RuntimeError::new("initialized entry owner has no active scope"))?;
        // Retain no strong CallOwner across the actual guest poll.
        drop(owner);
        if !entry.scope.accepts(&*token, row, &store).await {
            return Err(RuntimeError::new(
                "initialized entry import belongs to a replaced scope or ABI",
            ));
        }
        let (future, observer) = wasmer_api::sys::call_async_with_local_owner_observed(
            &function,
            &store,
            args,
            weak,
            Some(&entry.cleanup),
        )?;
        let observer = Rc::new(CallEntryObserver {
            observer: RefCell::new(Some(observer)),
            done: Default::default(),
        });
        // Commit cancellation custody BEFORE the first poll of this exact future.
        entry.observers.borrow_mut().push(observer.clone());
        ObservedEntryCall {
            future: Some(future),
            result: None,
            entry,
        }
        .await
    })
}

#[cfg(all(feature = "sys", feature = "extensions"))]
struct CallEntryRetirement {
    entry: Option<Rc<CallEntry>>,
    running: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>>,
}

#[cfg(all(feature = "sys", feature = "extensions"))]
impl std::future::Future for CallEntryRetirement {
    type Output = ();
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        let this = self.get_mut();
        if this.running.is_none() {
            let entry = this
                .entry
                .as_ref()
                .expect("unfinished entry custody")
                .clone();
            this.running = Some(Box::pin(async move {
                entry.finish().await;
            }));
        }
        if this
            .running
            .as_mut()
            .unwrap()
            .as_mut()
            .poll(cx)
            .is_pending()
        {
            return std::task::Poll::Pending;
        }
        drop(this.running.take());
        drop(this.entry.take()); // actual base capability released before Ready
        std::task::Poll::Ready(())
    }
}

#[cfg(all(feature = "sys", feature = "extensions"))]
impl Drop for CallEntryRetirement {
    fn drop(&mut self) {
        let Some(entry) = self.entry.take() else {
            return;
        };
        let failure = entry.failure.clone();
        failure.attempt(|| drop(self.running.take()));
        if !entry.done.get() {
            let cleanup = entry.cleanup.clone();
            failure.attempt(|| {
                cleanup.transfer_existing(Self {
                    entry: Some(entry),
                    running: None,
                })
            });
        }
    }
}

#[cfg(feature = "sys")]
impl Drop for CallOwner {
    fn drop(&mut self) {
        #[cfg(feature = "extensions")]
        if let Some(entry) = self.entry.get_mut().take()
            && !entry.done.get()
        {
            let cleanup = entry.cleanup.clone();
            // Same already-entered generation/physical receipt obligation.
            // No StoreRef, Driver or CallOwner is captured by this queue job.
            let failure = entry.failure.clone();
            failure.attempt(|| {
                cleanup.transfer_existing(CallEntryRetirement {
                    entry: Some(entry),
                    running: None,
                })
            });
        }
    }
}

#[cfg(feature = "sys")]
impl CallOwner {
    async fn drain(&self) -> Option<RuntimeError> {
        #[cfg(feature = "extensions")]
        {
            let mut outcome = None;
            loop {
                let next = self.pending.borrow_mut().pop_front();
                let Some(mut next) = next else {
                    return outcome;
                };
                let driver = match &next.store.inner.mode {
                    StoreMode::Native(native) => match native.driver() {
                        Ok(driver) => driver,
                        Err(error) => {
                            outcome = Some(error);
                            continue;
                        }
                    },
                    StoreMode::Plain(_) => {
                        outcome = Some(RuntimeError::new("native continuation lost its Store"));
                        continue;
                    }
                };
                let future = next.run.take().expect("queued continuation has one entry")();
                let result = if Rc::ptr_eq(&self.driver, &driver) {
                    future.await
                } else {
                    driver.run_local(future).await
                };
                if let Err(error) = result {
                    outcome = Some(error);
                }
                // A final Store retirement must see no escaped Driver clone.
                drop(driver);
                drop(next);
            }
        }
        #[cfg(not(feature = "extensions"))]
        {
            let _ = &self.driver;
            None
        }
    }
}

#[cfg(feature = "sys")]
fn store_cleanup_failures(
    failure: &Rc<RetirementFailure>,
    ledger: &Rc<RetirementLedger>,
    scopes: &Rc<DriverScopes>,
) -> wasmer_api::sys::NativeCleanupFailures {
    use wasmer_api::sys::NativeCleanupFailureDisposition as Disposition;
    let record = failure.clone();
    let claim = failure.clone();
    let claim_ledger = ledger.clone();
    let claim_scopes = scopes.clone();
    let terminal = failure.clone();
    let terminal_ledger = ledger.clone();
    wasmer_api::sys::NativeCleanupFailures::new(
        move |exact| record.record(exact),
        move |owner| {
            if claim.is_empty() {
                return Disposition::Empty;
            }
            if claim_ledger.service_claims.get() != 0 {
                return Disposition::Deferred;
            }
            let ready = if claim_ledger.finalization_started.get() {
                claim_ledger.final_done.get()
            } else {
                owner.is_some_and(|owner| owner.is_quiescent())
                    && claim_scopes.entered.get() == 0
                    && claim_scopes.users.get() == 0
                    && claim_ledger.resources_ready()
            };
            if !ready || claim.has_unstamped_event() {
                return Disposition::Deferred;
            }
            match claim.take() {
                Some(exact) => Disposition::Deliver(exact),
                None => Disposition::Empty,
            }
        },
        move || {
            if terminal.is_empty() {
                return None;
            }
            // Before Done the actual ManagedStore/unfinished phase owns another
            // strong receiver. Constructor rollback is empty before publication.
            assert!(
                terminal_ledger.final_done.get() && terminal_ledger.service_claims.get() == 0,
                "unfinished ordering owner lost its actual failure custodian"
            );
            terminal.take_terminal_payload()
        },
    )
}

impl StoreRef {
    #[allow(unused_mut)] // Only the native backend binds a generation before conversion.
    fn new(mut store: Store) -> Result<Self, RuntimeError> {
        let id = store.id();
        #[cfg(feature = "sys")]
        if store.engine().is_sys() {
            let ledger = RetirementLedger::new();
            let scopes = Rc::new(DriverScopes::default());
            let cleanup_failure = Rc::new(RetirementFailure::default());
            cleanup_failure.bind_clock(&ledger.clock)?;
            let failures_receiver = store_cleanup_failures(&cleanup_failure, &ledger, &scopes);
            let owner = wasmer_api::sys::NativeCleanupOwner::new_store_bound_with_failures(
                &failures_receiver,
            );
            let seal = owner.take_store_seal()?;
            let generation = wasmer_api::sys::NativeInvocationScope::bind_with_cleanup(
                &mut store,
                Some(owner.clone()),
            )?;
            let reservation = generation.store_destruction().reserve_cleanup()?;
            let store = store.into_async();
            // Actual unlock must repump a retirement fence even for a bare Store
            // that never constructed a WASI environment or cleanup driver.
            let owner = wasmer_api::sys::native_cleanup_owner(&store)
                .ok_or_else(|| RuntimeError::new("native cleanup owner is missing"))?;
            return Ok(Self {
                inner: Rc::new(StoreState {
                    id,
                    mode: StoreMode::Native(ManagedStore {
                        store: Some(store),
                        generation: Some(generation),
                        owner,
                        driver: RefCell::new(None),
                        ledger,
                        scopes,
                        cleanup_failure,
                        failures_receiver,
                        seal: Some(seal),
                        reservation: Some(reservation),
                        finalization: RefCell::default(),
                        #[cfg(feature = "extensions")]
                        endpoints: Box::default(),
                    }),
                }),
            });
        }
        Ok(Self {
            inner: Rc::new(StoreState {
                id,
                mode: StoreMode::Plain(PlainStore::new(store)),
            }),
        })
    }

    /// A host-only loan. No Store reference or guard escapes this closure.
    pub fn with_mut<R>(&self, f: impl FnOnce(&mut StoreMut<'_>) -> R) -> Result<R, RuntimeError> {
        #[cfg(all(feature = "v8", feature = "extensions"))]
        if let Some(v8) = self.v8_managed() {
            return v8.with_mut(f);
        }
        #[cfg(feature = "sys")]
        if let StoreMode::Native(native) = &self.inner.mode
            && !wasmer_api::sys::native_invocation_active()
        {
            return self.with_borrowed_mut(native, f);
        }
        let mut f = Some(f);
        if let Some(Some(result)) = Store::with_current(|current| {
            (current.objects_mut().id() == self.inner.id).then(|| f.take().unwrap()(current))
        }) {
            return Ok(result);
        }
        match &self.inner.mode {
            StoreMode::Plain(store) => {
                let mut guard = store
                    .try_borrow_mut()
                    .map_err(|_| RuntimeError::new("Store is already borrowed"))?;
                if guard.as_store_mut().objects_mut().id() != self.inner.id {
                    return Err(RuntimeError::new("host loan belongs to another Store"));
                }
                Ok(f.take().unwrap()(&mut guard.as_store_mut()))
            }
            #[cfg(feature = "sys")]
            StoreMode::Native(native) => {
                use std::{
                    future::Future,
                    task::{Context, Poll, Waker},
                };
                use wasmer_api::AsStoreAsync;
                let expected = native.store.as_ref().unwrap();
                let loan = wasmer_api::sys::current_native_store_loan(expected)?;
                let capability = loan.as_ref().unwrap_or(expected);
                let mut lock = std::pin::pin!(capability.write_lock());
                let Poll::Ready(mut guard) =
                    lock.as_mut().poll(&mut Context::from_waker(Waker::noop()))
                else {
                    return Err(RuntimeError::new("Store has no available host loan"));
                };
                Ok(f.take().unwrap()(&mut guard.as_store_mut()))
            }
        }
    }

    #[cfg(feature = "sys")]
    fn with_borrowed_mut<R>(
        &self,
        native: &ManagedStore,
        f: impl FnOnce(&mut StoreMut<'_>) -> R,
    ) -> Result<R, RuntimeError> {
        use std::{
            future::Future,
            task::{Context, Poll, Waker},
        };
        use wasmer_api::AsStoreAsync;
        let expected = native.store.as_ref().unwrap();
        let mut f = Some(f);
        let mut checked = |store: &mut StoreMut<'_>| {
            if store.objects_mut().id() != self.inner.id {
                return Err(RuntimeError::new("host loan belongs to another Store"));
            }
            Ok(f.take().unwrap()(store))
        };
        if let Some(result) = wasmer_api::sys::with_borrowed_native_store(expected, &mut checked)? {
            return result;
        }
        let mut lock = std::pin::pin!(expected.write_lock());
        let Poll::Ready(mut guard) = lock.as_mut().poll(&mut Context::from_waker(Waker::noop()))
        else {
            return Err(RuntimeError::new("Store has no available host loan"));
        };
        // Reject changed actual IDs BEFORE parked's general current-ID check.
        if guard.as_store_mut().objects_mut().id() != self.inner.id {
            return Err(RuntimeError::new("host loan belongs to another Store"));
        }
        // Validate the ACTUAL guard's Box through its derived parked row too;
        // never inspect expected.inner behind that write guard or trust ID alone.
        guard.as_store_mut().parked(|| {
            wasmer_api::sys::with_borrowed_native_store(expected, checked)?.ok_or_else(|| {
                RuntimeError::new("actual borrowed Store installation disappeared")
            })?
        })
    }

    pub fn call(
        &self,
        function: &Function,
        args: Vec<Value>,
    ) -> Result<Box<[Value]>, RuntimeError> {
        let _keepalive = self.clone();
        #[cfg(all(feature = "v8", feature = "extensions"))]
        if let Some(v8) = self.v8_managed() {
            return v8.call(function, args);
        }
        #[cfg(feature = "sys")]
        if matches!(&self.inner.mode, StoreMode::Native(_))
            && tokio::runtime::Handle::try_current().is_ok()
            && !wasmer_api::sys::native_invocation_active()
        {
            let StoreMode::Native(native) = &self.inner.mode else {
                unreachable!()
            };
            let driver = DriverUse::acquire(native)?;
            let _claim = ServiceClaimLease::new(native.ledger.clone());
            let _entered = driver.entered();
            let _runtime = driver.runtime.enter();
            let _local = driver.local.enter();
            // Supply the EXISTING host context for timer/loader first polls,
            // without polling its LocalSet or adopting an async root/producer.
            let result = Store::with_current(|current| {
                current.parked(|| self.with_mut(|store| function.call(store, &args)))
            })
            .unwrap_or_else(|| self.with_mut(|store| function.call(store, &args)))?;
            #[cfg(feature = "extensions")]
            if let Err(error) = &result {
                self.forward_handoff(error, None)?;
            }
            return result;
        }
        if let Some(result) = Store::with_current(|current| {
            if current.objects_mut().id() == self.inner.id {
                function.call(current, &args)
            } else {
                current.parked(|| match &self.inner.mode {
                    #[cfg(feature = "sys")]
                    StoreMode::Native(native) => {
                        let driver = DriverUse::acquire(native)?;
                        wasmer_api::sys::call_native_descendant(
                            function,
                            native.store.as_ref().unwrap(),
                            args.clone(),
                            move |child| async move { driver.run_descendant(child).await },
                        )
                    }
                    StoreMode::Plain(_) => self
                        .with_mut(|store| function.call(store, &args))
                        .and_then(|result| result),
                })
            }
        }) {
            #[cfg(all(feature = "sys", feature = "extensions"))]
            if let Err(error) = &result {
                self.forward_handoff(error, None)?;
            }
            return result;
        }
        match &self.inner.mode {
            StoreMode::Plain(_) => self.with_mut(|store| function.call(store, &args))?,
            #[cfg(feature = "sys")]
            StoreMode::Native(native) => self.native_call(native, function, args),
        }
    }

    #[cfg(feature = "sys")]
    fn native_call(
        &self,
        native: &ManagedStore,
        function: &Function,
        args: Vec<Value>,
    ) -> Result<Box<[Value]>, RuntimeError> {
        let driver = DriverUse::acquire(native)?;
        let owner = Rc::new(CallOwner {
            driver: driver.clone(),
            #[cfg(feature = "extensions")]
            entry: RefCell::default(),
            #[cfg(feature = "extensions")]
            pending: RefCell::default(),
        });
        let erased: Rc<dyn std::any::Any> = owner.clone();
        let store = native.store.as_ref().unwrap();
        #[cfg(feature = "extensions")]
        let entry = {
            let weak = native.endpoints.registered.borrow().clone();
            let endpoints = weak
                .into_iter()
                .filter_map(|endpoint| endpoint.upgrade())
                .collect::<Vec<_>>();
            let mut opener = None;
            let starts = self.with_mut(|store| {
                endpoints
                    .iter()
                    .filter_map(|endpoint| {
                        let start = endpoint.initialized_start(store, function)?;
                        opener.get_or_insert_with(|| endpoint.clone());
                        Some(start)
                    })
                    .collect::<Vec<_>>()
            })?;
            if let Some(opener) = opener {
                let scope =
                    opener.open_entry_scope(starts, wasmer_api::AsStoreAsync::store(store))?;
                drop(opener);
                let entry = Rc::new(CallEntry {
                    scope,
                    cleanup: native.owner.clone(),
                    failure: driver.canonical_failure(),
                    observers: RefCell::default(),
                    done: Default::default(),
                });
                // Outer owner exists before any depth mutation or guest factory.
                *owner.entry.borrow_mut() = Some(entry.clone());
                Some(entry)
            } else {
                None
            }
        };
        let weak_owner = Rc::downgrade(&erased);
        let call: NativeCallFuture<'_> = {
            #[cfg(feature = "extensions")]
            if let Some(entry) = &entry {
                let entry = entry.clone();
                Box::pin(async move {
                    let result = entry.scope.call(function, &args, weak_owner).await;
                    entry.finish().await;
                    result
                })
            } else {
                wasmer_api::sys::call_async_with_local_owner(function, store, args, weak_owner)
            }
            #[cfg(not(feature = "extensions"))]
            wasmer_api::sys::call_async_with_local_owner(function, store, args, weak_owner)
        };
        driver.run(async {
            use wasmer_api::AsStoreAsync;
            let result = call.await;
            // Native settlement notification precedes the actual unlock.
            drop(store.write_lock().await);
            #[cfg(feature = "extensions")]
            if let Err(error) = &result {
                self.forward_handoff(error, Some(&owner))?;
            }
            #[cfg(feature = "extensions")]
            let result = match &entry {
                Some(entry) => result.map_err(|error| entry.scope.map_error(error)),
                None => result,
            };
            match owner.drain().await {
                Some(error) => Err(error),
                None => result,
            }
        })?
    }

    // Preserve the engine's existing typed instantiation error at this boundary.
    #[allow(clippy::result_large_err)]
    pub(crate) fn instantiate(
        &self,
        module: &Module,
        externs: Vec<Extern>,
    ) -> Result<Result<Instance, InstantiationError>, RuntimeError> {
        let _keepalive = self.clone();
        #[cfg(all(feature = "v8", feature = "extensions"))]
        if let Some(v8) = self.v8_managed() {
            return v8.instantiate(module, externs);
        }
        if let Some(result) = Store::with_current(|current| {
            if current.objects_mut().id() == self.inner.id {
                Ok(Instance::new_by_index(current, module, &externs))
            } else {
                current.parked(|| match &self.inner.mode {
                    #[cfg(feature = "sys")]
                    StoreMode::Native(_) => self.instantiate_native(module, externs.clone()),
                    StoreMode::Plain(_) => {
                        self.with_mut(|store| Instance::new_by_index(store, module, &externs))
                    }
                })
            }
        }) {
            #[cfg(all(feature = "sys", feature = "extensions"))]
            match &result {
                Err(error) | Ok(Err(InstantiationError::Start(error))) => {
                    self.forward_handoff(error, None)?
                }
                _ => (),
            }
            return result;
        }
        match &self.inner.mode {
            StoreMode::Plain(_) => {
                self.with_mut(|store| Instance::new_by_index(store, module, &externs))
            }
            #[cfg(feature = "sys")]
            StoreMode::Native(_) => self.instantiate_native(module, externs),
        }
    }

    #[cfg(feature = "sys")]
    #[allow(clippy::result_large_err)]
    fn instantiate_native(
        &self,
        module: &Module,
        externs: Vec<Extern>,
    ) -> Result<Result<Instance, InstantiationError>, RuntimeError> {
        use std::sync::{Arc, Mutex};
        let result = Arc::new(Mutex::new(None));
        let output = Arc::downgrade(&result);
        let request = Arc::new(Mutex::new(Some((module.clone(), externs))));
        let input = Arc::downgrade(&request);
        // Per-operation weak payloads own no Store/driver/module roots.
        // Recursive calls have independent request and result custody.
        let trampoline = self.with_mut(|store| {
            let env = wasmer_api::FunctionEnv::new(store, ());
            Function::new_with_env(
                store,
                &env,
                wasmer_api::FunctionType::new([], []),
                move |mut env, _| {
                    let request = input
                        .upgrade()
                        .ok_or_else(|| RuntimeError::new("instantiation request expired"))?;
                    let (module, externs) = request
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .take()
                        .ok_or_else(|| {
                            RuntimeError::new("instantiation request already entered")
                        })?;
                    let output = output
                        .upgrade()
                        .ok_or_else(|| RuntimeError::new("instantiation result expired"))?;
                    let instance =
                        Instance::new_by_index(&mut env.as_store_mut(), &module, &externs);
                    let abort = match &instance {
                        Err(InstantiationError::Start(error)) if error.is_host_abort() => {
                            Some(error.clone())
                        }
                        _ => None,
                    };
                    *output.lock().unwrap_or_else(|error| error.into_inner()) = Some(instance);
                    // Committed start cannot be swallowed by the trampoline
                    // before the enclosing native caller latches its abort.
                    match abort {
                        Some(error) => Err(error),
                        None => Ok(Vec::new()),
                    }
                },
            )
        });
        let trampoline = trampoline?;
        if let Err(error) = self.call(&trampoline, Vec::new()) {
            #[cfg(feature = "extensions")]
            if self.is_call_handoff(&error) || error.downcast_ref::<CompletedHandoff>().is_some() {
                return Ok(Err(InstantiationError::Start(error)));
            }
            let value = result
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take();
            return match value {
                Some(Err(instance_error)) => Ok(Err(instance_error)),
                // The call can fail after the trampoline saved success, during
                // its final unlock/retirement. That instance was never admitted.
                Some(Ok(instance)) => {
                    drop(instance);
                    Err(error)
                }
                None => Err(error),
            };
        }
        let value = result
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        value.ok_or_else(|| RuntimeError::new("native instantiation did not settle"))
    }

    #[cfg(all(feature = "v8", feature = "extensions"))]
    fn v8_managed(&self) -> Option<&v8::ManagedStore> {
        match &self.inner.mode {
            StoreMode::Plain(PlainStore::V8(store)) => Some(store),
            _ => None,
        }
    }

    #[cfg(all(feature = "v8", feature = "extensions"))]
    pub fn is_v8_managed(&self) -> bool {
        self.v8_managed().is_some()
    }

    #[cfg(all(feature = "v8", feature = "extensions"))]
    pub fn v8_driver(&self) -> Result<Rc<v8::Driver>, RuntimeError> {
        self.v8_managed()
            .ok_or_else(|| RuntimeError::new("Store is not managed by V8"))?
            .driver()
    }

    #[cfg(feature = "extensions")]
    pub fn is_native(&self) -> bool {
        match &self.inner.mode {
            #[cfg(feature = "sys")]
            StoreMode::Native(_) => true,
            StoreMode::Plain(_) => false,
        }
    }

    #[cfg(all(feature = "sys", any(feature = "extensions", test)))]
    pub fn async_store(&self) -> Result<wasmer_api::StoreAsync, RuntimeError> {
        use wasmer_api::AsStoreAsync;
        #[cfg(all(feature = "v8", feature = "extensions"))]
        if let Some(v8) = self.v8_managed() {
            return Ok(v8.asynchronous());
        }
        match &self.inner.mode {
            StoreMode::Native(native) => Ok(native.store.as_ref().unwrap().store()),
            StoreMode::Plain(_) => Err(RuntimeError::new(
                "Store is not managed by the native driver",
            )),
        }
    }

    #[cfg(all(feature = "sys", any(feature = "extensions", test)))]
    pub fn run_host<R>(
        &self,
        future: impl std::future::Future<Output = R>,
    ) -> Result<R, RuntimeError> {
        #[cfg(all(feature = "v8", feature = "extensions"))]
        if let Some(v8) = self.v8_managed() {
            return v8.driver()?.run(future);
        }
        match &self.inner.mode {
            StoreMode::Native(native) => DriverUse::acquire(native)?.run(future),
            StoreMode::Plain(_) => Err(RuntimeError::new(
                "Store is not managed by the native driver",
            )),
        }
    }

    #[cfg(all(feature = "sys", test))]
    pub(crate) fn register_cleanup_fence(
        &self,
        fence: Box<dyn Fn() -> bool>,
    ) -> Result<(), RuntimeError> {
        match &self.inner.mode {
            StoreMode::Native(native) => {
                native
                    .owner
                    .bind_store_cleanup_fence(wasmer_api::sys::NativeCleanupFence::new(
                        Rc::new(|_| wasmer_api::sys::NativeCleanupFenceObservation::Passive),
                        Rc::from(fence),
                    ));
                Ok(())
            }
            StoreMode::Plain(_) => Err(RuntimeError::new("Store has no native cleanup owner")),
        }
    }

    #[cfg(all(feature = "sys", any(feature = "extensions", test)))]
    pub fn cleanup_owner(&self) -> Result<wasmer_api::sys::NativeCleanupOwner, RuntimeError> {
        match &self.inner.mode {
            StoreMode::Native(native) => Ok(native.owner.clone()),
            StoreMode::Plain(_) => Err(RuntimeError::new("Store has no native cleanup owner")),
        }
    }

    #[cfg(all(feature = "sys", feature = "extensions"))]
    pub fn retirement_owner(&self) -> Result<CleanupRetirement, RuntimeError> {
        let StoreMode::Native(native) = &self.inner.mode else {
            return Err(RuntimeError::new("Store has no native cleanup owner"));
        };
        let lease = DriverUseReceipt::new(native.scopes.clone());
        Ok(CleanupRetirement {
            driver: native.driver()?,
            owner: native.owner.clone(),
            _use_receipt: lease,
        })
    }

    #[cfg(all(feature = "sys", feature = "extensions"))]
    pub fn cleanup_retirement(
        &self,
        future: std::pin::Pin<Box<dyn std::future::Future<Output = ()>>>,
    ) -> Result<Box<dyn FnOnce()>, RuntimeError> {
        let owner = self.retirement_owner()?;
        Ok(Box::new(move || owner.retain(future)))
    }

    #[cfg(all(feature = "sys", feature = "extensions"))]
    pub fn register_endpoint(
        &self,
        endpoint: Weak<dyn ContinuationEndpoint>,
    ) -> Result<(), RuntimeError> {
        match &self.inner.mode {
            StoreMode::Native(native) => {
                if let Some(live) = endpoint.upgrade() {
                    native
                        .endpoints
                        .handoff
                        .set(Some(live.handoff_classifier()));
                }
                let mut endpoints = native.endpoints.registered.borrow_mut();
                endpoints.retain(|endpoint| endpoint.strong_count() != 0);
                endpoints.push(endpoint);
                Ok(())
            }
            StoreMode::Plain(_) => Err(RuntimeError::new("Store has no native continuation owner")),
        }
    }

    /// Whether `error` is a call handoff of an endpoint this Store registered.
    #[cfg(all(feature = "sys", feature = "extensions"))]
    fn is_call_handoff(&self, error: &RuntimeError) -> bool {
        match &self.inner.mode {
            StoreMode::Native(native) => native
                .endpoints
                .handoff
                .get()
                .is_some_and(|handoff| handoff(error)),
            StoreMode::Plain(_) => false,
        }
    }

    #[cfg(all(feature = "sys", feature = "extensions"))]
    fn forward_handoff(
        &self,
        error: &RuntimeError,
        explicit: Option<&Rc<CallOwner>>,
    ) -> Result<(), RuntimeError> {
        let StoreMode::Native(native) = &self.inner.mode else {
            return Ok(());
        };
        let weak = {
            let mut endpoints = native.endpoints.registered.borrow_mut();
            endpoints.retain(|endpoint| endpoint.strong_count() != 0);
            endpoints.clone()
        };
        let endpoints = weak
            .into_iter()
            .filter_map(|endpoint| endpoint.upgrade())
            .collect::<Vec<_>>();
        let endpoint = self.with_mut(|store| {
            endpoints
                .into_iter()
                .find(|endpoint| endpoint.matches(store, error))
        })?;
        let Some(endpoint) = endpoint else {
            return Ok(());
        };
        let owner = explicit
            .cloned()
            .or_else(wasmer_api::sys::current_native_local_owner::<CallOwner>);
        let Some(owner) = owner else {
            // Never claim work that has no exact outer local dispatcher.
            return endpoint.cancel_unclaimed(self, error);
        };
        owner.pending.borrow_mut().reserve(1);
        let continuation = endpoint.prepare(self, error)?;
        owner.pending.borrow_mut().push_back(continuation);
        Ok(())
    }

    #[cfg(all(
        any(test, feature = "test-support"),
        feature = "sys",
        feature = "extensions"
    ))]
    pub fn enroll_native_retirement(
        &self,
        completed: Rc<std::cell::Cell<bool>>,
    ) -> Result<Rc<NativeRetirementState>, RuntimeError> {
        let registration =
            self.register_owned_retirement(completed, RetirementFailure::default())?;
        let state = registration.state();
        self.retain_delivery(state.clone())?;
        self.enroll_owned_retirement_tail(registration)?;
        Ok(state)
    }
    #[cfg(all(feature = "sys", feature = "extensions"))]
    pub fn retirement_failure(&self) -> Result<RetirementFailure, RuntimeError> {
        let StoreMode::Native(native) = &self.inner.mode else {
            return Err(RuntimeError::new("Store has no native retirement ledger"));
        };
        let failure = RetirementFailure::default();
        failure.bind_clock(&native.ledger.clock)?;
        Ok(failure)
    }
    #[cfg(all(feature = "sys", feature = "extensions"))]
    pub fn register_owned_retirement(
        &self,
        completed: Rc<std::cell::Cell<bool>>,
        failure: RetirementFailure,
    ) -> Result<RetirementRegistration, RuntimeError> {
        let StoreMode::Native(native) = &self.inner.mode else {
            return Err(RuntimeError::new("Store has no native retirement ledger"));
        };
        let service_claim = ServiceClaimLease::new(native.ledger.clone());
        failure.bind_clock(&native.ledger.clock)?;
        let delivery_ack = failure.1.clone();
        let state = Rc::new(NativeRetirementState {
            ledger: native.ledger.clone(),
            disposition: std::cell::Cell::new(DeliveryDisposition::AdmittedContinuation),
            committed: std::cell::Cell::new(false),
            terminal_done: completed,
            tail_receipt_released: std::cell::Cell::new(false),
            delivery_ack,
            publishing: std::cell::Cell::new(false),
            receipt_queued: std::cell::Cell::new(false),
            failure,
            returned: RefCell::default(),
            release_waiter: RefCell::default(),
            service_claim: RefCell::new(Some(service_claim)),
        });
        let registration = RetirementRegistration { state, armed: true };
        native.ledger.enroll(&registration.state);
        Ok(registration)
    }
    // Existing metadata-only fixtures explicitly model a relinquished receiver.
    #[cfg(all(test, feature = "sys", feature = "extensions"))]
    pub(crate) fn register_retirement(
        &self,
        completed: Rc<std::cell::Cell<bool>>,
        failure: RetirementFailure,
    ) -> Result<Rc<NativeRetirementState>, RuntimeError> {
        let registration = self.register_owned_retirement(completed, failure)?;
        let state = registration.state();
        registration.relinquish();
        Ok(state)
    }

    #[cfg(all(feature = "sys", feature = "extensions"))]
    pub fn retain_delivery(&self, state: Rc<NativeRetirementState>) -> Result<(), RuntimeError> {
        let owner = self.retirement_owner()?;
        assert!(
            !state.receipt_queued.replace(true),
            "one exact delivery receipt"
        );
        state.committed.set(true); // no callback/refusal before actual insertion
        owner.owner.transfer_existing(Box::pin(DeliveryReceipt {
            state,
            ledger: owner.driver.ledger.clone(),
        }));
        Ok(())
    }

    #[cfg(all(feature = "sys", feature = "extensions"))]
    pub fn retirement_release(
        &self,
        state: Rc<NativeRetirementState>,
    ) -> Result<TailRelease, RuntimeError> {
        let owner = self.retirement_owner()?;
        Ok(TailRelease {
            state,
            ledger: owner.driver.ledger.clone(),
            owner: owner.owner.clone(),
            wake: owner.driver.wake.clone(),
        })
    }

    #[cfg(all(feature = "sys", feature = "extensions"))]
    pub fn enroll_owned_retirement_tail(
        &self,
        registration: RetirementRegistration,
    ) -> Result<(), RuntimeError> {
        let owner = self.retirement_owner()?;
        let state = registration.state();
        state.disposition.set(DeliveryDisposition::LiveTail);
        let release = self.retirement_release(state.clone())?;
        let finalization = self.finalization_handoff(&owner);
        let first = self.retirement_failure()?;
        let tail = Box::pin(NativeRetirementTail {
            subscription: Some(owner.subscribe()),
            _store: self.downgrade(),
            base: Some(self.async_store()?),
            cleanup: Some(owner.owner.clone()),
            owner: Some(owner),
            release: Some(release),
            state,
            registration: Some(registration),
            first,
            finalization_subscription: None,
            finalization,
            exit: None,
        });
        if let Err(tail) = wasmer_api::sys::enroll_native_ancestry_tail(tail) {
            drop(tail);
        }
        Ok(())
    }
    #[cfg(all(feature = "sys", feature = "extensions"))]
    fn finalization_handoff(
        &self,
        owner: &CleanupRetirement,
    ) -> Rc<NativeStoreFinalizationHandoff> {
        let StoreMode::Native(native) = &self.inner.mode else {
            unreachable!()
        };
        let handoff = native.finalization.borrow().upgrade().unwrap_or_else(|| {
            NativeStoreFinalizationHandoff::new(native, owner.driver.wake.clone())
        });
        *native.finalization.borrow_mut() = Rc::downgrade(&handoff);
        *native.scopes.finalization.borrow_mut() = Rc::downgrade(&handoff);
        handoff
    }

    #[cfg(all(
        any(test, feature = "test-support"),
        feature = "sys",
        feature = "extensions"
    ))]
    pub fn strong_handles_for_test(&self) -> usize {
        Rc::strong_count(&self.inner)
    }

    #[cfg(all(
        any(test, feature = "test-support"),
        feature = "sys",
        feature = "extensions"
    ))]
    pub fn retirement_states_for_test(&self) -> Vec<Rc<NativeRetirementState>> {
        self.retirement_owner()
            .unwrap()
            .driver
            .retirement_snapshot()
    }

    #[cfg(all(
        any(test, feature = "test-support"),
        feature = "sys",
        feature = "extensions"
    ))]
    pub fn forward_handoff_for_test(&self, error: &RuntimeError) -> Result<(), RuntimeError> {
        self.forward_handoff(error, None)
    }

    /// The native Store's current driver, if it has one.
    #[cfg(all(
        any(test, feature = "test-support"),
        feature = "sys",
        feature = "extensions"
    ))]
    pub fn native_driver_for_test(&self) -> Option<Rc<Driver>> {
        match &self.inner.mode {
            StoreMode::Native(native) => native.driver.borrow().clone(),
            StoreMode::Plain(_) => None,
        }
    }

    #[cfg(all(
        any(test, feature = "test-support"),
        feature = "sys",
        feature = "extensions"
    ))]
    pub fn native_ledger_for_test(&self) -> Option<Rc<RetirementLedger>> {
        match &self.inner.mode {
            StoreMode::Native(native) => Some(native.ledger.clone()),
            StoreMode::Plain(_) => None,
        }
    }

    #[cfg(all(
        any(test, feature = "test-support"),
        feature = "sys",
        feature = "extensions"
    ))]
    pub fn has_finalization_handoff_for_test(&self) -> bool {
        match &self.inner.mode {
            StoreMode::Native(native) => native.finalization.borrow().upgrade().is_some(),
            StoreMode::Plain(_) => false,
        }
    }

    pub fn downgrade(&self) -> WeakStoreRef {
        WeakStoreRef {
            inner: Rc::downgrade(&self.inner),
        }
    }
}

#[cfg(feature = "sys")]
fn new_owner_runtime() -> Result<tokio::runtime::Runtime, RuntimeError> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| RuntimeError::new(error.to_string()))
}

#[cfg(feature = "sys")]
impl ManagedStore {
    fn driver(&self) -> Result<Rc<Driver>, RuntimeError> {
        if let Some(driver) = self.driver.borrow().as_ref() {
            return Ok(driver.clone());
        }
        let runtime = new_owner_runtime()?;
        let driver = Rc::new(Driver {
            runtime: std::sync::Arc::new(runtime),
            local: tokio::task::LocalSet::new(),
            active: std::cell::Cell::new(false),
            scopes: self.scopes.clone(),
            owner: self.owner.clone(),
            ledger: self.ledger.clone(),
            cleanup_failure: Rc::downgrade(&self.cleanup_failure),
            wake: Default::default(),
        });
        *self.driver.borrow_mut() = Some(driver.clone());
        Ok(driver)
    }
}

#[cfg(feature = "sys")]
impl Driver {
    fn entered(&self) -> DriverEntered<'_> {
        self.scopes.entered.set(self.scopes.entered.get() + 1);
        DriverEntered(&self.scopes)
    }
    fn subscribe(&self) -> DriverSubscription {
        DriverSubscription::new(self.wake.clone(), self.owner.clone())
    }
}
#[cfg(feature = "sys")]
impl DriverSubscription {
    fn new(wake: std::sync::Arc<DriverWake>, owner: wasmer_api::sys::NativeCleanupOwner) -> Self {
        let token = {
            let mut state = wake.state.lock().unwrap_or_else(|e| e.into_inner());
            let token = state.next;
            state.next = token.checked_add(1).expect("native wake token exhausted");
            state.subscribers.insert(token, None);
            token
        };
        Self { wake, owner, token }
    }
}
#[cfg(feature = "sys")]
impl Driver {
    fn retirement_snapshot(&self) -> Vec<Rc<NativeRetirementState>> {
        self.ledger.snapshot()
    }
    fn retirements_pending(&self) -> bool {
        self.retirement_snapshot().iter().any(|state| {
            !state.physically_ready() || state.has_unstamped_event() || {
                #[cfg(feature = "extensions")]
                {
                    state.publishing.get()
                }
                #[cfg(not(feature = "extensions"))]
                {
                    false
                }
            }
        })
    }
    fn canonical_failure(&self) -> Rc<RetirementFailure> {
        self.cleanup_failure
            .upgrade()
            .expect("actual Driver ordering custodian lost canonical carrier")
    }
    fn try_common_collector(&self) -> Option<CommonCollectorLease> {
        if !self.scopes.canonical_allowed(None) {
            return None;
        }
        let state = Rc::new(CommonCollectorState {
            scopes: self.scopes.clone(),
            canonical: self.cleanup_failure.clone(),
            wake: std::sync::Arc::downgrade(&self.wake),
            service_claim: RefCell::new(Some(ServiceClaimLease::new(self.ledger.clone()))),
            released: std::cell::Cell::new(false),
        });
        *self.scopes.common_collector.borrow_mut() = Rc::downgrade(&state);
        Some(CommonCollectorLease { state })
    }
    fn collect_retirement_failures(&self, owner: &CommonCollectorLease) {
        assert!(
            self.scopes.canonical_allowed(Some(owner)),
            "wrong common collector identity"
        );
        let canonical = self.canonical_failure();
        self.ledger.collect_into(&canonical);
    }

    async fn drive<R>(
        &self,
        future: impl std::future::Future<Output = R>,
        observer: &DriverSubscription,
    ) -> R {
        let mut state = DriveState {
            driver: self,
            future: Some(Box::pin(future)),
            output: None,
            main_ready: false,
            collector: None,
        };
        std::future::poll_fn(|cx| {
            let _ = self.owner.poll_idle(cx);
            if !state.main_ready
                && let std::task::Poll::Ready(value) = observer
                    .with_real_context(cx, |cx| state.future.as_mut().unwrap().as_mut().poll(cx))
            {
                state.output = Some(value);
                state.main_ready = true;
            }
            let _ = self.owner.poll_idle(cx);
            if !state.main_ready {
                return std::task::Poll::Pending;
            }
            // Own applicable tail delivery occurred before main Ready. A gets
            // its explicit identity before FIRST common/Store event collection.
            if state.collector.is_none() {
                state.collector = self.try_common_collector();
                if state.collector.is_none() {
                    // A distinct common owner already progresses the global
                    // receipts. B must not wait for A or take its events.
                    state.dispose_completed_frame();
                    return std::task::Poll::Ready(state.output.take().unwrap());
                }
            }
            let canonical = self.canonical_failure();
            self.collect_retirement_failures(state.collector.as_ref().unwrap());
            state.dispose_completed_frame(); // ONCE, even after output disposal
            canonical.attempt(|| self.owner.notify());
            self.collect_retirement_failures(state.collector.as_ref().unwrap());
            if self.retirements_pending() || canonical.has_unstamped_event() {
                return std::task::Poll::Pending;
            }
            if !canonical.is_empty() {
                state.dispose_output();
                canonical.attempt(|| self.owner.notify());
                self.collect_retirement_failures(state.collector.as_ref().unwrap());
                if self.retirements_pending() || canonical.has_unstamped_event() {
                    return std::task::Poll::Pending;
                }
            }
            // The identity stays registered and I stays canonical during EVERY
            // release callback. B can complete only its own applicable outcome.
            state.collector.as_ref().unwrap().wake_while_owned();
            canonical.attempt(|| self.owner.notify());
            self.collect_retirement_failures(state.collector.as_ref().unwrap());
            if self.retirements_pending() || canonical.has_unstamped_event() {
                return std::task::Poll::Pending;
            }
            if !canonical.is_empty() && state.output.is_some() {
                // Release wake itself may have produced the first error.
                state.dispose_output();
                canonical.attempt(|| self.owner.notify());
                self.collect_retirement_failures(state.collector.as_ref().unwrap());
                if self.retirements_pending() || canonical.has_unstamped_event() {
                    return std::task::Poll::Pending;
                }
            }
            let exact = canonical.take();
            state.collector.take().unwrap().release_without_callbacks();
            if let Some(exact) = exact {
                std::panic::resume_unwind(exact);
            }
            std::task::Poll::Ready(state.output.take().unwrap())
        })
        .await
    }

    async fn run_descendant<R>(&self, future: impl std::future::Future<Output = R>) -> R {
        let _claim = ServiceClaimLease::new(self.ledger.clone());
        let _entered = self.entered();
        let observer = self.subscribe();
        if self.active.get() {
            // An ancestor's LocalSet already has an outer poller. Enter only
            // its spawn/runtime context; let that poller run its queued tasks.
            let mut future = std::pin::pin!(future);
            return std::future::poll_fn(|cx| {
                observer.with_context(cx, |cx| {
                    let _runtime = self.runtime.enter();
                    let _local = self.local.enter();
                    let _ = self.owner.poll_idle(cx);
                    observer.with_real_context(cx, |cx| future.as_mut().poll(cx))
                })
            })
            .await;
        }
        self.active.set(true);
        struct Reset<'a>(&'a std::cell::Cell<bool>);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _reset = Reset(&self.active);
        // Nested guest completion must return to its still-parked ancestor.
        // Global enrolled retirement joins belong to that outer call owner.
        // Only the LocalSet/pump observes the fanout Waker. The pinned user
        // future sees its actual caller's Waker, so nested A/B/A boundaries
        // cannot subscribe fanouts to each other.
        let mut future = std::pin::pin!(future);
        let user = std::future::poll_fn(|cx| {
            observer.with_real_context(cx, |cx| future.as_mut().poll(cx))
        });
        let mut run = std::pin::pin!(self.local.run_until(user));
        std::future::poll_fn(|cx| {
            observer.with_context(cx, |cx| {
                let _runtime = self.runtime.enter();
                std::future::Future::poll(run.as_mut(), cx)
            })
        })
        .await
    }

    #[cfg(feature = "extensions")]
    async fn run_local<R>(&self, future: impl std::future::Future<Output = R>) -> R {
        let _claim = ServiceClaimLease::new(self.ledger.clone());
        let _entered = self.entered();
        let observer = self.subscribe();
        assert!(!self.active.replace(true), "recursive native LocalSet poll");
        struct Reset<'a>(&'a std::cell::Cell<bool>);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _reset = Reset(&self.active);
        let mut run = std::pin::pin!(self.local.run_until(self.drive(future, &observer)));
        std::future::poll_fn(|cx| {
            observer.with_context(cx, |cx| {
                let _runtime = self.runtime.enter();
                std::future::Future::poll(run.as_mut(), cx)
            })
        })
        .await
    }

    #[cfg(feature = "extensions")]
    fn poll_retirement(&self, cx: &mut std::task::Context<'_>) {
        let _entered = self.entered();
        let _runtime = self.runtime.enter();
        if !self.active.get() {
            self.active.set(true);
            struct Reset<'a>(&'a std::cell::Cell<bool>);
            impl Drop for Reset<'_> {
                fn drop(&mut self) {
                    self.0.set(false);
                }
            }
            let _reset = Reset(&self.active);
            let local = self.local.run_until(std::future::pending::<()>());
            let _ = std::pin::pin!(local).as_mut().poll(cx);
        }
        // Another active call already owns this LocalSet's outer poller. Its
        // queued tasks retain that poller; only enter the spawn context here.
        let _local = self.local.enter();
        let _ = self.owner.poll_idle(cx);
    }

    fn run<R>(&self, future: impl std::future::Future<Output = R>) -> Result<R, RuntimeError> {
        let _claim = ServiceClaimLease::new(self.ledger.clone());
        let _entered = self.entered();
        // This blocking boundary is not a Tokio task. A callback with an exact
        // parked loan takes the synchronous path before reaching the driver.
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(RuntimeError::new(
                "blocking C API entry has no parked Store loan in the entered runtime",
            ));
        }
        if self.active.replace(true) {
            return Err(RuntimeError::new(
                "nested native entry requires a parked Store loan",
            ));
        }
        struct Reset<'a>(&'a std::cell::Cell<bool>);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _reset = Reset(&self.active);
        let observer = self.subscribe();
        let mut run = std::pin::pin!(self.local.run_until(self.drive(future, &observer)));
        Ok(self.runtime.block_on(std::future::poll_fn(|cx| {
            observer.with_context(cx, |cx| run.as_mut().poll(cx))
        })))
    }
}

#[cfg(feature = "sys")]
pub type RetirementPanic = Box<dyn std::any::Any + Send>;

#[cfg(feature = "sys")]
fn discard_retirement_panic(mut panic: RetirementPanic) {
    loop {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(panic))) {
            Ok(()) => return,
            Err(next) => panic = next,
        }
    }
}

#[cfg(feature = "sys")]
fn remember_retirement_panic(first: &mut Option<RetirementPanic>, panic: RetirementPanic) {
    if first.is_none() {
        *first = Some(panic);
    } else {
        // A later user panic payload may itself panic on destruction.
        discard_retirement_panic(panic);
    }
}

#[cfg(feature = "sys")]
fn attempt_retirement(first: &mut Option<RetirementPanic>, cleanup: impl FnOnce()) -> bool {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(cleanup)) {
        Ok(()) => true,
        Err(panic) => {
            remember_retirement_panic(first, panic);
            false
        }
    }
}

#[cfg(feature = "sys")]
async fn catch_retirement_future<F: std::future::Future>(
    future: F,
    failure: &RetirementFailure,
) -> Result<F::Output, ()> {
    let mut future = Box::pin(future);
    let output = std::future::poll_fn(|cx| {
        let unwinding = std::thread::panicking();
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| future.as_mut().poll(cx))) {
            Ok(poll) => poll.map(Ok),
            Err(panic) => {
                failure.record_caught(panic, unwinding); // ORIGINAL catch, before frame Drop
                std::task::Poll::Ready(Err(()))
            }
        }
    })
    .await;
    failure.attempt(|| drop(future)); // canonical frame Drop has its own entry flag
    output
}

#[cfg(feature = "sys")]
struct NativeStoreFinalizationHandoff {
    pending: RefCell<Option<NativeStoreFinalization>>,
    failure: RetirementFailure,
    _failures_receiver: wasmer_api::sys::NativeCleanupFailures,
    #[cfg(feature = "extensions")]
    scopes: Rc<DriverScopes>,
    wake: std::sync::Arc<DriverWake>, // SAME original final-caller notification metadata
    ledger: Rc<RetirementLedger>,
    polling: std::cell::Cell<bool>,
    blocked: std::cell::Cell<bool>,
    complete: std::cell::Cell<bool>,
    owner: wasmer_api::sys::NativeCleanupOwner,
    exit: Option<wasmer_api::sys::NativeCleanupExitBoundary>, // LAST
}
#[cfg(feature = "sys")]
impl NativeStoreFinalizationHandoff {
    fn new(native: &ManagedStore, wake: std::sync::Arc<DriverWake>) -> Rc<Self> {
        Rc::new(Self {
            pending: RefCell::default(),
            failure: native.cleanup_failure.as_ref().clone(),
            _failures_receiver: native.failures_receiver.clone(),
            #[cfg(feature = "extensions")]
            scopes: native.scopes.clone(),
            wake,
            ledger: native.ledger.clone(),
            polling: std::cell::Cell::new(false),
            blocked: std::cell::Cell::new(false),
            complete: std::cell::Cell::new(false),
            owner: native.owner.clone(),
            exit: None,
        })
    }

    fn notify(&self) {
        let wake = std::task::Waker::from(self.wake.clone());
        self.failure.attempt(|| wake.wake_by_ref());
        self.failure.attempt(|| drop(wake));
    }
    fn poll(&self, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        let _exit = self.owner.enter_exit_boundary();
        if self.complete.get() {
            return std::task::Poll::Ready(());
        }
        if self.polling.replace(true) {
            return std::task::Poll::Pending;
        }
        struct Reset<'a>(&'a std::cell::Cell<bool>);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _reset = Reset(&self.polling);
        let actual = self.pending.borrow_mut().take();
        let Some(mut actual) = actual else {
            return std::task::Poll::Ready(());
        };
        match self.failure.attempt(|| actual.poll(cx)) {
            Some(std::task::Poll::Ready(())) => {
                // Actual resource/runtime fields are gone before final callers
                // can reenter. A reentrant caller may finish while polling is
                // still true: complete is the truthful resource certificate.
                assert!(self.ledger.final_done.get());
                self.complete.set(true);
                self.notify();
                std::task::Poll::Ready(())
            }
            outcome => {
                *self.pending.borrow_mut() = Some(actual);
                if outcome.is_none() {
                    self.failure.attempt(|| cx.waker().wake_by_ref());
                }
                std::task::Poll::Pending
            }
        }
    }
}
#[cfg(feature = "sys")]
impl Drop for NativeStoreFinalizationHandoff {
    fn drop(&mut self) {
        self.exit = Some(self.owner.enter_exit_boundary());
        if let Some(actual) = self.pending.get_mut().take() {
            self.exit
                .as_ref()
                .unwrap()
                .defer(Box::new(move || actual.finish_blocking()));
        } else if self.complete.get() && self.ledger.final_done.get() {
            // pending=None alone is not a resource certificate: another poll
            // can temporarily own the phase. Only actual completed resources
            // permit LAST-owned exceptional delivery, including exhaustion.
            let first = self.failure.take_terminal_payload();
            if let Some(first) = first {
                self.exit
                    .as_ref()
                    .unwrap()
                    .defer(Box::new(move || Some(first)));
            }
        }
        // A live Store/unfinished phase retains the SAME canonical carrier and
        // receiver even when this particular handoff never published a phase.
    }
}
// Finalization-only custody on an original Native caller. This is not an
// invocation receipt and never invents terminal/physical readiness bits.
#[cfg(feature = "sys")]
struct NativeStoreFinalizationTail {
    subscription: Option<DriverSubscription>,
    handoff: Option<Rc<NativeStoreFinalizationHandoff>>,
    claim: Option<ServiceClaimLease>,
    exit: Option<wasmer_api::sys::NativeCleanupExitBoundary>, // LAST through all fields
}
#[cfg(feature = "sys")]
impl NativeStoreFinalizationTail {
    fn enroll(handoff: Rc<NativeStoreFinalizationHandoff>) {
        let tail = Box::pin(Self {
            subscription: None,
            claim: Some(ServiceClaimLease::new(handoff.ledger.clone())),
            handoff: Some(handoff.clone()),
            exit: None,
        });
        let unwinding = std::thread::panicking();
        // Legal precommit steps in this existing API are metadata-only. The
        // queue owns the job BEFORE its saved caller-Waker can invoke user code.
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            wasmer_api::sys::enroll_native_ancestry_tail(tail)
        })) {
            Ok(Ok(())) => (),
            Ok(Err(exact)) => {
                // The exact returned job makes ONE lexical transfer in Drop.
                // Keep local H alive until that transfer; never take it again.
                drop(exact);
            }
            Err(panic) => handoff.failure.record_caught(panic, unwinding),
        }
        // A missing actual Native ancestry retains the existing explicit outer
        // fallback; active pure-Tokio progress remains MANDATORY OPEN.
        drop(handoff);
    }
}
#[cfg(feature = "sys")]
impl std::future::Future for NativeStoreFinalizationTail {
    type Output = ();
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        use std::task::Poll;
        let this = self.get_mut();
        let handoff = this.handoff.as_ref().unwrap();
        if this.subscription.is_none() {
            this.subscription = handoff
                .failure
                .attempt(|| DriverSubscription::new(handoff.wake.clone(), handoff.owner.clone()));
        }
        let Some(subscription) = this.subscription.as_ref() else {
            handoff.failure.attempt(|| cx.waker().wake_by_ref());
            return Poll::Pending;
        };
        let ready = handoff
            .failure
            .attempt(|| subscription.with_context(cx, |cx| handoff.poll(cx)));
        match ready {
            None => {
                // Clone/replacement Drop may fail BEFORE H or its observer is
                // polled. Preserve that first event, then request a real retry.
                handoff.failure.attempt(|| cx.waker().wake_by_ref());
                return Poll::Pending;
            }
            Some(Poll::Pending) => return Poll::Pending,
            Some(Poll::Ready(())) => (),
        }
        assert!(handoff.complete.get() && handoff.ledger.final_done.get());
        handoff.failure.attempt(|| drop(this.subscription.take()));
        if handoff.failure.has_unstamped_event() {
            return Poll::Pending;
        }
        let first = handoff.failure.take(); // exact ownership BEFORE claim release
        drop(this.claim.take()); // callback-free; no competing service claim
        if let Some(first) = first {
            // Existing core catches I BEFORE dropping this job and its H. A
            // following final-caller metadata error Q therefore cannot replace I.
            std::panic::resume_unwind(first);
        }
        Poll::Ready(())
    }
}
#[cfg(feature = "sys")]
impl Drop for NativeStoreFinalizationTail {
    fn drop(&mut self) {
        let handoff = self.handoff.take().unwrap();
        self.exit = Some(handoff.owner.enter_exit_boundary());
        handoff.failure.attempt(|| drop(self.subscription.take()));
        let actual = handoff.pending.borrow_mut().take();
        if let Some(actual) = actual {
            let claim = self.claim.take();
            self.exit.as_ref().unwrap().defer(Box::new(move || {
                let _claim = claim;
                actual.finish_blocking()
            }));
        } else {
            assert!(handoff.complete.get() && handoff.ledger.final_done.get());
            // H's existing terminal Drop transfers exact owned delivery into
            // this LAST outer boundary, whose receiver spans every H field.
            drop(self.claim.take());
        }
        drop(handoff);
    }
}

#[cfg(feature = "sys")]
#[derive(Clone, Copy)]
enum StoreFinalizationPhase {
    Quiescence,
    Drain,
    Local,
    LocalDrain,
    Store,
    StoreDestruction,
    StoreDrain,
    Observers,
    ObserverDrain,
    Seal,
    Runtime,
    Done,
}
#[cfg(feature = "sys")]
type StoreQuiescence =
    std::pin::Pin<Box<dyn std::future::Future<Output = Option<wasmer_api::StoreAsync>>>>;
#[cfg(feature = "sys")]
struct NativeStoreFinalization {
    phase: StoreFinalizationPhase,
    quiescence: Option<StoreQuiescence>,
    store: Option<wasmer_api::StoreAsync>,
    destruction: wasmer_api::sys::NativeStoreDestruction,
    destruction_observer: Option<wasmer_api::sys::NativeStoreDestructionObserver>,
    driver: Option<Rc<Driver>>,
    runtime: Option<std::sync::Arc<tokio::runtime::Runtime>>,
    ledger: Rc<RetirementLedger>,
    scopes: Rc<DriverScopes>,
    seal: wasmer_api::sys::NativeCleanupSeal,
    reservation: Option<wasmer_api::sys::NativeStoreDestructionReservation>,
    owner: wasmer_api::sys::NativeCleanupOwner,
    failure: RetirementFailure,
    _failures_receiver: wasmer_api::sys::NativeCleanupFailures,
    poisoned: bool,
}
#[cfg(feature = "sys")]
impl RetirementLedger {
    fn resources_ready(&self) -> bool {
        self.snapshot().iter().all(|state| {
            state.physically_ready() && !state.has_unstamped_event() && {
                #[cfg(feature = "extensions")]
                {
                    !state.publishing.get()
                }
                #[cfg(not(feature = "extensions"))]
                {
                    true
                }
            }
        })
    }
    fn collect_into(&self, destination: &RetirementFailure) {
        if destination.has_unstamped_event() {
            return;
        }
        for state in self.snapshot() {
            if !matches!(
                state.disposition.get(),
                DeliveryDisposition::Cancelled | DeliveryDisposition::Relinquished
            ) || !state.physically_ready()
            {
                continue;
            }
            #[cfg(feature = "extensions")]
            if state.publishing.get() {
                continue;
            }
            if state.move_delivery_into(destination) {
                state.delivery_ack.set(true);
                state.release_service_claim();
            }
        }
    }
}
#[cfg(feature = "sys")]
impl RetirementFailure {
    fn move_into(&self, destination: &Self) -> bool {
        if self.has_unstamped_event() || destination.has_unstamped_event() {
            return false;
        }
        while let Some(event) = self.take_event() {
            if let Err(event) = destination.accept_event(event) {
                // Same carrier restoration; no stamp or identity is changed.
                let restored = self.accept_event(event);
                assert!(
                    restored.is_ok(),
                    "ordered retirement event lost its receiver"
                );
                return false;
            }
            if self.has_unstamped_event() {
                return false;
            }
        }
        true
    }
}
#[cfg(feature = "sys")]
impl NativeStoreFinalization {
    #[allow(
        clippy::too_many_arguments,
        reason = "Keep each actual resource custody and its independent metadata fence explicit at finalization transfer"
    )]
    fn new(
        mut generation: wasmer_api::sys::NativeInvocationScope,
        store: Option<wasmer_api::StoreAsync>,
        driver: Option<Rc<Driver>>,
        ledger: Rc<RetirementLedger>,
        scopes: Rc<DriverScopes>,
        seal: wasmer_api::sys::NativeCleanupSeal,
        reservation: wasmer_api::sys::NativeStoreDestructionReservation,
        owner: wasmer_api::sys::NativeCleanupOwner,
        failure: RetirementFailure,
        failures_receiver: wasmer_api::sys::NativeCleanupFailures,
    ) -> Self {
        let runtime = driver.as_ref().map(|driver| driver.runtime.clone());
        let destruction = generation.store_destruction();
        let destruction_observer = Some(destruction.observer());
        let observe_failure = failure.clone();
        let quiescence = Box::pin(async move {
            use wasmer_api::AsStoreAsync;
            // These two observers are canonical and may be recreated after a
            // notification panic. No arbitrary consumed async frame is replayed.
            loop {
                if catch_retirement_future(
                    generation.close_and_wait_for_cleanup(),
                    &observe_failure,
                )
                .await
                .is_ok()
                {
                    break;
                }
            }
            if let Some(store) = &store {
                loop {
                    if catch_retirement_future(
                        async { drop(store.write_lock().await) },
                        &observe_failure,
                    )
                    .await
                    .is_ok()
                    {
                        break;
                    }
                }
            }
            observe_failure.attempt(|| drop(generation));
            store
        });
        Self {
            phase: StoreFinalizationPhase::Quiescence,
            quiescence: Some(quiescence),
            store: None,
            destruction,
            destruction_observer,
            driver,
            runtime,
            ledger,
            scopes,
            seal,
            reservation: Some(reservation),
            owner,
            failure,
            _failures_receiver: failures_receiver,
            poisoned: false,
        }
    }
    fn drain(&mut self, cx: &mut std::task::Context<'_>) -> bool {
        use std::future::Future;
        let _exit = self.owner.enter_exit_boundary();
        let _entered = self.driver.as_ref().map(|driver| driver.entered());
        let _runtime = self.runtime.as_ref().map(|runtime| runtime.enter());
        if let Some(driver) = &self.driver
            && !driver.active.get()
        {
            driver.active.set(true);
            struct Reset<'a>(&'a std::cell::Cell<bool>);
            impl Drop for Reset<'_> {
                fn drop(&mut self) {
                    self.0.set(false);
                }
            }
            let _reset = Reset(&driver.active);
            let local = driver.local.run_until(std::future::pending::<()>());
            let _ = std::pin::pin!(local).as_mut().poll(cx);
        }
        let _local = self.driver.as_ref().map(|driver| driver.local.enter());
        // This SAME receiver continues after Driver extraction. Only explicitly
        // cancelled/relinquished invocations may contribute to cleanup delivery.
        self.ledger.collect_into(&self.failure);
        let idle = self.owner.poll_quiescent(cx).is_ready();
        // notify can publish/retire exact receipts; collect before the next pass.
        self.ledger.collect_into(&self.failure);
        idle && self.ledger.resources_ready() && !self.failure.has_unstamped_event()
    }
    fn poll(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        use std::{future::Future, task::Poll};
        if self.poisoned {
            return Poll::Pending;
        }
        loop {
            match self.phase {
                StoreFinalizationPhase::Quiescence => {
                    let poll = {
                        let _exit = self.owner.enter_exit_boundary();
                        let _runtime = self.runtime.as_ref().map(|runtime| runtime.enter());
                        self.quiescence.as_mut().unwrap().as_mut().poll(cx)
                    };
                    let Poll::Ready(store) = poll else {
                        return Poll::Pending;
                    };
                    self.store = store;
                    self.phase = StoreFinalizationPhase::Drain;
                    self.failure.attempt(|| drop(self.quiescence.take()));
                }
                StoreFinalizationPhase::Drain => {
                    if !self.drain(cx) {
                        return Poll::Pending;
                    }
                    if self.scopes.entered.get() != 0 || self.scopes.users.get() != 0 {
                        if let Some(handoff) = self.scopes.finalization.borrow().upgrade() {
                            handoff.blocked.set(true);
                        }
                        // Register/check closes synchronous release reentry.
                        if self.scopes.entered.get() != 0 || self.scopes.users.get() != 0 {
                            return Poll::Pending;
                        }
                    }
                    self.phase = StoreFinalizationPhase::Local;
                }
                StoreFinalizationPhase::Local => {
                    if let Some(driver) = self.driver.take() {
                        let driver = match Rc::try_unwrap(driver) {
                            Ok(driver) => driver,
                            Err(driver) => {
                                self.driver = Some(driver);
                                self.poisoned = true;
                                self.failure
                                    .record(Box::new("native Driver survived real receipt fences"));
                                return Poll::Pending; // invariant custody only, never a completion proof
                            }
                        };
                        let Driver { local, .. } = driver;
                        let _exit = self.owner.enter_exit_boundary();
                        let _runtime = self.runtime.as_ref().map(|runtime| runtime.enter());
                        self.failure.attempt(|| drop(local));
                    }
                    self.phase = StoreFinalizationPhase::LocalDrain;
                }
                StoreFinalizationPhase::LocalDrain => {
                    if !self.drain(cx) {
                        return Poll::Pending;
                    }
                    self.phase = StoreFinalizationPhase::Store;
                }
                StoreFinalizationPhase::Store => {
                    self.phase = StoreFinalizationPhase::StoreDestruction;
                    let _exit = self.owner.enter_exit_boundary();
                    let _runtime = self.runtime.as_ref().map(|runtime| runtime.enter());
                    self.failure.attempt(|| drop(self.store.take()));
                }
                StoreFinalizationPhase::StoreDestruction => {
                    if self.runtime.is_none() && !self.destruction.notifications_complete() {
                        return Poll::Pending;
                    }
                    let _exit = self.owner.enter_exit_boundary();
                    let _runtime = self.runtime.as_ref().map(|runtime| runtime.enter());
                    let poll = self.destruction_observer.as_mut().unwrap();
                    let ready = self.failure.attempt(|| std::pin::Pin::new(poll).poll(cx));
                    if !matches!(ready, Some(Poll::Ready(()))) {
                        return Poll::Pending;
                    }
                    assert!(self.destruction.is_complete());
                    self.phase = StoreFinalizationPhase::StoreDrain;
                    self.failure
                        .attempt(|| drop(self.destruction_observer.take()));
                }
                StoreFinalizationPhase::StoreDrain => {
                    if !self.drain(cx) {
                        return Poll::Pending;
                    }
                    self.phase = StoreFinalizationPhase::Observers;
                }
                StoreFinalizationPhase::Observers => {
                    self.phase = StoreFinalizationPhase::ObserverDrain;
                    let _exit = self.owner.enter_exit_boundary();
                    let _runtime = self.runtime.as_ref().map(|runtime| runtime.enter());
                    self.failure
                        .attempt(|| self.owner.dispose_resource_observers());
                }
                StoreFinalizationPhase::ObserverDrain => {
                    if !self.drain(cx) {
                        return Poll::Pending;
                    }
                    self.phase = StoreFinalizationPhase::Seal;
                }
                StoreFinalizationPhase::Seal => {
                    // All resource/receipt callbacks and actual VM field Drop
                    // have returned. The check/commit invokes no user callback.
                    if !self.destruction.is_complete()
                        || !self.ledger.resources_ready()
                        || !self.owner.is_quiescent()
                    {
                        self.phase = StoreFinalizationPhase::StoreDrain;
                        continue;
                    }
                    self.failure.attempt(|| self.owner.clear_idle_waker());
                    if !self.seal.try_seal() {
                        self.phase = StoreFinalizationPhase::Observers;
                        continue;
                    }
                    self.phase = StoreFinalizationPhase::Runtime;
                }
                StoreFinalizationPhase::Runtime => {
                    // No callback-bearing owner-thread disposal between valid
                    // seal and final_done. Hold a shutdown panic until Done,
                    // since disposing a secondary payload may cancel a caller.
                    let shutdown_unwinding = std::thread::panicking();
                    let shutdown = self
                        .runtime
                        .take()
                        .and_then(|runtime| std::thread::spawn(move || drop(runtime)).join().err());
                    self.phase = StoreFinalizationPhase::Done;
                    self.ledger.final_done.set(true);
                    if let Some(panic) = shutdown {
                        self.failure.record_caught(panic, shutdown_unwinding);
                    }
                }
                StoreFinalizationPhase::Done => return Poll::Ready(()),
            }
        }
    }
    fn finish_blocking(self) -> Option<RetirementPanic> {
        self.finish_at_boundary(false)
    }
    fn finish_no_driver(self) -> Option<RetirementPanic> {
        self.finish_at_boundary(true)
    }
    fn finish_at_boundary(mut self, park_external: bool) -> Option<RetirementPanic> {
        use std::{
            sync::{Arc, Condvar, Mutex},
            task::{Context, Wake, Waker},
        };
        #[derive(Default)]
        struct Signal(Mutex<bool>, Condvar);
        impl Wake for Signal {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                *self.0.lock().unwrap_or_else(|e| e.into_inner()) = true;
                self.1.notify_one();
            }
        }
        let signal = Arc::new(Signal::default());
        let wake = Waker::from(signal.clone());
        let mut cx = Context::from_waker(&wake);
        loop {
            *signal.0.lock().unwrap_or_else(|e| e.into_inner()) = false;
            let failure = self.failure.clone();
            match failure.attempt(|| self.poll(&mut cx)) {
                Some(std::task::Poll::Ready(())) => {
                    assert!(matches!(self.phase, StoreFinalizationPhase::Done));
                    assert!(self.ledger.final_done.get());
                    // Return the exact terminal payload to the owning lexical
                    // panic boundary. None must mean EMPTY, never unstamped.
                    return self.failure.take_terminal_payload();
                }
                None => continue,
                Some(std::task::Poll::Pending) => (),
            }
            if park_external && matches!(self.phase, StoreFinalizationPhase::StoreDestruction) {
                assert!(
                    self.store.is_none()
                        && self.driver.is_none()
                        && self.runtime.is_none()
                        && self.quiescence.is_none()
                );
                self.failure
                    .attempt(|| drop(self.destruction_observer.take()));
                let reservation = self.reservation.take().unwrap();
                let owner = self.owner.clone();
                let resume = move |notification_first| {
                    let exit = owner.enter_exit_boundary(); // BEFORE moving/polling phase
                    let entry_unwinding = std::thread::panicking();
                    let mut actual = self;
                    assert!(
                        actual.destruction.is_complete()
                            && actual.destruction.notifications_complete()
                    );
                    actual.phase = StoreFinalizationPhase::StoreDrain;
                    if let Some(first) = notification_first {
                        actual.failure.record_caught(first, entry_unwinding);
                    }
                    exit.defer(Box::new(move || actual.finish_blocking()));
                    None
                };
                if let Err(resume) = reservation.install(resume) {
                    // Exclusive reservation refuses ONLY actual completion;
                    // exact closure remains owned and no Occupied path exists.
                    return resume(None);
                }
                return None; // SAME unfinished phase is actually guard-owned
            }
            let mut ready = signal.0.lock().unwrap_or_else(|e| e.into_inner());
            while !*ready {
                ready = signal.1.wait(ready).unwrap_or_else(|e| e.into_inner());
            }
        }
    }
}
#[cfg(feature = "sys")]
struct NativeStorePrelude(Option<NativeStoreFinalization>);
#[cfg(feature = "sys")]
impl std::future::Future for NativeStorePrelude {
    type Output = ();
    fn poll(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        let this = self.get_mut();
        let actual = this.0.as_mut().unwrap();
        let failure = actual.failure.clone();
        let polled = failure.attempt(|| actual.quiescence.as_mut().unwrap().as_mut().poll(cx));
        let store = match polled {
            Some(std::task::Poll::Ready(store)) => store,
            Some(std::task::Poll::Pending) => return std::task::Poll::Pending,
            None => {
                failure.attempt(|| cx.waker().wake_by_ref());
                return std::task::Poll::Pending;
            }
        };
        actual.store = store;
        actual.phase = StoreFinalizationPhase::Drain;
        failure.attempt(|| drop(actual.quiescence.take()));
        let actual = this.0.take().unwrap();
        let owner = actual.owner.clone();
        owner.clone().after_pump_existing(move || {
            let exit = owner.enter_exit_boundary();
            exit.defer(Box::new(move || actual.finish_no_driver()));
        });
        std::task::Poll::Ready(())
    }
}
#[cfg(feature = "sys")]
impl Drop for NativeStorePrelude {
    fn drop(&mut self) {
        if let Some(actual) = self.0.take() {
            // A callback panic/cancellation never implicitly drops unfinished
            // physical custody. Existing notify's outer boundary owns it.
            let exit = actual.owner.enter_exit_boundary();
            exit.defer(Box::new(move || actual.finish_no_driver()));
        }
    }
}

#[cfg(feature = "sys")]
impl Drop for ManagedStore {
    fn drop(&mut self) {
        let Some(generation) = self.generation.take() else {
            return;
        };
        self.ledger.finalization_started.set(true); // BEFORE any callback/publication
        let _exit = self.owner.enter_exit_boundary();
        let handoff = self.finalization.get_mut().upgrade();
        let failure = self.cleanup_failure.as_ref().clone();
        let no_driver = self.driver.get_mut().is_none();
        let actual = NativeStoreFinalization::new(
            generation,
            self.store.take(),
            self.driver.get_mut().take(),
            self.ledger.clone(),
            self.scopes.clone(),
            self.seal.take().unwrap(),
            self.reservation.take().unwrap(),
            self.owner.clone(),
            failure,
            self.failures_receiver.clone(),
        );
        if no_driver {
            // Preserve original queued generation/write-unlock producer. Only
            // this noDriver prelude queues phase custody, never a Driver owner.
            assert!(self.ledger.snapshot().is_empty());
            self.owner
                .transfer_existing(NativeStorePrelude(Some(actual)));
        } else if let Some(handoff) = handoff {
            assert!(!handoff.complete.get());
            assert!(handoff.pending.borrow().is_none());
            *handoff.pending.borrow_mut() = Some(actual);
            handoff.notify();
        } else {
            let wake = actual.driver.as_ref().unwrap().wake.clone();
            let handoff = NativeStoreFinalizationHandoff::new(self, wake);
            *handoff.pending.borrow_mut() = Some(actual);
            *self.finalization.get_mut() = Rc::downgrade(&handoff);
            *self.scopes.finalization.borrow_mut() = Rc::downgrade(&handoff);
            NativeStoreFinalizationTail::enroll(handoff);
        }
    }
}

#[derive(Clone)]
pub struct WeakStoreRef {
    inner: Weak<StoreState>,
}
// SAFETY: C API Stores and these callbacks are confined to their construction thread.
unsafe impl Send for WeakStoreRef {}
unsafe impl Sync for WeakStoreRef {}
impl WeakStoreRef {
    pub fn upgrade(&self) -> Option<StoreRef> {
        self.inner.upgrade().map(|inner| StoreRef { inner })
    }
}

/// Opaque type representing a WebAssembly store.
#[allow(non_camel_case_types)]
pub struct wasm_store_t {
    pub inner: StoreRef,
}

/// Creates a new WebAssembly store given a specific [engine][super::engine].
///
/// # Example
///
/// See the module's documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_store_new(
    engine: Option<&wasm_engine_t>,
) -> Option<Box<wasm_store_t>> {
    let engine = engine?;
    let store = Store::new(engine.inner.clone());

    Some(Box::new(wasm_store_t {
        inner: c_try!(StoreRef::new(store)),
    }))
}

/// Deletes a WebAssembly store.
///
/// # Example
///
/// See the module's documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_store_delete(_store: Option<Box<wasm_store_t>>) {}

#[cfg(all(test, feature = "sys"))]
mod tests {
    use super::*;
    use crate::wasm_c_api::{
        externals::{
            wasm_extern_t, wasm_extern_vec_t, wasm_func_call, wasm_func_new_with_env, wasm_func_t,
        },
        instance::wasm_instance_new,
        module::wasm_module_t,
        trap::wasm_trap_t,
        types::wasm_functype_t,
        value::{wasm_val_t, wasm_val_vec_t},
    };

    #[cfg(feature = "extensions")]
    #[cfg(feature = "extensions")]
    #[test]
    fn retirement_clock_binding_keeps_history_and_exact_event_identity() {
        let store = StoreRef::new(Store::default()).unwrap();
        let failure = store.retirement_failure().unwrap();
        failure.record(Box::new(String::from("original bound value")));
        let event = failure.take_event().unwrap();
        let order = event.order;
        let clock = event.clock.clone();
        // Same-owner binding is legal after record AND take; no restamping.
        failure.bind_clock(&clock).unwrap();
        assert!(failure.bind_clock(&Rc::default()).is_err());
        assert!(failure.accept_event(event).is_ok());
        let exact = failure.take_event().unwrap();
        assert_eq!(exact.order, order);
        assert!(Rc::ptr_eq(&exact.clock, &clock));
        assert_eq!(
            exact.payload.downcast_ref::<String>().map(String::as_str),
            Some("original bound value")
        );
        let unbound = RetirementFailure::default();
        unbound.record(Box::new("unbound value"));
        assert!(unbound.bind_clock(&clock).is_err());
        assert_eq!(
            unbound.take().unwrap().downcast_ref::<&str>(),
            Some(&"unbound value")
        );
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn retirement_ordinal_exhaustion_retains_exact_unstamped_payload_without_ack() {
        struct Payload(std::sync::Arc<std::sync::atomic::AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let store = StoreRef::new(Store::default()).unwrap();
        let registration = store
            .register_owned_retirement(
                Rc::new(std::cell::Cell::new(true)),
                RetirementFailure::default(),
            )
            .unwrap();
        let state = registration.state();
        let clock = state.ledger.clock.clone();
        let earlier = store.retirement_failure().unwrap();
        earlier.record(Box::new("already owned earlier event"));
        let owned = earlier.take_event().unwrap();
        clock.set(u64::MAX);
        let drops = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let payload = Box::new(Payload(drops.clone()));
        let address = (&*payload) as *const Payload;
        state.failure.record(payload);
        assert_eq!(clock.get(), u64::MAX);
        assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert!(!state.delivery_empty());
        assert!(state.has_unstamped_event());
        assert!(state.failure.take_event().is_none());
        let stored = state.failure.0.borrow();
        assert_eq!(
            stored
                .as_ref()
                .unwrap()
                .payload
                .downcast_ref::<Payload>()
                .unwrap() as *const Payload,
            address
        );
        drop(stored);
        let mut claim = None;
        assert!(!state.claim(&mut claim));
        assert!(claim.is_none());
        assert!(!state.delivery_ack.get());
        let owned = state.failure.accept_event(owned).err().unwrap();
        assert_eq!(
            owned.payload.downcast_ref::<&str>(),
            Some(&"already owned earlier event")
        );
        assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 0);
        // The unresolved value stays in its original carrier until real
        // cancellation destroys that carrier; no ordered completion is faked.
        drop(registration);
        drop(state);
        assert_eq!(drops.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn retirement_event_delivery_uses_catch_order_after_later_claims() {
        use std::future::Future;
        for close_first in [false, true] {
            let store = StoreRef::new(Store::default()).unwrap();
            let invocation = store.retirement_failure().unwrap();
            let cleanup = store.retirement_failure().unwrap();
            if close_first {
                cleanup.record(Box::new("cleanup"));
            }
            let recording = invocation.clone();
            let mut actual = Box::pin(std::future::poll_fn(move |_| {
                recording.record(Box::new("inside actual poll"));
                std::task::Poll::Ready(())
            }));
            assert!(
                actual
                    .as_mut()
                    .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                    .is_ready()
            );
            if !close_first {
                cleanup.record(Box::new("cleanup"));
            }
            // Collector arrival order is deliberately opposite catch order.
            let delivered = store.retirement_failure().unwrap();
            assert!(
                delivered
                    .accept_event(cleanup.take_event().unwrap())
                    .is_ok()
            );
            assert!(
                delivered
                    .accept_event(invocation.take_event().unwrap())
                    .is_ok()
            );
            let value = delivered.take().unwrap();
            assert_eq!(
                value.downcast_ref::<&str>(),
                Some(&if close_first {
                    "cleanup"
                } else {
                    "inside actual poll"
                })
            );
        }
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn completed_registration_cancellation_after_receipt_retirement_has_no_dead_weak() {
        use std::future::Future;
        // Actual LocalContinuation cancellation AFTER its terminal producer and
        // receipt finish, but BEFORE its already-ready result is consumed.
        {
            let store = StoreRef::new(Store::default()).unwrap();
            let owner = store.retirement_owner().unwrap();
            let done = Rc::new(std::cell::Cell::new(false));
            let registration = store
                .register_owned_retirement(done.clone(), RetirementFailure::default())
                .unwrap();
            let state = registration.state();
            let weak = Rc::downgrade(&state);
            let release = store.retirement_release(state.clone()).unwrap();
            store.retain_delivery(state).unwrap();
            let registration = Rc::new(RefCell::new(Some(registration)));
            let run_registration = registration.clone();
            let consumed = Rc::new(std::cell::Cell::new(false));
            let ran = consumed.clone();
            let (send, receive) = tokio::sync::oneshot::channel::<()>();
            let continuation = LocalContinuation {
                store: store.clone(),
                run: Some(Box::new(move || {
                    let registration = run_registration.borrow_mut().take();
                    Box::pin(async move {
                        receive
                            .await
                            .map_err(|_| RuntimeError::new("test terminal disappeared"))?;
                        ran.set(true);
                        registration.unwrap().relinquish();
                        Ok(())
                    }) as ContinuationFuture
                })),
                cancel: Some(Box::new(move || {
                    let owned = registration.borrow_mut().take();
                    drop(owned);
                })),
            };
            owner.retain(Box::pin(async move {
                done.set(true); // real test terminal task has completed its work
                send.send(()).unwrap();
                release.release();
            }));
            assert!(owner.driver.ledger.snapshot().is_empty());
            assert_eq!(
                weak.strong_count(),
                1,
                "only the unconsumed registration owns the state"
            );
            assert!(!consumed.get());
            drop(continuation); // production Drop runs its actual cancel entry
            assert!(weak.upgrade().is_none());
            assert!(
                store.run_host(async {}).is_ok(),
                "next actual Driver snapshot must not see a dead Weak"
            );
            assert!(owner.driver.ledger.snapshot().is_empty());
            assert!(!consumed.get());
        }
        // Analogous live tail: first poll releases its actual receipts while an
        // existing after_pump callback delays publication. Cancel only AFTER
        // publication/empty-receipt retirement and BEFORE its next caller poll.
        {
            let store = StoreRef::new(Store::default()).unwrap();
            let owner = store.retirement_owner().unwrap();
            let done = Rc::new(std::cell::Cell::new(false));
            let registration = store
                .register_owned_retirement(done.clone(), RetirementFailure::default())
                .unwrap();
            let state = registration.state();
            let weak = Rc::downgrade(&state);
            store.retain_delivery(state.clone()).unwrap();
            state.disposition.set(DeliveryDisposition::LiveTail);
            let tail = Rc::new(RefCell::new(Box::pin(NativeRetirementTail {
                subscription: Some(owner.subscribe()),
                _store: store.downgrade(),
                base: Some(store.async_store().unwrap()),
                owner: Some(owner.clone()),
                cleanup: Some(owner.owner.clone()),
                release: Some(store.retirement_release(state.clone()).unwrap()),
                state,
                registration: Some(registration),
                first: store.retirement_failure().unwrap(),
                finalization_subscription: None,
                finalization: store.finalization_handoff(&owner),
                exit: None,
            })));
            let first = tail.clone();
            let terminal_owner = owner.clone();
            owner.owner.after_pump_existing(Box::new(move || {
                terminal_owner.retain(Box::pin(async move {
                    done.set(true);
                }));
                assert!(
                    first
                        .borrow_mut()
                        .as_mut()
                        .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
                        .is_pending()
                );
            }));
            assert!(owner.driver.ledger.snapshot().is_empty());
            assert_eq!(
                weak.strong_count(),
                2,
                "the released tail owns its state and registration"
            );
            drop(tail);
            assert!(weak.upgrade().is_none());
            assert!(
                store.run_host(async {}).is_ok(),
                "released-tail cancellation must not resurrect a dead ledger row"
            );
            assert!(owner.driver.ledger.snapshot().is_empty());
        }
    }

    #[cfg(feature = "extensions")]
    fn terminal_cleanup_exhaustion_at_actual_done(blocking: bool) {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Payload(Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let store = StoreRef::new(Store::default()).unwrap();
        let owner = store.retirement_owner().unwrap();
        let handoff = store.finalization_handoff(&owner);
        let failure = handoff.failure.clone();
        let ledger = handoff.ledger.clone();
        drop(owner); // actual Driver receipt disappears BEFORE public Store
        drop(store);
        let drops = Arc::new(AtomicUsize::new(0));
        let payload = Box::new(Payload(drops.clone()));
        let address = (&*payload) as *const Payload;
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let terminal = if blocking {
            // Drive the SAME actual resources to Done before injecting the
            // shutdown-error exhaustion profile; no physical bits are faked.
            let mut actual = handoff.pending.borrow_mut().take().unwrap();
            assert!(actual.poll(&mut cx).is_ready());
            assert!(matches!(actual.phase, StoreFinalizationPhase::Done));
            assert!(actual.destruction.is_complete());
            assert!(ledger.final_done.get());
            ledger.clock.set(u64::MAX);
            failure.record(payload);
            assert!(failure.take_event().is_none());
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            let exact = actual
                .finish_blocking()
                .expect("terminal error is not empty");
            // The shared handoff is only empty metadata after exact transfer.
            drop(handoff);
            exact
        } else {
            assert!(handoff.poll(&mut cx).is_ready());
            assert!(handoff.complete.get());
            assert!(ledger.final_done.get());
            assert!(handoff.pending.borrow().is_none());
            ledger.clock.set(u64::MAX);
            failure.record(payload);
            assert!(failure.take_event().is_none());
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(handoff)))
                .expect_err("LAST completed handoff must deliver its exact terminal error")
        };
        assert_eq!(
            terminal.downcast_ref::<Payload>().unwrap() as *const _,
            address
        );
        assert!(failure.is_empty());
        assert!(
            !failure.1.get(),
            "exceptional delivery is not an ordered acknowledgement"
        );
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(terminal);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn last_completed_handoff_delivers_exact_unstamped_cleanup_payload() {
        terminal_cleanup_exhaustion_at_actual_done(false);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn blocking_actual_done_returns_exact_unstamped_cleanup_payload() {
        terminal_cleanup_exhaustion_at_actual_done(true);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn cancelled_tail_returns_its_ordered_event_to_the_same_already_queued_unstamped_receipt() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Payload(Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let store = StoreRef::new(Store::default()).unwrap();
        let owner = store.retirement_owner().unwrap();
        let registration = store
            .register_owned_retirement(
                Rc::new(std::cell::Cell::new(true)),
                RetirementFailure::default(),
            )
            .unwrap();
        registration.commit();
        let state = registration.state();
        state.disposition.set(DeliveryDisposition::LiveTail);
        let first = store.retirement_failure().unwrap();
        let drops = Arc::new(AtomicUsize::new(0));
        let ordered = Box::new(Payload(drops.clone()));
        let ordered_address = (&*ordered) as *const Payload;
        first.record(ordered);
        state.ledger.clock.set(u64::MAX);
        let unstamped = Box::new(Payload(drops.clone()));
        let unstamped_address = (&*unstamped) as *const Payload;
        state.failure.record(unstamped);
        store.retain_delivery(state.clone()).unwrap();
        assert!(state.receipt_queued.get());
        let ledger = state.ledger.clone();
        let tail = NativeRetirementTail {
            subscription: Some(owner.subscribe()),
            _store: store.downgrade(),
            base: Some(store.async_store().unwrap()),
            owner: Some(owner.clone()),
            cleanup: Some(owner.owner.clone()),
            release: Some(store.retirement_release(state.clone()).unwrap()),
            state: state.clone(),
            registration: Some(registration),
            first,
            finalization_subscription: None,
            finalization: store.finalization_handoff(&owner),
            exit: None,
        };
        drop(tail); // actual tail Drop returns BEFORE its callback-bearing fields
        assert!(state.physically_ready());
        assert!(
            state.receipt_queued.get(),
            "retain the SAME existing receipt"
        );
        assert!(!state.delivery_ack.get());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let queued = ledger.snapshot();
        assert_eq!(queued.len(), 1);
        assert!(Rc::ptr_eq(&queued[0], &state));
        let returned_guard = state.returned.borrow();
        let returned = returned_guard.as_ref().unwrap();
        assert!(returned.order.is_some());
        assert_eq!(
            returned.payload.downcast_ref::<Payload>().unwrap() as *const _,
            ordered_address
        );
        let unresolved_guard = state.failure.0.borrow();
        let unresolved = unresolved_guard.as_ref().unwrap();
        assert!(unresolved.order.is_none());
        assert_eq!(
            unresolved.payload.downcast_ref::<Payload>().unwrap() as *const _,
            unstamped_address
        );
        drop(unresolved_guard);
        drop(returned_guard);
        let destination = store.retirement_failure().unwrap();
        ledger.collect_into(&destination);
        assert!(destination.is_empty());
        assert!(!state.delivery_ack.get());
        owner.owner.notify();
        assert!(state.receipt_queued.get());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        // This primitive exhaustion control has no legal ordered completion.
        // Explicitly dispose its two owned test values only after proving custody.
        let unresolved = state.failure.0.borrow_mut().take();
        let returned = state.returned.borrow_mut().take();
        drop(unresolved);
        drop(returned);
        owner.owner.notify();
        assert!(state.delivery_ack.get());
        assert!(!state.receipt_queued.get());
        assert_eq!(drops.load(Ordering::SeqCst), 2);
        assert!(ledger.snapshot().is_empty());
        drop(queued);
        drop(owner);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn retirement_registration_rolls_back_only_uncommitted_metadata() {
        let store = StoreRef::new(Store::default()).unwrap();
        let registration = store
            .register_owned_retirement(
                Rc::new(std::cell::Cell::new(false)),
                RetirementFailure::default(),
            )
            .unwrap();
        let state = registration.state();
        let ledger = state.ledger.clone();
        drop(registration);
        assert!(ledger.snapshot().is_empty());
        assert!(!state.terminal_done.get());
        assert!(!state.tail_receipt_released.get());
        let registration = store
            .register_owned_retirement(
                Rc::new(std::cell::Cell::new(false)),
                RetirementFailure::default(),
            )
            .unwrap();
        let state = registration.state();
        registration.commit();
        state
            .failure
            .record(Box::new("committed setup cancellation"));
        let original_order = state.failure.0.borrow().as_ref().unwrap().order;
        drop(registration);
        let retained = ledger.snapshot();
        assert_eq!(retained.len(), 1);
        assert!(Rc::ptr_eq(&retained[0], &state));
        assert!(state.disposition.get() == DeliveryDisposition::Cancelled);
        assert_eq!(
            state.failure.0.borrow().as_ref().unwrap().order,
            original_order
        );
        assert!(!state.terminal_done.get());
        assert!(!state.tail_receipt_released.get());
        // No real terminal was built in this primitive control; remove its
        // metadata explicitly rather than inventing physical completion bits.
        ledger.remove(&state);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn endpoint_reuse_prunes_dead_controls_and_preserves_live_matching_outside_registry_borrows() {
        struct Endpoint(Rc<std::cell::Cell<usize>>);
        impl ContinuationEndpoint for Endpoint {
            fn matches(&self, _: &mut StoreMut<'_>, _: &RuntimeError) -> bool {
                self.0.set(self.0.get() + 1);
                false
            }
            fn prepare(
                self: Rc<Self>,
                _: &StoreRef,
                _: &RuntimeError,
            ) -> Result<LocalContinuation, RuntimeError> {
                unreachable!()
            }
            fn cancel_unclaimed(
                self: Rc<Self>,
                _: &StoreRef,
                _: &RuntimeError,
            ) -> Result<(), RuntimeError> {
                unreachable!()
            }
        }
        let store = StoreRef::new(Store::default()).unwrap();
        let calls = Rc::new(std::cell::Cell::new(0));
        let live: Rc<dyn ContinuationEndpoint> = Rc::new(Endpoint(calls.clone()));
        store.register_endpoint(Rc::downgrade(&live)).unwrap();
        for _ in 0..1000 {
            let endpoint: Rc<dyn ContinuationEndpoint> = Rc::new(Endpoint(calls.clone()));
            store.register_endpoint(Rc::downgrade(&endpoint)).unwrap();
            drop(endpoint);
            let StoreMode::Native(native) = &store.inner.mode else {
                unreachable!()
            };
            assert!(
                native.endpoints.registered.borrow().len() <= 2,
                "history must stay bounded"
            );
        }
        store
            .forward_handoff(&RuntimeError::new("ordinary guest trap"), None)
            .unwrap();
        assert_eq!(calls.get(), 1, "snapshot retains exactly the live endpoint");
        let StoreMode::Native(native) = &store.inner.mode else {
            unreachable!()
        };
        assert_eq!(native.endpoints.registered.borrow().len(), 1);
        drop(live);
        store
            .forward_handoff(&RuntimeError::new("ordinary guest trap"), None)
            .unwrap();
        assert!(native.endpoints.registered.borrow().is_empty());
    }

    #[cfg(feature = "extensions")]
    struct ParkedBareStore {
        owner: wasmer_api::sys::NativeCleanupOwner,
        failure: RetirementFailure,
        ledger: Rc<RetirementLedger>,
        backing: Option<wasmer_api::StoreAsync>,
    }

    #[cfg(feature = "extensions")]
    fn parked_bare_store(first: Option<RetirementPanic>) -> ParkedBareStore {
        let store = StoreRef::new(Store::default()).unwrap();
        let backing = store.async_store().unwrap();
        let weak = store.downgrade();
        let StoreMode::Native(native) = &store.inner.mode else {
            unreachable!()
        };
        assert!(native.driver.borrow().is_none());
        let owner = native.owner.clone();
        let failure = native.cleanup_failure.as_ref().clone();
        let ledger = native.ledger.clone();
        let exit = owner.enter_exit_boundary();
        if let Some(first) = first {
            owner
                .try_retain(async move { std::panic::resume_unwind(first) })
                .unwrap_or_else(|_| panic!("prelude error job refused"));
        }
        drop(store);
        drop(exit); // same phase runs outside real prelude/pump/context guards
        assert!(weak.upgrade().is_none());
        assert!(
            owner.is_idle(),
            "external backing must not keep the owner pump busy"
        );
        assert!(ledger.finalization_started.get());
        assert!(
            !ledger.final_done.get(),
            "last external backing has not actually dropped"
        );
        ParkedBareStore {
            owner,
            failure,
            ledger,
            backing: Some(backing),
        }
    }

    #[cfg(feature = "extensions")]
    fn no_driver_same_pump_error_order(first_i: bool, field_drop: bool) {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Payload(&'static str, Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.1.fetch_add(1, Ordering::SeqCst);
            }
        }
        struct TaskFields {
            owner: wasmer_api::sys::NativeCleanupOwner,
            ledger: Rc<RetirementLedger>,
            dropped: Rc<std::cell::Cell<bool>>,
            admitted_done: Rc<std::cell::Cell<bool>>,
        }
        impl Drop for TaskFields {
            fn drop(&mut self) {
                assert!(
                    !self.owner.is_idle(),
                    "actual task field Drop still owns running guard"
                );
                assert!(
                    !self.ledger.final_done.get(),
                    "phase must wait for actual task fields"
                );
                self.dropped.set(true);
                let ledger = self.ledger.clone();
                let done = self.admitted_done.clone();
                self.owner
                    .try_retain(async move {
                        assert!(
                            !ledger.final_done.get(),
                            "admitted finite work precedes phase completion"
                        );
                        done.set(true);
                    })
                    .unwrap_or_else(|_| panic!("actual task destructor work refused before seal"));
            }
        }
        struct TaskTwo {
            error: Option<RetirementPanic>,
            direct: bool,
            external: Option<wasmer_api::StoreAsync>, // actual field Drop fallback
            fields: TaskFields, // LAST: actual external destructor already returned
        }
        impl std::future::Future for TaskTwo {
            type Output = ();
            fn poll(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<()> {
                let this = self.get_mut();
                if let Some(error) = this.error.take() {
                    std::panic::resume_unwind(error);
                }
                if this.direct {
                    drop(this.external.take());
                    assert!(
                        !this.fields.ledger.final_done.get(),
                        "guard F must not self-join running task"
                    );
                }
                std::task::Poll::Ready(())
            }
        }
        let i_drops = Arc::new(AtomicUsize::new(0));
        let q_drops = Arc::new(AtomicUsize::new(0));
        let i = Box::new(Payload("I", i_drops.clone()));
        let q = Box::new(Payload("Q", q_drops.clone()));
        let expected_address = if first_i {
            (&*i) as *const Payload
        } else {
            (&*q) as *const Payload
        };
        let mut later_i: Option<RetirementPanic> = Some(i);
        let mut parked = parked_bare_store(if first_i { later_i.take() } else { None });
        let weak_ledger = Rc::downgrade(&parked.ledger);
        let weak_carrier = Rc::downgrade(&parked.failure.0);
        let dropped = Rc::new(std::cell::Cell::new(false));
        let admitted_done = Rc::new(std::cell::Cell::new(false));
        let task2 = TaskTwo {
            error: later_i,
            direct: !field_drop,
            external: parked.backing.take(),
            fields: TaskFields {
                owner: parked.owner.clone(),
                ledger: parked.ledger.clone(),
                dropped: dropped.clone(),
                admitted_done: admitted_done.clone(),
            },
        };
        let schedule = parked.owner.clone();
        let exact = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            parked
                .owner
                .try_retain(async move {
                    schedule
                        .try_retain(async move { std::panic::resume_unwind(q) })
                        .unwrap_or_else(|_| panic!("same-pump task1 refused"));
                    schedule
                        .try_retain(task2)
                        .unwrap_or_else(|_| panic!("same-pump task2 refused"));
                })
                .unwrap_or_else(|_| panic!("original owner task refused"));
        }))
        .expect_err("actual completed phase must deliver its first cleanup value");
        let payload = exact.downcast_ref::<Payload>().unwrap();
        assert_eq!(payload as *const _, expected_address);
        assert_eq!(payload.0, if first_i { "I" } else { "Q" });
        assert!(dropped.get() && admitted_done.get());
        assert!(parked.owner.is_idle() && parked.ledger.final_done.get());
        assert!(parked.failure.is_empty());
        assert_eq!(i_drops.load(Ordering::SeqCst), usize::from(!first_i));
        assert_eq!(q_drops.load(Ordering::SeqCst), usize::from(first_i));
        drop(exact);
        assert_eq!(i_drops.load(Ordering::SeqCst), 1);
        assert_eq!(q_drops.load(Ordering::SeqCst), 1);
        drop(parked);
        assert!(
            weak_ledger.upgrade().is_none(),
            "no owner/guard/receiver metadata cycle"
        );
        assert!(weak_carrier.upgrade().is_none());
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn no_driver_last_backing_inside_owner_task_preserves_earlier_phase_error() {
        no_driver_same_pump_error_order(true, false);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn no_driver_last_backing_in_task_field_drop_preserves_earlier_phase_error() {
        no_driver_same_pump_error_order(true, true);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn no_driver_same_pump_raw_error_precedes_later_phase_error_and_last_backing_drop() {
        no_driver_same_pump_error_order(false, true);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn no_driver_external_backing_returns_before_drop_and_preserves_active_rust_primary() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Secondary(Arc<AtomicUsize>);
        impl Drop for Secondary {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let mut parked = parked_bare_store(Some(Box::new(Secondary(drops.clone()))));
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let external = parked.backing.take().unwrap();
        let primary = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _actual_last_backing = external;
            std::panic::panic_any("original active Rust primary");
        }))
        .unwrap_err();
        assert_eq!(
            primary.downcast_ref::<&str>(),
            Some(&"original active Rust primary")
        );
        assert!(parked.ledger.final_done.get() && parked.owner.is_idle());
        assert!(parked.failure.is_empty());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn actual_retained_future_drop_preserves_g_before_receipt_release_wake_n() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct WakeN(AtomicUsize);
        impl std::task::Wake for WakeN {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::SeqCst);
                std::panic::panic_any("secondary release wake N");
            }
        }
        struct DropG;
        impl std::future::Future for DropG {
            type Output = ();
            fn poll(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<()> {
                std::task::Poll::Ready(())
            }
        }
        impl Drop for DropG {
            fn drop(&mut self) {
                std::panic::panic_any("actual future Drop G");
            }
        }
        let store = StoreRef::new(Store::default()).unwrap();
        let receipt = store.retirement_owner().unwrap();
        let handoff = store.finalization_handoff(&receipt);
        let failure = handoff.failure.clone();
        let cleanup = receipt.owner.clone();
        drop(store);
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(handoff.poll(&mut cx).is_pending());
        assert!(
            handoff.blocked.get(),
            "real phase is waiting for actual Driver receipt"
        );
        let wake = Arc::new(WakeN(AtomicUsize::new(0)));
        let waker = std::task::Waker::from(wake.clone());
        let subscription = receipt.subscribe();
        subscription.with_context(&mut std::task::Context::from_waker(&waker), |_| ());
        receipt.retain_receipt(Box::pin(DropG));
        assert_eq!(wake.0.load(Ordering::SeqCst), 1);
        assert_eq!(
            failure.inspect_message().as_deref(),
            Some("actual future Drop G")
        );
        assert_eq!(
            failure.0.borrow().as_ref().unwrap().order,
            Some(1),
            "N must not stamp before outer G catch"
        );
        drop(subscription);
        let exact =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handoff.poll(&mut cx)))
                .unwrap_err();
        assert_eq!(exact.downcast_ref::<&str>(), Some(&"actual future Drop G"));
        assert!(handoff.complete.get() && cleanup.is_idle());
    }

    #[cfg(feature = "extensions")]
    fn last_no_driver_receiver_exhaustion(active_primary: bool) {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Payload(Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let mut parked = parked_bare_store(None);
        let children = Rc::new(RefCell::new(None));
        let child_out = children.clone();
        let owner = parked.owner.clone();
        let ledger = parked.ledger.clone();
        let drops = Arc::new(AtomicUsize::new(0));
        let payload = Box::new(Payload(drops.clone()));
        let address = (&*payload) as *const Payload;
        let exit = parked.owner.enter_exit_boundary();
        drop(parked.backing.take()); // exact phase becomes FIRST deferred action
        exit.defer(Box::new(move || {
            assert!(
                ledger.final_done.get(),
                "late action follows actual phase completion"
            );
            *child_out.borrow_mut() = Some(owner.enter_exit_boundary());
            ledger.clock.set(u64::MAX);
            std::panic::resume_unwind(payload)
        }));
        drop(exit); // actual child pins the root/receiver with unstamped Q
        assert!(parked.ledger.final_done.get());
        assert!(parked.failure.has_unstamped_event());
        assert!(parked.failure.take_event().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        let child = children.borrow_mut().take().unwrap();
        let exact = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if active_primary {
                let _last_actual_child = child;
                std::panic::panic_any("active primary across LAST receiver");
            } else {
                drop(child);
            }
        }))
        .expect_err("LAST receiver is an explicit terminal Rust boundary");
        if active_primary {
            assert_eq!(
                exact.downcast_ref::<&str>(),
                Some(&"active primary across LAST receiver")
            );
            assert_eq!(drops.load(Ordering::SeqCst), 1);
        } else {
            assert_eq!(
                exact.downcast_ref::<Payload>().unwrap() as *const _,
                address
            );
            assert_eq!(drops.load(Ordering::SeqCst), 0);
        }
        assert!(parked.failure.is_empty());
        assert!(
            !parked.failure.1.get(),
            "terminal transport is no ordered acknowledgement"
        );
        drop(exact);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn last_no_driver_lexical_receiver_delivers_late_unstamped_payload() {
        last_no_driver_receiver_exhaustion(false);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn last_no_driver_lexical_receiver_preserves_active_rust_primary() {
        last_no_driver_receiver_exhaustion(true);
    }

    #[test]
    fn store_constructor_rollback_keeps_unpublished_failure_receiver_empty() {
        let mut store = Store::default();
        let original = wasmer_api::sys::NativeInvocationScope::bind(&mut store).unwrap();
        let destruction = original.store_destruction();
        assert!(StoreRef::new(store).is_err());
        assert!(
            destruction.is_complete(),
            "failed constructor releases its actual backing"
        );
        assert!(destruction.notifications_complete());
        drop(original);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn finite_secondary_payload_chain_reenters_source_without_replacing_owned_or_active_primary() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        struct Reenter {
            owner: wasmer_api::sys::NativeCleanupOwner,
            jobs: Rc<std::cell::Cell<usize>>,
            consume: std::cell::Cell<bool>,
            state: Option<Rc<NativeRetirementState>>,
            claimed: Rc<RefCell<Option<RetirementPanic>>>,
        }
        unsafe extern "C" fn admit(
            data: *mut std::ffi::c_void,
            _: &wasm_val_vec_t,
            _: &mut wasm_val_vec_t,
        ) -> Option<Box<wasm_trap_t>> {
            let control = unsafe { &*data.cast::<Reenter>() };
            if control.consume.replace(false) {
                let state = control.state.as_ref().unwrap();
                assert!(state.physically_ready());
                let mut owned = None;
                assert!(state.claim(&mut owned)); // OWN exact I BEFORE real ack
                *control.claimed.borrow_mut() = owned;
                assert!(state.delivery_ack.get());
            }
            let jobs = control.jobs.clone();
            control
                .owner
                .try_retain(async move {
                    jobs.set(jobs.get() + 1);
                })
                .unwrap_or_else(|_| panic!("pre-seal secondary destructor work refused"));
            None
        }
        unsafe extern "C" fn release(data: *mut std::ffi::c_void) {
            drop(unsafe { Box::from_raw(data.cast::<Reenter>()) });
        }
        struct Chain {
            remaining: usize,
            drops: Arc<AtomicUsize>,
            owner: std::thread::ThreadId,
            source: WeakStoreRef,
            callback: Function,
        }
        impl Drop for Chain {
            fn drop(&mut self) {
                // SAME existing legal Send transport as the actual caller-Waker
                // fixtures. Rc owner state stays ONLY in SOURCE's C userdata.
                assert_eq!(self.owner, std::thread::current().id());
                let source = self.source.upgrade().unwrap();
                let output = source
                    .with_mut(|store| self.callback.call(store, &[]))
                    .unwrap()
                    .unwrap();
                assert!(output.is_empty());
                self.drops.fetch_add(1, Ordering::SeqCst);
                if self.remaining != 0 {
                    std::panic::resume_unwind(Box::new(Chain {
                        remaining: self.remaining - 1,
                        drops: self.drops.clone(),
                        owner: self.owner,
                        source: self.source.clone(),
                        callback: self.callback.clone(),
                    }));
                }
            }
        }
        struct Primary(u64);
        struct DuringUnwind {
            failure: RetirementFailure,
            chain: Option<Chain>,
        }
        impl Drop for DuringUnwind {
            fn drop(&mut self) {
                let chain = self.chain.take().unwrap();
                self.failure
                    .attempt(|| std::panic::resume_unwind(Box::new(chain)));
            }
        }
        for active in [false, true] {
            let target = StoreRef::new(Store::default()).unwrap();
            let StoreMode::Native(native) = &target.inner.mode else {
                unreachable!()
            };
            let owner = native.owner.clone();
            let failure = native.cleanup_failure.as_ref().clone();
            let weak_ledger = Rc::downgrade(&native.ledger);
            let exit = owner.enter_exit_boundary();
            let primary = Box::new(Primary(0x1234_5678_9abc_def0));
            let address = (&*primary) as *const Primary;
            owner
                .try_retain(async move { std::panic::resume_unwind(primary) })
                .unwrap_or_else(|_| panic!("first cleanup value refused"));
            let source = StoreRef::new(Store::default()).unwrap();
            let jobs = Rc::new(std::cell::Cell::new(0));
            let control = Box::into_raw(Box::new(Reenter {
                owner: owner.clone(),
                jobs: jobs.clone(),
                consume: std::cell::Cell::new(false),
                state: None,
                claimed: Rc::default(),
            }));
            let mut public = wasm_store_t {
                inner: source.clone(),
            };
            let ty = wasm_functype_t::new(wasmer_api::FunctionType::new([], []));
            let function = unsafe {
                wasm_func_new_with_env(
                    Some(&mut public),
                    Some(&ty),
                    Some(admit),
                    control.cast(),
                    Some(release),
                )
            }
            .unwrap();
            let visits = Arc::new(AtomicUsize::new(0));
            let chain = Chain {
                remaining: 3,
                drops: visits.clone(),
                owner: std::thread::current().id(),
                source: source.downgrade(),
                callback: function.extern_.function(),
            };
            if active {
                let original = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let _during = DuringUnwind {
                        failure: failure.clone(),
                        chain: Some(chain),
                    };
                    std::panic::panic_any("original active Rust value");
                }))
                .unwrap_err();
                assert_eq!(
                    original.downcast_ref::<&str>(),
                    Some(&"original active Rust value")
                );
            } else {
                failure.record(Box::new(chain));
            }
            assert_eq!(
                visits.load(Ordering::SeqCst),
                4,
                "each finite caught payload is actually destroyed"
            );
            assert_eq!(
                jobs.get(),
                4,
                "each actual SOURCE reentry admits and finishes real work"
            );
            let exact = failure.take().unwrap();
            assert_eq!(
                exact.downcast_ref::<Primary>().unwrap() as *const _,
                address
            );
            assert_eq!(
                exact.downcast_ref::<Primary>().unwrap().0,
                0x1234_5678_9abc_def0
            );
            assert!(failure.is_empty());
            drop(exact);
            drop(exit);
            drop(target);
            drop(failure);
            drop(owner);
            assert!(
                weak_ledger.upgrade().is_none(),
                "SOURCE may retain owner metadata without retaining its receiver/Store"
            );
            drop(function);
            drop(public);
            drop(source);
        }
        // Actual release publication owns the SAME receipt while SOURCE claims
        // and acknowledges I, then Q's destructor throws newly unclaimed N.
        {
            struct WakeQ(std::sync::Mutex<Option<Chain>>);
            impl std::task::Wake for WakeQ {
                fn wake(self: Arc<Self>) {
                    self.wake_by_ref();
                }
                fn wake_by_ref(self: &Arc<Self>) {
                    let chain = self.0.lock().unwrap().take();
                    if let Some(chain) = chain {
                        std::panic::resume_unwind(Box::new(chain));
                    }
                }
            }
            let target = StoreRef::new(Store::default()).unwrap();
            let owner = target.cleanup_owner().unwrap();
            let exit = owner.enter_exit_boundary();
            let done = Rc::new(std::cell::Cell::new(false));
            let registration = target
                .register_owned_retirement(done.clone(), RetirementFailure::default())
                .unwrap();
            registration.commit();
            let state = registration.state();
            registration.relinquish();
            target.retain_delivery(state.clone()).unwrap();
            let first = Box::new(Primary(0x1357_2468));
            let first_address = (&*first) as *const Primary;
            state.failure.record(first);
            let claimed = Rc::new(RefCell::new(None));
            let source = StoreRef::new(Store::default()).unwrap();
            let jobs = Rc::new(std::cell::Cell::new(0));
            let control = Box::into_raw(Box::new(Reenter {
                owner: owner.clone(),
                jobs: jobs.clone(),
                consume: std::cell::Cell::new(true),
                state: Some(state.clone()),
                claimed: claimed.clone(),
            }));
            let mut public = wasm_store_t {
                inner: source.clone(),
            };
            let ty = wasm_functype_t::new(wasmer_api::FunctionType::new([], []));
            let function = unsafe {
                wasm_func_new_with_env(
                    Some(&mut public),
                    Some(&ty),
                    Some(admit),
                    control.cast(),
                    Some(release),
                )
            }
            .unwrap();
            let visits = Arc::new(AtomicUsize::new(0));
            let wake =
                std::task::Waker::from(Arc::new(WakeQ(std::sync::Mutex::new(Some(Chain {
                    remaining: 3,
                    drops: visits.clone(),
                    owner: std::thread::current().id(),
                    source: source.downgrade(),
                    callback: function.extern_.function(),
                })))));
            assert!(state.register_release_waker(&wake));
            let release_receipt = target.retirement_release(state.clone()).unwrap();
            owner
                .try_retain(async move {
                    done.set(true);
                    release_receipt.release();
                })
                .unwrap_or_else(|_| panic!("actual terminal task refused"));
            assert_eq!(
                claimed
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .downcast_ref::<Primary>()
                    .unwrap() as *const _,
                first_address
            );
            assert!(state.physically_ready() && !state.publishing.get());
            assert!(
                !state.delivery_ack.get(),
                "new N REOPENS the same acknowledged carrier"
            );
            assert!(
                state.receipt_queued.get(),
                "actual publisher repairs SAME delivery receipt after reentry"
            );
            assert_eq!(state.failure.0.borrow().as_ref().unwrap().order, Some(3));
            assert_eq!(
                state
                    .failure
                    .0
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .payload
                    .downcast_ref::<Chain>()
                    .unwrap()
                    .remaining,
                2
            );
            assert_eq!(
                visits.load(Ordering::SeqCst),
                1,
                "N is still OWNED, never discarded at its original catch"
            );
            assert_eq!(jobs.get(), 1);
            let pending = state.ledger.snapshot();
            assert_eq!(pending.len(), 1);
            assert!(Rc::ptr_eq(&pending[0], &state));
            let mut n = None;
            assert!(state.claim(&mut n));
            owner.notify();
            assert!(state.delivery_ack.get() && !state.receipt_queued.get());
            assert!(state.ledger.snapshot().is_empty());
            discard_retirement_panic(n.unwrap()); // explicit final consumer owns N
            assert_eq!(visits.load(Ordering::SeqCst), 4);
            assert_eq!(jobs.get(), 4);
            drop(claimed.borrow_mut().take());
            drop(wake);
            drop(exit);
            drop(function);
            drop(public);
            drop(source);
            drop(pending);
            drop(state);
            drop(target);
        }
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn active_store_drop_does_not_retain_secondary_canonical_write_observer_panic() {
        use std::{
            future::Future,
            sync::{
                Arc,
                atomic::{AtomicUsize, Ordering},
            },
            task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
        };
        use wasmer_api::AsStoreAsync;
        struct Signal {
            clones: AtomicUsize,
            wakes: AtomicUsize,
        }
        unsafe fn clone_signal(data: *const ()) -> RawWaker {
            let signal = unsafe { &*data.cast::<Signal>() };
            if signal.clones.fetch_add(1, Ordering::SeqCst) == 1 {
                std::panic::panic_any("secondary canonical write observer N");
            }
            unsafe { Arc::increment_strong_count(data.cast::<Signal>()) };
            RawWaker::new(data, &SIGNAL)
        }
        unsafe fn wake_signal(data: *const ()) {
            unsafe {
                wake_signal_ref(data);
                drop_signal(data);
            }
        }
        unsafe fn wake_signal_ref(data: *const ()) {
            unsafe { &*data.cast::<Signal>() }
                .wakes
                .fetch_add(1, Ordering::SeqCst);
        }
        unsafe fn drop_signal(data: *const ()) {
            drop(unsafe { Arc::from_raw(data.cast::<Signal>()) });
        }
        static SIGNAL: RawWakerVTable =
            RawWakerVTable::new(clone_signal, wake_signal, wake_signal_ref, drop_signal);
        let store = StoreRef::new(Store::default()).unwrap();
        let StoreMode::Native(native) = &store.inner.mode else {
            unreachable!()
        };
        let owner = native.owner.clone();
        let failure = native.cleanup_failure.as_ref().clone();
        let ledger = native.ledger.clone();
        let weak = store.downgrade();
        let signal = Arc::new(Signal {
            clones: AtomicUsize::new(0),
            wakes: AtomicUsize::new(0),
        });
        let gate = Rc::new(std::cell::Cell::new(false));
        let pending = gate.clone();
        owner
            .try_retain(std::future::poll_fn(move |_| {
                if pending.get() {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }))
            .unwrap_or_else(|_| panic!("original finite owner job refused"));
        {
            let asynchronous = store.async_store().unwrap();
            let mut lock = std::pin::pin!(asynchronous.write_lock());
            let Poll::Ready(guard) = lock.as_mut().poll(&mut Context::from_waker(Waker::noop()))
            else {
                panic!("unused actual Store guard must be available");
            };
            let waker = unsafe {
                Waker::from_raw(RawWaker::new(Arc::into_raw(signal.clone()).cast(), &SIGNAL))
            };
            assert!(
                owner
                    .poll_idle(&mut Context::from_waker(&waker))
                    .is_pending()
            );
            assert_eq!(signal.clones.load(Ordering::SeqCst), 1);
            let original = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _actual_public_store = store;
                std::panic::panic_any("external active Rust G");
            }))
            .unwrap_err();
            assert_eq!(
                original.downcast_ref::<&str>(),
                Some(&"external active Rust G")
            );
            assert!(weak.upgrade().is_none());
            assert!(
                signal.clones.load(Ordering::SeqCst) >= 3,
                "actual canonical observer caught N and retried its own observer"
            );
            assert!(
                failure.is_empty(),
                "secondary N cannot outlive G as a fresh cleanup event"
            );
            gate.set(true); // original surviving caller, then real unlock producer
            drop(guard);
            assert!(owner.is_idle());
            assert!(
                !ledger.final_done.get(),
                "independent backing still owns actual VM resources"
            );
        }
        assert!(ledger.final_done.get() && owner.is_idle());
        assert!(failure.is_empty());
        assert_eq!(
            ledger.clock.get(),
            0,
            "no ordinal assigned to active-unwind secondary N"
        );
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn exclusive_destruction_reservation_installs_during_real_source_notification_reentry() {
        use std::{future::Future, task::Context};
        type Action = Box<dyn FnOnce(Option<RetirementPanic>) -> Option<RetirementPanic>>;
        struct Control {
            reservation: RefCell<Option<wasmer_api::sys::NativeStoreDestructionReservation>>,
            receipt: wasmer_api::sys::NativeStoreDestruction,
            events: Rc<std::cell::Cell<usize>>,
            resource_seen: Rc<std::cell::Cell<bool>>,
            notification_pending_seen: Rc<std::cell::Cell<bool>>,
            returned: Rc<RefCell<Option<Action>>>,
        }
        unsafe extern "C" fn install(
            data: *mut std::ffi::c_void,
            _: &wasm_val_vec_t,
            _: &mut wasm_val_vec_t,
        ) -> Option<Box<wasm_trap_t>> {
            let control = unsafe { &*data.cast::<Control>() };
            control.resource_seen.set(control.receipt.is_complete());
            control
                .notification_pending_seen
                .set(!control.receipt.notifications_complete());
            let reservation = control.reservation.borrow_mut().take().unwrap();
            let events = control.events.clone();
            let receipt = control.receipt.clone();
            let action: Action = Box::new(move |_| {
                events.set(
                    if receipt.is_complete() && receipt.notifications_complete() {
                        2
                    } else {
                        99
                    },
                );
                None
            });
            match reservation.install(action) {
                Ok(()) => control.events.set(1),
                Err(exact) => *control.returned.borrow_mut() = Some(exact),
            }
            None
        }
        unsafe extern "C" fn delete(data: *mut std::ffi::c_void) {
            drop(unsafe { Box::from_raw(data.cast::<Control>()) });
        }
        struct Reenter {
            owner: std::thread::ThreadId,
            source: WeakStoreRef,
            function: Function,
        }
        impl std::task::Wake for Reenter {
            fn wake(self: std::sync::Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &std::sync::Arc<Self>) {
                assert_eq!(self.owner, std::thread::current().id());
                let source = self.source.upgrade().unwrap();
                assert!(
                    source
                        .with_mut(|store| self.function.call(store, &[]))
                        .unwrap()
                        .unwrap()
                        .is_empty()
                );
            }
        }
        let mut target = Store::default();
        let scope = wasmer_api::sys::NativeInvocationScope::bind(&mut target).unwrap();
        let receipt = scope.store_destruction();
        let reservation = receipt.reserve_cleanup().unwrap();
        let source = StoreRef::new(Store::default()).unwrap();
        let events = Rc::new(std::cell::Cell::new(0));
        let resource_seen = Rc::new(std::cell::Cell::new(false));
        let pending_seen = Rc::new(std::cell::Cell::new(false));
        let returned = Rc::new(RefCell::new(None));
        let control = Box::into_raw(Box::new(Control {
            reservation: RefCell::new(Some(reservation)),
            receipt: receipt.clone(),
            events: events.clone(),
            resource_seen: resource_seen.clone(),
            notification_pending_seen: pending_seen.clone(),
            returned: returned.clone(),
        }));
        let mut public = wasm_store_t {
            inner: source.clone(),
        };
        let ty = wasm_functype_t::new(wasmer_api::FunctionType::new([], []));
        let function = unsafe {
            wasm_func_new_with_env(
                Some(&mut public),
                Some(&ty),
                Some(install),
                control.cast(),
                Some(delete),
            )
        }
        .unwrap();
        let wake = std::task::Waker::from(std::sync::Arc::new(Reenter {
            owner: std::thread::current().id(),
            source: source.downgrade(),
            function: function.extern_.function(),
        }));
        let mut observer = receipt.observer();
        assert!(
            std::pin::Pin::new(&mut observer)
                .poll(&mut Context::from_waker(&wake))
                .is_pending()
        );
        drop(target); // actual guard invokes SOURCE, which installs while Reserved
        assert!(resource_seen.get() && pending_seen.get());
        assert_eq!(events.get(), 2);
        assert!(
            returned.borrow().is_none(),
            "actual notification return did not precede reentrant install"
        );
        assert!(receipt.notifications_complete());
        drop(observer);
        drop(wake);
        drop(function);
        drop(public);
        drop(source);
        drop(scope);
    }

    #[cfg(feature = "extensions")]
    fn common_collector_reentrant_tail_profile(
        i_first: bool,
        b_error: bool,
        new_pending: bool,
        cancel: bool,
        release_error: bool,
    ) {
        use std::{
            future::Future,
            sync::{
                Arc,
                atomic::{AtomicUsize, Ordering},
            },
            task::{Context, Poll, Waker},
        };
        struct Payload(&'static str, Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.1.fetch_add(1, Ordering::SeqCst);
            }
        }
        struct Control {
            tail: Rc<RefCell<std::pin::Pin<Box<NativeRetirementTail>>>>,
            completed: Rc<std::cell::Cell<bool>>,
            own_error: Rc<RefCell<Option<RetirementPanic>>>,
            target: WeakStoreRef,
            create_pending: std::cell::Cell<bool>,
            pending_done: Rc<std::cell::Cell<bool>>,
            pending_wait: RefCell<Option<tokio::sync::oneshot::Receiver<()>>>,
            release_waker: Option<Waker>,
            release_subscription: Rc<RefCell<Option<DriverSubscription>>>,
        }
        unsafe extern "C" fn poll_b(
            data: *mut std::ffi::c_void,
            _: &wasm_val_vec_t,
            _: &mut wasm_val_vec_t,
        ) -> Option<Box<wasm_trap_t>> {
            let control = unsafe { &*data.cast::<Control>() };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                control
                    .tail
                    .borrow_mut()
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
            }));
            match result {
                Ok(Poll::Ready(())) => control.completed.set(true),
                Ok(Poll::Pending) => control.completed.set(false),
                Err(own) => {
                    *control.own_error.borrow_mut() = Some(own);
                    control.completed.set(true);
                }
            }
            if let Some(waker) = &control.release_waker {
                let target = control.target.upgrade().unwrap();
                let owner = target.retirement_owner().unwrap();
                let subscription = owner.subscribe();
                subscription.with_context(&mut Context::from_waker(waker), |_| ());
                *control.release_subscription.borrow_mut() = Some(subscription);
            }
            if control.create_pending.replace(false) {
                let target = control.target.upgrade().unwrap();
                let done = Rc::new(std::cell::Cell::new(false));
                let registration = target
                    .register_owned_retirement(done.clone(), RetirementFailure::default())
                    .unwrap();
                let state = registration.state();
                target.retain_delivery(state.clone()).unwrap();
                registration.relinquish();
                let release = target.retirement_release(state).unwrap();
                let pending_done = control.pending_done.clone();
                let pending_wait = control.pending_wait.borrow_mut().take().unwrap();
                target
                    .cleanup_owner()
                    .unwrap()
                    .try_retain(async move {
                        pending_wait.await.unwrap();
                        tokio::task::yield_now().await;
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        done.set(true);
                        release.release();
                        pending_done.set(true);
                    })
                    .unwrap_or_else(|_| panic!("actual new physical retirement refused"));
            }
            None
        }
        unsafe extern "C" fn delete(data: *mut std::ffi::c_void) {
            drop(unsafe { Box::from_raw(data.cast::<Control>()) });
        }
        struct Output {
            owner: std::thread::ThreadId,
            source: WeakStoreRef,
            callback: Function,
            drops: Arc<AtomicUsize>,
        }
        impl Drop for Output {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
                assert_eq!(self.owner, std::thread::current().id());
                let source = self.source.upgrade().unwrap();
                assert!(
                    source
                        .with_mut(|store| self.callback.call(store, &[]))
                        .unwrap()
                        .unwrap()
                        .is_empty()
                );
            }
        }
        struct ReadyOnce {
            value: Option<Output>,
            polls: Arc<AtomicUsize>,
        }
        impl Future for ReadyOnce {
            type Output = Output;
            fn poll(self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Output> {
                let this = self.get_mut();
                assert_eq!(
                    this.polls.fetch_add(1, Ordering::SeqCst),
                    0,
                    "completed main future must NEVER be repolled after output disposal"
                );
                Poll::Ready(this.value.take().unwrap())
            }
        }
        let store = StoreRef::new(Store::default()).unwrap();
        let owner = store.retirement_owner().unwrap();
        let b_done = Rc::new(std::cell::Cell::new(false));
        let b_registration = store
            .register_owned_retirement(b_done.clone(), RetirementFailure::default())
            .unwrap();
        let b_state = b_registration.state();
        store.retain_delivery(b_state.clone()).unwrap();
        b_state.disposition.set(DeliveryDisposition::LiveTail);
        let b_drops = Arc::new(AtomicUsize::new(0));
        let b_payload = Box::new(Payload("B-own", b_drops.clone()));
        let b_address = (&*b_payload) as *const Payload;
        let b_payload = if b_error {
            Some(b_payload)
        } else {
            drop(b_payload);
            None
        };
        let tail = Rc::new(RefCell::new(Box::pin(NativeRetirementTail {
            subscription: Some(owner.subscribe()),
            _store: store.downgrade(),
            base: Some(store.async_store().unwrap()),
            owner: Some(owner.clone()),
            cleanup: Some(owner.owner.clone()),
            release: Some(store.retirement_release(b_state.clone()).unwrap()),
            state: b_state.clone(),
            registration: Some(b_registration),
            first: store.retirement_failure().unwrap(),
            finalization_subscription: None,
            finalization: store.finalization_handoff(&owner),
            exit: None,
        })));
        // Existing publication path stages B genuinely Pending, then completes
        // its actual physical receipt WITHOUT consuming B's next caller poll.
        let first = tail.clone();
        let terminal_owner = owner.owner.clone();
        let b_failure = b_state.failure.clone();
        owner.owner.after_pump_existing(move || {
            terminal_owner
                .try_retain(async move {
                    if let Some(exact) = b_payload {
                        b_failure.attempt(|| std::panic::resume_unwind(exact));
                    }
                    b_done.set(true);
                })
                .unwrap_or_else(|_| panic!("B actual terminal refused"));
            assert!(
                first
                    .borrow_mut()
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
        });
        assert!(b_state.physically_ready() && !b_state.publishing.get());
        let i_drops = Arc::new(AtomicUsize::new(0));
        let q_drops = Arc::new(AtomicUsize::new(0));
        let i = Box::new(Payload("I", i_drops.clone()));
        let q = Box::new(Payload("Q", q_drops.clone()));
        let expected = if i_first {
            (&*i) as *const Payload
        } else {
            (&*q) as *const Payload
        };
        let mut q = Some(q);
        if !i_first {
            let q = q.take().unwrap();
            owner
                .owner
                .try_retain(async move { std::panic::resume_unwind(q) })
                .unwrap_or_else(|_| panic!("earlier raw Q refused"));
        }
        let i_done = Rc::new(std::cell::Cell::new(false));
        let i_registration = store
            .register_owned_retirement(i_done.clone(), RetirementFailure::default())
            .unwrap();
        let i_state = i_registration.state();
        let i_failure = i_state.failure.clone();
        store.retain_delivery(i_state.clone()).unwrap();
        i_registration.relinquish();
        let i_release = store.retirement_release(i_state.clone()).unwrap();
        owner
            .owner
            .try_retain(async move {
                i_failure.attempt(|| std::panic::resume_unwind(i));
                i_done.set(true);
                i_release.release();
            })
            .unwrap_or_else(|_| panic!("I actual terminal refused"));
        if let Some(q) = q {
            owner
                .owner
                .try_retain(async move { std::panic::resume_unwind(q) })
                .unwrap_or_else(|_| panic!("later raw Q refused"));
        }
        let source = StoreRef::new(Store::default()).unwrap();
        let completed = Rc::new(std::cell::Cell::new(false));
        let own_error = Rc::new(RefCell::new(None));
        let pending_done = Rc::new(std::cell::Cell::new(false));
        let (pending_tx, pending_wait) = tokio::sync::oneshot::channel();
        let mut pending_tx = Some(pending_tx);
        if !cancel {
            pending_tx.take().unwrap().send(()).unwrap();
        }
        struct ReleaseWake(std::sync::Mutex<Option<Box<Payload>>>);
        impl std::task::Wake for ReleaseWake {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                let exact = self.0.lock().unwrap().take();
                if let Some(exact) = exact {
                    std::panic::resume_unwind(exact);
                }
            }
        }
        let release_drops = Arc::new(AtomicUsize::new(0));
        let release_waker = release_error.then(|| {
            Waker::from(Arc::new(ReleaseWake(std::sync::Mutex::new(Some(
                Box::new(Payload("release-Q", release_drops.clone())),
            )))))
        });
        let release_subscription = Rc::new(RefCell::new(None));
        let control = Box::into_raw(Box::new(Control {
            tail: tail.clone(),
            completed: completed.clone(),
            own_error: own_error.clone(),
            target: store.downgrade(),
            create_pending: std::cell::Cell::new(new_pending),
            pending_done: pending_done.clone(),
            pending_wait: RefCell::new(Some(pending_wait)),
            release_waker,
            release_subscription: release_subscription.clone(),
        }));
        let mut public = wasm_store_t {
            inner: source.clone(),
        };
        let ty = wasm_functype_t::new(wasmer_api::FunctionType::new([], []));
        let function = unsafe {
            wasm_func_new_with_env(
                Some(&mut public),
                Some(&ty),
                Some(poll_b),
                control.cast(),
                Some(delete),
            )
        }
        .unwrap();
        let polls = Arc::new(AtomicUsize::new(0));
        let output_drops = Arc::new(AtomicUsize::new(0));
        let value = Output {
            owner: std::thread::current().id(),
            source: source.downgrade(),
            callback: function.extern_.function(),
            drops: output_drops.clone(),
        };
        let ready = ReadyOnce {
            value: Some(value),
            polls: polls.clone(),
        };
        let exact = if cancel {
            let mut call = Box::pin(owner.driver.run_local(ready));
            assert!(
                call.as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert_eq!(polls.load(Ordering::SeqCst), 1);
            assert_eq!(output_drops.load(Ordering::SeqCst), 1);
            assert!(
                !pending_done.get(),
                "actual signal keeps physical job Pending"
            );
            let original = owner.driver.scopes.common_collector.borrow().clone();
            assert!(original.upgrade().is_some());
            let canonical = owner.driver.canonical_failure();
            let order = canonical.0.borrow().as_ref().unwrap().order;
            drop(call); // actual drive envelope cancels while I remains canonical
            assert!(original.upgrade().is_none());
            assert!(
                owner
                    .driver
                    .scopes
                    .common_collector
                    .borrow()
                    .upgrade()
                    .is_none()
            );
            assert_eq!(canonical.0.borrow().as_ref().unwrap().order, order);
            assert_eq!(
                canonical
                    .0
                    .borrow()
                    .as_ref()
                    .unwrap()
                    .payload
                    .downcast_ref::<Payload>()
                    .unwrap() as *const _,
                expected
            );
            assert_eq!(
                owner.driver.ledger.service_claims.get(),
                1,
                "ONLY real pending receipt still owns a lease"
            );
            pending_tx.take().unwrap().send(()).unwrap();
            struct NewCollector {
                scopes: Rc<DriverScopes>,
                identity: Rc<RefCell<Option<Weak<CommonCollectorState>>>>,
            }
            impl Drop for NewCollector {
                fn drop(&mut self) {
                    *self.identity.borrow_mut() =
                        Some(self.scopes.common_collector.borrow().clone());
                }
            }
            let identity = Rc::new(RefCell::new(None));
            let probe = NewCollector {
                scopes: owner.driver.scopes.clone(),
                identity: identity.clone(),
            };
            let exact = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                store.run_host(std::future::ready(probe)).unwrap();
            }))
            .expect_err("surviving original public Driver receives SAME cancelled common value");
            let next = identity.borrow_mut().take().unwrap();
            assert!(
                !original.ptr_eq(&next),
                "new actual common caller receives its OWN identity"
            );
            assert!(next.upgrade().is_none());
            exact
        } else {
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                store.run_host(ready).unwrap();
            }))
            .expect_err("A retains its common first outcome through B reentry")
        };
        let payload = exact.downcast_ref::<Payload>().unwrap();
        assert_eq!(payload as *const _, expected);
        assert_eq!(payload.0, if i_first { "I" } else { "Q" });
        assert!(
            completed.get(),
            "B completes OWN outcome instead of ownership self-wait"
        );
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        assert_eq!(output_drops.load(Ordering::SeqCst), 1);
        if new_pending {
            assert!(
                pending_done.get(),
                "A drives actual new receipt after completed output disposal"
            );
        }
        assert_eq!(i_drops.load(Ordering::SeqCst), usize::from(!i_first));
        assert_eq!(q_drops.load(Ordering::SeqCst), usize::from(i_first));
        if b_error {
            let own = own_error.borrow_mut().take().unwrap();
            assert_eq!(
                own.downcast_ref::<Payload>().unwrap() as *const _,
                b_address
            );
            assert_eq!(own.downcast_ref::<Payload>().unwrap().0, "B-own");
            assert_eq!(b_drops.load(Ordering::SeqCst), 0);
            drop(own);
        } else {
            assert!(own_error.borrow().is_none());
        }
        assert_eq!(b_drops.load(Ordering::SeqCst), 1);
        drop(exact);
        assert_eq!(i_drops.load(Ordering::SeqCst), 1);
        assert_eq!(q_drops.load(Ordering::SeqCst), 1);
        assert!(
            owner
                .driver
                .scopes
                .common_collector
                .borrow()
                .upgrade()
                .is_none()
        );
        assert_eq!(owner.driver.ledger.service_claims.get(), 0);
        assert!(owner.owner.is_idle());
        assert_eq!(
            release_drops.load(Ordering::SeqCst),
            usize::from(release_error),
            "release-wake Q was reconciled while A's exact primary remained owned"
        );
        drop(release_subscription.borrow_mut().take());
        drop(tail);
        drop(function);
        drop(public);
        drop(source);
        drop(owner);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn common_collector_preserves_i_through_output_drop_and_different_tail_b() {
        common_collector_reentrant_tail_profile(true, false, false, false, false);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn common_collector_preserves_earlier_q_and_b_own_error_identity() {
        common_collector_reentrant_tail_profile(false, true, false, false, false);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn common_collector_keeps_ready_latch_through_new_physical_pending_after_output_drop() {
        common_collector_reentrant_tail_profile(true, false, true, false, false);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn common_collector_cancellation_retains_event_and_pending_ready_latch_for_surviving_driver() {
        common_collector_reentrant_tail_profile(true, false, true, true, false);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn common_collector_release_wake_cannot_replace_extracted_primary() {
        common_collector_reentrant_tail_profile(true, false, false, false, true);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn cancelled_common_collector_keeps_exact_unstamped_event_without_ordered_completion() {
        use std::{
            future::Future,
            sync::{
                Arc,
                atomic::{AtomicUsize, Ordering},
            },
            task::{Context, Poll, Waker},
        };
        struct Payload(Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        struct ReadyOnce(Arc<AtomicUsize>);
        impl Future for ReadyOnce {
            type Output = ();
            fn poll(self: std::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
                assert_eq!(self.0.fetch_add(1, Ordering::SeqCst), 0);
                Poll::Ready(())
            }
        }
        let store = StoreRef::new(Store::default()).unwrap();
        let owner = store.retirement_owner().unwrap();
        let canonical = owner.driver.canonical_failure();
        let weak_canonical = Rc::downgrade(&canonical);
        let weak_driver = Rc::downgrade(&owner.driver);
        owner.driver.ledger.clock.set(u64::MAX); // explicit primitive exhaustion injection
        let drops = Arc::new(AtomicUsize::new(0));
        let exact = Box::new(Payload(drops.clone()));
        let address = (&*exact) as *const Payload;
        owner
            .owner
            .try_retain(async move { std::panic::resume_unwind(exact) })
            .unwrap_or_else(|_| panic!("original cleanup error refused"));
        assert!(canonical.has_unstamped_event());
        let polls = Arc::new(AtomicUsize::new(0));
        let mut call = Box::pin(owner.driver.run_local(ReadyOnce(polls.clone())));
        assert!(
            call.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        let identity = owner.driver.scopes.common_collector.borrow().clone();
        assert!(identity.upgrade().is_some());
        drop(call);
        assert_eq!(polls.load(Ordering::SeqCst), 1);
        assert!(identity.upgrade().is_none());
        assert!(
            owner
                .driver
                .scopes
                .common_collector
                .borrow()
                .upgrade()
                .is_none()
        );
        assert_eq!(owner.driver.ledger.service_claims.get(), 0);
        assert!(canonical.has_unstamped_event() && canonical.take_event().is_none());
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        assert_eq!(
            canonical
                .0
                .borrow()
                .as_ref()
                .unwrap()
                .payload
                .downcast_ref::<Payload>()
                .unwrap() as *const _,
            address
        );
        assert!(!canonical.1.get());
        // This injected exhaustion has NO normal ordered completion. Withdraw
        // ONLY the test-owned injected value for fixture cleanup, without polling
        // Ready or manufacturing an acknowledgement/ordinal/physical fence.
        let event = canonical.0.borrow_mut().take().unwrap();
        assert!(event.order.is_none());
        drop(event);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        drop(owner);
        drop(store);
        assert!(weak_driver.upgrade().is_none());
        drop(canonical);
        assert!(weak_canonical.upgrade().is_none());
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn post_driver_tail_cancel_owns_both_ordered_and_late_unstamped_payloads_until_terminal_drop() {
        use std::{
            future::Future,
            sync::{
                Arc,
                atomic::{AtomicBool, AtomicUsize, Ordering},
            },
            task::{Context, Poll, RawWaker, RawWakerVTable, Waker},
        };
        struct Payload(&'static str, Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.1.fetch_add(1, Ordering::SeqCst);
            }
        }
        struct DropWork {
            owner: wasmer_api::sys::NativeCleanupOwner,
            gate: Rc<std::cell::Cell<bool>>,
            completed: Rc<std::cell::Cell<bool>>,
        }
        impl Drop for DropWork {
            fn drop(&mut self) {
                let gate = self.gate.clone();
                let completed = self.completed.clone();
                self.owner
                    .try_retain(std::future::poll_fn(move |_| {
                        if gate.get() {
                            completed.set(true);
                            Poll::Ready(())
                        } else {
                            Poll::Pending
                        }
                    }))
                    .unwrap_or_else(|_| panic!("actual LocalSet destructor job refused"));
            }
        }
        struct Notify {
            armed: AtomicBool,
            payload: std::sync::Mutex<Option<RetirementPanic>>,
        }
        unsafe fn clone_notify(data: *const ()) -> RawWaker {
            unsafe { Arc::increment_strong_count(data.cast::<Notify>()) };
            RawWaker::new(data, &NOTIFY)
        }
        unsafe fn wake_notify(data: *const ()) {
            unsafe {
                drop_notify(data);
            }
        }
        unsafe fn wake_notify_ref(_: *const ()) {}
        unsafe fn drop_notify(data: *const ()) {
            let notify = unsafe { Arc::from_raw(data.cast::<Notify>()) };
            let exact = if notify.armed.load(Ordering::SeqCst) {
                notify.payload.lock().unwrap().take()
            } else {
                None
            };
            if let Some(exact) = exact {
                std::panic::resume_unwind(exact);
            }
        }
        static NOTIFY: RawWakerVTable =
            RawWakerVTable::new(clone_notify, wake_notify, wake_notify_ref, drop_notify);
        let store = StoreRef::new(Store::default()).unwrap();
        let owner = store.retirement_owner().unwrap();
        let cleanup = owner.owner.clone();
        let weak_driver = Rc::downgrade(&owner.driver);
        let gate = Rc::new(std::cell::Cell::new(false));
        let completed = Rc::new(std::cell::Cell::new(false));
        let started = Rc::new(std::cell::Cell::new(false));
        let work = DropWork {
            owner: cleanup.clone(),
            gate: gate.clone(),
            completed: completed.clone(),
        };
        let start = started.clone();
        store
            .run_host(async move {
                tokio::task::spawn_local(async move {
                    let _work = work;
                    start.set(true);
                    std::future::pending::<()>().await;
                });
                tokio::task::yield_now().await;
            })
            .unwrap();
        assert!(started.get());
        let a_done = Rc::new(std::cell::Cell::new(false));
        let b_done = Rc::new(std::cell::Cell::new(false));
        let a_reg = store
            .register_owned_retirement(a_done.clone(), RetirementFailure::default())
            .unwrap();
        let b_reg = store
            .register_owned_retirement(b_done.clone(), RetirementFailure::default())
            .unwrap();
        let a_state = a_reg.state();
        let b_state = b_reg.state();
        store.retain_delivery(a_state.clone()).unwrap();
        store.retain_delivery(b_state.clone()).unwrap();
        a_state.disposition.set(DeliveryDisposition::LiveTail);
        b_state.disposition.set(DeliveryDisposition::LiveTail);
        let i_drops = Arc::new(AtomicUsize::new(0));
        let n_drops = Arc::new(AtomicUsize::new(0));
        let i = Box::new(Payload("owned I", i_drops.clone()));
        let i_address = (&*i) as *const Payload;
        let a = Rc::new(RefCell::new(Box::pin(NativeRetirementTail {
            subscription: Some(owner.subscribe()),
            _store: store.downgrade(),
            base: Some(store.async_store().unwrap()),
            owner: Some(owner.clone()),
            cleanup: Some(cleanup.clone()),
            release: Some(store.retirement_release(a_state.clone()).unwrap()),
            state: a_state.clone(),
            registration: Some(a_reg),
            first: store.retirement_failure().unwrap(),
            finalization_subscription: None,
            finalization: store.finalization_handoff(&owner),
            exit: None,
        })));
        let b = Rc::new(RefCell::new(Box::pin(NativeRetirementTail {
            subscription: Some(owner.subscribe()),
            _store: store.downgrade(),
            base: Some(store.async_store().unwrap()),
            owner: Some(owner.clone()),
            cleanup: Some(cleanup.clone()),
            release: Some(store.retirement_release(b_state.clone()).unwrap()),
            state: b_state.clone(),
            registration: Some(b_reg),
            first: store.retirement_failure().unwrap(),
            finalization_subscription: None,
            finalization: store.finalization_handoff(&owner),
            exit: None,
        })));
        let first_a = a.clone();
        let first_b = b.clone();
        let terminal_owner = cleanup.clone();
        let b_failure = b_state.failure.clone();
        cleanup.after_pump_existing(move || {
            terminal_owner
                .try_retain(async move {
                    b_failure.attempt(|| std::panic::resume_unwind(i));
                    a_done.set(true);
                    b_done.set(true);
                })
                .unwrap_or_else(|_| panic!("actual tail terminal task refused"));
            assert!(
                first_a
                    .borrow_mut()
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            assert!(
                first_b
                    .borrow_mut()
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
        });
        assert!(a_state.physically_ready() && b_state.physically_ready());
        let notify = Arc::new(Notify {
            armed: AtomicBool::new(false),
            payload: std::sync::Mutex::new(Some(Box::new(Payload("late N", n_drops.clone())))),
        });
        let wake = unsafe {
            Waker::from_raw(RawWaker::new(Arc::into_raw(notify.clone()).cast(), &NOTIFY))
        };
        drop(owner);
        drop(store);
        assert!(
            b.borrow_mut()
                .as_mut()
                .poll(&mut Context::from_waker(&wake))
                .is_pending()
        );
        assert!(
            weak_driver.upgrade().is_none(),
            "actual Driver/LocalSet destruction precedes cancel"
        );
        assert!(!completed.get());
        gate.set(true); // original A producer drives the SAME unfinished phase
        assert!(
            a.borrow_mut()
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_ready()
        );
        assert!(completed.get() && b_state.ledger.final_done.get());
        assert_eq!(
            b_state.ledger.service_claims.get(),
            1,
            "ONLY B retains its owned first ordering lease"
        );
        b_state.ledger.clock.set(u64::MAX);
        notify.armed.store(true, Ordering::SeqCst);
        let exact = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(b))).unwrap_err();
        assert_eq!(
            exact.downcast_ref::<Payload>().unwrap() as *const _,
            i_address
        );
        assert_eq!(exact.downcast_ref::<Payload>().unwrap().0, "owned I");
        assert_eq!(i_drops.load(Ordering::SeqCst), 0);
        assert_eq!(
            n_drops.load(Ordering::SeqCst),
            1,
            "exact N safely disposed only after actual Done"
        );
        assert!(b_state.delivery_empty() && !b_state.delivery_ack.get());
        assert!(
            !b_state.receipt_queued.get(),
            "no postseal receipt readmission"
        );
        assert_eq!(b_state.ledger.service_claims.get(), 0);
        assert!(cleanup.is_idle() && cleanup.is_sealed());
        drop(exact);
        assert_eq!(i_drops.load(Ordering::SeqCst), 1);
        drop(a);
        drop(wake);
    }

    #[test]
    fn final_store_retirement_waits_for_a_bare_stores_actual_unlock() {
        use std::{future::Future, task::Context};
        use wasmer_api::AsStoreAsync;
        let store = StoreRef::new(Store::default()).unwrap();
        let weak = store.downgrade();
        let asynchronous = store.async_store().unwrap();
        let owner = match &store.inner.mode {
            StoreMode::Native(native) => native.owner.clone(),
            StoreMode::Plain(_) => unreachable!(),
        };
        let mut lock = std::pin::pin!(asynchronous.write_lock());
        let std::task::Poll::Ready(guard) = lock
            .as_mut()
            .poll(&mut Context::from_waker(std::task::Waker::noop()))
        else {
            panic!("the unused Store must have an available write guard");
        };
        drop(store);
        assert!(weak.upgrade().is_none());
        assert!(
            !owner.is_idle(),
            "physical generation settlement must not bypass the actual guard"
        );
        drop(guard);
        assert!(
            owner.is_idle(),
            "unlock alone must repump the bare Store fence"
        );
    }

    #[cfg(feature = "extensions")]
    struct LateTailFixture {
        target: Box<wasm_store_t>,
        owner: wasmer_api::sys::NativeCleanupOwner,
        ledger: Rc<RetirementLedger>,
        weak: WeakStoreRef,
        driver: Weak<Driver>,
        local: Rc<std::cell::Cell<usize>>,
        vm: Rc<std::cell::Cell<usize>>,
    }
    #[cfg(feature = "extensions")]
    fn late_tail_fixture(first: Option<RetirementPanic>) -> LateTailFixture {
        struct Resource {
            owner: wasmer_api::sys::NativeCleanupOwner,
            weak: WeakStoreRef,
            completed: Rc<std::cell::Cell<usize>>,
            first: Option<RetirementPanic>,
        }
        impl Drop for Resource {
            fn drop(&mut self) {
                assert!(self.weak.upgrade().is_none());
                let completed = self.completed.clone();
                let first = self.first.take();
                self.owner
                    .try_retain(async move {
                        tokio::task::yield_now().await;
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        completed.set(completed.get() + 1);
                        if let Some(first) = first {
                            std::panic::resume_unwind(first);
                        }
                    })
                    .unwrap_or_else(|_| panic!("actual destructor cleanup refused"));
            }
        }
        unsafe extern "C" fn callback(
            _: *mut std::ffi::c_void,
            _: &wasm_val_vec_t,
            _: &mut wasm_val_vec_t,
        ) -> Option<Box<wasm_trap_t>> {
            None
        }
        unsafe extern "C" fn finalize(data: *mut std::ffi::c_void) {
            drop(unsafe { Box::from_raw(data.cast::<Resource>()) });
        }
        let engine = crate::wasm_c_api::engine::wasm_engine_new();
        let mut target = unsafe { wasm_store_new(Some(&engine)) }.unwrap();
        let owner = target.inner.cleanup_owner().unwrap();
        let weak = target.inner.downgrade();
        let local = Rc::new(std::cell::Cell::new(0));
        let vm = Rc::new(std::cell::Cell::new(0));
        let started = Rc::new(std::cell::Cell::new(false));
        let resource = Resource {
            owner: owner.clone(),
            weak: weak.clone(),
            completed: local.clone(),
            first,
        };
        let task_started = started.clone();
        target
            .inner
            .run_host(async move {
                tokio::task::spawn_local(async move {
                    let _resource = resource;
                    task_started.set(true);
                    std::future::pending::<()>().await;
                });
                tokio::task::yield_now().await;
            })
            .unwrap();
        assert!(started.get());
        let data = Box::into_raw(Box::new(Resource {
            owner: owner.clone(),
            weak: weak.clone(),
            completed: vm.clone(),
            first: None,
        }));
        let ty = wasm_functype_t::new(wasmer_api::FunctionType::new([], []));
        let function = unsafe {
            wasm_func_new_with_env(
                Some(&mut target),
                Some(&ty),
                Some(callback),
                data.cast(),
                Some(finalize),
            )
        }
        .unwrap();
        drop(function);
        assert_eq!(vm.get(), 0);
        let StoreMode::Native(native) = &target.inner.inner.mode else {
            unreachable!()
        };
        let driver = Rc::downgrade(native.driver.borrow().as_ref().unwrap());
        let ledger = native.ledger.clone();
        assert!(ledger.snapshot().is_empty() && native.finalization.borrow().upgrade().is_none());
        LateTailFixture {
            target,
            owner,
            ledger,
            weak,
            driver,
            local,
            vm,
        }
    }
    #[cfg(feature = "extensions")]
    #[test]
    fn late_finalization_refusal_without_native_ancestry_transfers_the_actual_phase_once() {
        assert!(wasmer_api::sys::WeakNativeInvocation::current().is_none());
        assert!(wasmer_api::sys::NativeAncestrySettlement::current(None).is_none());
        let LateTailFixture {
            target,
            owner,
            ledger,
            weak,
            driver,
            local,
            vm,
        } = late_tail_fixture(None);
        // No Native R exists: existing API returns the EXACT unpublished job;
        // its Drop makes the ONE lexical transfer at this legal outer boundary.
        unsafe { wasm_store_delete(Some(target)) };
        assert!(weak.upgrade().is_none() && driver.upgrade().is_none());
        assert_eq!((local.get(), vm.get()), (1, 1));
        assert!(ledger.final_done.get() && ledger.snapshot().is_empty());
        assert_eq!(ledger.service_claims.get(), 0);
        assert!(owner.is_idle() && owner.is_sealed());
    }
    #[cfg(feature = "extensions")]
    fn late_tail_original_caller_cancel_profile(active_primary: bool) {
        use std::{
            future::Future,
            sync::{
                Arc,
                atomic::{AtomicUsize, Ordering},
            },
            task::Poll,
        };
        struct Payload(&'static str, Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.1.fetch_add(1, Ordering::SeqCst);
            }
        }
        let i_drops = Arc::new(AtomicUsize::new(0));
        let i = Box::new(Payload("cleanup I", i_drops.clone()));
        let address = (&*i) as *const Payload;
        let LateTailFixture {
            target,
            owner,
            ledger,
            weak,
            driver,
            local,
            vm,
        } = late_tail_fixture(Some(i));
        let target = Rc::new(RefCell::new(Some(target)));
        let deleting = target.clone();
        let mut source = Store::default();
        let callback = Function::new_async(
            &mut source,
            wasmer_api::FunctionType::new([], []),
            move |_| {
                assert!(
                    wasmer_api::sys::WeakNativeInvocation::current()
                        .unwrap()
                        .is_current()
                );
                let target = deleting.borrow_mut().take().unwrap();
                assert_eq!(target.inner.strong_handles_for_test(), 1);
                unsafe { wasm_store_delete(Some(target)) };
                std::future::ready(Ok(Vec::new()))
            },
        );
        let asynchronous = source.into_async();
        let mut original = Box::pin(callback.call_async(&asynchronous, vec![]));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(std::future::poll_fn(|cx| {
            assert!(original.as_mut().poll(cx).is_pending());
            Poll::Ready(())
        }));
        assert!(target.borrow().is_none() && weak.upgrade().is_none());
        assert!(!ledger.final_done.get());
        assert_eq!(ledger.service_claims.get(), 1);
        assert_eq!((local.get(), vm.get()), (0, 0));
        // Original R returned; dropping its SAME live call now uses the existing
        // legal outer lexical fallback, without another driver or recovery poll.
        let p_drops = Arc::new(AtomicUsize::new(0));
        let exact = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if active_primary {
                let _original = original;
                std::panic::resume_unwind(Box::new(Payload("Rust P", p_drops.clone())));
            } else {
                drop(original);
            }
        }))
        .unwrap_err();
        assert_eq!(
            exact.downcast_ref::<Payload>().unwrap().0,
            if active_primary {
                "Rust P"
            } else {
                "cleanup I"
            }
        );
        if !active_primary {
            assert_eq!(
                exact.downcast_ref::<Payload>().unwrap() as *const _,
                address
            );
        }
        assert!(driver.upgrade().is_none());
        assert_eq!((local.get(), vm.get()), (1, 1));
        assert!(ledger.final_done.get() && ledger.snapshot().is_empty());
        assert_eq!(ledger.service_claims.get(), 0);
        assert!(owner.is_idle() && owner.is_sealed());
        assert_eq!(i_drops.load(Ordering::SeqCst), usize::from(active_primary));
        drop(exact);
        assert_eq!(i_drops.load(Ordering::SeqCst), 1);
        assert_eq!(p_drops.load(Ordering::SeqCst), usize::from(active_primary));
    }
    #[cfg(feature = "extensions")]
    #[test]
    fn late_finalization_original_caller_drop_retains_actual_cleanup_and_first_value() {
        late_tail_original_caller_cancel_profile(false);
    }
    #[cfg(feature = "extensions")]
    #[test]
    fn late_finalization_original_caller_drop_preserves_active_rust_primary() {
        late_tail_original_caller_cancel_profile(true);
    }

    #[cfg(feature = "extensions")]
    fn late_tail_observer_failure_profile(clone_failure: bool) {
        use std::{
            future::Future,
            sync::{
                Arc, Mutex,
                atomic::{AtomicBool, AtomicUsize, Ordering},
            },
            task::{RawWaker, RawWakerVTable, Waker},
        };
        struct Payload(Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        struct Observer {
            caller: Mutex<Option<Arc<Waker>>>,
            payload: Mutex<Option<RetirementPanic>>,
            armed: AtomicBool,
            clone_failure: bool,
            clones: AtomicUsize,
            drops: AtomicUsize,
            fired: AtomicBool,
            retry_wakes: AtomicUsize,
        }
        // Distinct identity forces actual subscriber replacement, while its
        // ordinary Drop cannot trigger the original observer's one-shot fault.
        struct RetryObserver(Arc<Observer>);
        impl std::task::Wake for RetryObserver {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &Arc<Self>) {
                forward(&self.0);
            }
        }
        unsafe fn clone_observer(data: *const ()) -> RawWaker {
            let state = unsafe { &*data.cast::<Observer>() };
            if state.clone_failure
                && state.armed.load(Ordering::SeqCst)
                && state.clones.fetch_add(1, Ordering::SeqCst) == 1
            {
                // First clone is core poll_tails' saved caller. SECOND is the
                // actual late-tail DriverSubscription observer, before H.poll.
                state.fired.store(true, Ordering::SeqCst);
                let exact = state.payload.lock().unwrap().take().unwrap();
                std::panic::resume_unwind(exact);
            }
            unsafe { Arc::increment_strong_count(data.cast::<Observer>()) };
            RawWaker::new(data, &OBSERVER)
        }
        unsafe fn wake_observer(data: *const ()) {
            let state = unsafe { Arc::from_raw(data.cast::<Observer>()) };
            forward(&state);
        }
        unsafe fn wake_observer_ref(data: *const ()) {
            forward(unsafe { &*data.cast::<Observer>() });
        }
        fn forward(state: &Observer) {
            if state.fired.load(Ordering::SeqCst) {
                state.retry_wakes.fetch_add(1, Ordering::SeqCst);
            }
            let wake = state.caller.lock().unwrap().as_ref().unwrap().clone();
            wake.wake_by_ref();
        }
        unsafe fn drop_observer(data: *const ()) {
            let state = unsafe { Arc::from_raw(data.cast::<Observer>()) };
            if !state.clone_failure
                && state.armed.load(Ordering::SeqCst)
                && state.drops.fetch_add(1, Ordering::SeqCst) == 1
            {
                // First Drop is the core saved caller; SECOND is the actual
                // subscription replacement, after the new observer is stored.
                state.fired.store(true, Ordering::SeqCst);
                let exact = state.payload.lock().unwrap().take().unwrap();
                std::panic::resume_unwind(exact);
            }
        }
        static OBSERVER: RawWakerVTable = RawWakerVTable::new(
            clone_observer,
            wake_observer,
            wake_observer_ref,
            drop_observer,
        );
        let payload_drops = Arc::new(AtomicUsize::new(0));
        let exact = Box::new(Payload(payload_drops.clone()));
        let address = (&*exact) as *const Payload;
        let state = Arc::new(Observer {
            caller: Mutex::new(None),
            payload: Mutex::new(Some(exact)),
            armed: AtomicBool::new(false),
            clone_failure,
            clones: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
            fired: AtomicBool::new(false),
            retry_wakes: AtomicUsize::new(0),
        });
        let LateTailFixture {
            target,
            owner,
            ledger,
            weak,
            driver,
            local,
            vm,
        } = late_tail_fixture(None);
        let target = Rc::new(RefCell::new(Some(target)));
        let deleting = target.clone();
        let arm = state.clone();
        let mut source = Store::default();
        let callback = Function::new_async(
            &mut source,
            wasmer_api::FunctionType::new([], []),
            move |_| {
                assert!(
                    wasmer_api::sys::WeakNativeInvocation::current()
                        .unwrap()
                        .is_current()
                );
                unsafe { wasm_store_delete(deleting.borrow_mut().take()) };
                if clone_failure {
                    arm.armed.store(true, Ordering::SeqCst);
                }
                std::future::ready(Ok(Vec::new()))
            },
        );
        let asynchronous = source.into_async();
        let mut original = Box::pin(callback.call_async(&asynchronous, vec![]));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let retry = Waker::from(Arc::new(RetryObserver(state.clone())));
        let mut first_poll = true;
        let exact = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            runtime.block_on(std::future::poll_fn(|cx| {
                let next = Arc::new(cx.waker().clone());
                let old = state.caller.lock().unwrap().replace(next);
                drop(old);
                let custom = clone_failure || first_poll;
                let result = if custom {
                    let wake = unsafe {
                        Waker::from_raw(RawWaker::new(
                            Arc::into_raw(state.clone()).cast(),
                            &OBSERVER,
                        ))
                    };
                    let result = original
                        .as_mut()
                        .poll(&mut std::task::Context::from_waker(&wake));
                    drop(wake); // BEFORE arming the later replacement-Drop probe
                    result
                } else {
                    original
                        .as_mut()
                        .poll(&mut std::task::Context::from_waker(&retry))
                };
                if first_poll {
                    assert!(result.is_pending());
                    if !clone_failure {
                        state.armed.store(true, Ordering::SeqCst);
                    }
                    first_poll = false;
                }
                result
            }))
        }))
        .unwrap_err();
        assert!(
            state.fired.load(Ordering::SeqCst),
            "fault must reach actual late observer"
        );
        assert!(
            state.retry_wakes.load(Ordering::SeqCst) > 0,
            "original R receives a real retry wake"
        );
        assert_eq!(
            exact.downcast_ref::<Payload>().unwrap() as *const _,
            address
        );
        assert_eq!(payload_drops.load(Ordering::SeqCst), 0);
        assert!(
            target.borrow().is_none() && weak.upgrade().is_none() && driver.upgrade().is_none()
        );
        assert_eq!((local.get(), vm.get()), (1, 1));
        assert!(ledger.final_done.get() && ledger.service_claims.get() == 0);
        assert_eq!(
            ledger.clock.get(),
            1,
            "observer error caught and stamped ONCE in canonical T carrier"
        );
        assert!(owner.is_idle() && owner.is_sealed());
        drop(exact);
        assert_eq!(payload_drops.load(Ordering::SeqCst), 1);
        drop(original);
    }
    #[cfg(feature = "extensions")]
    #[test]
    fn late_finalization_observer_clone_failure_retries_original_caller_and_preserves_first() {
        late_tail_observer_failure_profile(true);
    }
    #[cfg(feature = "extensions")]
    #[test]
    fn late_finalization_observer_replacement_drop_failure_retries_and_preserves_first() {
        late_tail_observer_failure_profile(false);
    }

    #[allow(
        clippy::drop_non_drop,
        reason = "Preserve the frozen a708 public oracle body and its explicit caller handle lifetime boundary"
    )]
    #[cfg(all(unix, feature = "extensions"))]
    #[test]
    fn active_foreign_caller_without_target_tail_drives_last_public_store_cleanup() {
        use std::{future::Future, io::Write, task::Poll};
        use wasmer_api::sys::{NativeAncestrySettlement, WeakNativeInvocation};
        const CHILD: &str = "WASMER_CAPI_FOREIGN_NO_TAIL_FINAL_STORE_CHILD";
        const TEST: &str = "wasm_c_api::store::tests::active_foreign_caller_without_target_tail_drives_last_public_store_cleanup";
        struct Directory(std::path::PathBuf);
        impl Drop for Directory {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        #[derive(Clone)]
        struct Trace(std::sync::Arc<std::path::PathBuf>);
        impl Trace {
            fn record(&self, stage: &str) {
                let mut file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(self.0.as_path())
                    .unwrap();
                writeln!(file, "{stage}").unwrap();
            }
        }
        let directory = if let Some(path) = std::env::var_os(CHILD) {
            std::path::PathBuf::from(path)
        } else {
            // Match the existing public active-caller oracle: only this owned
            // child is killed/reaped if synchronous deletion blocks its caller.
            struct OwnedChild(std::process::Child);
            impl Drop for OwnedChild {
                fn drop(&mut self) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
            let directory = Directory(std::env::temp_dir().join(format!(
                "wasmer-capi-foreign-no-tail-final-store-{}",
                std::process::id()
            )));
            std::fs::create_dir(&directory.0).unwrap();
            let mut child = OwnedChild(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture"])
                    .env(CHILD, &directory.0)
                    .spawn()
                    .unwrap(),
            );
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                if let Some(status) = child.0.try_wait().unwrap() {
                    let stages = std::fs::read_to_string(directory.0.join("stages"))
                        .unwrap_or_else(|e| format!("no stages: {e}"));
                    assert!(
                        status.success(),
                        "no-tail final Store cleanup failed: {status}\n{stages}"
                    );
                    assert!(
                        stages.contains("all-no-tail-final-store-fences-completed"),
                        "child must execute the actual complete oracle: {stages}"
                    );
                    return;
                }
                if std::time::Instant::now() >= deadline {
                    child.0.kill().unwrap();
                    let status = child.0.wait().unwrap();
                    let stages = std::fs::read_to_string(directory.0.join("stages"))
                        .unwrap_or_else(|e| format!("no stages: {e}"));
                    panic!(
                        "no-tail final Store cleanup did not complete in 15s: {status}\n{stages}"
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        let trace = Trace(std::sync::Arc::new(directory.join("stages")));
        trace.record("child-started");
        struct CleanupOnDrop {
            owner: wasmer_api::sys::NativeCleanupOwner,
            store: WeakStoreRef,
            trace: Trace,
            name: &'static str,
            complete: Rc<std::cell::Cell<bool>>,
        }
        impl Drop for CleanupOnDrop {
            fn drop(&mut self) {
                assert!(self.store.upgrade().is_none());
                self.trace
                    .record(&format!("{}-destructor-weak-expired", self.name));
                let trace = self.trace.clone();
                let name = self.name;
                let complete = self.complete.clone();
                self.owner
                    .try_retain(async move {
                        trace.record(&format!("{name}-before-yield-1"));
                        tokio::task::yield_now().await;
                        trace.record(&format!("{name}-after-yield-1-before-timer"));
                        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                        trace.record(&format!("{name}-after-timer-before-yield-2"));
                        tokio::task::yield_now().await;
                        complete.set(true);
                        trace.record(&format!("{name}-completed"));
                    })
                    .unwrap_or_else(|_| panic!("actual resource destructor cleanup refused"));
            }
        }
        unsafe extern "C" fn callback(
            _: *mut std::ffi::c_void,
            _: &wasm_val_vec_t,
            _: &mut wasm_val_vec_t,
        ) -> Option<Box<wasm_trap_t>> {
            None
        }
        unsafe extern "C" fn finalize(data: *mut std::ffi::c_void) {
            drop(unsafe { Box::from_raw(data.cast::<CleanupOnDrop>()) });
        }
        let engine = crate::wasm_c_api::engine::wasm_engine_new();
        let mut target = unsafe { wasm_store_new(Some(&engine)) }.unwrap();
        let weak_target = target.inner.downgrade();
        let owner = target.inner.cleanup_owner().unwrap();
        let local_completed = Rc::new(std::cell::Cell::new(false));
        let vm_completed = Rc::new(std::cell::Cell::new(false));
        let started = Rc::new(std::cell::Cell::new(false));
        let local = CleanupOnDrop {
            owner: owner.clone(),
            store: weak_target.clone(),
            trace: trace.clone(),
            name: "local",
            complete: local_completed.clone(),
        };
        let task_started = started.clone();
        let task_trace = trace.clone();
        // Seed one real T LocalSet resource before R starts. This helper neither
        // enrolls retirement nor exports a private Driver to the caller.
        target
            .inner
            .run_host(async move {
                tokio::task::spawn_local(async move {
                    let _local = local;
                    task_started.set(true);
                    task_trace.record("local-task-actually-started");
                    std::future::pending::<()>().await;
                });
                tokio::task::yield_now().await;
            })
            .unwrap();
        assert!(started.get());
        let data = Box::into_raw(Box::new(CleanupOnDrop {
            owner: owner.clone(),
            store: weak_target.clone(),
            trace: trace.clone(),
            name: "vm",
            complete: vm_completed.clone(),
        }));
        let ty = wasm_functype_t::new(wasmer_api::FunctionType::new([], []));
        let function = unsafe {
            wasm_func_new_with_env(
                Some(&mut target),
                Some(&ty),
                Some(callback),
                data.cast(),
                Some(finalize),
            )
        }
        .unwrap();
        drop(function); // only T's actual VM arena retains this public environment
        assert!(!vm_completed.get());
        let target = Rc::new(RefCell::new(Some(target)));
        let deletion_slot = target.clone();
        let deletion_trace = trace.clone();
        let deletion_weak = weak_target.clone();
        let ancestry = Rc::new(RefCell::new(None));
        let capture_ancestry = ancestry.clone();
        let mut source = Store::default();
        // R owns this exact ordinary public call_async future on S. Its native
        // callback deletes unrelated T; no target call or retirement precedes it.
        let caller = Function::new_async(
            &mut source,
            wasmer_api::FunctionType::new([], [wasmer_api::Type::I32]),
            move |_| {
                let deletion_slot = deletion_slot.clone();
                let trace = deletion_trace.clone();
                let weak = deletion_weak.clone();
                let capture_ancestry = capture_ancestry.clone();
                let actual = WeakNativeInvocation::current().expect("actual original S root");
                assert!(actual.is_current());
                let physical = NativeAncestrySettlement::current(None).expect("actual S ancestry");
                *capture_ancestry.borrow_mut() = Some(physical);
                let store = deletion_slot.borrow_mut().take().unwrap();
                assert_eq!(store.inner.strong_handles_for_test(), 1);
                let StoreMode::Native(native) = &store.inner.inner.mode else {
                    unreachable!()
                };
                assert!(native.driver.borrow().is_some());
                assert!(
                    native.ledger.snapshot().is_empty(),
                    "T has no enrolled retirement"
                );
                assert!(
                    native.finalization.borrow().upgrade().is_none(),
                    "no pre-enrolled T handoff"
                );
                trace.record("actual-S-root-no-T-tail-delete-sole-public-store-before");
                unsafe { wasm_store_delete(Some(store)) };
                assert!(weak.upgrade().is_none());
                trace.record("public-delete-returned-weak-expired");
                std::future::ready(Ok(vec![Value::I32(37)]))
            },
        );
        let asynchronous = source.into_async();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut polls = 0;
        let result = runtime
            .block_on(async {
                // A real scheduler yield precedes the call. R's original future is
                // kept alive and polled in place, never replaced by a cleanup task.
                tokio::task::yield_now().await;
                let mut original = Box::pin(caller.call_async(&asynchronous, vec![]));
                std::future::poll_fn(|cx| {
                    polls += 1;
                    trace.record(&format!("R-poll-original-caller-{polls}"));
                    let result = original.as_mut().poll(cx);
                    if result.is_ready() {
                        trace.record("original-caller-completed");
                    }
                    result
                })
                .await
            })
            .unwrap();
        assert_eq!(result[0].i32(), Some(37));
        assert!(target.borrow().is_none() && weak_target.upgrade().is_none());
        assert!(
            local_completed.get(),
            "actual LocalSet destructor cleanup finished"
        );
        assert!(
            vm_completed.get(),
            "actual VM environment destructor cleanup finished"
        );
        assert!(owner.is_idle());
        let mut physical = ancestry.borrow_mut().take().unwrap();
        assert!(runtime.block_on(std::future::poll_fn(|cx| {
            match std::pin::Pin::new(&mut physical).poll(cx) {
                Poll::Ready(()) => Poll::Ready(true),
                Poll::Pending => Poll::Pending,
            }
        })));
        drop(physical);
        drop(caller);
        drop(asynchronous);
        trace.record("all-no-tail-final-store-fences-completed");
    }

    #[cfg(feature = "extensions")]
    fn borrowed_capi_result(function: &mut wasm_func_t) -> (Option<Box<wasm_trap_t>>, i32) {
        use crate::wasm_c_api::{types::wasm_valkind_enum, value::wasm_val_inner};
        let args: wasm_val_vec_t = Vec::<wasm_val_t>::new().into();
        let mut results: wasm_val_vec_t = vec![wasm_val_t {
            kind: wasm_valkind_enum::WASM_I32 as u8,
            of: wasm_val_inner { int32_t: -1973 },
        }]
        .into();
        let trap = unsafe { wasm_func_call(Some(function), Some(&args), &mut results) };
        (trap, unsafe { results.as_slice()[0].of.int32_t })
    }
    #[cfg(feature = "extensions")]
    #[test]
    fn borrowed_capi_ready_host_pending_exact_disposal_trap_and_store_reuse() {
        use std::{
            future::Future,
            pin::Pin,
            task::{Context, Poll},
        };
        struct PendingBody {
            polls: Rc<std::cell::Cell<usize>>,
            drops: Rc<std::cell::Cell<usize>>,
        }
        impl Future for PendingBody {
            type Output = Result<Vec<Value>, RuntimeError>;
            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Self::Output> {
                self.polls.set(self.polls.get() + 1);
                Poll::Pending
            }
        }
        impl Drop for PendingBody {
            fn drop(&mut self) {
                self.drops.set(self.drops.get() + 1);
            }
        }
        let engine = crate::wasm_c_api::engine::wasm_engine_new();
        let store = unsafe { wasm_store_new(Some(&engine)) }.unwrap();
        let ready_count = Rc::new(std::cell::Cell::new(0));
        let ready_observed = ready_count.clone();
        let ready = store
            .inner
            .with_mut(|view| {
                Function::new_async(
                    view,
                    wasmer_api::FunctionType::new([], [wasmer_api::Type::I32]),
                    move |_| {
                        let observed = ready_observed.clone();
                        async move {
                            assert!(!wasmer_api::sys::native_invocation_active());
                            observed.set(observed.get() + 1);
                            Ok(vec![Value::I32(59)])
                        }
                    },
                )
            })
            .unwrap();
        let polls = Rc::new(std::cell::Cell::new(0));
        let drops = Rc::new(std::cell::Cell::new(0));
        let p = polls.clone();
        let d = drops.clone();
        let pending = store
            .inner
            .with_mut(|view| {
                Function::new_async(
                    view,
                    wasmer_api::FunctionType::new([], [wasmer_api::Type::I32]),
                    move |_| PendingBody {
                        polls: p.clone(),
                        drops: d.clone(),
                    },
                )
            })
            .unwrap();
        let trapped = store
            .inner
            .with_mut(|view| {
                let module = Module::new(
                    view,
                    "(module (func (export \"fail\") (result i32) unreachable))",
                )
                .unwrap();
                Instance::new(view, &module, &wasmer_api::imports! {})
                    .unwrap()
                    .exports
                    .get_function("fail")
                    .unwrap()
                    .clone()
            })
            .unwrap();
        let mut ready = wasm_func_t {
            extern_: wasm_extern_t::new(store.inner.clone(), ready.into()),
        };
        let mut pending = wasm_func_t {
            extern_: wasm_extern_t::new(store.inner.clone(), pending.into()),
        };
        let mut trapped = wasm_func_t {
            extern_: wasm_extern_t::new(store.inner.clone(), trapped.into()),
        };
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (error, value) = borrowed_capi_result(&mut ready);
            assert!(error.is_none());
            assert_eq!(value, 59);
            let (error, value) = borrowed_capi_result(&mut pending);
            assert_eq!(
                error.unwrap().inner.to_trap(),
                Some(wasmer_types::TrapCode::YieldOutsideAsyncContext)
            );
            assert_eq!(value, -1973, "controlled Pending must not write results");
            assert_eq!((polls.get(), drops.get()), (1, 1));
            let (error, value) = borrowed_capi_result(&mut trapped);
            assert_eq!(
                error.unwrap().inner.to_trap(),
                Some(wasmer_types::TrapCode::UnreachableCodeReached)
            );
            assert_eq!(value, -1973);
            let (error, value) = borrowed_capi_result(&mut ready);
            assert!(error.is_none());
            assert_eq!(value, 59);
            assert_eq!(ready_count.get(), 2);
            assert!(Store::with_current(|_| ()).is_none());
            assert!(!wasmer_api::sys::native_invocation_active());
            store.inner.with_mut(|_| ()).unwrap();
        });
    }

    #[cfg(feature = "extensions")]
    fn borrowed_capi_guest(
        store: &mut wasm_store_t,
        wat: &str,
        imports: wasm_extern_vec_t,
    ) -> wasm_func_t {
        use crate::wasm_c_api::{module::wasm_module_new, types::wasm_byte_vec_t};
        let bytes: wasm_byte_vec_t = wat::parse_str(wat).unwrap().into();
        let module = unsafe { wasm_module_new(Some(store), Some(&bytes)) }.unwrap();
        let instance =
            unsafe { wasm_instance_new(Some(store), Some(&module), Some(&imports), None) }.unwrap();
        wasm_func_t {
            extern_: wasm_extern_t::new(
                store.inner.clone(),
                instance
                    .inner
                    .exports
                    .get_function("run")
                    .unwrap()
                    .clone()
                    .into(),
            ),
        }
    }
    #[cfg(feature = "extensions")]
    fn borrowed_public_foreign_chain(trap: bool) {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        };
        struct Other {
            target: WeakStoreRef,
            function: Function,
            parent: wasmer_types::StoreId,
            observations: Arc<AtomicBool>,
            calls: Arc<AtomicUsize>,
        }
        unsafe extern "C" fn bridge(
            data: *mut std::ffi::c_void,
            args: &wasm_val_vec_t,
            results: &mut wasm_val_vec_t,
        ) -> Option<Box<wasm_trap_t>> {
            let data = unsafe { &*data.cast::<Other>() };
            let before = Store::with_current(|store| store.objects_mut().id()) == Some(data.parent);
            let Some(target) = data.target.upgrade() else {
                return Some(Box::new(
                    RuntimeError::new("test foreign Store disappeared").into(),
                ));
            };
            let mut function = wasm_func_t {
                extern_: wasm_extern_t::new(target, data.function.clone().into()),
            };
            let result = unsafe { wasm_func_call(Some(&mut function), Some(args), results) };
            let after = Store::with_current(|store| store.objects_mut().id()) == Some(data.parent);
            data.observations.fetch_and(
                before && after && !wasmer_api::sys::native_invocation_active(),
                Ordering::SeqCst,
            );
            data.calls.fetch_add(1, Ordering::SeqCst);
            result
        }
        unsafe extern "C" fn finalize(data: *mut std::ffi::c_void) {
            drop(unsafe { Box::from_raw(data.cast::<Other>()) });
        }
        fn callback(
            store: &mut wasm_store_t,
            target: &wasm_store_t,
            function: &wasm_func_t,
            observations: &Arc<AtomicBool>,
            calls: &Arc<AtomicUsize>,
        ) -> Box<wasm_func_t> {
            let data = Box::into_raw(Box::new(Other {
                target: target.inner.downgrade(),
                function: function.extern_.function(),
                parent: store.inner.inner.id,
                observations: observations.clone(),
                calls: calls.clone(),
            }));
            let ty =
                wasm_functype_t::new(wasmer_api::FunctionType::new([], [wasmer_api::Type::I32]));
            unsafe {
                wasm_func_new_with_env(
                    Some(store),
                    Some(&ty),
                    Some(bridge),
                    data.cast(),
                    Some(finalize),
                )
            }
            .unwrap()
        }
        let engine = crate::wasm_c_api::engine::wasm_engine_new();
        let mut a = unsafe { wasm_store_new(Some(&engine)) }.unwrap();
        let mut b = unsafe { wasm_store_new(Some(&engine)) }.unwrap();
        let mut leaf = borrowed_capi_guest(
            &mut a,
            if trap {
                "(module (func (export \"run\") (result i32) unreachable))"
            } else {
                "(module (func (export \"run\") (result i32) i32.const 37))"
            },
            Vec::<Option<Box<wasm_extern_t>>>::new().into(),
        );
        let observations = Arc::new(AtomicBool::new(true));
        let calls = Arc::new(AtomicUsize::new(0));
        let b_bridge = callback(&mut b, &a, &leaf, &observations, &calls);
        let b_guest = borrowed_capi_guest(
            &mut b,
            "(module (import \"test\" \"bridge\" (func $b (result i32))) (func (export \"run\") (result i32) call $b i32.const 5 i32.add))",
            vec![Some(Box::new(b_bridge.extern_.clone()))].into(),
        );
        let a_bridge = callback(&mut a, &b, &b_guest, &observations, &calls);
        let mut outer = borrowed_capi_guest(
            &mut a,
            "(module (import \"test\" \"bridge\" (func $b (result i32))) (func (export \"run\") (result i32) call $b i32.const 10 i32.add))",
            vec![Some(Box::new(a_bridge.extern_.clone()))].into(),
        );
        let mut reuse = borrowed_capi_guest(
            &mut a,
            "(module (func (export \"run\") (result i32) i32.const 61))",
            Vec::<Option<Box<wasm_extern_t>>>::new().into(),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (error, value) = borrowed_capi_result(&mut outer);
            if trap {
                assert_eq!(
                    error.unwrap().inner.to_trap(),
                    Some(wasmer_types::TrapCode::UnreachableCodeReached)
                );
                assert_eq!(
                    value, -1973,
                    "no old outer guest code resumes after callee trap"
                );
            } else {
                assert!(
                    error.is_none(),
                    "{:?}",
                    error.map(|error| error.inner.message())
                );
                assert_eq!(value, 52);
            }
            assert!(observations.load(Ordering::SeqCst));
            assert_eq!(calls.load(Ordering::SeqCst), 2);
            assert!(Store::with_current(|_| ()).is_none());
            let (error, value) = borrowed_capi_result(&mut reuse);
            assert!(error.is_none());
            assert_eq!(value, 61);
            a.inner.with_mut(|_| ()).unwrap();
            b.inner.with_mut(|_| ()).unwrap();
            assert!(!wasmer_api::sys::native_invocation_active());
            // Leaf remains a real public handle after the outer chain too.
            if !trap {
                assert_eq!(borrowed_capi_result(&mut leaf).1, 37);
            }
        });
    }
    #[cfg(feature = "extensions")]
    #[test]
    fn borrowed_public_cross_store_a_b_a_returns_values_and_restores_parents() {
        borrowed_public_foreign_chain(false);
    }
    #[cfg(feature = "extensions")]
    #[test]
    fn borrowed_public_cross_store_a_b_a_trap_unwinds_and_preserves_store_reuse() {
        borrowed_public_foreign_chain(true);
    }

    #[test]
    fn entered_runtime_final_store_retirement_drains_timer_cleanup_from_both_drop_phases() {
        struct TimerOnDrop {
            owner: wasmer_api::sys::NativeCleanupOwner,
            complete: Rc<std::cell::Cell<bool>>,
        }
        impl Drop for TimerOnDrop {
            fn drop(&mut self) {
                let complete = self.complete.clone();
                self.owner
                    .retain(Box::pin(async move {
                        // Construct the timer on a later retained poll, after the
                        // cancellation/finalizer's own runtime guard has ended.
                        tokio::task::yield_now().await;
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                        complete.set(true);
                    }))
                    .unwrap_or_else(|_| panic!("test cleanup admission was refused"));
            }
        }
        unsafe extern "C" fn callback(
            _: *mut std::ffi::c_void,
            _: &wasm_val_vec_t,
            _: &mut wasm_val_vec_t,
        ) -> Option<Box<wasm_trap_t>> {
            None
        }
        unsafe extern "C" fn finalize(data: *mut std::ffi::c_void) {
            // This C API Store and its user environment stay on this thread.
            drop(unsafe { Box::from_raw(data.cast::<TimerOnDrop>()) });
        }
        let mut store = wasm_store_t {
            inner: StoreRef::new(Store::default()).unwrap(),
        };
        let owner = match &store.inner.inner.mode {
            StoreMode::Native(native) => native.owner.clone(),
            StoreMode::Plain(_) => unreachable!(),
        };
        let cancellation = Rc::new(std::cell::Cell::new(false));
        let vm_finalizer = Rc::new(std::cell::Cell::new(false));
        let local_guard = TimerOnDrop {
            owner: owner.clone(),
            complete: cancellation.clone(),
        };
        store
            .inner
            .run_host(async move {
                tokio::task::spawn_local(async move {
                    let _guard = local_guard;
                    std::future::pending::<()>().await;
                });
            })
            .unwrap();
        let data = Box::into_raw(Box::new(TimerOnDrop {
            owner: owner.clone(),
            complete: vm_finalizer.clone(),
        }));
        let ty = wasm_functype_t::new(wasmer_api::FunctionType::new([], []));
        let function = unsafe {
            wasm_func_new_with_env(
                Some(&mut store),
                Some(&ty),
                Some(callback),
                data.cast(),
                Some(finalize),
            )
        }
        .unwrap();
        drop(function);
        assert!(
            !vm_finalizer.get(),
            "the VM arena still owns the environment"
        );
        let foreign = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        {
            let _entered = foreign.enter();
            drop(store);
        }
        assert!(
            cancellation.get(),
            "cancellation-enqueued timer must finish"
        );
        assert!(
            vm_finalizer.get(),
            "VM-finalizer-enqueued timer must finish"
        );
        assert!(owner.is_idle());
    }

    #[test]
    fn entered_runtime_final_store_retirement_keeps_custody_until_the_first_panic_is_resumed() {
        let store = StoreRef::new(Store::default()).unwrap();
        store.run_host(async {}).unwrap();
        let (owner, runtime) = match &store.inner.mode {
            StoreMode::Native(native) => (
                native.owner.clone(),
                std::sync::Arc::downgrade(&native.driver.borrow().as_ref().unwrap().runtime),
            ),
            StoreMode::Plain(_) => unreachable!(),
        };
        owner
            .retain(Box::pin(async {
                for _ in 0..16 {
                    tokio::task::yield_now().await;
                }
                panic!("retirement panic sentinel");
            }))
            .unwrap_or_else(|_| panic!("test cleanup admission was refused"));
        let foreign = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let panic = {
            let _entered = foreign.enter();
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)))
        }
        .expect_err("the original cleanup panic must be resumed");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"retirement panic sentinel")
        );
        assert!(
            runtime.upgrade().is_none(),
            "the owned runtime must be destroyed first"
        );
        assert!(owner.is_idle());
    }

    #[test]
    fn final_store_retirement_cancels_unrelated_pending_local_work() {
        struct Dropped(Rc<std::cell::Cell<bool>>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.set(true);
            }
        }
        let store = StoreRef::new(Store::default()).unwrap();
        let weak = store.downgrade();
        let dropped = Rc::new(std::cell::Cell::new(false));
        let guard = Dropped(dropped.clone());
        store
            .run_host(async move {
                tokio::task::spawn_local(async move {
                    let _guard = guard;
                    for _ in 0..256 {
                        tokio::task::yield_now().await;
                    }
                    std::future::pending::<()>().await;
                });
            })
            .unwrap();
        drop(store);
        assert!(weak.upgrade().is_none());
        assert!(dropped.get(), "unrelated pending work must be cancelled");
    }

    #[test]
    fn final_store_retirement_pumps_owner_continuations_needed_by_managed_local_work() {
        let store = StoreRef::new(Store::default()).unwrap();
        let StoreMode::Native(native) = &store.inner.mode else {
            panic!("native test requires the sys backend");
        };
        let owner = native.owner.clone();
        let gate = Rc::new(std::cell::Cell::new(false));
        let waiter = Rc::new(RefCell::new(None::<std::task::Waker>));
        let (send, receive) = tokio::sync::oneshot::channel();
        let owner_gate = gate.clone();
        let owner_waiter = waiter.clone();
        owner
            .retain(Box::pin(async move {
                std::future::poll_fn(|cx| {
                    if owner_gate.get() {
                        std::task::Poll::Ready(())
                    } else {
                        *owner_waiter.borrow_mut() = Some(cx.waker().clone());
                        std::task::Poll::Pending
                    }
                })
                .await;
                send.send(()).unwrap();
            }))
            .unwrap_or_else(|_| panic!("test cleanup admission was refused"));
        let completed = Rc::new(std::cell::Cell::new(false));
        let fence = completed.clone();
        store
            .register_cleanup_fence(Box::new(move || fence.get()))
            .unwrap();
        let task_completed = completed.clone();
        store
            .run_host(async move {
                tokio::task::spawn_local(async move {
                    // Exceed a LocalSet poll's budget: progress must come from its
                    // real wake signal, not a later unrelated native call.
                    for _ in 0..256 {
                        tokio::task::yield_now().await;
                    }
                    gate.set(true);
                    let wake = waiter.borrow_mut().take();
                    if let Some(wake) = wake {
                        wake.wake();
                    }
                    receive.await.unwrap();
                    task_completed.set(true);
                });
            })
            .unwrap();
        drop(store);
        assert!(
            completed.get(),
            "managed work must finish before cancellation"
        );
        assert!(owner.is_idle());
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn native_post_tail_returns_to_the_source_scheduler_for_target_yield_and_timer() {
        let target = StoreRef::new(Store::default()).unwrap();
        let retirement = target.retirement_owner().unwrap();
        let owner = target.cleanup_owner().unwrap();
        let completed = Rc::new(std::cell::Cell::new(false));
        let cleaned = completed.clone();
        let completion = retirement.clone();
        retirement.retain(Box::pin(async move {
            let (send, receive) = tokio::sync::oneshot::channel();
            tokio::task::spawn_local(async move {
                // This wake belongs to source current-thread scheduler R even
                // while the target runtime's handle is entered for this poll.
                tokio::task::yield_now().await;
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                cleaned.set(true);
                let _ = send.send(());
                completion.notify_completion();
            });
            let _ = receive.await;
        }));
        let weak = target.downgrade();
        let enrolled = completed.clone();
        let mut source = Store::default();
        let function = Function::new_async(
            &mut source,
            wasmer_api::FunctionType::new([], [wasmer_api::Type::I32]),
            move |_| {
                weak.upgrade()
                    .unwrap()
                    .enroll_native_retirement(enrolled.clone())
                    .unwrap();
                std::future::ready(Ok(vec![Value::I32(37)]))
            },
        );
        let source = source.into_async();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let output = runtime
            .block_on(function.call_async(&source, vec![]))
            .unwrap();
        assert_eq!(output[0].i32(), Some(37));
        assert!(completed.get());
        assert!(owner.is_idle());
        assert_eq!(Rc::strong_count(&target.inner), 1);
        drop(retirement);
    }

    #[test]
    fn native_post_tail_fanout_wakes_every_caller_before_resuming_a_waker_panic() {
        struct Caller {
            calls: std::sync::atomic::AtomicUsize,
            panics: bool,
        }
        impl std::task::Wake for Caller {
            fn wake(self: std::sync::Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &std::sync::Arc<Self>) {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                assert!(!self.panics, "one caller Waker panicked");
            }
        }
        let failing = std::sync::Arc::new(Caller {
            calls: Default::default(),
            panics: true,
        });
        let sibling = std::sync::Arc::new(Caller {
            calls: Default::default(),
            panics: false,
        });
        let wake = std::sync::Arc::new(DriverWake::default());
        {
            let mut state = wake.state.lock().unwrap();
            for (token, caller) in [failing.clone(), sibling.clone()].into_iter().enumerate() {
                state.subscribers.insert(
                    token as u64,
                    Some(std::sync::Arc::new(std::task::Waker::from(caller))),
                );
            }
        }
        let waker = std::task::Waker::from(wake);
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| waker.wake_by_ref()))
            .expect_err("the caller panic is preserved after fanout");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"one caller Waker panicked")
        );
        assert_eq!(failing.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(sibling.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn native_post_tail_two_local_callers_share_one_target_without_stranding_the_first() {
        let target = StoreRef::new(Store::default()).unwrap();
        let retirement = target.retirement_owner().unwrap();
        let first_done = Rc::new(std::cell::Cell::new(false));
        let second_done = Rc::new(std::cell::Cell::new(false));
        let mut releases = Vec::new();
        let mut callers = Vec::new();
        let mut entered = Vec::new();
        for done in [&first_done, &second_done] {
            let (release, receive) = tokio::sync::oneshot::channel();
            releases.push(release);
            let cleaned = done.clone();
            let completion = retirement.clone();
            retirement.retain(Box::pin(async move {
                let task = tokio::task::spawn_local(async move {
                    receive.await.unwrap();
                    tokio::task::yield_now().await;
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    cleaned.set(true);
                    completion.notify_completion();
                });
                task.await.unwrap();
            }));
            let (started, observed) = tokio::sync::oneshot::channel();
            entered.push(observed);
            let started = RefCell::new(Some(started));
            let weak = target.downgrade();
            let done = done.clone();
            let mut source = Store::default();
            let function = Function::new_async(
                &mut source,
                wasmer_api::FunctionType::new([], [wasmer_api::Type::I32]),
                move |_| {
                    weak.upgrade()
                        .unwrap()
                        .enroll_native_retirement(done.clone())
                        .unwrap();
                    started.borrow_mut().take().unwrap().send(()).unwrap();
                    std::future::ready(Ok(vec![Value::I32(37)]))
                },
            );
            callers.push((function, source.into_async()));
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let source_tasks = tokio::task::LocalSet::new();
        runtime.block_on(source_tasks.run_until(async {
            let mut calls = callers.into_iter().map(|(function, source)| {
                tokio::task::spawn_local(async move {
                    function.call_async(&source, vec![]).await.unwrap()
                })
            });
            let first = calls.next().unwrap();
            let second = calls.next().unwrap();
            for observed in entered {
                observed.await.unwrap();
            }
            // Both actual source tasks have reached their pending tails. The
            // first target job must wake its caller while the second stays held.
            let mut releases = releases.into_iter();
            releases.next().unwrap().send(()).unwrap();
            let first = tokio::time::timeout(std::time::Duration::from_secs(5), first)
                .await
                .expect("the later caller must not replace the first caller's wake")
                .unwrap();
            assert_eq!(first[0].i32(), Some(37));
            assert!(first_done.get());
            assert!(!second_done.get());
            assert!(!second.is_finished());
            assert_eq!(
                retirement
                    .driver
                    .wake
                    .state
                    .lock()
                    .unwrap()
                    .subscribers
                    .len(),
                1,
                "the completed caller must not stay retained by unrelated work"
            );
            releases.next().unwrap().send(()).unwrap();
            let second = tokio::time::timeout(std::time::Duration::from_secs(5), second)
                .await
                .expect("the remaining caller keeps its own subscription")
                .unwrap();
            assert_eq!(second[0].i32(), Some(37));
        }));
        assert!(second_done.get());
        assert!(
            retirement
                .driver
                .wake
                .state
                .lock()
                .unwrap()
                .subscribers
                .is_empty()
        );
        assert!(target.cleanup_owner().unwrap().is_idle());
        assert_eq!(Rc::strong_count(&target.inner), 1);
        drop(retirement);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn settled_unacknowledged_failure_is_owned_through_legal_last_store_retirement() {
        let store = StoreRef::new(Store::default()).unwrap();
        let weak_store = store.downgrade();
        let owner = store.cleanup_owner().unwrap();
        let weak_driver = match &store.inner.mode {
            StoreMode::Native(native) => Rc::downgrade(&native.driver().unwrap()),
            StoreMode::Plain(_) => unreachable!(),
        };
        let failure = store.retirement_failure().unwrap();
        failure.record(Box::new("legal last Store pending first error"));
        let state = store
            .register_retirement(Rc::new(std::cell::Cell::new(true)), failure)
            .unwrap();
        store.retain_delivery(state.clone()).unwrap();
        store.retirement_release(state.clone()).unwrap().release();
        assert!(state.physically_ready());
        assert!(!state.delivery_ack.get());
        assert!(
            !owner.is_idle(),
            "unobserved value remains owned by exact metadata receipt"
        );
        drop(state); // The existing queue, not a test pin, owns the sole value.
        let panic =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store))).unwrap_err();
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"legal last Store pending first error")
        );
        assert!(weak_store.upgrade().is_none());
        assert!(
            weak_driver.upgrade().is_none(),
            "no escaped private Driver Rc at final destruction"
        );
        assert!(
            owner.is_idle(),
            "ack receipt disposed before final Driver destruction"
        );
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn native_tail_waits_for_after_pump_receipt_and_preserves_its_real_release_wake() {
        struct Counter(std::sync::atomic::AtomicUsize);
        impl std::task::Wake for Counter {
            fn wake(self: std::sync::Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &std::sync::Arc<Self>) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        }
        let store = StoreRef::new(Store::default()).unwrap();
        let owner = store.retirement_owner().unwrap();
        let state = store
            .register_retirement(
                Rc::new(std::cell::Cell::new(true)),
                RetirementFailure::default(),
            )
            .unwrap();
        store.retain_delivery(state.clone()).unwrap();
        let tail = Rc::new(RefCell::new(Box::pin(NativeRetirementTail {
            subscription: Some(owner.subscribe()),
            _store: store.downgrade(),
            base: Some(store.async_store().unwrap()),
            owner: Some(owner.clone()),
            cleanup: Some(owner.owner.clone()),
            release: Some(store.retirement_release(state.clone()).unwrap()),
            first: store.retirement_failure().unwrap(),
            finalization_subscription: None,
            finalization: store.finalization_handoff(&owner),
            exit: None,
            state: state.clone(),
            registration: Some({
                state.disposition.set(DeliveryDisposition::LiveTail);
                RetirementRegistration {
                    state: state.clone(),
                    armed: true,
                }
            }),
        })));
        let counter = std::sync::Arc::new(Counter(std::sync::atomic::AtomicUsize::new(0)));
        let waker = std::task::Waker::from(counter.clone());
        let first = tail.clone();
        let first_waker = waker.clone();
        let release = state.clone();
        owner.owner.after_pump_existing(Box::new(move || {
            let mut cx = std::task::Context::from_waker(&first_waker);
            assert!(first.borrow_mut().as_mut().poll(&mut cx).is_pending());
            assert!(
                !release.tail_receipt_released.get(),
                "queued release callback has not run while callbacks_running"
            );
        }));
        assert!(state.tail_receipt_released.get());
        assert!(
            counter.0.load(std::sync::atomic::Ordering::SeqCst) > 0,
            "actual release schedules the awaiting original caller"
        );
        assert!(
            tail.borrow_mut()
                .as_mut()
                .poll(&mut std::task::Context::from_waker(&waker))
                .is_ready()
        );
        assert!(store.retirement_states_for_test().is_empty());
        drop(tail);
        drop(owner); // Never escape a private Driver Rc through last Store drop.
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn release_wake_reentrantly_polls_the_live_tail_after_readiness_publication() {
        struct PollLive {
            tail: Rc<RefCell<std::pin::Pin<Box<NativeRetirementTail>>>>,
            ready: Rc<std::cell::Cell<Option<bool>>>,
        }
        unsafe extern "C" fn poll_live(
            data: *mut std::ffi::c_void,
            _: &wasm_val_vec_t,
            _: &mut wasm_val_vec_t,
        ) -> Option<Box<wasm_trap_t>> {
            let control = unsafe { &*data.cast::<PollLive>() };
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            let ready = control.tail.borrow_mut().as_mut().poll(&mut cx).is_ready();
            control.ready.set(Some(ready));
            None
        }
        unsafe extern "C" fn delete_control(data: *mut std::ffi::c_void) {
            drop(unsafe { Box::from_raw(data.cast::<PollLive>()) });
        }
        struct Reentrant {
            owner: std::thread::ThreadId,
            store: WeakStoreRef,
            callback: Function,
        }
        impl std::task::Wake for Reentrant {
            fn wake(self: std::sync::Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &std::sync::Arc<Self>) {
                // Waker carries only existing Send handles. Rc test state stays
                // behind the ordinary C callback environment on its owner thread.
                assert_eq!(std::thread::current().id(), self.owner);
                let store = self.store.upgrade().unwrap();
                let values = store
                    .with_mut(|store| self.callback.call(store, &[]))
                    .unwrap()
                    .unwrap();
                assert!(values.is_empty());
            }
        }
        let store = StoreRef::new(Store::default()).unwrap();
        let owner = store.retirement_owner().unwrap();
        let state = store
            .register_retirement(
                Rc::new(std::cell::Cell::new(true)),
                RetirementFailure::default(),
            )
            .unwrap();
        store.retain_delivery(state.clone()).unwrap();
        let tail = Rc::new(RefCell::new(Box::pin(NativeRetirementTail {
            subscription: Some(owner.subscribe()),
            _store: store.downgrade(),
            base: Some(store.async_store().unwrap()),
            owner: Some(owner.clone()),
            cleanup: Some(owner.owner.clone()),
            release: Some(store.retirement_release(state.clone()).unwrap()),
            first: store.retirement_failure().unwrap(),
            finalization_subscription: None,
            finalization: store.finalization_handoff(&owner),
            exit: None,
            state: state.clone(),
            registration: Some({
                state.disposition.set(DeliveryDisposition::LiveTail);
                RetirementRegistration {
                    state: state.clone(),
                    armed: true,
                }
            }),
        })));
        let ready = Rc::new(std::cell::Cell::new(None));
        let first = tail.clone();
        let observation = ready.clone();
        let target = store.clone();
        let release = state.clone();
        owner.owner.after_pump_existing(Box::new(move || {
            let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
            assert!(first.borrow_mut().as_mut().poll(&mut cx).is_pending());
            assert!(!release.tail_receipt_released.get());
            // Create the callback ONLY after the tail released base/Driver
            // receipts: its Store-owned environment cannot pin its own Store.
            let data = Box::into_raw(Box::new(PollLive {
                tail: first,
                ready: observation,
            }));
            let mut public = wasm_store_t {
                inner: target.clone(),
            };
            let ty = wasm_functype_t::new(wasmer_api::FunctionType::new([], []));
            let function = unsafe {
                wasm_func_new_with_env(
                    Some(&mut public),
                    Some(&ty),
                    Some(poll_live),
                    data.cast(),
                    Some(delete_control),
                )
            }
            .unwrap();
            let wake = std::task::Waker::from(std::sync::Arc::new(Reentrant {
                owner: std::thread::current().id(),
                store: target.downgrade(),
                callback: function.extern_.function(),
            }));
            assert!(release.register_release_waker(&wake));
            drop(wake);
            drop(function);
        }));
        assert_eq!(
            ready.get(),
            Some(true),
            "the actual wake must re-poll the LIVE tail with receipt readiness already published"
        );
        assert!(state.tail_receipt_released.get());
        assert!(store.retirement_states_for_test().is_empty());
        drop(tail);
        drop(owner);
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn late_release_notify_panic_keeps_exact_delivery_for_driver_or_legal_outer_shutdown() {
        struct IdlePanic(std::sync::atomic::AtomicUsize);
        impl std::task::Wake for IdlePanic {
            fn wake(self: std::sync::Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &std::sync::Arc<Self>) {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                panic!("late release idle waker failure");
            }
        }
        for final_store in [false, true] {
            let store = StoreRef::new(Store::default()).unwrap();
            let weak_store = store.downgrade();
            let owner = store.cleanup_owner().unwrap();
            let state = store
                .register_retirement(
                    Rc::new(std::cell::Cell::new(true)),
                    RetirementFailure::default(),
                )
                .unwrap();
            store.retain_delivery(state.clone()).unwrap();
            let release = store.retirement_release(state.clone()).unwrap();
            let idle = std::sync::Arc::new(IdlePanic(std::sync::atomic::AtomicUsize::new(0)));
            let waker = std::task::Waker::from(idle.clone());
            assert!(
                owner
                    .poll_idle(&mut std::task::Context::from_waker(&waker))
                    .is_pending()
            );
            let mut remaining = Some(store);
            if final_store {
                // Queue legal outer finalization BEFORE the publisher sees the
                // late error. Its initial physical join is still Pending.
                let last = remaining.take().unwrap();
                owner.after_pump_existing(Box::new(move || drop(last)));
                assert!(weak_store.upgrade().is_none());
            }
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| release.release()));
            if final_store {
                // The publisher captures the value; existing retire_store then
                // recollects it inside its drain BEFORE owner-idle/sleep/drop.
                let panic = result.unwrap_err();
                assert_eq!(
                    panic.downcast_ref::<&str>(),
                    Some(&"late release idle waker failure")
                );
                assert!(owner.is_idle());
                assert!(!state.receipt_queued.get());
            } else {
                assert!(
                    result.is_ok(),
                    "known notification error cannot escape the publisher into a detached task boundary"
                );
                let store = remaining.as_ref().unwrap();
                assert_eq!(
                    state.failure.inspect_message().as_deref(),
                    Some("late release idle waker failure")
                );
                assert!(
                    !state.delivery_ack.get(),
                    "late value reopens an earlier empty acknowledgement"
                );
                assert!(state.receipt_queued.get());
                assert_eq!(
                    store.retirement_states_for_test().len(),
                    1,
                    "same exact enrollment survives receipt retirement and late notify panic"
                );
                let sibling = store
                    .with_mut(|store| Function::new_typed(store, || 42i32))
                    .unwrap();
                let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    store.call(&sibling, Vec::new())
                }))
                .unwrap_err();
                assert_eq!(
                    panic.downcast_ref::<&str>(),
                    Some(&"late release idle waker failure")
                );
                assert_eq!(store.call(&sibling, Vec::new()).unwrap()[0].i32(), Some(42));
                assert!(store.retirement_states_for_test().is_empty());
                drop(remaining);
                assert!(weak_store.upgrade().is_none());
            }
            assert_eq!(
                idle.0.load(std::sync::atomic::Ordering::SeqCst),
                1,
                "late notification value is delivered once, not replayed"
            );
        }
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn re_admission_retains_a_followup_notify_error_after_reentrant_acknowledgement() {
        struct Observe {
            state: Rc<NativeRetirementState>,
            owner: wasmer_api::sys::NativeCleanupOwner,
            first: Rc<RefCell<Option<RetirementPanic>>>,
            claims: Rc<std::cell::Cell<usize>>,
        }
        unsafe extern "C" fn enqueue_observer(
            data: *mut std::ffi::c_void,
            _: &wasm_val_vec_t,
            _: &mut wasm_val_vec_t,
        ) -> Option<Box<wasm_trap_t>> {
            let observe = unsafe { &*data.cast::<Observe>() };
            let state = observe.state.clone();
            let first = observe.first.clone();
            let claims = observe.claims.clone();
            observe
                .owner
                .retain(Box::pin(std::future::poll_fn(move |_| {
                    // First poll precedes re-admission. Its actual retain/notify
                    // later polls this observer before the new receipt, permitting
                    // reentrant acknowledgement and same-pump receipt retirement.
                    if !state.receipt_queued.get() || state.delivery_empty() {
                        return std::task::Poll::Pending;
                    }
                    assert!(state.physically_ready());
                    let mut owned = None;
                    state.claim(&mut owned);
                    assert!(first.borrow().is_none());
                    *first.borrow_mut() = owned;
                    claims.set(claims.get() + 1);
                    std::task::Poll::Ready(())
                })))
                .unwrap_or_else(|_| panic!("test cleanup admission was refused"));
            None
        }
        unsafe extern "C" fn delete_observer(data: *mut std::ffi::c_void) {
            drop(unsafe { Box::from_raw(data.cast::<Observe>()) });
        }
        struct NotifyTwice {
            thread: std::thread::ThreadId,
            store: WeakStoreRef,
            callback: Function,
            wakes: std::sync::atomic::AtomicUsize,
        }
        impl std::task::Wake for NotifyTwice {
            fn wake(self: std::sync::Arc<Self>) {
                self.wake_by_ref();
            }
            fn wake_by_ref(self: &std::sync::Arc<Self>) {
                assert_eq!(self.thread, std::thread::current().id());
                match self.wakes.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 => {
                        let store = self.store.upgrade().unwrap();
                        let values = store
                            .with_mut(|store| self.callback.call(store, &[]))
                            .unwrap()
                            .unwrap();
                        assert!(values.is_empty());
                        panic!("first publication notify error");
                    }
                    1 => panic!("followup re-admission notify error"),
                    _ => panic!("notification unexpectedly replayed"),
                }
            }
        }
        let store = StoreRef::new(Store::default()).unwrap();
        let owner = store.cleanup_owner().unwrap();
        let state = store
            .register_retirement(
                Rc::new(std::cell::Cell::new(true)),
                RetirementFailure::default(),
            )
            .unwrap();
        store.retain_delivery(state.clone()).unwrap();
        let first = Rc::new(RefCell::new(None));
        let claims = Rc::new(std::cell::Cell::new(0));
        let data = Box::into_raw(Box::new(Observe {
            state: state.clone(),
            owner: owner.clone(),
            first: first.clone(),
            claims: claims.clone(),
        }));
        let mut public = wasm_store_t {
            inner: store.clone(),
        };
        let ty = wasm_functype_t::new(wasmer_api::FunctionType::new([], []));
        let function = unsafe {
            wasm_func_new_with_env(
                Some(&mut public),
                Some(&ty),
                Some(enqueue_observer),
                data.cast(),
                Some(delete_observer),
            )
        }
        .unwrap();
        let wake = std::sync::Arc::new(NotifyTwice {
            thread: std::thread::current().id(),
            store: store.downgrade(),
            callback: function.extern_.function(),
            wakes: std::sync::atomic::AtomicUsize::new(0),
        });
        let waker = std::task::Waker::from(wake.clone());
        assert!(
            owner
                .poll_idle(&mut std::task::Context::from_waker(&waker))
                .is_pending()
        );
        let release = store.retirement_release(state.clone()).unwrap();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| release.release()));
        assert!(
            result.is_ok(),
            "both notification errors remain owned values, never escaped detached-task panics"
        );
        assert_eq!(
            claims.get(),
            1,
            "actual reentrant consumer acknowledged the first value during retain/notify"
        );
        assert_eq!(
            first.borrow().as_ref().unwrap().downcast_ref::<&str>(),
            Some(&"first publication notify error")
        );
        assert_eq!(
            state.failure.inspect_message().as_deref(),
            Some("followup re-admission notify error")
        );
        assert!(!state.publishing.get());
        assert!(!state.delivery_ack.get());
        assert!(
            state.receipt_queued.get(),
            "followup error needs a rechecked, owned delivery receipt"
        );
        assert_eq!(
            store.retirement_states_for_test().len(),
            1,
            "the SAME enrollment survives the entire fallible re-admission"
        );
        let sibling = store
            .with_mut(|store| Function::new_typed(store, || 42i32))
            .unwrap();
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            store.call(&sibling, Vec::new())
        }))
        .unwrap_err();
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"followup re-admission notify error")
        );
        assert_eq!(store.call(&sibling, Vec::new()).unwrap()[0].i32(), Some(42));
        assert!(store.retirement_states_for_test().is_empty());
        assert_eq!(wake.wakes.load(std::sync::atomic::Ordering::SeqCst), 2);
        // The first observation remains intact; the later error was delivered
        // at the NEXT boundary, never retroactively substituted or replayed.
        assert_eq!(
            first.borrow().as_ref().unwrap().downcast_ref::<&str>(),
            Some(&"first publication notify error")
        );
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn native_post_tail_drop_keeps_custody_for_an_existing_driver() {
        let target = StoreRef::new(Store::default()).unwrap();
        let retirement = target.retirement_owner().unwrap();
        let driver = retirement.driver.clone();
        let owner = target.cleanup_owner().unwrap();
        let completed = Rc::new(std::cell::Cell::new(false));
        let cleaned = completed.clone();
        let completion = retirement.clone();
        retirement.retain(Box::pin(async move {
            let (send, receive) = tokio::sync::oneshot::channel();
            tokio::task::spawn_local(async move {
                tokio::task::yield_now().await;
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                cleaned.set(true);
                let _ = send.send(());
                completion.notify_completion();
            });
            let _ = receive.await;
        }));
        let weak = target.downgrade();
        let enrolled = completed.clone();
        let mut source = Store::default();
        let function = Function::new_async(
            &mut source,
            wasmer_api::FunctionType::new([], []),
            move |_| {
                weak.upgrade()
                    .unwrap()
                    .enroll_native_retirement(enrolled.clone())
                    .unwrap();
                std::future::ready(Ok(Vec::new()))
            },
        );
        let source = source.into_async();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let mut call = Box::pin(function.call_async(&source, vec![]));
            std::future::poll_fn(|cx| {
                assert!(call.as_mut().poll(cx).is_pending());
                std::task::Poll::Ready(())
            })
            .await;
            assert!(!completed.get());
            drop(call); // No synchronous wait inside this current-thread poll.
            assert!(
                !completed.get(),
                "retention itself does not claim completion"
            );
            assert_eq!(
                Rc::strong_count(&target.inner), 1,
                "PENDING retained cleanup must not own a C API StoreRef; the old completion cycle added a second strong reference"
            );
            // Explicitly retain the existing async driver after cancellation.
            // It, rather than a new executor/task, is the actual poll producer.
            driver.run_local(async {}).await;
        });
        assert!(completed.get());
        assert!(owner.is_idle());
        assert_eq!(Rc::strong_count(&target.inner), 1);
        drop(driver);
        drop(retirement);
    }

    #[test]
    fn native_foreign_calls_reenter_the_exact_ancestor_loan_and_poll_its_queued_work() {
        let a = StoreRef::new(Store::default()).unwrap();
        let b = StoreRef::new(Store::default()).unwrap();
        let queued = Rc::new(std::cell::Cell::new(0));
        let observed = queued.clone();
        let leaf = a
            .with_mut(|store| {
                Function::new_async(
                    store,
                    wasmer_api::FunctionType::new([], [wasmer_api::Type::I32]),
                    move |_| {
                        let queued = queued.clone();
                        async move {
                            let (send, receive) = tokio::sync::oneshot::channel();
                            tokio::task::spawn_local(async move {
                                tokio::task::yield_now().await;
                                queued.set(queued.get() + 1);
                                send.send(()).unwrap();
                            });
                            receive.await.unwrap();
                            Ok(vec![Value::I32(37)])
                        }
                    },
                )
            })
            .unwrap();
        let module = a
            .with_mut(|store| {
                Module::new(
                    store,
                    wat::parse_str(
                        r#"(module
            (import "test" "wait" (func $wait (result i32)))
            (global $value (mut i32) (i32.const 0))
            (func $init call $wait global.set $value) (start $init)
            (func (export "value") (result i32) global.get $value))"#,
                    )
                    .unwrap(),
                )
            })
            .unwrap()
            .unwrap();
        let weak_a = a.downgrade();
        let middle = b
            .with_mut(|store| {
                Function::new_async(
                    store,
                    wasmer_api::FunctionType::new([], [wasmer_api::Type::I32]),
                    move |_| {
                        let a = weak_a.upgrade().unwrap();
                        a.with_mut(|store| assert_eq!(store.objects_mut().id(), a.inner.id))
                            .unwrap();
                        let result = a.call(&leaf, vec![]).and_then(|values| {
                            assert_eq!(values[0].i32(), Some(37));
                            let instance = a
                                .instantiate(&module, vec![leaf.clone().into()])?
                                .map_err(|error| RuntimeError::new(error.to_string()))?;
                            a.call(instance.exports.get_function("value").unwrap(), vec![])
                        });
                        std::future::ready(result.map(Vec::from))
                    },
                )
            })
            .unwrap();
        let weak_b = b.downgrade();
        let root = a
            .with_mut(|store| {
                Function::new_async(
                    store,
                    wasmer_api::FunctionType::new([], [wasmer_api::Type::I32]),
                    move |_| {
                        let result = weak_b.upgrade().unwrap().call(&middle, vec![]);
                        std::future::ready(result.map(Vec::from))
                    },
                )
            })
            .unwrap();
        for expected in 1..=2 {
            let result = a.call(&root, vec![]).unwrap();
            assert_eq!(result[0].i32(), Some(37));
            assert_eq!(observed.get(), 2 * expected);
            assert_eq!(
                a.with_mut(|store| store.objects_mut().id()).unwrap(),
                a.inner.id
            );
            assert_eq!(
                b.with_mut(|store| store.objects_mut().id()).unwrap(),
                b.inner.id
            );
        }
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn native_ancestor_handoff_matches_once_with_host_access_after_the_child_abort() {
        struct Endpoint {
            store: WeakStoreRef,
            marker: RuntimeError,
            claimed: Rc<std::cell::Cell<bool>>,
            completed: Rc<std::cell::Cell<usize>>,
        }
        impl ContinuationEndpoint for Endpoint {
            fn matches(&self, store: &mut StoreMut<'_>, error: &RuntimeError) -> bool {
                store.objects_mut().id() == self.store.upgrade().unwrap().inner.id
                    && !self.claimed.get()
                    && std::ptr::eq(
                        error.downcast_ref::<std::io::Error>().unwrap(),
                        self.marker.downcast_ref::<std::io::Error>().unwrap(),
                    )
            }
            fn prepare(
                self: Rc<Self>,
                store: &StoreRef,
                _: &RuntimeError,
            ) -> Result<LocalContinuation, RuntimeError> {
                assert!(!self.claimed.replace(true));
                // Original A is still parked. This must use its captured loan,
                // including when current B has already latched the child abort.
                store.with_mut(|loan| assert_eq!(loan.objects_mut().id(), store.inner.id))?;
                let weak = self.store.clone();
                let completed = self.completed.clone();
                Ok(LocalContinuation {
                    store: store.clone(),
                    run: Some(Box::new(move || {
                        Box::pin(async move {
                            let a = weak.upgrade().unwrap();
                            a.with_mut(|loan| assert_eq!(loan.objects_mut().id(), a.inner.id))?;
                            completed.set(completed.get() + 1);
                            Err(RuntimeError::new("completed exact ancestor continuation"))
                        })
                    })),
                    cancel: None,
                })
            }
            fn cancel_unclaimed(
                self: Rc<Self>,
                _: &StoreRef,
                _: &RuntimeError,
            ) -> Result<(), RuntimeError> {
                panic!("the real outer call owner must be found")
            }
        }
        let a = StoreRef::new(Store::default()).unwrap();
        let b = StoreRef::new(Store::default()).unwrap();
        let marker =
            RuntimeError::host_abort(Box::new(std::io::Error::other("exact ancestor handoff")));
        let claimed = Rc::new(std::cell::Cell::new(false));
        let completed = Rc::new(std::cell::Cell::new(0));
        let endpoint: Rc<dyn ContinuationEndpoint> = Rc::new(Endpoint {
            store: a.downgrade(),
            marker: marker.clone(),
            claimed: claimed.clone(),
            completed: completed.clone(),
        });
        a.register_endpoint(Rc::downgrade(&endpoint)).unwrap();
        let leaf = a
            .with_mut(|store| {
                Function::new_async(store, wasmer_api::FunctionType::new([], []), move |_| {
                    std::future::ready(Err(marker.clone()))
                })
            })
            .unwrap();
        let weak_a = a.downgrade();
        let middle = b
            .with_mut(|store| {
                Function::new_async(store, wasmer_api::FunctionType::new([], []), move |_| {
                    assert!(weak_a.upgrade().unwrap().call(&leaf, vec![]).is_err());
                    std::future::ready(Ok(Vec::new())) // Deliberately swallow the trap.
                })
            })
            .unwrap();
        let weak_b = b.downgrade();
        let root = a
            .with_mut(|store| {
                Function::new_async(store, wasmer_api::FunctionType::new([], []), move |_| {
                    assert!(weak_b.upgrade().unwrap().call(&middle, vec![]).is_err());
                    std::future::ready(Ok(Vec::new()))
                })
            })
            .unwrap();
        let error = a.call(&root, vec![]).unwrap_err();
        assert_eq!(error.message(), "completed exact ancestor continuation");
        assert!(claimed.get());
        assert_eq!(completed.get(), 1);
        a.with_mut(|loan| assert_eq!(loan.objects_mut().id(), a.inner.id))
            .unwrap();
        b.with_mut(|loan| assert_eq!(loan.objects_mut().id(), b.inner.id))
            .unwrap();
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn native_instantiation_never_publishes_saved_success_after_call_owner_failure() {
        let mut store = wasm_store_t {
            inner: StoreRef::new(Store::default()).unwrap(),
        };
        let weak = store.inner.downgrade();
        let completed = Rc::new(std::cell::Cell::new(0));
        let observed = completed.clone();
        let error = RuntimeError::user(Box::new(std::io::Error::other(
            "exact post-instantiation continuation failure",
        )));
        let expected = error.to_string();
        let arm = store
            .inner
            .with_mut(|store| {
                Function::new_async(store, wasmer_api::FunctionType::new([], []), move |_| {
                    let owner = wasmer_api::sys::current_native_local_owner::<CallOwner>().unwrap();
                    let store = weak.upgrade().unwrap();
                    let weak = weak.clone();
                    let completed = completed.clone();
                    let error = error.clone();
                    owner.pending.borrow_mut().push_back(LocalContinuation {
                        store,
                        run: Some(Box::new(move || {
                            Box::pin(async move {
                                // drain runs AFTER the native trampoline saved Ok(instance)
                                // and the original Store guard physically unlocked.
                                let store = weak.upgrade().unwrap();
                                store.with_mut(|loan| {
                                    assert_eq!(loan.objects_mut().id(), store.inner.id)
                                })?;
                                completed.set(completed.get() + 1);
                                Err(error)
                            })
                        })),
                        cancel: None,
                    });
                    std::future::ready(Ok(Vec::new()))
                })
            })
            .unwrap();
        let module = wasm_module_t {
            inner: store
                .inner
                .with_mut(|store| {
                    Module::new(
                        store,
                        wat::parse_str(
                            r#"(module
                (import "test" "arm" (func $arm))
                (func $init call $arm) (start $init)
                (func (export "value") (result i32) i32.const 42))"#,
                        )
                        .unwrap(),
                    )
                })
                .unwrap()
                .unwrap(),
        };
        let imports: wasm_extern_vec_t = vec![Some(Box::new(wasm_extern_t::new(
            store.inner.clone(),
            arm.into(),
        )))]
        .into();
        let mut trap = std::ptr::null_mut();
        let instance = unsafe {
            wasm_instance_new(
                Some(&mut store),
                Some(&module),
                Some(&imports),
                Some(&mut trap),
            )
        };
        assert!(
            instance.is_none(),
            "saved success must not override the enclosing call failure"
        );
        assert!(
            trap.is_null(),
            "host completion errors keep their existing last-error phase"
        );
        assert_eq!(observed.get(), 1);
        let length = crate::error::wasmer_last_error_length();
        assert_eq!(length as usize, expected.len() + 1);
        let mut message = vec![0u8; length as usize];
        let written = unsafe {
            crate::error::wasmer_last_error_message(
                std::ptr::NonNull::new(message.as_mut_ptr().cast()),
                length,
            )
        };
        assert_eq!(written, length);
        let mut expected = expected.into_bytes();
        expected.push(0);
        assert_eq!(message, expected);
        store
            .inner
            .with_mut(|loan| assert_eq!(loan.objects_mut().id(), store.inner.inner.id))
            .unwrap();
    }

    #[test]
    fn native_foreign_instantiation_suspends_start_and_preserves_the_callers_store() {
        let a = StoreRef::new(Store::default()).unwrap();
        let b = StoreRef::new(Store::default()).unwrap();
        let wait = b
            .with_mut(|store| {
                Function::new_async(
                    store,
                    wasmer_api::FunctionType::new([], [wasmer_api::Type::I32]),
                    |_| async {
                        tokio::task::yield_now().await;
                        Ok(vec![Value::I32(42)])
                    },
                )
            })
            .unwrap();
        let module = b
            .with_mut(|store| {
                Module::new(
                    store,
                    wat::parse_str(
                        r#"(module
            (import "test" "wait" (func $wait (result i32)))
            (global $value (mut i32) (i32.const 0))
            (func $init call $wait global.set $value) (start $init)
            (func (export "value") (result i32) global.get $value))"#,
                    )
                    .unwrap(),
                )
            })
            .unwrap()
            .unwrap();
        let weak_b = b.downgrade();
        let root = a
            .with_mut(|store| {
                Function::new_async(
                    store,
                    wasmer_api::FunctionType::new([], [wasmer_api::Type::I32]),
                    move |_| {
                        let b = weak_b.upgrade().unwrap();
                        let result =
                            b.instantiate(&module, vec![wait.clone().into()])
                                .and_then(|result| {
                                    let instance = result
                                        .map_err(|error| RuntimeError::new(error.to_string()))?;
                                    b.call(instance.exports.get_function("value").unwrap(), vec![])
                                });
                        std::future::ready(result.map(Vec::from))
                    },
                )
            })
            .unwrap();
        assert_eq!(a.call(&root, vec![]).unwrap()[0].i32(), Some(42));
        assert_eq!(
            a.with_mut(|store| store.objects_mut().id()).unwrap(),
            a.inner.id
        );
        assert_eq!(
            b.with_mut(|store| store.objects_mut().id()).unwrap(),
            b.inner.id
        );
    }

    #[test]
    fn entered_runtime_calls_fail_without_panicking_and_leave_the_store_usable() {
        let mut store = wasm_store_t {
            inner: StoreRef::new(Store::default()).unwrap(),
        };
        let function = store
            .inner
            .with_mut(|store| Function::new_typed(store, || 19i32))
            .unwrap();
        let mut function = wasm_func_t {
            extern_: wasm_extern_t::new(store.inner.clone(), function.into()),
        };
        let module = wasm_module_t {
            inner: store
                .inner
                .with_mut(|store| Module::new(store, wat::parse_str("(module)").unwrap()))
                .unwrap()
                .unwrap(),
        };
        let imports: wasm_extern_vec_t = Vec::<Option<Box<wasm_extern_t>>>::new().into();
        let args: wasm_val_vec_t = Vec::<wasm_val_t>::new().into();
        let mut results: wasm_val_vec_t = vec![wasm_val_t::default()].into();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        {
            let _entered = runtime.enter();
            let trap = unsafe { wasm_func_call(Some(&mut function), Some(&args), &mut results) };
            assert!(trap.is_some(), "an entered-runtime error remains a C trap");
            let mut trap = std::ptr::null_mut();
            assert!(
                unsafe {
                    wasm_instance_new(
                        Some(&mut store),
                        Some(&module),
                        Some(&imports),
                        Some(&mut trap),
                    )
                }
                .is_none()
            );
            assert!(trap.is_null(), "host admission errors use last_error");
        }
        assert!(
            unsafe { wasm_func_call(Some(&mut function), Some(&args), &mut results) }.is_none()
        );
        assert_eq!(unsafe { results.as_slice()[0].of.int32_t }, 19);
        assert!(
            unsafe { wasm_instance_new(Some(&mut store), Some(&module), Some(&imports), None) }
                .is_some()
        );
    }

    #[test]
    fn native_c_entrypoints_suspend_engine_start_and_return_reactor_values() {
        let mut store = wasm_store_t {
            inner: StoreRef::new(Store::default()).unwrap(),
        };
        let wait = store
            .inner
            .with_mut(|store| {
                Function::new_async(
                    store,
                    wasmer_api::FunctionType::new([], [wasmer_api::Type::I32]),
                    |_| async {
                        tokio::task::yield_now().await;
                        Ok(vec![Value::I32(42)])
                    },
                )
            })
            .unwrap();
        let source = wat::parse_str(
            r#"(module
            (import "proof" "wait" (func $wait (result i32)))
            (global $result (mut i32) (i32.const 0))
            (func $init call $wait global.set $result) (start $init)
            (func (export "value") (result i32) global.get $result))"#,
        )
        .unwrap();
        let module = wasm_module_t {
            inner: store
                .inner
                .with_mut(|store| Module::new(store, &source))
                .unwrap()
                .unwrap(),
        };
        let imports: wasm_extern_vec_t = vec![Some(Box::new(wasm_extern_t::new(
            store.inner.clone(),
            wait.into(),
        )))]
        .into();
        let instance =
            unsafe { wasm_instance_new(Some(&mut store), Some(&module), Some(&imports), None) }
                .unwrap();
        let mut function = wasm_func_t {
            extern_: wasm_extern_t::new(
                store.inner.clone(),
                instance
                    .inner
                    .exports
                    .get_function("value")
                    .unwrap()
                    .clone()
                    .into(),
            ),
        };
        let arguments: wasm_val_vec_t = Vec::<wasm_val_t>::new().into();
        let mut results: wasm_val_vec_t = vec![wasm_val_t::default()].into();
        for _ in 0..2 {
            assert!(
                unsafe { wasm_func_call(Some(&mut function), Some(&arguments), &mut results) }
                    .is_none()
            );
            assert_eq!(unsafe { results.as_slice()[0].of.int32_t }, 42);
        }
        assert!(
            store
                .inner
                .with_mut(|store| function.extern_.function().is_from_store(store))
                .unwrap()
        );
    }
}

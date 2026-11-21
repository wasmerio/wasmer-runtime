//! The canonical C API owner for one V8 Store and its original-thread jobs.

use super::{DriverWake, RetirementPanic, discard_retirement_panic};
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    future::{Future, poll_fn},
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    pin::Pin,
    rc::{Rc, Weak},
    sync::{Arc, Condvar, Mutex},
    task::{Context, Poll, Wake, Waker},
};
use wasmer_api::{
    AsStoreAsync, AsStoreMut, Extern, Function, Instance, InstantiationError, Module, RuntimeError,
    Store, StoreAsync, StoreMut,
};

type Failure = RefCell<Option<RetirementPanic>>;

/// A local, `'a`-bounded future.
pub type LocalFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// Extension state, such as a WASI environment, that a [`Driver`] consults
/// when a call fails.
pub trait V8Endpoint {
    /// Replaces the error `function` failed with, such as a WASI exit.
    fn map_error(&self, function: &Function, error: &RuntimeError) -> Option<RuntimeError>;
}

/// A cleanup scope whose settlement a [`Driver`] waits for before finalization.
pub trait DriverScope {
    /// Registers `cx` for settlement; false when nothing could be armed.
    fn observe(&self, cx: &mut Context<'_>) -> bool;
    fn is_ready(&self) -> bool;
}

/// Where cleanup jobs bound to a [`Driver`] are retained.
pub trait DriverSink {
    /// Trusted internal transfer of the SAME counted/canonical obligation.
    fn retain(&self, future: LocalFuture<'static, ()>);
    /// Fresh work; refused once the driver is closing.
    fn try_retain(&self, future: LocalFuture<'static, ()>) -> Result<(), LocalFuture<'static, ()>>;
    fn notify(&self);
}

fn record(first: &Failure, panic: RetirementPanic, unwinding: bool) {
    if unwinding {
        discard_retirement_panic(panic);
        return;
    }
    let previous = first.borrow_mut().take();
    if let Some(previous) = previous {
        *first.borrow_mut() = Some(previous);
        discard_retirement_panic(panic);
    } else {
        *first.borrow_mut() = Some(panic);
    }
}
fn attempt<R>(first: &Failure, action: impl FnOnce() -> R) -> Option<R> {
    let unwinding = std::thread::panicking();
    match catch_unwind(AssertUnwindSafe(action)) {
        Ok(value) => Some(value),
        Err(panic) => {
            record(first, panic, unwinding);
            None
        }
    }
}

pub(super) struct ManagedStore {
    store: Option<StoreAsync>,
    driver: RefCell<Option<Rc<Driver>>>,
}
impl ManagedStore {
    pub(super) fn new(store: Store) -> Self {
        Self {
            store: Some(store.into_async()),
            driver: RefCell::new(None),
        }
    }
    pub(super) fn asynchronous(&self) -> StoreAsync {
        self.store.as_ref().unwrap().store()
    }
    pub(super) fn driver(&self) -> Result<Rc<Driver>, RuntimeError> {
        if let Some(driver) = self.driver.borrow().as_ref() {
            return Ok(driver.clone());
        }
        let cap = self.store.as_ref().unwrap();
        let mut lock = std::pin::pin!(cap.write_lock());
        let Poll::Ready(mut guard) = lock.as_mut().poll(&mut Context::from_waker(Waker::noop()))
        else {
            return Err(RuntimeError::new(
                "V8 Store has no available owner binding guard",
            ));
        };
        let runtime = super::new_owner_runtime()?;
        let driver = Rc::new(Driver {
            runtime: RefCell::new(Some(Arc::new(runtime))),
            local: RefCell::new(Some(Rc::new(tokio::task::LocalSet::new()))),
            wake: Arc::default(),
            entered: Cell::new(0),
            draining: Cell::new(false),
            closing: Cell::new(false),
            jobs: RefCell::new(VecDeque::new()),
            scopes: RefCell::new(Vec::new()),
            endpoints: RefCell::new(Vec::new()),
            #[cfg(any(test, feature = "test-support"))]
            pending_imports: Cell::new(0),
        });
        let executor: Rc<dyn wasmer_api::v8::BlockingExecutor> = driver.clone();
        let bound = wasmer_api::v8::set_blocking_executor(&mut guard, Rc::downgrade(&executor));
        drop(executor);
        if let Err(error) = bound {
            driver.shutdown();
            return Err(error);
        }
        // Publish before unlocking: an unlock callback must find this SAME owner.
        *self.driver.borrow_mut() = Some(driver.clone());
        drop(guard);
        Ok(driver)
    }
    pub(super) fn with_mut<R>(
        &self,
        action: impl FnOnce(&mut StoreMut<'_>) -> R,
    ) -> Result<R, RuntimeError> {
        let driver = self.driver()?;
        driver.in_context(|| {
            let _entry = driver.entry()?;
            let cap = self.store.as_ref().unwrap();
            let mut action = Some(action);
            if let Some(result) =
                wasmer_api::v8::with_borrowed_store(cap, |store| action.take().unwrap()(store))?
            {
                return Ok(result);
            }
            let mut lock = std::pin::pin!(cap.write_lock());
            let Poll::Ready(mut guard) =
                lock.as_mut().poll(&mut Context::from_waker(Waker::noop()))
            else {
                return Err(RuntimeError::new("V8 Store has no available host guard"));
            };
            guard.as_store_mut().parked(|| {
                wasmer_api::v8::with_borrowed_store(cap, |store| action.take().unwrap()(store))?
                    .ok_or_else(|| RuntimeError::new("actual V8 Store installation disappeared"))
            })
        })
    }
    pub(super) fn call(
        &self,
        function: &Function,
        args: Vec<wasmer_api::Value>,
    ) -> Result<Box<[wasmer_api::Value]>, RuntimeError> {
        let driver = self.driver()?;
        driver.in_context(|| {
            let entry = driver.entry()?;
            let cap = self.store.as_ref().unwrap();
            let result = match wasmer_api::v8::with_borrowed_store(cap, |store| {
                function.call(store, &args)
            })? {
                Some(result) => result,
                None => entry.root(function.call_async(cap, args)),
            };
            match result {
                Err(error) => {
                    let endpoints = driver
                        .endpoints
                        .borrow()
                        .iter()
                        .filter_map(Weak::upgrade)
                        .collect::<Vec<_>>();
                    let mapped = endpoints
                        .iter()
                        .find_map(|endpoint| endpoint.map_error(function, &error));
                    Err(mapped.unwrap_or(error))
                }
                result => result,
            }
        })
    }
    #[allow(clippy::result_large_err)]
    pub(super) fn instantiate(
        &self,
        module: &Module,
        imports: Vec<Extern>,
    ) -> Result<Result<Instance, InstantiationError>, RuntimeError> {
        let driver = self.driver()?;
        driver.in_context(|| {
            let entry = driver.entry()?;
            let cap = self.store.as_ref().unwrap();
            if let Some(result) = wasmer_api::v8::with_borrowed_store(cap, |store| {
                Instance::new_by_index(store, module, &imports)
            })? {
                return Ok(result);
            }
            Ok(entry.root(wasmer_api::v8::instantiate_async(
                cap,
                module.clone(),
                imports,
            )))
        })
    }
}
impl Drop for ManagedStore {
    fn drop(&mut self) {
        let Some(cap) = self.store.take() else {
            return;
        };
        if let Some(driver) = self.driver.get_mut().take() {
            driver.finalize(cap);
        } else {
            // No driver/import/job was ever published from this Store.
            drop(cap);
        }
    }
}

pub struct Driver {
    runtime: RefCell<Option<Arc<tokio::runtime::Runtime>>>,
    local: RefCell<Option<Rc<tokio::task::LocalSet>>>,
    wake: Arc<DriverWake>,
    entered: Cell<usize>,
    draining: Cell<bool>,
    closing: Cell<bool>,
    jobs: RefCell<VecDeque<LocalFuture<'static, ()>>>,
    scopes: RefCell<Vec<Rc<dyn DriverScope>>>,
    endpoints: RefCell<Vec<Weak<dyn V8Endpoint>>>,
    #[cfg(any(test, feature = "test-support"))]
    pending_imports: Cell<usize>,
}
struct Sink(Weak<Driver>);
impl DriverSink for Sink {
    fn retain(&self, future: LocalFuture<'static, ()>) {
        // All actual runner/scope fields end before the owner's strong custody.
        let driver = self
            .0
            .upgrade()
            .expect("accepted V8 cleanup lost its owner");
        driver.jobs.borrow_mut().push_back(future);
        driver.wake.wake_by_ref();
    }
    fn try_retain(&self, future: LocalFuture<'static, ()>) -> Result<(), LocalFuture<'static, ()>> {
        let Some(driver) = self.0.upgrade().filter(|driver| !driver.closing.get()) else {
            return Err(future);
        };
        driver.jobs.borrow_mut().push_back(future);
        driver.wake.wake_by_ref();
        Ok(())
    }
    fn notify(&self) {
        if let Some(driver) = self.0.upgrade() {
            driver.wake.wake_by_ref();
        }
    }
}
#[derive(Default)]
struct Parker {
    notified: Mutex<bool>,
    ready: Condvar,
}
impl Wake for Parker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        *self.notified.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.ready.notify_one();
    }
}
struct Subscription {
    wake: Arc<DriverWake>,
    token: u64,
}
impl Drop for Subscription {
    fn drop(&mut self) {
        let previous = self
            .wake
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .subscribers
            .remove(&self.token);
        drop(previous);
    }
}
struct Entry<'a> {
    driver: &'a Driver,
    subscription: Option<Subscription>,
    parker: Arc<Parker>,
    first: Failure,
}
impl Driver {
    fn in_context<R>(&self, action: impl FnOnce() -> R) -> R {
        let runtime = self
            .runtime
            .borrow()
            .as_ref()
            .cloned()
            .expect("live V8 runtime");
        let local = self.local.borrow().clone();
        let _runtime = runtime.enter();
        let _local = local.as_ref().map(|local| local.enter());
        action()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn pending_imports(&self) -> usize {
        self.pending_imports.get()
    }
    fn entry(&self) -> Result<Entry<'_>, RuntimeError> {
        let token = {
            let mut state = self.wake.state.lock().unwrap_or_else(|e| e.into_inner());
            let token = state.next;
            state.next = token
                .checked_add(1)
                .ok_or_else(|| RuntimeError::new("V8 wake token exhausted"))?;
            state.subscribers.insert(token, None);
            token
        };
        self.entered.set(self.entered.get() + 1);
        Ok(Entry {
            driver: self,
            subscription: Some(Subscription {
                wake: self.wake.clone(),
                token,
            }),
            parker: Arc::default(),
            first: RefCell::new(None),
        })
    }
    pub fn run<R>(&self, future: impl Future<Output = R>) -> Result<R, RuntimeError> {
        let entry = self.entry()?;
        Ok(entry.host(future))
    }
    /// Waits for `scope` before finalization and binds its runner with `bind`.
    pub fn bind_scope<R>(
        self: &Rc<Self>,
        scope: Rc<dyn DriverScope>,
        bind: impl FnOnce(Rc<dyn DriverSink>) -> R,
    ) -> R {
        self.scopes.borrow_mut().push(scope);
        // Same actual metadata is installed before any publication/wake.
        let local = bind(Rc::new(Sink(Rc::downgrade(self))));
        self.wake.wake_by_ref();
        local
    }
    /// Consults `endpoint` when a call through this driver fails.
    pub fn register_endpoint(&self, endpoint: Weak<dyn V8Endpoint>) {
        self.endpoints.borrow_mut().push(endpoint);
    }
    pub fn retire(self: &Rc<Self>, future: LocalFuture<'static, ()>, close: impl FnOnce()) {
        let entry = self
            .entry()
            .expect("accepted V8 retirement wake registration");
        // Commit callback-free, preserve process-close before ANY retirement
        // polling/notification, and retain the same value if close unwinds.
        self.jobs.borrow_mut().push_back(future);
        attempt(&entry.first, close);
        attempt(&entry.first, || self.wake.wake_by_ref());
        // Entry Drop drains only after all actual enclosing guard spans end.
    }
    fn poll_jobs(&self, cx: &mut Context<'_>, first: &Failure) {
        let runtime = self.runtime.borrow().as_ref().cloned();
        let _runtime = runtime.as_ref().map(|runtime| runtime.enter());
        let local = self.local.borrow().clone();
        let _local = local.as_ref().map(|local| local.enter());
        let budget = self.jobs.borrow().len();
        for _ in 0..budget {
            let next = self.jobs.borrow_mut().pop_front();
            let Some(mut next) = next else {
                break;
            };
            // ONLY this current future leaves the queue; child drives can poll
            // distinct jobs but cannot poll this actual ancestor a second time.
            match attempt(first, || next.as_mut().poll(cx)) {
                Some(Poll::Pending) => self.jobs.borrow_mut().push_back(next),
                Some(Poll::Ready(())) | None => {
                    attempt(first, || drop(next));
                }
            }
        }
    }
    fn poll_scopes(&self, cx: &mut Context<'_>, first: &Failure) -> bool {
        let mut observed = Vec::new();
        loop {
            let next = self.scopes.borrow().get(observed.len()).cloned();
            let Some(next) = next else {
                break;
            };
            let armed = attempt(first, || next.observe(cx)).unwrap_or(false);
            observed.push((next, armed));
        }
        let mut ready = true;
        for (scope, armed) in observed {
            if !scope.is_ready() {
                ready = false;
                if !armed {
                    attempt(first, || cx.waker().wake_by_ref());
                }
            }
        }
        ready
    }
    fn finalize(&self, cap: StoreAsync) {
        self.closing.set(true);
        let entry = self.entry().expect("V8 finalization wake registration");
        entry.drain(false);
        let mut cap = Some(cap);
        entry.cleanup(|cx| {
            self.poll_jobs(cx, &entry.first);
            if !self.jobs.borrow().is_empty() {
                return Poll::Pending;
            }
            let actual = cap.take().unwrap();
            match actual.into_store() {
                Ok(store) => {
                    attempt(&entry.first, || drop(store));
                    Poll::Ready(())
                }
                Err(actual) => {
                    cap = Some(actual);
                    Poll::Pending
                }
            }
        });
        entry.drain(true);
        let runtime = self
            .runtime
            .borrow()
            .as_ref()
            .cloned()
            .expect("live V8 runtime");
        let _runtime = runtime.enter();
        let local = self.local.borrow_mut().take();
        if let Some(local) = local {
            match Rc::try_unwrap(local) {
                Ok(local) => {
                    attempt(&entry.first, || drop(local));
                }
                Err(local) => {
                    *self.local.borrow_mut() = Some(local);
                    // Exact custody, not a completion claim. The private holder
                    // allowlist excludes a surviving lexical LocalSet clone.
                    entry.cleanup(|_| Poll::Pending);
                }
            }
        }
        entry.drain(true);
        let scopes = std::mem::take(&mut *self.scopes.borrow_mut());
        for scope in scopes {
            attempt(&entry.first, || drop(scope));
        }
        entry.drain(false);
        drop(_runtime);
        drop(runtime);
        self.shutdown_into(&entry.first);
        drop(entry);
    }
    fn shutdown(&self) {
        let first = RefCell::new(None);
        self.shutdown_into(&first);
        let panic = first.borrow_mut().take();
        if let Some(panic) = panic {
            resume_unwind(panic);
        }
    }
    fn shutdown_into(&self, first: &Failure) {
        let unwinding = std::thread::panicking();
        let runtime = self.runtime.borrow_mut().take();
        if let Some(runtime) = runtime {
            // Existing CAPI runtime destruction stage, not a progress worker.
            if let Err(panic) = std::thread::spawn(move || drop(runtime)).join() {
                record(first, panic, unwinding);
            }
        }
    }
}
impl wasmer_api::v8::BlockingExecutor for Driver {
    fn block_on(&self, mut future: Pin<&mut dyn Future<Output = ()>>) -> Result<(), RuntimeError> {
        let entry = self.entry()?;
        entry.host(poll_fn(|cx| {
            let polled = future.as_mut().poll(cx);
            #[cfg(any(test, feature = "test-support"))]
            if polled.is_pending() {
                self.pending_imports.set(self.pending_imports.get() + 1);
            }
            polled
        }));
        Ok(())
    }
}
impl Entry<'_> {
    fn root<R>(&self, future: impl Future<Output = R>) -> R {
        self.execute(future, false, true).unwrap()
    }
    fn host<R>(&self, future: impl Future<Output = R>) -> R {
        self.execute(future, true, true).unwrap()
    }
    fn execute<R>(
        &self,
        future: impl Future<Output = R>,
        host: bool,
        transport: bool,
    ) -> Option<R> {
        self.driver.in_context(|| {
            let mut future = Some(Box::pin(future));
            let mut output;
            if host {
                let runtime = self
                    .driver
                    .runtime
                    .borrow()
                    .as_ref()
                    .cloned()
                    .expect("live V8 runtime");
                let _runtime = runtime.enter();
                let local = self.driver.local.borrow().clone();
                if let Some(local) = local {
                    let subscription = self.subscription.as_ref().unwrap();
                    let user = poll_fn(|cx| {
                        subscription
                            .wake
                            .with_real_context(subscription.token, cx, |cx| {
                                future.as_mut().unwrap().as_mut().poll(cx)
                            })
                    });
                    let mut run = Box::pin(local.run_until(user));
                    output = self.drive(run.as_mut(), transport);
                    attempt(&self.first, || drop(run));
                } else {
                    output = self.drive(future.as_mut().unwrap().as_mut(), transport);
                }
            } else {
                output = self.drive(future.as_mut().unwrap().as_mut(), transport);
            }
            // Outer root/retirement entry subscription remains live after adapter
            // return through the actual boxed host-future field destruction.
            attempt(&self.first, || drop(future.take()));
            if transport && self.first.borrow().is_some() {
                attempt(&self.first, || drop(output.take()));
            }
            if transport {
                let panic = self.first.borrow_mut().take();
                if let Some(panic) = panic {
                    resume_unwind(panic);
                }
            }
            output
        })
    }
    fn drive<R>(
        &self,
        mut future: Pin<&mut (impl Future<Output = R> + ?Sized)>,
        halt_on_panic: bool,
    ) -> Option<R> {
        let subscription = self.subscription.as_ref().unwrap();
        let waker = Waker::from(self.parker.clone());
        loop {
            if halt_on_panic && self.first.borrow().is_some() {
                return None;
            }
            *self
                .parker
                .notified
                .lock()
                .unwrap_or_else(|e| e.into_inner()) = false;
            // Own the actual result outside BOTH callback-bearing catches.
            // A job panic must be committed before disposing a ready output.
            let mut next = None;
            attempt(&self.first, || {
                subscription.wake.with_context(
                    subscription.token,
                    &mut Context::from_waker(&waker),
                    |cx| next = Some(future.as_mut().poll(cx)),
                )
            });
            if next.is_some() {
                attempt(&self.first, || {
                    subscription.wake.with_context(
                        subscription.token,
                        &mut Context::from_waker(&waker),
                        |cx| self.driver.poll_jobs(cx, &self.first),
                    )
                });
            }
            if next.is_none() || (halt_on_panic && self.first.borrow().is_some()) {
                attempt(&self.first, || drop(next.take()));
                return None;
            }
            if let Some(Poll::Ready(output)) = next {
                return Some(output);
            }
            let mut notified = self
                .parker
                .notified
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            while !*notified {
                notified = self
                    .parker
                    .ready
                    .wait(notified)
                    .unwrap_or_else(|e| e.into_inner());
            }
        }
    }
    fn cleanup(&self, mut ready: impl FnMut(&mut Context<'_>) -> Poll<()>) {
        // This is a reconstructible cleanup leaf: ownership lives in Driver or
        // the finalization slots, never in the polled closure. A caught callback
        // panic retries the leaf; ordinary Pending only parks on a real wake.
        while self.execute(poll_fn(&mut ready), true, false).is_none() {}
    }
    fn drain(&self, scopes: bool) {
        self.cleanup(|cx| {
            self.driver.poll_jobs(cx, &self.first);
            if !self.driver.jobs.borrow().is_empty() {
                return Poll::Pending;
            }
            if scopes && !self.driver.poll_scopes(cx, &self.first) {
                return Poll::Pending;
            }
            Poll::Ready(())
        });
    }
}
impl Drop for Entry<'_> {
    fn drop(&mut self) {
        let unwinding = std::thread::panicking();
        self.driver.entered.set(self.driver.entered.get() - 1);
        let live_runtime = self.driver.runtime.borrow().is_some();
        if live_runtime && self.driver.entered.get() == 0 && !self.driver.draining.replace(true) {
            attempt(&self.first, || self.drain(false));
            self.driver.draining.set(false);
        }
        attempt(&self.first, || drop(self.subscription.take()));
        let panic = self.first.borrow_mut().take();
        if let Some(panic) = panic {
            if unwinding {
                discard_retirement_panic(panic);
            } else {
                resume_unwind(panic);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn ready_output_drop_preserves_the_actual_admitted_job_panic() {
        const TEST: &str = "wasm_c_api::store::v8::tests::ready_output_drop_preserves_the_actual_admitted_job_panic";
        const CHILD: &str = "WASMER_CAPI_V8_READY_OUTPUT_CHILD";
        if std::env::var_os(CHILD).is_none() {
            struct OwnedChild(std::process::Child);
            impl Drop for OwnedChild {
                fn drop(&mut self) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
            let mut child = OwnedChild(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture"])
                    .env(CHILD, "1")
                    .spawn()
                    .unwrap(),
            );
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                match child.0.try_wait() {
                    Ok(Some(status)) => {
                        assert!(status.success(), "V8 output custody child failed: {status}");
                        return;
                    }
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    status => panic!("V8 output custody did not finish: {status:?}"),
                }
            }
        }
        #[derive(Debug)]
        struct Payload(Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        #[derive(Debug)]
        struct Output {
            drops: Arc<AtomicUsize>,
            secondary: Option<Payload>,
        }
        impl Drop for Output {
            fn drop(&mut self) {
                self.drops.fetch_add(1, Ordering::SeqCst);
                if let Some(secondary) = self.secondary.take() {
                    resume_unwind(Box::new(secondary));
                }
            }
        }
        for secondary in [false, true] {
            let managed = ManagedStore::new(Store::new(wasmer_api::v8::V8::new()));
            let function = managed
                .with_mut(|store| Function::new_typed_async(store, || async { 37i32 }))
                .unwrap();
            let driver = managed.driver().unwrap();
            let first_drops = Arc::new(AtomicUsize::new(0));
            let secondary_drops = Arc::new(AtomicUsize::new(0));
            let output_drops = Arc::new(AtomicUsize::new(0));
            let main_polls = Arc::new(AtomicUsize::new(0));
            let job_polls = Arc::new(AtomicUsize::new(0));
            let first = Box::new(Payload(first_drops.clone()));
            let identity = first.as_ref() as *const Payload;
            let entry = driver.entry().unwrap();
            let jobs = job_polls.clone();
            // Actual admission into the SAME queue polled by Entry.drive.
            Sink(Rc::downgrade(&driver)).retain(Box::pin(async move {
                jobs.fetch_add(1, Ordering::SeqCst);
                resume_unwind(first);
            }));
            let mut output = Some(Output {
                drops: output_drops.clone(),
                secondary: secondary.then(|| Payload(secondary_drops.clone())),
            });
            let caught = catch_unwind(AssertUnwindSafe(|| {
                entry.root(poll_fn(|_| {
                    main_polls.fetch_add(1, Ordering::SeqCst);
                    Poll::Ready(output.take().unwrap())
                }))
            }))
            .expect_err("the actual admitted job must propagate its first panic");
            drop(entry);
            assert!(std::ptr::eq(
                caught.downcast_ref::<Payload>().unwrap(),
                identity
            ));
            assert_eq!(main_polls.load(Ordering::SeqCst), 1);
            assert_eq!(job_polls.load(Ordering::SeqCst), 1);
            assert_eq!(output_drops.load(Ordering::SeqCst), 1);
            assert_eq!(first_drops.load(Ordering::SeqCst), 0);
            assert_eq!(
                secondary_drops.load(Ordering::SeqCst),
                usize::from(secondary)
            );
            assert!(driver.jobs.borrow().is_empty());
            drop(caught);
            assert_eq!(first_drops.load(Ordering::SeqCst), 1);
            let result = managed.call(&function, vec![]).unwrap();
            assert_eq!(result[0].i32(), Some(37));
            drop(result);
            drop(function);
            drop(driver);
            drop(managed);
        }
    }
}

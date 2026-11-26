#![cfg(all(
    feature = "async",
    feature = "sys",
    not(feature = "v8-default"),
    not(target_arch = "wasm32")
))]
#![forbid(unsafe_code)]
//! Owning-thread execution: an embedder thread makes a store, shares it, and
//! is the only thread that ever enters it. Other threads ask it for work over
//! a channel and wake its tasks, and the guest can end while calls are still
//! on the native stack.

use std::{
    cell::RefCell,
    collections::BTreeMap,
    fmt,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex, mpsc},
    task::{Context, Poll, Wake, Waker},
    thread,
    time::Duration,
};

use futures::channel::oneshot;
use wasmer::{
    AsyncEnv, Engine, Function, FunctionEnv, FunctionEnvMut, Instance, Module, RuntimeError,
    SharedStore, Store, StoreBusy, StoreMut, Value, WeakSharedStore, imports,
};
use wasmer_vm::TrapCode;

const GUEST: &str = r#"(module
  (import "h" "reenter" (func $reenter (param i32) (result i32)))
  (import "h" "lend" (func $lend (param i32) (result i32)))
  (import "h" "exit" (func $exit (param i32)))
  (import "h" "wait" (func $wait (param i32) (result i32)))
  (import "h" "relay" (func $relay (param i32) (result i32)))
  (func (export "add_one") (param i32) (result i32)
    (i32.add (local.get 0) (i32.const 1)))
  ;; Re-enters the guest through a host function, and adds one.
  (func (export "outer") (param i32) (result i32)
    (i32.add (call $reenter (local.get 0)) (i32.const 1)))
  ;; The same through a host function that lends its store.
  (func (export "outer_lent") (param i32) (result i32)
    (i32.add (call $lend (local.get 0)) (i32.const 1)))
  ;; Ends the guest for a negative argument, traps for zero, and doubles any
  ;; other.
  (func (export "inner") (param i32) (result i32)
    (if (i32.lt_s (local.get 0) (i32.const 0))
      (then (call $exit (local.get 0))))
    (if (i32.eqz (local.get 0))
      (then (unreachable)))
    (i32.mul (local.get 0) (i32.const 2)))
  ;; Waits for another thread, and adds one to what it sent.
  (func (export "wait_then_add_one") (param i32) (result i32)
    (i32.add (call $wait (local.get 0)) (i32.const 1)))
  ;; Re-enters the guest through a host function that waits for it.
  (func (export "outer_relayed") (param i32) (result i32)
    (i32.add (call $relay (local.get 0)) (i32.const 1))))"#;

const TIMEOUT: Duration = Duration::from_secs(30);

/// What happened, in order, on whichever thread.
#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    fn note(&self, what: impl Into<String>) {
        self.0.lock().unwrap().push(what.into());
    }

    fn take(&self) -> Vec<String> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

/// How the guest ends: the error its `exit` import returns.
#[derive(Debug)]
struct Exit(i32);

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the guest ended with {}", self.0)
    }
}

impl std::error::Error for Exit {}

/// The host state. It is `Send`, as host state must be, so nothing bound to
/// the owning thread is in it: that is in [`OWNED`].
struct Host {
    inner: Option<Function>,
    waits_then_adds_one: Option<Function>,
    /// The code the guest ended with.
    exit: Option<i32>,
    /// How many host futures released what they held in the store.
    released: u32,
    log: Log,
    /// Where `wait` sends what another thread is to complete.
    waits: mpsc::Sender<oneshot::Sender<i32>>,
}

impl Drop for Host {
    fn drop(&mut self) {
        self.log.note("host state dropped");
    }
}

type Reply = Result<i32, String>;
type Task = Pin<Box<dyn Future<Output = Result<Box<[Value]>, RuntimeError>>>>;
type CleanUp = Box<dyn FnOnce(&mut StoreMut<'_>)>;

/// A task whose guest waits, and who asked for it.
struct Suspended {
    task: Task,
    reply: mpsc::Sender<Reply>,
}

/// What only the owning thread can reach. A task of a shared store is
/// `!Send`, so it cannot be in the host state, and imports find it here.
#[derive(Default)]
struct Owned {
    /// A task is taken out while it is polled.
    suspended: BTreeMap<usize, Suspended>,
    /// Clean-up that found the store busy, in the order it was refused.
    deferred: Vec<CleanUp>,
}

thread_local! {
    static OWNED: RefCell<Owned> = RefCell::default();
}

/// Runs `clean_up` with the store now, or leaves it for the owning thread to
/// run once it has left the store, if frames below still hold it.
fn run_or_defer(
    store: &SharedStore,
    log: &Log,
    clean_up: impl FnOnce(&mut StoreMut<'_>) + 'static,
) {
    let mut clean_up = Some(clean_up);
    let ran = store.try_with(|mut store| (clean_up.take().unwrap())(&mut store));
    if ran == Err(StoreBusy) {
        log.note("clean-up deferred: the store is busy");
        let clean_up = Box::new(clean_up.take().unwrap());
        OWNED.with_borrow_mut(|owned| owned.deferred.push(clean_up));
    }
}

/// Runs the clean-up that was refused while the store was busy, oldest first.
fn run_deferred(store: &SharedStore) {
    let deferred = OWNED.with_borrow_mut(|owned| std::mem::take(&mut owned.deferred));
    for clean_up in deferred {
        store
            .try_with(|mut store| clean_up(&mut store))
            .expect("the owning thread has left the store");
    }
}

/// What a `wait` holds in the store while it waits. Its destructor needs
/// the store, wherever its call is dropped, so it asks a handle:
/// `AsyncEnv::with` would panic in another slice of the store.
struct Held {
    store: WeakSharedStore,
    env: FunctionEnv<Host>,
    log: Log,
}

impl Drop for Held {
    fn drop(&mut self) {
        let store = self.store.upgrade().expect("a task keeps its store alive");
        let env = self.env.clone();
        run_or_defer(&store, &self.log, move |store| {
            let host = env.as_mut(store);
            host.released += 1;
            host.log.note("released with the store");
        });
    }
}

/// Notes when a host function is over: its frame left the native stack, or
/// its future was dropped.
struct Frame(&'static str, Log);

impl Drop for Frame {
    fn drop(&mut self) {
        self.1.note(format!("{} is over", self.0));
    }
}

/// Calls `inner` on the store this host function was given, and goes on with
/// its own borrow whatever became of the guest.
fn reenter(mut env: FunctionEnvMut<Host>, x: i32) -> Result<i32, RuntimeError> {
    let _frame = Frame("reenter", env.data().log.clone());
    let inner = env.data().inner.clone().unwrap();
    let result = inner.call(&mut env, &[Value::I32(x)]);
    note_outcome(&env, "reenter", &result);
    Ok(result?[0].unwrap_i32())
}

/// Calls `inner` as code reached through foreign frames would: with a handle
/// of the store, which this host function lends for as long as it waits.
fn lend(mut env: FunctionEnvMut<Host>, x: i32) -> Result<i32, RuntimeError> {
    let _frame = Frame("lend", env.data().log.clone());
    let inner = env.data().inner.clone().unwrap();
    let shared = env.shared().expect("the store is shared");
    assert_eq!(shared.try_with(|_| ()), Err(StoreBusy));
    let result = env
        .release(|| shared.try_with(|mut store| inner.call(&mut store, &[Value::I32(x)])))
        .expect("the store is lent");
    assert_eq!(shared.try_with(|_| ()), Err(StoreBusy));
    note_outcome(&env, "lend", &result);
    Ok(result?[0].unwrap_i32())
}

fn note_outcome(
    env: &FunctionEnvMut<Host>,
    name: &str,
    result: &Result<Box<[Value]>, RuntimeError>,
) {
    let outcome = match result {
        Ok(_) => "return",
        Err(error) if error.is::<Exit>() => "end",
        Err(_) => "trap",
    };
    env.data()
        .log
        .note(format!("{name} saw the guest {outcome}"));
}

/// Ends the guest from wherever it is called. The store can be neither
/// entered nor taken while frames below hold it, the tasks that wait are
/// cancelled here, and the error unwinds the calls on the stack.
fn exit(mut env: FunctionEnvMut<Host>, code: i32) -> Result<(), RuntimeError> {
    let log = env.data().log.clone();
    let shared = env.shared().expect("the store is shared");
    assert_eq!(shared.try_with(|_| ()), Err(StoreBusy));
    let shared = shared
        .into_store()
        .expect_err("frames below hold the store");
    cancel_suspended(&log);
    // The tasks are gone, and the frames below still keep the store alive.
    assert!(shared.downgrade().upgrade().is_some());
    env.data_mut().exit = Some(code);
    log.note("exit returns");
    Err(RuntimeError::user(Box::new(Exit(code))))
}

/// Drops the tasks that wait, oldest first, which discards their guests
/// without running them, and tells who asked.
fn cancel_suspended(log: &Log) {
    let suspended = OWNED.with_borrow_mut(|owned| std::mem::take(&mut owned.suspended));
    for (id, Suspended { task, reply }) in suspended {
        log.note(format!("task {id} cancelled"));
        drop(task);
        let _ = reply.send(Err("cancelled".into()));
    }
}

/// Waits for another thread to send a number, and adds it.
async fn wait(cx: &mut AsyncEnv<Host>, x: i32) -> Result<i32, RuntimeError> {
    let (complete, completed) = oneshot::channel();
    let _held = cx.with(|env| {
        env.data().waits.send(complete).unwrap();
        Held {
            store: env.shared().expect("the store is shared").downgrade(),
            env: env.as_ref(),
            log: env.data().log.clone(),
        }
    });
    let sent = completed
        .await
        .map_err(|_| RuntimeError::new("nobody completed the wait"))?;
    Ok(x + sent)
}

/// Calls `wait_then_add_one` and waits for it, which a host function can
/// only do if it is async.
async fn relay(cx: &mut AsyncEnv<Host>, x: i32) -> Result<i32, RuntimeError> {
    let (nested, log) = cx.with(|env| {
        let host = env.data();
        (host.waits_then_adds_one.clone().unwrap(), host.log.clone())
    });
    let _frame = Frame("relay", log);
    let results = nested.call_async(cx, &[Value::I32(x)]).await?;
    Ok(results[0].unwrap_i32())
}

/// A guest on a shared store, made on the thread that calls [`Guest::new`].
struct Guest {
    store: SharedStore,
    env: FunctionEnv<Host>,
    instance: Instance,
}

impl Guest {
    fn new(
        engine: Engine,
        module: &Module,
        waits: mpsc::Sender<oneshot::Sender<i32>>,
        log: Log,
    ) -> Self {
        let mut store = Store::new(engine);
        let host = Host {
            inner: None,
            waits_then_adds_one: None,
            exit: None,
            released: 0,
            log,
            waits,
        };
        let env = FunctionEnv::new(&mut store, host);
        let imports = imports! { "h" => {
            "reenter" => Function::new_typed_with_env(&mut store, &env, reenter),
            "lend" => Function::new_typed_with_env(&mut store, &env, lend),
            "exit" => Function::new_typed_with_env(&mut store, &env, exit),
            "wait" => Function::new_typed_async(&mut store, &env, wait),
            "relay" => Function::new_typed_async(&mut store, &env, relay),
        } };
        let instance = Instance::new(&mut store, module, &imports).unwrap();
        let guest = Self {
            store: store.into_shared(),
            env,
            instance,
        };
        guest.host(|host| {
            host.inner = Some(guest.export("inner"));
            host.waits_then_adds_one = Some(guest.export("wait_then_add_one"));
        });
        guest
    }

    fn export(&self, name: &str) -> Function {
        self.instance.exports.get_function(name).unwrap().clone()
    }

    /// Runs an export to its end on this thread.
    fn call(&self, name: &str, arg: i32) -> Reply {
        let export = self.export(name);
        let result = self
            .store
            .try_with(|mut store| export.call(&mut store, &[Value::I32(arg)]))
            .map_err(|busy| busy.to_string())?;
        reply(result)
    }

    /// A task that runs an export, and can wait.
    fn task(&self, name: &str, arg: i32) -> Task {
        let export = self.export(name);
        Box::pin(export.call_async(&mut self.store.clone(), &[Value::I32(arg)]))
    }

    /// Reaches the host state, which needs the store to be free.
    fn host<R>(&self, f: impl FnOnce(&mut Host) -> R) -> R {
        self.store
            .try_with(|mut store| f(self.env.as_mut(&mut store)))
            .expect("the owning thread is not in the store")
    }
}

fn reply(result: Result<Box<[Value]>, RuntimeError>) -> Reply {
    match result {
        Ok(results) => Ok(results[0].unwrap_i32()),
        Err(error) => Err(error.message()),
    }
}

/// What another thread asks of the owning thread.
enum Request {
    /// Run an export to its end.
    Call(&'static str, i32, mpsc::Sender<Reply>),
    /// Run an export as a task, which may wait.
    Spawn(&'static str, i32, mpsc::Sender<Reply>),
    /// A task's waker fired.
    Wake(usize),
    Stop,
}

/// Wakes a task from any thread, by asking its owner to poll it.
struct WakeTask {
    id: usize,
    requests: mpsc::Sender<Request>,
}

impl Wake for WakeTask {
    fn wake(self: Arc<Self>) {
        let _ = self.requests.send(Request::Wake(self.id));
    }
}

/// The embedder's thread: it makes the guest, and nothing else enters it.
struct Owner {
    /// Gone once the guest ended and its store was dropped.
    guest: Option<Guest>,
    ended_with: Option<i32>,
    requests: mpsc::Sender<Request>,
    tasks: usize,
    log: Log,
}

impl Owner {
    fn new(
        engine: Engine,
        module: &Module,
        requests: mpsc::Sender<Request>,
        waits: mpsc::Sender<oneshot::Sender<i32>>,
        log: Log,
    ) -> Self {
        Self {
            guest: Some(Guest::new(engine, module, waits, log.clone())),
            ended_with: None,
            requests,
            tasks: 0,
            log,
        }
    }

    fn serve(mut self, inbox: mpsc::Receiver<Request>) {
        for request in inbox {
            match request {
                Request::Call(name, arg, reply) => {
                    let _ = reply.send(self.call(name, arg));
                }
                Request::Spawn(name, arg, reply) => self.spawn(name, arg, reply),
                Request::Wake(id) => self.wake(id),
                Request::Stop => break,
            }
            self.settle();
        }
    }

    /// Why work is refused once the guest ended.
    fn refusal(&self) -> String {
        Exit(self.ended_with.expect("the guest ended")).to_string()
    }

    fn call(&self, name: &'static str, arg: i32) -> Reply {
        match &self.guest {
            Some(guest) => guest.call(name, arg),
            None => Err(self.refusal()),
        }
    }

    fn spawn(&mut self, name: &'static str, arg: i32, reply: mpsc::Sender<Reply>) {
        let Some(guest) = &self.guest else {
            let _ = reply.send(Err(self.refusal()));
            return;
        };
        let task = guest.task(name, arg);
        self.tasks += 1;
        self.poll(self.tasks, Suspended { task, reply });
    }

    /// Polls a task outside every slice of its store: a waker only sends a
    /// request, so the poll never runs inside the code that woke it.
    fn poll(&self, id: usize, mut suspended: Suspended) {
        let waker = Waker::from(Arc::new(WakeTask {
            id,
            requests: self.requests.clone(),
        }));
        match suspended
            .task
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
        {
            Poll::Ready(result) => {
                let _ = suspended.reply.send(reply(result));
            }
            Poll::Pending => {
                OWNED.with_borrow_mut(|owned| owned.suspended.insert(id, suspended));
            }
        }
    }

    /// A wake of a task that was cancelled meanwhile finds nothing to poll.
    fn wake(&self, id: usize) {
        let suspended = OWNED.with_borrow_mut(|owned| owned.suspended.remove(&id));
        if let Some(suspended) = suspended {
            self.poll(id, suspended);
        }
    }

    /// Runs after each request, when no call of this thread is on the stack:
    /// the store is free, so deferred clean-up runs, and a guest that ended
    /// gives its store back.
    fn settle(&mut self) {
        let Some(guest) = &self.guest else { return };
        run_deferred(&guest.store);
        let Some(code) = guest.host(|host| host.exit) else {
            return;
        };
        cancel_suspended(&self.log);
        run_deferred(&guest.store);
        let released = guest.host(|host| host.released);
        self.log.note(format!("released {released} in all"));
        self.ended_with = Some(code);
        self.drop_store();
    }

    /// With no task and no frame left, the handle is the last one.
    fn drop_store(&mut self) {
        let Guest { store, .. } = self.guest.take().expect("a guest");
        let weak = store.downgrade();
        let store = store.into_store().expect("nothing else holds the store");
        self.log.note("the store came back");
        drop(store);
        assert!(weak.upgrade().is_none());
    }
}

impl Drop for Owner {
    /// The embedder drops its tasks with its guest, so that a test leaves
    /// none to the next one on its thread.
    fn drop(&mut self) {
        drop(OWNED.take());
    }
}

/// Another thread's end of the owning thread's inbox.
struct Client(mpsc::Sender<Request>);

impl Client {
    fn call(&self, name: &'static str, arg: i32) -> Reply {
        let (reply, replied) = mpsc::channel();
        self.0.send(Request::Call(name, arg, reply)).unwrap();
        replied.recv_timeout(TIMEOUT).expect("no reply")
    }

    fn spawn(&self, name: &'static str, arg: i32) -> mpsc::Receiver<Reply> {
        let (reply, replied) = mpsc::channel();
        self.0.send(Request::Spawn(name, arg, reply)).unwrap();
        replied
    }
}

/// What another thread is to complete for the `wait` import.
type Waiting = mpsc::Receiver<oneshot::Sender<i32>>;

/// Starts an owning thread for a guest compiled on this one: an engine and a
/// module can cross threads, and a store cannot.
fn start_owner(log: &Log) -> (Client, Waiting, thread::JoinHandle<()>) {
    let engine = Engine::default();
    let module = Module::new(&engine, GUEST).unwrap();
    let (requests, inbox) = mpsc::channel();
    let (waits, waiting) = mpsc::channel();
    let owner_requests = requests.clone();
    let log = log.clone();
    let owner = thread::spawn(move || {
        Owner::new(engine, &module, owner_requests, waits, log).serve(inbox);
    });
    (Client(requests), waiting, owner)
}

/// An owner on this thread, for a test to drive by hand.
fn local_owner(log: &Log) -> (Owner, Waiting) {
    let engine = Engine::default();
    let module = Module::new(&engine, GUEST).unwrap();
    let (requests, _inbox) = mpsc::channel();
    let (waits, waiting) = mpsc::channel();
    let owner = Owner::new(engine, &module, requests, waits, log.clone());
    (owner, waiting)
}

/// Starts a task on `owner`, and returns where its result goes.
fn spawn(owner: &mut Owner, name: &'static str, arg: i32) -> mpsc::Receiver<Reply> {
    let (reply, replied) = mpsc::channel();
    owner.spawn(name, arg, reply);
    replied
}

/// The whole of it: sync calls and tasks asked for by another thread, a task
/// woken from another thread, an import that re-enters, and a guest that
/// ends in a nested call while two tasks wait.
#[test]
fn an_embedder_thread_owns_a_shared_store() {
    let log = Log::default();
    let (client, waiting, owner) = start_owner(&log);

    // Synchronous calls, two of them through an import that re-enters.
    assert_eq!(client.call("add_one", 1), Ok(2));
    assert_eq!(client.call("outer", 5), Ok(11));
    assert_eq!(client.call("outer_lent", 5), Ok(11));

    // A task waits for this thread. Meanwhile the store is free for others.
    let first = client.spawn("wait_then_add_one", 10);
    let complete = waiting.recv_timeout(TIMEOUT).unwrap();
    assert_eq!(client.call("add_one", 2), Ok(3));
    complete.send(5).unwrap();
    assert_eq!(first.recv_timeout(TIMEOUT), Ok(Ok(16)));

    // Two tasks wait as the guest ends in a nested call.
    let second = client.spawn("wait_then_add_one", 1);
    let third = client.spawn("wait_then_add_one", 2);
    let _never_completed = [
        waiting.recv_timeout(TIMEOUT).unwrap(),
        waiting.recv_timeout(TIMEOUT).unwrap(),
    ];
    assert_eq!(
        log.take(),
        [
            "reenter saw the guest return",
            "reenter is over",
            "lend saw the guest return",
            "lend is over",
            "released with the store"
        ]
    );
    assert_eq!(client.call("outer", -3), Err(Exit(-3).to_string()));
    assert_eq!(second.recv_timeout(TIMEOUT), Ok(Err("cancelled".into())));
    assert_eq!(third.recv_timeout(TIMEOUT), Ok(Err("cancelled".into())));

    // The store is gone, and work is refused with the reason.
    assert_eq!(client.call("add_one", 1), Err(Exit(-3).to_string()));
    let refused = client.spawn("wait_then_add_one", 1);
    assert_eq!(refused.recv_timeout(TIMEOUT), Ok(Err(Exit(-3).to_string())));
    client.0.send(Request::Stop).unwrap();
    owner.join().unwrap();

    assert_eq!(
        log.take(),
        [
            "task 2 cancelled",
            "clean-up deferred: the store is busy",
            "task 3 cancelled",
            "clean-up deferred: the store is busy",
            "exit returns",
            "reenter saw the guest end",
            "reenter is over",
            "released with the store",
            "released with the store",
            "released 3 in all",
            "the store came back",
            "host state dropped",
        ]
    );
}

/// The guest ends in a host function of a task: its sibling is dropped from
/// there, and the task ends with the guest's error.
#[test]
fn a_guest_that_ends_under_a_task_unwinds_the_same_way() {
    let log = Log::default();
    let (mut owner, _waiting) = local_owner(&log);
    let sibling = spawn(&mut owner, "wait_then_add_one", 1);
    let ending = spawn(&mut owner, "outer", -7);
    assert_eq!(ending.try_recv(), Ok(Err(Exit(-7).to_string())));
    assert_eq!(sibling.try_recv(), Ok(Err("cancelled".into())));
    owner.settle();
    assert!(owner.guest.is_none());
    assert_eq!(
        log.take(),
        [
            "task 1 cancelled",
            "clean-up deferred: the store is busy",
            "exit returns",
            "reenter saw the guest end",
            "reenter is over",
            "released with the store",
            "released 1 in all",
            "the store came back",
            "host state dropped",
        ]
    );
}

/// The guest ends in a call made with a lent store, as a callback of an
/// embedded engine makes it: the error passes through the lender.
#[test]
fn a_guest_that_ends_on_a_lent_store_unwinds_through_its_lender() {
    for as_task in [false, true] {
        let log = Log::default();
        let (mut owner, _waiting) = local_owner(&log);
        let sibling = spawn(&mut owner, "wait_then_add_one", 1);
        let ended = if as_task {
            spawn(&mut owner, "outer_lent", -7).try_recv().unwrap()
        } else {
            owner.call("outer_lent", -7)
        };
        assert_eq!(ended, Err(Exit(-7).to_string()));
        assert_eq!(sibling.try_recv(), Ok(Err("cancelled".into())));
        owner.settle();
        assert!(owner.guest.is_none());
        assert_eq!(
            log.take(),
            [
                "task 1 cancelled",
                "clean-up deferred: the store is busy",
                "exit returns",
                "lend saw the guest end",
                "lend is over",
                "released with the store",
                "released 1 in all",
                "the store came back",
                "host state dropped",
            ]
        );
    }
}

/// A trap ends the nested call alone: the host function that made it decides
/// what becomes of its own, and the guest goes on.
#[test]
fn a_nested_call_that_traps_leaves_the_guest_running() {
    let log = Log::default();
    let (mut owner, _waiting) = local_owner(&log);
    let waits = spawn(&mut owner, "wait_then_add_one", 1);
    let trap = TrapCode::UnreachableCodeReached.message();
    assert_eq!(owner.call("outer", 0), Err(trap.to_string()));
    owner.settle();
    assert_eq!(
        log.take(),
        ["reenter saw the guest trap", "reenter is over"]
    );
    assert_eq!(owner.call("outer", 4), Ok(9));
    assert!(waits.try_recv().is_err(), "the task still waits");
}

/// A task holds a handle of its store, so the store outlives every handle
/// the embedder drops while the task waits, and goes with the last task.
#[test]
fn a_task_that_waits_keeps_its_store() {
    let log = Log::default();
    let (mut owner, _waiting) = local_owner(&log);
    let _replied = spawn(&mut owner, "wait_then_add_one", 1);
    let Guest { store, .. } = owner.guest.take().unwrap();
    let weak = store.downgrade();
    drop(store);
    assert!(weak.upgrade().is_some(), "the task holds the store");
    assert!(log.take().is_empty());

    drop(OWNED.take());
    assert!(weak.upgrade().is_none());
    assert_eq!(
        log.take(),
        ["released with the store", "host state dropped"]
    );
}

/// A store is refused to whoever asks while frames below hold it, and a task
/// polled there panics without harm: both are for the owning thread to do
/// once its call has returned.
#[test]
fn work_inside_a_call_is_refused_until_the_call_returns() {
    let log = Log::default();
    let (owner, _waiting) = local_owner(&log);
    let guest = owner.guest.as_ref().unwrap();
    let mut task = guest.task("add_one", 1);
    let poll = |task: &mut Task| task.as_mut().poll(&mut Context::from_waker(Waker::noop()));
    guest
        .store
        .try_with(|_| {
            assert_eq!(guest.call("add_one", 1), Err(StoreBusy.to_string()));
            let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| poll(&mut task)));
            assert!(polled.is_err());
        })
        .unwrap();
    assert_eq!(guest.call("add_one", 1), Ok(2));
    assert!(matches!(poll(&mut task), Poll::Ready(Ok(_))));
}

/// A host function that waits for a guest holds its call in its future, so
/// dropping the outer task ends the calls innermost first, each with the
/// store in reach.
#[test]
fn dropping_a_task_ends_its_nested_calls_innermost_first() {
    let log = Log::default();
    let (mut owner, waiting) = local_owner(&log);
    let relayed = spawn(&mut owner, "outer_relayed", 1);
    waiting.try_recv().unwrap().send(5).unwrap();
    owner.wake(1);
    assert_eq!(relayed.try_recv(), Ok(Ok(8)));
    assert_eq!(log.take(), ["released with the store", "relay is over"]);

    let _dropped = spawn(&mut owner, "outer_relayed", 1);
    let suspended = OWNED.with_borrow_mut(|owned| owned.suspended.remove(&2));
    drop(suspended.expect("the task waits"));
    assert_eq!(log.take(), ["released with the store", "relay is over"]);
}

/// Entered synchronously, a guest cannot wait, and says so. Its host future
/// is dropped under the frames of that entry, which hold the store, so its
/// clean-up is left for when they have returned.
#[test]
fn a_guest_that_cannot_wait_is_refused_and_cleaned_up_after_its_entry() {
    let log = Log::default();
    let (mut owner, _waiting) = local_owner(&log);
    let refused = TrapCode::YieldOutsideAsyncContext.message();
    assert_eq!(owner.call("outer_relayed", 1), Err(refused.to_string()));
    assert_eq!(
        log.take(),
        ["clean-up deferred: the store is busy", "relay is over"]
    );
    owner.settle();
    assert_eq!(log.take(), ["released with the store"]);
    assert_eq!(owner.call("add_one", 1), Ok(2));
}

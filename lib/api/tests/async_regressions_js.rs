//! Regressions from adversarial reviews of the JavaScript guest guard. Each
//! test uses only safe public API.
#![cfg(all(feature = "experimental-async", feature = "js", target_arch = "wasm32"))]
#![deny(unsafe_code)]

use js_sys::Promise;
use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
};
use wasm_bindgen::JsCast;
use wasm_bindgen::{JsValue, closure::Closure};
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::wasm_bindgen_test;
use wasmer::{
    AsStoreMut, AsStoreRef, AsyncEnv, Function, FunctionEnv, FunctionEnvMut, FunctionType,
    Instance, Module, RuntimeError, Store, StoreBusy, Type, Value, imports,
};

type Call = Pin<Box<dyn Future<Output = Result<Box<[Value]>, RuntimeError>>>>;

/// An async host function with no environment, from a closure making its future.
fn async_host<F, Fut>(store: &mut Store, ty: impl Into<FunctionType>, f: F) -> Function
where
    F: Fn(&[Value]) -> Fut + 'static,
    Fut: Future<Output = Result<Vec<Value>, RuntimeError>> + 'static,
{
    let env = FunctionEnv::new(store, ());
    Function::new_async(
        store,
        &env,
        ty,
        async move |_: &mut AsyncEnv<()>, values: &[Value]| f(values).await,
    )
}

fn poll<F: Future + ?Sized>(f: Pin<&mut F>) -> Poll<F::Output> {
    f.poll(&mut Context::from_waker(Waker::noop()))
}

/// Whether `f` panics. A panic is a trap on this target, which a JavaScript
/// frame can catch, so the test harness stays usable afterwards.
fn panics(f: impl FnOnce() + 'static) -> bool {
    let f = Closure::once_into_js(f);
    f.unchecked_ref::<js_sys::Function>()
        .call0(&JsValue::NULL)
        .is_err()
}

async fn tick() {
    JsFuture::from(Promise::resolve(&JsValue::UNDEFINED))
        .await
        .unwrap();
}

async fn settle() {
    for _ in 0..20 {
        tick().await;
    }
}

/// What a test opens to let the host futures that wait for it go on.
#[derive(Default)]
struct GateState {
    open: Cell<bool>,
    waiting: RefCell<Vec<Waker>>,
}

impl GateState {
    fn set(&self, open: bool) {
        self.open.set(open);
        for waker in self.waiting.take() {
            waker.wake();
        }
    }
}

struct Gate(Rc<GateState>);
impl Future for Gate {
    type Output = ();
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0.open.get() {
            Poll::Ready(())
        } else {
            self.0.waiting.borrow_mut().push(cx.waker().clone());
            Poll::Pending
        }
    }
}

fn waiter(store: &mut Store, gate: Rc<GateState>) -> Function {
    async_host(
        store,
        FunctionType::new(vec![], vec![]),
        move |_: &[Value]| {
            let gate = gate.clone();
            async move {
                Gate(gate).await;
                Ok(vec![])
            }
        },
    )
}

fn bump(mut env: FunctionEnvMut<u64>) {
    *env.data_mut() += 1;
}

const WAIT_BUMP: &str = r#"(module
  (import "h" "wait" (func $wait))
  (import "h" "bump" (func $bump))
  (func (export "run") (call $wait) (call $bump)))"#;

fn wait_bump_guest(store: &mut Store, wait: Function, bump: Function) -> Function {
    let module = Module::new(&*store, WAIT_BUMP).unwrap();
    let imports = imports! { "h" => { "wait" => wait, "bump" => bump } };
    Instance::new(store, &module, &imports)
        .unwrap()
        .exports
        .get_function("run")
        .unwrap()
        .clone()
}

/// Guard rule 2: after a forgotten exclusive call, "every public store entry
/// panics until the guest leaves". A `StoreMut` taken before the call does not
/// go through the guarded `Deref`, so `&mut T` stays live while the engine
/// resumes the guest and a host function writes the same `T`.
#[wasm_bindgen_test]
async fn a_store_handle_under_a_forgotten_call_is_refused() {
    let mut store: &'static mut Store = Box::leak(Box::new(Store::default()));
    let env = FunctionEnv::new(&mut store, 0u64);
    let gate = Rc::new(GateState::default());
    let wait = waiter(store, gate.clone());
    let bump = Function::new_typed_with_env(&mut store, &env, bump);
    let run = wait_bump_guest(store, wait, bump);

    let mut lent: wasmer::StoreMut<'static> = store.as_store_mut();
    let mut call = Box::pin(run.call_async(&mut lent, &[]));
    assert!(poll(call.as_mut()).is_pending());
    settle().await; // the guest is suspended and accepted
    gate.set(true);
    assert!(poll(call.as_mut()).is_pending()); // Woken: the engine will resume it
    std::mem::forget(call);

    assert!(
        panics(move || {
            let _ = env.as_mut(&mut lent);
        }),
        "a public store entry reached a store its guest still holds"
    );
}

/// Two shared guests whose imports become ready in the same tick. Each then
/// calls another import. `woken` marks both `Running` before either is on the
/// stack, and `running()` picks the newest, so the older guest's second import
/// is recorded on the newer guest's activation.
#[wasm_bindgen_test]
async fn two_guests_woken_together_keep_their_imports() {
    let mut store = Store::default();
    let gate = Rc::new(GateState::default());
    let seen = Rc::new(RefCell::new(Vec::new()));
    let wait = async_host(&mut store, FunctionType::new(vec![Type::I32], vec![]), {
        let (gate, seen) = (gate.clone(), seen.clone());
        move |args: &[Value]| {
            let gate = gate.clone();
            let seen = seen.clone();
            let arg = args[0].unwrap_i32();
            async move {
                seen.borrow_mut().push(arg);
                Gate(gate).await;
                Ok(vec![])
            }
        }
    });
    let module = Module::new(
        &store,
        r#"(module
          (import "h" "wait" (func $wait (param i32)))
          (func (export "run") (param i32)
            (call $wait (local.get 0))
            (call $wait (i32.add (local.get 0) (i32.const 100)))))"#,
    )
    .unwrap();
    let run = Instance::new(&mut store, &module, &imports! { "h" => { "wait" => wait } })
        .unwrap()
        .exports
        .get_function("run")
        .unwrap()
        .clone();
    let shared = store.into_shared();

    let mut a: Call = Box::pin(run.call_async(&mut shared.clone(), &[Value::I32(1)]));
    assert!(poll(a.as_mut()).is_pending());
    settle().await;
    let mut b: Call = Box::pin(run.call_async(&mut shared.clone(), &[Value::I32(2)]));
    assert!(poll(b.as_mut()).is_pending());
    settle().await;
    assert!(poll(a.as_mut()).is_pending()); // runs host future 1
    assert!(poll(b.as_mut()).is_pending()); // runs host future 2
    settle().await;

    gate.set(true);
    let (mut a_result, mut b_result) = (None, None);
    for _ in 0..400 {
        if a_result.is_none()
            && let Poll::Ready(r) = poll(a.as_mut())
        {
            a_result = Some(r);
        }
        if b_result.is_none()
            && let Poll::Ready(r) = poll(b.as_mut())
        {
            b_result = Some(r);
        }
        if a_result.is_some() && b_result.is_some() {
            break;
        }
        tick().await;
    }
    assert!(
        a_result.is_some() && b_result.is_some(),
        "one guest never finished"
    );
    a_result.unwrap().unwrap();
    b_result.unwrap().unwrap();
}

/// Guard rule 3: a dropped call's guest may run on, "with its host functions
/// refused". When another guest was woken in the same tick, the cancelled
/// guest's host function is admitted under the other guest's activation.
#[wasm_bindgen_test]
async fn a_cancelled_guests_host_function_is_refused() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0u64);
    let gate = Rc::new(GateState::default());
    let wait = waiter(&mut store, gate.clone());
    let bump = Function::new_typed_with_env(&mut store, &env, bump);
    let run = wait_bump_guest(&mut store, wait, bump);
    let shared = store.into_shared();

    let mut a: Call = Box::pin(run.call_async(&mut shared.clone(), &[]));
    assert!(poll(a.as_mut()).is_pending());
    settle().await;
    let mut b: Call = Box::pin(run.call_async(&mut shared.clone(), &[]));
    assert!(poll(b.as_mut()).is_pending());
    settle().await;

    gate.set(true);
    assert!(poll(a.as_mut()).is_pending()); // A: Woken
    assert!(poll(b.as_mut()).is_pending()); // B: Woken
    // Runs after both `woken` microtasks, and before either guest continues.
    let mut slot = Some(a);
    let drop_a = Closure::<dyn FnMut(JsValue)>::new(move |_: JsValue| drop(slot.take()));
    let _ = Promise::resolve(&JsValue::UNDEFINED).then(&drop_a);
    settle().await;
    drop(drop_a);
    for _ in 0..50 {
        if poll(b.as_mut()).is_ready() {
            break;
        }
        tick().await;
    }
    let bumps = shared.try_with(|store| *env.as_ref(&store)).unwrap();
    assert_eq!(bumps, 1, "the cancelled guest's host function ran");
}

fn replace_vec(mut env: FunctionEnvMut<Vec<u8>>) {
    *env.data_mut() = vec![0u8; 4096];
}

/// The same scenario, taken towards a use-after-free: the guest's host
/// function would free the buffer that a live `&mut u8` points into. Taking
/// that reference from the owner's handle is refused.
#[wasm_bindgen_test]
async fn a_forgotten_call_cannot_free_host_state() {
    let mut store: &'static mut Store = Box::leak(Box::new(Store::default()));
    let env = FunctionEnv::new(&mut store, vec![7u8; 64]);
    let gate = Rc::new(GateState::default());
    let wait = waiter(store, gate.clone());
    let replace = Function::new_typed_with_env(&mut store, &env, replace_vec);
    let run = wait_bump_guest(store, wait, replace);

    let mut lent: wasmer::StoreMut<'static> = store.as_store_mut();
    let mut call = Box::pin(run.call_async(&mut lent, &[]));
    assert!(poll(call.as_mut()).is_pending());
    settle().await;
    gate.set(true);
    assert!(poll(call.as_mut()).is_pending());
    std::mem::forget(call);

    assert!(
        panics(move || {
            let _ = env.as_mut(&mut lent);
        }),
        "a reference into host state was taken under a running guest"
    );
}

/// After the crossed imports above, the stranded guest stays `Running`. Even
/// once its call is dropped, the shared store can never enter a guest again.
#[wasm_bindgen_test]
async fn guests_woken_together_leave_the_store_usable() {
    let mut store = Store::default();
    let gate = Rc::new(GateState::default());
    let wait = async_host(&mut store, FunctionType::new(vec![Type::I32], vec![]), {
        let gate = gate.clone();
        move |_: &[Value]| {
            let gate = gate.clone();
            async move {
                Gate(gate).await;
                Ok(vec![])
            }
        }
    });
    let module = Module::new(
        &store,
        r#"(module
          (import "h" "wait" (func $wait (param i32)))
          (func (export "plain") (result i32) i32.const 7)
          (func (export "run") (param i32)
            (call $wait (local.get 0))
            (call $wait (local.get 0))))"#,
    )
    .unwrap();
    let instance =
        Instance::new(&mut store, &module, &imports! { "h" => { "wait" => wait } }).unwrap();
    let run = instance.exports.get_function("run").unwrap().clone();
    let plain = instance.exports.get_function("plain").unwrap().clone();
    let shared = store.into_shared();

    let mut a: Call = Box::pin(run.call_async(&mut shared.clone(), &[Value::I32(1)]));
    assert!(poll(a.as_mut()).is_pending());
    settle().await;
    let mut b: Call = Box::pin(run.call_async(&mut shared.clone(), &[Value::I32(2)]));
    assert!(poll(b.as_mut()).is_pending());
    settle().await;
    gate.set(true);
    let (mut a_done, mut b_done) = (false, false);
    for _ in 0..400 {
        a_done = a_done || poll(a.as_mut()).is_ready();
        b_done = b_done || poll(b.as_mut()).is_ready();
        tick().await;
    }
    drop(a);
    drop(b);
    settle().await;

    let sync = shared.try_with(|mut store| plain.call(&mut store, &[]));
    let mut fresh: Call = Box::pin(plain.call_async(&mut shared.clone(), &[]));
    let mut fresh_done = false;
    for _ in 0..200 {
        if poll(fresh.as_mut()).is_ready() {
            fresh_done = true;
            break;
        }
        tick().await;
    }
    assert!(
        matches!(sync, Ok(Ok(_))) && fresh_done,
        "the store is bricked"
    );
}

const READY_GUEST: &str = r#"(module
  (import "h" "wait" (func $wait (result i32)))
  (func (export "run") (result i32) call $wait))"#;

fn ready_import(store: &mut Store) -> Function {
    let env = FunctionEnv::new(store, ());
    Function::new_typed_async(store, &env, async |_: &mut AsyncEnv<()>| -> i32 { 7 })
}

fn guest_of(store: &mut Store, wait: Function) -> Function {
    let module = Module::new(&*store, READY_GUEST).unwrap();
    let imports = imports! { "h" => { "wait" => wait } };
    let instance = Instance::new(store, &module, &imports).unwrap();
    instance.exports.get_function("run").unwrap().clone()
}

/// The engine suspends a guest for every async import, also a ready one, and
/// refuses to where the guest was entered synchronously.
#[wasm_bindgen_test]
fn an_async_import_fails_on_a_sync_call() {
    let mut store = Store::default();
    let wait = ready_import(&mut store);
    let run = guest_of(&mut store, wait);
    let error = run.call(&mut store, &[]).unwrap_err();
    assert_eq!(
        error.clone().to_trap(),
        Some(wasmer_types::TrapCode::YieldOutsideAsyncContext),
        "{error}"
    );
}

#[wasm_bindgen_test]
fn an_async_import_fails_in_a_start_function() {
    let mut store = Store::default();
    let wait = ready_import(&mut store);
    let module = Module::new(
        &store,
        r#"(module
          (import "h" "wait" (func $wait (result i32)))
          (global (export "started") (mut i32) (i32.const 0))
          (func $start (global.set 0 (call $wait)))
          (start $start))"#,
    )
    .unwrap();
    let imports = imports! { "h" => { "wait" => wait } };
    assert!(Instance::new(&mut store, &module, &imports).is_err());
}

/// A JavaScript exception crosses a host slice without running its Rust
/// destructors. The call fails, and the store stays usable.
#[wasm_bindgen_test]
async fn an_exception_through_a_host_slice_fails_the_call() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 1i32);
    let wait = Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<i32>| -> i32 {
        tick().await;
        cx.with(|_| -> i32 { wasm_bindgen::throw_str("thrown in a slice") })
    });
    let run = guest_of(&mut store, wait);
    let mut shared = store.into_shared();
    let error = run.call_async(&mut shared, &[]).await.unwrap_err();
    assert!(error.message().contains("thrown in a slice"), "{error}");
    let value = shared.try_with(|store| *env.as_ref(&store)).unwrap();
    assert_eq!(value, 1);
    assert!(shared.into_store().is_ok());
}

/// An exception that skipped a slice must not switch the guard off.
#[wasm_bindgen_test]
async fn an_exception_does_not_disable_the_guard() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    let throws =
        Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<()>| -> i32 {
            tick().await;
            cx.with(|_| -> i32 { wasm_bindgen::throw_str("thrown in a slice") })
        });
    let throwing = guest_of(&mut store, throws);
    assert!(throwing.call_async(&mut store, &[]).await.is_err());

    let gate = Rc::new(GateState::default());
    let wait = waiter(&mut store, gate);
    let bump_env = FunctionEnv::new(&mut store, 0u64);
    let bump = Function::new_typed_with_env(&mut store, &bump_env, bump);
    let run = wait_bump_guest(&mut store, wait, bump);
    let mut call = Box::pin(run.call_async(&mut store, &[]));
    assert!(poll(call.as_mut()).is_pending());
    std::mem::forget(call);
    assert!(panics(move || {
        let _ = store.as_store_ref();
    }));
}

/// An exception through `parked` leaves no lent store behind.
#[wasm_bindgen_test]
fn an_exception_through_a_lend_leaves_nothing_lent() {
    let store: &'static mut Store = Box::leak(Box::new(Store::default()));
    let throws = Closure::once_into_js(move || {
        store
            .as_store_mut()
            .parked(|| wasm_bindgen::throw_str("thrown while lent"));
    });
    let thrown = throws
        .unchecked_ref::<js_sys::Function>()
        .call0(&JsValue::NULL);
    assert!(thrown.is_err());
    assert!(Store::with_current(|_| ()).is_none());
}

/// An exception through `try_with` gives the slice back.
#[wasm_bindgen_test]
fn an_exception_through_try_with_frees_the_store() {
    let shared = Store::default().into_shared();
    let handle = shared.clone();
    let throws = Closure::once_into_js(move || {
        let _ = handle.try_with(|_| wasm_bindgen::throw_str("thrown in a slice"));
    });
    let thrown = throws
        .unchecked_ref::<js_sys::Function>()
        .call0(&JsValue::NULL);
    assert!(thrown.is_err());
    assert!(shared.try_with(|_| ()).is_ok());
}

thread_local! {
    static SLOT: RefCell<Option<Call>> = const { RefCell::new(None) };
    static OBSERVED: Cell<Option<(bool, i32)>> = const { Cell::new(None) };
    static OWNER: RefCell<Option<Store>> = const { RefCell::new(None) };
}

const WAIT_THEN_HOST: &str = r#"(module
  (import "h" "wait" (func $wait))
  (import "h" "host" (func $host))
  (func (export "run") (call $wait) (call $host)))"#;

/// Dropping a call from its own guest's host function leaves that function
/// the only holder of the store until it returns.
#[wasm_bindgen_test]
async fn a_call_dropped_by_its_host_function_keeps_the_store_held() {
    fn host(mut env: FunctionEnvMut<i32>) {
        let shared = env.shared().expect("a shared store");
        let handle = env.as_ref();
        drop(SLOT.with(|slot| slot.borrow_mut().take()));
        let data: &mut i32 = env.data_mut();
        *data = 1;
        let granted = shared
            .try_with(|mut store| *handle.as_mut(&mut store) = 2)
            .is_ok();
        OBSERVED.set(Some((granted, *data)));
    }

    let mut store = Store::default();
    let module = Module::new(&store, WAIT_THEN_HOST).unwrap();
    let env = FunctionEnv::new(&mut store, 0i32);
    let wait = Function::new_typed_async(&mut store, &env, async |_: &mut AsyncEnv<i32>| {});
    let host = Function::new_typed_with_env(&mut store, &env, host);
    let imports = imports! { "h" => { "wait" => wait, "host" => host } };
    let instance = Instance::new(&mut store, &module, &imports).unwrap();
    let run = instance.exports.get_function("run").unwrap().clone();

    let mut shared = store.into_shared();
    let mut call: Call = Box::pin(run.call_async(&mut shared, &[]));
    assert!(poll(call.as_mut()).is_pending());
    SLOT.with(|slot| *slot.borrow_mut() = Some(call));
    settle().await;
    assert_eq!(OBSERVED.get(), Some((false, 1)));
    assert!(
        shared.try_with(|_| ()).is_ok(),
        "free once the host function returned"
    );
}

/// The store's owner is refused inside a slice of a forgotten call, however
/// a host function reaches it.
#[wasm_bindgen_test]
async fn the_owner_is_refused_inside_a_forgotten_calls_slice() {
    fn host(mut env: FunctionEnvMut<i32>) {
        let handle = env.as_ref();
        let data: &mut i32 = env.data_mut();
        *data = 1;
        // Panics: the guard refuses the owner, so nothing below runs.
        OWNER.with(|owner| *handle.as_mut(owner.borrow_mut().as_mut().unwrap()) = 2);
        OBSERVED.set(Some((true, *data)));
    }

    let mut store = Store::default();
    let module = Module::new(&store, WAIT_THEN_HOST).unwrap();
    let env = FunctionEnv::new(&mut store, 0i32);
    let wait = Function::new_typed_async(&mut store, &env, async |_: &mut AsyncEnv<i32>| {});
    let host = Function::new_typed_with_env(&mut store, &env, host);
    let imports = imports! { "h" => { "wait" => wait, "host" => host } };
    let instance = Instance::new(&mut store, &module, &imports).unwrap();
    let run = instance.exports.get_function("run").unwrap().clone();

    let mut call = Box::pin(run.call_async(&mut store, &[]));
    assert!(poll(call.as_mut()).is_pending());
    std::mem::forget(call);
    OWNER.with(|owner| *owner.borrow_mut() = Some(store));
    OBSERVED.set(None);
    settle().await;
    assert_eq!(
        OBSERVED.get(),
        None,
        "the owner reached a store held by a slice"
    );
}

/// A forgotten exclusive call made from a slice keeps the shared store held.
#[wasm_bindgen_test]
fn a_forgotten_call_from_a_slice_keeps_the_shared_store_busy() {
    let gate = Rc::new(GateState::default());
    let mut store = Store::default();
    let wait = waiter(&mut store, gate);
    let env = FunctionEnv::new(&mut store, 0u64);
    let bump = Function::new_typed_with_env(&mut store, &env, bump);
    let run = wait_bump_guest(&mut store, wait, bump);
    let shared = store.into_shared();
    shared
        .try_with(|mut store| {
            let mut call = Box::pin(run.call_async(&mut store, &[]));
            assert!(poll(call.as_mut()).is_pending());
            std::mem::forget(call);
        })
        .unwrap();
    assert_eq!(shared.try_with(|_| ()).err(), Some(StoreBusy));
}

/// A waker that polls inline cannot serve a wake from inside its own poll.
#[wasm_bindgen_test]
async fn a_wake_during_an_inline_poll_is_not_lost() {
    use std::{sync::Arc, task::Wake};

    struct Once(bool);
    impl Future for Once {
        type Output = ();
        fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
            if std::mem::replace(&mut self.0, true) {
                return Poll::Ready(());
            }
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
    struct Inline {
        call: RefCell<Option<Call>>,
        done: Cell<bool>,
    }
    // The test runs on one thread; a waker has to be `Send` and `Sync`.
    struct OnOneThread(Inline);
    #[allow(unsafe_code)]
    unsafe impl Send for OnOneThread {}
    #[allow(unsafe_code)]
    unsafe impl Sync for OnOneThread {}
    impl Wake for OnOneThread {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            let Ok(mut call) = self.0.call.try_borrow_mut() else {
                return;
            };
            let Some(future) = call.as_mut() else {
                return;
            };
            let waker = Waker::from(self.clone());
            if future
                .as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_ready()
            {
                self.0.done.set(true);
                *call = None;
            }
        }
    }

    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    let wait = Function::new_typed_async(&mut store, &env, async |_: &mut AsyncEnv<()>| -> i32 {
        Once(false).await;
        7
    });
    let run = guest_of(&mut store, wait);
    let mut shared = store.into_shared();
    let call: Call = Box::pin(run.call_async(&mut shared, &[]));
    let inline = Arc::new(OnOneThread(Inline {
        call: RefCell::new(Some(call)),
        done: Cell::new(false),
    }));
    inline.wake_by_ref();
    for _ in 0..200 {
        if inline.0.done.get() {
            break;
        }
        tick().await;
    }
    assert!(inline.0.done.get(), "the wake was lost");
}

//! Acceptance tests for the borrowing API and its JavaScript guest guard.
#![cfg(all(feature = "experimental-async", feature = "js", target_arch = "wasm32"))]

use js_sys::Promise;
use std::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
};
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::wasm_bindgen_test;
use wasmer::{
    AsyncEnv, Function, FunctionEnv, FunctionEnvMut, Instance, Module, RuntimeError, Store, Value,
    WeakSharedStore, imports,
};

type Call = Pin<Box<dyn Future<Output = Result<Box<[Value]>, RuntimeError>>>>;

fn poll<F: Future>(f: Pin<&mut F>) -> Poll<F::Output> {
    f.poll(&mut Context::from_waker(Waker::noop()))
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

fn guest(store: &mut Store, host: Function) -> Function {
    let module = Module::new(
        &store,
        r#"(module
      (import "h" "f" (func $f (param i32) (result i32)))
      (func (export "run") (param i32) (result i32) local.get 0 call $f))"#,
    )
    .unwrap();
    Instance::new(store, &module, &imports! { "h" => { "f" => host } })
        .unwrap()
        .exports
        .get_function("run")
        .unwrap()
        .clone()
}

#[wasm_bindgen_test]
async fn typed_host_borrows_in_slices_and_resumes() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0i32);
    let host =
        Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<i32>, n: i32| {
            cx.with(|mut env| *env.data_mut() = n);
            tick().await;
            cx.with(|mut env| {
                *env.data_mut() += 1;
                *env.data()
            })
        });
    let run = guest(&mut store, host);
    let typed = run.typed::<i32, i32>(&store).unwrap();
    assert_eq!(typed.call_async(&mut store, 41).await.unwrap(), 42);
    assert_eq!(*env.as_ref(&store), 42);
}

#[wasm_bindgen_test]
fn ready_typed_import_works_on_sync_entry_and_start() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0i32);
    let host =
        Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<i32>, n: i32| {
            cx.with(|mut env| *env.data_mut() += 1);
            n + 1
        });
    assert_eq!(
        host.call(&mut store, &[Value::I32(10)]).unwrap()[0],
        Value::I32(11)
    );
    let run = guest(&mut store, host.clone());
    assert_eq!(
        run.call(&mut store, &[Value::I32(41)]).unwrap()[0],
        Value::I32(42)
    );
    let module = Module::new(
        &store,
        r#"(module
      (import "h" "f" (func $f (param i32) (result i32)))
      (func $start (drop (call $f (i32.const 1)))) (start $start))"#,
    )
    .unwrap();
    Instance::new(&mut store, &module, &imports! { "h" => { "f" => host } }).unwrap();
    assert_eq!(*env.as_ref(&store), 3);
}

#[wasm_bindgen_test]
fn pending_import_traps_on_sync_entry() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    let host = Function::new_typed_async(&mut store, &env, async |_: &mut AsyncEnv<()>, _: i32| {
        std::future::pending::<i32>().await
    });
    let run = guest(&mut store, host);
    let error = run.call(&mut store, &[Value::I32(0)]).unwrap_err();
    assert_eq!(
        error.to_trap(),
        Some(wasmer_types::TrapCode::YieldOutsideAsyncContext)
    );
}

#[wasm_bindgen_test]
async fn dropping_pending_call_allows_immediate_store_use_and_next_call() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0i32);
    let host =
        Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<i32>, n: i32| {
            cx.with(|mut env| *env.data_mut() += 1);
            if n == 0 {
                std::future::pending::<()>().await;
            }
            n
        });
    let run = guest(&mut store, host);
    let args = [Value::I32(0)];
    let mut call = Box::pin(run.call_async(&mut store, &args));
    assert!(poll(call.as_mut()).is_pending());
    settle().await;
    assert!(poll(call.as_mut()).is_pending());
    drop(call);
    *env.as_mut(&mut store) = 10;
    assert_eq!(
        run.call_async(&mut store, &[Value::I32(7)]).await.unwrap()[0],
        Value::I32(7)
    );
    assert_eq!(*env.as_ref(&store), 11);
}

#[wasm_bindgen_test]
#[should_panic(expected = "store is held by an unfinished async JavaScript call")]
fn forgotten_call_keeps_public_store_entry_closed() {
    let mut store = Store::default();
    let module = Module::new(&store, "(module (func (export \"run\")))").unwrap();
    let instance = Instance::new(&mut store, &module, &wasmer::Imports::new()).unwrap();
    let run = instance.exports.get_function("run").unwrap();
    let mut call = Box::pin(run.call_async(&mut store, &[]));
    assert!(poll(call.as_mut()).is_pending());
    std::mem::forget(call);
    let _ = store.engine();
}

#[wasm_bindgen_test]
async fn forgotten_call_coowns_store_until_the_engine_finishes() {
    struct State(Arc<AtomicUsize>);
    impl Drop for State {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }
    let mut store = Store::default();
    let dropped = Arc::new(AtomicUsize::new(0));
    let env = FunctionEnv::new(&mut store, State(dropped.clone()));
    let host =
        Function::new_typed_async(&mut store, &env, async |_: &mut AsyncEnv<State>, n: i32| n);
    let run = guest(&mut store, host);
    let args = [Value::I32(7)];
    let mut call = Box::pin(run.call_async(&mut store, &args));
    assert!(poll(call.as_mut()).is_pending());
    std::mem::forget(call);
    drop(store);
    settle().await;
    // The forgotten future may leak its owner. It must not free the store
    // under the engine while the engine can still reach its imports.
    assert_eq!(dropped.load(Ordering::Relaxed), 0);
}

#[wasm_bindgen_test]
async fn shared_calls_resume_out_of_order() {
    let mut store = Store::default();
    let gates = [
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    ];
    let env = FunctionEnv::new(&mut store, gates.clone());
    let host = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<[Arc<AtomicBool>; 2]>, n: i32| {
            let gate = cx.with(|env| env.data()[n as usize].clone());
            std::future::poll_fn(move |_| {
                if gate.load(Ordering::Relaxed) {
                    Poll::Ready(n)
                } else {
                    Poll::Pending
                }
            })
            .await
        },
    );
    let run = guest(&mut store, host);
    let mut shared = store.into_shared();
    let mut first = Box::pin(run.call_async(&mut shared, &[Value::I32(0)]));
    let mut second = Box::pin(run.call_async(&mut shared, &[Value::I32(1)]));
    for _ in 0..10 {
        assert!(poll(first.as_mut()).is_pending());
        assert!(poll(second.as_mut()).is_pending());
        tick().await;
    }
    gates[1].store(true, Ordering::Relaxed);
    assert_eq!(second.await.unwrap()[0], Value::I32(1));
    assert!(poll(first.as_mut()).is_pending());
    gates[0].store(true, Ordering::Relaxed);
    assert_eq!(first.await.unwrap()[0], Value::I32(0));
    assert!(shared.into_store().is_ok());
}

#[wasm_bindgen_test]
async fn nested_async_guest_uses_the_parent_capability() {
    let mut store = Store::default();
    let unit = FunctionEnv::new(&mut store, ());
    let child =
        Function::new_typed_async(&mut store, &unit, async |_: &mut AsyncEnv<()>, n: i32| {
            tick().await;
            n + 1
        });
    let inner = guest(&mut store, child);
    let env = FunctionEnv::new(&mut store, inner);
    let host = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<Function>, n: i32| -> Result<i32, RuntimeError> {
            let inner = cx.with(|env| env.data().clone());
            Ok(inner.call_async(cx, &[Value::I32(n)]).await?[0].unwrap_i32())
        },
    );
    let outer = guest(&mut store, host);
    assert_eq!(
        outer
            .call_async(&mut store, &[Value::I32(40)])
            .await
            .unwrap()[0],
        Value::I32(41)
    );
}

#[wasm_bindgen_test]
async fn cancelled_running_guest_cannot_reenter_host_state() {
    thread_local! { static SLOT: RefCell<Option<Call>> = const { RefCell::new(None) }; }
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    let wait = Function::new_typed_async(&mut store, &env, async |_: &mut AsyncEnv<()>| {
        tick().await;
    });
    let touched = Arc::new(AtomicUsize::new(0));
    let cancel = Function::new_typed(&mut store, || {
        SLOT.with(|slot| drop(slot.borrow_mut().take()))
    });
    let touch_env = FunctionEnv::new(&mut store, touched.clone());
    let touch = Function::new_typed_with_env(
        &mut store,
        &touch_env,
        |env: FunctionEnvMut<Arc<AtomicUsize>>| {
            env.data().fetch_add(1, Ordering::Relaxed);
        },
    );
    let module = Module::new(&store, r#"(module
      (import "h" "wait" (func $wait)) (import "h" "cancel" (func $cancel)) (import "h" "touch" (func $touch))
      (func (export "run") call $wait call $cancel call $touch))"#).unwrap();
    let instance = Instance::new(
        &mut store,
        &module,
        &imports! { "h" => { "wait" => wait, "cancel" => cancel, "touch" => touch } },
    )
    .unwrap();
    let run = instance.exports.get_function("run").unwrap();
    let mut shared = store.into_shared();
    SLOT.with(|slot| *slot.borrow_mut() = Some(Box::pin(run.call_async(&mut shared, &[]))));
    for _ in 0..20 {
        let done = SLOT.with(|slot| {
            let mut slot = slot.borrow_mut();
            match slot.as_mut() {
                Some(call) => {
                    assert!(
                        call.as_mut()
                            .poll(&mut Context::from_waker(Waker::noop()))
                            .is_pending()
                    );
                    false
                }
                None => true,
            }
        });
        if done {
            break;
        }
        tick().await;
    }
    settle().await;
    assert!(SLOT.with(|slot| slot.borrow().is_none()));
    assert_eq!(touched.load(Ordering::Relaxed), 0);
    assert!(shared.into_store().is_ok());
}

#[wasm_bindgen_test]
fn persistent_callback_defers_when_busy_and_reenters_during_release() {
    thread_local! { static CALLBACK: RefCell<Option<WeakSharedStore>> = const { RefCell::new(None) }; }
    fn callback() -> bool {
        CALLBACK.with(|callback| {
            callback
                .borrow()
                .as_ref()
                .unwrap()
                .upgrade()
                .unwrap()
                .try_with(|_| ())
                .is_ok()
        })
    }
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    let host = Function::new_typed_with_env(&mut store, &env, |mut env: FunctionEnvMut<()>| {
        assert!(env.shared().is_some());
        assert!(!callback(), "busy callbacks must be deferred");
        assert!(
            env.release(callback),
            "a released borrow allows the callback"
        );
        assert!(!callback(), "the host owns the store again");
    });
    let shared = store.into_shared();
    CALLBACK.with(|callback| *callback.borrow_mut() = Some(shared.downgrade()));
    shared
        .try_with(|mut store| host.call(&mut store, &[]))
        .unwrap()
        .unwrap();
    drop(shared);
    CALLBACK.with(|callback| assert!(callback.borrow_mut().take().unwrap().upgrade().is_none()));
}

#[wasm_bindgen_test]
async fn resumed_guest_releases_its_store_to_a_persistent_callback() {
    thread_local! { static CALLBACK: RefCell<Option<WeakSharedStore>> = const { RefCell::new(None) }; }
    fn callback(target: &Function) -> bool {
        CALLBACK.with(|callback| {
            callback
                .borrow()
                .as_ref()
                .unwrap()
                .upgrade()
                .unwrap()
                .try_with(|mut store| target.call(&mut store, &[]).unwrap()[0] == Value::I32(7))
                .unwrap_or(false)
        })
    }
    let mut store = Store::default();
    let callback_module = Module::new(
        &store,
        "(module (func (export \"callback\") (result i32) i32.const 7))",
    )
    .unwrap();
    let instance = Instance::new(&mut store, &callback_module, &wasmer::Imports::new()).unwrap();
    let target = instance.exports.get_function("callback").unwrap().clone();
    let env = FunctionEnv::new(&mut store, target);
    let host =
        Function::new_typed_with_env(&mut store, &env, |mut env: FunctionEnvMut<Function>| {
            let target = env.data().clone();
            assert!(!callback(&target));
            assert!(env.release(|| callback(&target)));
            assert!(!callback(&target));
        });
    let unit = FunctionEnv::new(&mut store, ());
    let wait = Function::new_typed_async(&mut store, &unit, async |_: &mut AsyncEnv<()>| {
        tick().await;
    });
    let module = Module::new(
        &store,
        r#"(module
      (import "h" "wait" (func $wait)) (import "h" "host" (func $host))
      (func (export "run") call $wait call $host))"#,
    )
    .unwrap();
    let instance = Instance::new(
        &mut store,
        &module,
        &imports! { "h" => { "wait" => wait, "host" => host } },
    )
    .unwrap();
    let run = instance.exports.get_function("run").unwrap();
    let mut shared = store.into_shared();
    CALLBACK.with(|callback| *callback.borrow_mut() = Some(shared.downgrade()));
    run.call_async(&mut shared, &[]).await.unwrap();
    drop(shared);
    CALLBACK.with(|callback| assert!(callback.borrow_mut().take().unwrap().upgrade().is_none()));
}

#[wasm_bindgen_test]
async fn cancellation_allows_store_access_but_waits_before_another_guest() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0i32);
    let module = Module::new(&store, "(module (func (export \"run\")))").unwrap();
    let instance = Instance::new(&mut store, &module, &wasmer::Imports::new()).unwrap();
    let run = instance.exports.get_function("run").unwrap();
    let mut call = Box::pin(run.call_async(&mut store, &[]));
    assert!(poll(call.as_mut()).is_pending());
    drop(call);
    *env.as_mut(&mut store) = 7;
    assert!(
        run.call(&mut store, &[])
            .unwrap_err()
            .message()
            .contains("cancelled")
    );
    let mut next = Box::pin(run.call_async(&mut store, &[]));
    assert!(poll(next.as_mut()).is_pending());
    next.await.unwrap();
    assert_eq!(*env.as_ref(&store), 7);
}

#[wasm_bindgen_test]
async fn dropping_a_call_waiting_for_guest_entry_releases_its_waker() {
    struct Wake(AtomicBool);
    impl std::task::Wake for Wake {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    let mut store = Store::default();
    let module = Module::new(&store, r#"(module (func (export "run")))"#).unwrap();
    let instance = Instance::new(&mut store, &module, &wasmer::Imports::new()).unwrap();
    let run = instance.exports.get_function("run").unwrap();
    let mut call = Box::pin(run.call_async(&mut store, &[]));
    assert!(poll(call.as_mut()).is_pending());
    drop(call);
    let counter = Arc::new(Wake(AtomicBool::new(false)));
    let waker = Waker::from(counter.clone());
    let mut next = Box::pin(run.call_async(&mut store, &[]));
    assert!(
        next.as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(next);
    drop(waker);
    assert_eq!(Arc::strong_count(&counter), 1);
    settle().await;
    assert!(!counter.0.load(Ordering::Relaxed));
}

#[wasm_bindgen_test]
fn pending_start_import_traps_and_leaves_the_store_usable() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0i32);
    let host = Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<i32>| {
        cx.with(|mut env| *env.data_mut() += 1);
        std::future::pending::<()>().await;
    });
    let module = Module::new(
        &store,
        r#"(module
      (import "h" "f" (func $f)) (func $start call $f) (start $start))"#,
    )
    .unwrap();
    let error =
        Instance::new(&mut store, &module, &imports! { "h" => { "f" => host } }).unwrap_err();
    let wasmer::InstantiationError::Start(error) = error else {
        panic!("unexpected instantiation error");
    };
    assert_eq!(
        error.to_trap(),
        Some(wasmer_types::TrapCode::YieldOutsideAsyncContext)
    );
    assert_eq!(*env.as_ref(&store), 1);
    let ready = Function::new_with_env_async(
        &mut store,
        &env,
        wasmer::FunctionType::new([], [wasmer::Type::I32]),
        async |cx: &mut AsyncEnv<i32>, _: Vec<Value>| {
            Ok(cx.with(|mut env| {
                *env.data_mut() += 1;
                vec![Value::I32(*env.data())]
            }))
        },
    );
    assert_eq!(ready.call(&mut store, &[]).unwrap()[0], Value::I32(2));
}

#[wasm_bindgen_test]
async fn a_throwing_resumed_host_releases_its_store_and_owner() {
    let mut store = Store::default();
    let unit = FunctionEnv::new(&mut store, ());
    let wait = Function::new_typed_async(&mut store, &unit, async |_: &mut AsyncEnv<()>| {
        tick().await;
    });
    let throws = Function::new_typed(&mut store, || -> () {
        wasm_bindgen::throw_str("unwound");
    });
    let module = Module::new(
        &store,
        r#"(module
      (import "h" "wait" (func $wait)) (import "h" "throws" (func $throws))
      (func (export "fails") call $wait call $throws)
      (func (export "catches")
        call $wait
        (block $caught (try_table (catch_all $caught) call $throws))
        call $wait))"#,
    )
    .unwrap();
    let instance = Instance::new(
        &mut store,
        &module,
        &imports! { "h" => { "wait" => wait, "throws" => throws } },
    )
    .unwrap();
    let fails = instance.exports.get_function("fails").unwrap();
    let catches = instance.exports.get_function("catches").unwrap();
    let mut shared = store.into_shared();
    assert!(fails.call_async(&mut shared, &[]).await.is_err());
    assert!(shared.try_with(|_| ()).is_ok());
    catches.call_async(&mut shared, &[]).await.unwrap();
    assert!(
        shared.into_store().is_ok(),
        "a skipped host destructor must not retain its activation"
    );
}

#[wasm_bindgen_test]
#[should_panic(expected = "the store is held by an unfinished async JavaScript call")]
async fn a_forgotten_exclusive_call_cannot_be_converted_to_shared() {
    let mut store = Store::default();
    let unit = FunctionEnv::new(&mut store, ());
    let wait = Function::new_typed_async(&mut store, &unit, async |_: &mut AsyncEnv<()>| {
        std::future::pending::<()>().await;
    });
    let mut call = Box::pin(wait.call_async(&mut store, &[]));
    assert!(poll(call.as_mut()).is_pending());
    settle().await;
    assert!(poll(call.as_mut()).is_pending());
    std::mem::forget(call);
    let _ = store.into_shared();
}

#[wasm_bindgen_test]
async fn a_nested_throw_preserves_the_resumed_outer_hosts_hold() {
    struct Host {
        fails: Function,
        waits: Function,
    }
    let mut store = Store::default();
    let unit = FunctionEnv::new(&mut store, ());
    let wait = Function::new_typed_async(&mut store, &unit, async |_: &mut AsyncEnv<()>| {
        tick().await;
    });
    let throws = Function::new_with_env(
        &mut store,
        &unit,
        wasmer::FunctionType::new([], []),
        |_, _| {
            wasm_bindgen::throw_str("nested unwind");
        },
    );
    let module = Module::new(
        &store,
        r#"(module
      (import "h" "wait" (func $wait)) (import "h" "throws" (func $throws))
      (func (export "fails") call $throws) (func (export "waits") call $wait))"#,
    )
    .unwrap();
    let instance = Instance::new(
        &mut store,
        &module,
        &imports! { "h" => { "wait" => wait.clone(), "throws" => throws } },
    )
    .unwrap();
    let env = FunctionEnv::new(
        &mut store,
        Host {
            fails: instance.exports.get_function("fails").unwrap().clone(),
            waits: instance.exports.get_function("waits").unwrap().clone(),
        },
    );
    let outer = Function::new_typed_with_env(&mut store, &env, |mut env: FunctionEnvMut<Host>| {
        let fails = env.data().fails.clone();
        let waits = env.data().waits.clone();
        assert!(fails.call(&mut env, &[]).is_err());
        assert!(env.shared().unwrap().try_with(|_| ()).is_err());
        let error = waits.call(&mut env, &[]).unwrap_err();
        assert_eq!(
            error.to_trap(),
            Some(wasmer_types::TrapCode::YieldOutsideAsyncContext)
        );
        assert!(env.shared().unwrap().try_with(|_| ()).is_err());
    });
    let module = Module::new(
        &store,
        r#"(module
      (import "h" "wait" (func $wait)) (import "h" "outer" (func $outer))
      (func (export "run") call $wait call $outer call $wait))"#,
    )
    .unwrap();
    let instance = Instance::new(
        &mut store,
        &module,
        &imports! { "h" => { "wait" => wait, "outer" => outer } },
    )
    .unwrap();
    let mut shared = store.into_shared();
    instance
        .exports
        .get_function("run")
        .unwrap()
        .call_async(&mut shared, &[])
        .await
        .unwrap();
    assert!(shared.into_store().is_ok());
}

#[wasm_bindgen_test]
async fn a_throw_during_release_restores_the_callback_context() {
    let mut store = Store::default();
    let unit = FunctionEnv::new(&mut store, ());
    let wait = Function::new_typed_async(&mut store, &unit, async |_: &mut AsyncEnv<()>| {
        tick().await;
    });
    let throws =
        Function::new_typed_with_env(&mut store, &unit, |mut env: FunctionEnvMut<()>| -> () {
            env.release(|| {
                assert!(Store::with_current(|_| ()).is_some());
                Store::with_current(|_| wasm_bindgen::throw_str("released"));
            });
        });
    let module = Module::new(
        &store,
        r#"(module
      (import "h" "wait" (func $wait)) (import "h" "throws" (func $throws))
      (func (export "fails") call $wait call $throws)
      (func (export "catches")
        call $wait
        (block $caught (try_table (catch_all $caught) call $throws))
        call $wait))"#,
    )
    .unwrap();
    let instance = Instance::new(
        &mut store,
        &module,
        &imports! { "h" => { "wait" => wait, "throws" => throws } },
    )
    .unwrap();
    let mut shared = store.into_shared();
    assert!(
        instance
            .exports
            .get_function("fails")
            .unwrap()
            .call_async(&mut shared, &[])
            .await
            .is_err()
    );
    assert!(Store::with_current(|_| ()).is_none());
    assert!(shared.try_with(|_| ()).is_ok());
    instance
        .exports
        .get_function("catches")
        .unwrap()
        .call_async(&mut shared, &[])
        .await
        .unwrap();
    assert!(Store::with_current(|_| ()).is_none());
    drop(shared.into_store().unwrap());
    assert!(Store::with_current(|_| ()).is_none());
}

#[wasm_bindgen_test]
fn a_ready_async_import_throw_revokes_its_synchronous_scope() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0i32);
    let host = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<i32>, n: i32| -> i32 {
            cx.with(|mut env| {
                *env.data_mut() += 1;
                if n == 0 {
                    wasm_bindgen::throw_str("ready import unwound");
                }
            });
            n + 1
        },
    );
    let run = guest(&mut store, host);
    assert!(run.call(&mut store, &[Value::I32(0)]).is_err());
    assert_eq!(*env.as_ref(&store), 1);
    assert!(Store::with_current(|_| ()).is_none());
    let mut store = store.into_shared().into_store().unwrap();
    assert_eq!(
        run.call(&mut store, &[Value::I32(1)]).unwrap()[0],
        Value::I32(2)
    );
    assert_eq!(*env.as_ref(&store), 2);
}

#[wasm_bindgen_test]
fn a_nested_call_in_a_sync_import_is_ready_or_traps_without_suspending() {
    let mut store = Store::default();
    let unit = FunctionEnv::new(&mut store, ());
    let child =
        Function::new_typed_async(&mut store, &unit, async |_: &mut AsyncEnv<()>, n: i32| {
            if n == 0 {
                std::future::pending::<()>().await;
            }
            n + 1
        });
    let child = guest(&mut store, child);
    let env = FunctionEnv::new(&mut store, child);
    let outer = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<Function>, n: i32| -> Result<i32, RuntimeError> {
            let child = cx.with(|env| env.data().clone());
            Ok(child.call_async(cx, &[Value::I32(n)]).await?[0].unwrap_i32())
        },
    );
    assert_eq!(
        outer.call(&mut store, &[Value::I32(41)]).unwrap()[0],
        Value::I32(42)
    );
    let run = guest(&mut store, outer);
    let error = run.call(&mut store, &[Value::I32(0)]).unwrap_err();
    assert_eq!(
        error.to_trap(),
        Some(wasmer_types::TrapCode::YieldOutsideAsyncContext)
    );
    assert_eq!(
        run.call(&mut store, &[Value::I32(1)]).unwrap()[0],
        Value::I32(2)
    );
    assert!(store.into_shared().into_store().is_ok());
}

#[wasm_bindgen_test]
async fn a_throw_from_host_poll_restores_slices_and_released_callbacks() {
    for released in [false, true] {
        let mut store = Store::default();
        let env = FunctionEnv::new(&mut store, 0i32);
        let host = Function::new_typed_async(
            &mut store,
            &env,
            async move |cx: &mut AsyncEnv<i32>, n: i32| {
                tick().await;
                cx.with(|mut env| {
                    *env.data_mut() += 1;
                    if n == 0 {
                        if released {
                            env.release(|| {
                                Store::with_current(|_| wasm_bindgen::throw_str("poll escaped"));
                            });
                        } else {
                            wasm_bindgen::throw_str("poll escaped");
                        }
                    }
                });
                n + 1
            },
        );
        let run = guest(&mut store, host);
        let mut shared = store.into_shared();
        let error = run
            .call_async(&mut shared, &[Value::I32(0)])
            .await
            .unwrap_err();
        assert!(error.message().contains("poll escaped"), "{error}");
        assert!(Store::with_current(|_| ()).is_none());
        assert_eq!(shared.try_with(|store| *env.as_ref(&store)).unwrap(), 1);
        assert_eq!(
            run.call_async(&mut shared, &[Value::I32(1)]).await.unwrap()[0],
            Value::I32(2)
        );
        let store = shared.into_store().unwrap();
        assert_eq!(*env.as_ref(&store), 2);
        drop(store);
        assert!(Store::with_current(|_| ()).is_none());
    }
}

#[wasm_bindgen_test]
async fn a_throwing_host_destructor_still_cancels_its_guest() {
    struct ThrowsOnDrop(Arc<AtomicUsize>);
    impl Drop for ThrowsOnDrop {
        fn drop(&mut self) {
            self.0.fetch_add(1, Ordering::Relaxed);
            wasm_bindgen::throw_str("destructor escaped");
        }
    }
    let mut store = Store::default();
    let dropped = Arc::new(AtomicUsize::new(0));
    let env = FunctionEnv::new(&mut store, dropped.clone());
    let host = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<Arc<AtomicUsize>>, n: i32| {
            if n == 0 {
                let _guard = ThrowsOnDrop(cx.with(|env| env.data().clone()));
                std::future::pending::<()>().await;
            }
            n + 1
        },
    );
    let run = guest(&mut store, host);
    let mut shared = store.into_shared();
    let mut call = Box::pin(run.call_async(&mut shared, &[Value::I32(0)]));
    for _ in 0..5 {
        assert!(poll(call.as_mut()).is_pending());
        tick().await;
    }
    drop(call);
    assert_eq!(dropped.load(Ordering::Relaxed), 1);
    assert!(Store::with_current(|_| ()).is_none());
    assert!(shared.try_with(|_| ()).is_ok());
    assert_eq!(
        run.call_async(&mut shared, &[Value::I32(1)]).await.unwrap()[0],
        Value::I32(2)
    );
    assert_eq!(
        dropped.load(Ordering::Relaxed),
        1,
        "a throwing destructor is not retried"
    );
    assert!(shared.into_store().is_ok());
}

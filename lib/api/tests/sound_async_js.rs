//! Regression tests for synchronous async imports, exception cleanup, and cancellation.
#![cfg(all(feature = "experimental-async", feature = "js", target_arch = "wasm32"))]

use js_sys::Promise;
use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    rc::Rc,
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
    imports,
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
async fn ready_dynamic_import_preserves_bigint_and_multiple_results() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0u32);
    let signature = wasmer::FunctionType::new(
        [wasmer::Type::I64, wasmer::Type::F64],
        [wasmer::Type::I64, wasmer::Type::F64],
    );
    let host = Function::new_async(
        &mut store,
        &env,
        signature,
        async |cx: &mut AsyncEnv<u32>, args: &[Value]| {
            cx.with(|mut env| *env.data_mut() += 1);
            Ok(vec![
                Value::I64(args[0].unwrap_i64() + 1),
                Value::F64(args[1].unwrap_f64() + 0.5),
            ])
        },
    );
    let module = Module::new(
        &store,
        r#"(module
          (import "h" "f" (func $f (param i64 f64) (result i64 f64)))
          (func $start i64.const 0 f64.const 0 call $f drop drop)
          (start $start)
          (func (export "run") (param i64 f64) (result i64 f64)
            local.get 0 local.get 1 call $f))"#,
    )
    .unwrap();
    let instance = Instance::new(
        &mut store,
        &module,
        &imports! { "h" => { "f" => host.clone() } },
    )
    .unwrap();
    assert_eq!(*env.as_ref(&store), 1);
    let args = [Value::I64(1i64 << 40), Value::F64(1.5)];
    let expected = [Value::I64((1i64 << 40) + 1), Value::F64(2.0)];
    assert_eq!(&*host.call(&mut store, &args).unwrap(), &expected);
    let run = instance.exports.get_function("run").unwrap();
    assert_eq!(&*run.call(&mut store, &args).unwrap(), &expected);
    assert_eq!(
        run.typed::<(i64, f64), (i64, f64)>(&store)
            .unwrap()
            .call(&mut store, 1i64 << 40, 1.5)
            .unwrap(),
        ((1i64 << 40) + 1, 2.0)
    );
    assert_eq!(
        &*run.call_async(&mut store, &args).await.unwrap(),
        &expected
    );
    assert_eq!(*env.as_ref(&store), 5);
}

#[wasm_bindgen_test]
async fn an_independent_async_task_keeps_its_mode_inside_a_ready_sync_host() {
    let mut other = Store::default();
    let unit = FunctionEnv::new(&mut other, ());
    let pending = Function::new_typed_async(&mut other, &unit, async |_: &mut AsyncEnv<()>| {
        std::future::pending::<()>().await;
    });
    let child = FunctionEnv::new(&mut other, pending);
    let independent = Function::new_async(
        &mut other,
        &child,
        wasmer::FunctionType::new([], []),
        async |cx: &mut AsyncEnv<Function>, _: &[Value]| {
            let child = cx.with(|env| env.data().clone());
            child.call_async(cx, &[]).await?;
            Ok(vec![])
        },
    );
    let other = other.into_shared();
    let slot: Rc<RefCell<Option<Call>>> = Rc::default();
    let output = slot.clone();
    let mut store = Store::default();
    let unit = FunctionEnv::new(&mut store, ());
    let pending = Function::new_typed_async(&mut store, &unit, async |_: &mut AsyncEnv<()>| {
        std::future::pending::<()>().await;
    });
    let child = FunctionEnv::new(&mut store, pending);
    let host = Function::new_typed_async(
        &mut store,
        &child,
        async move |cx: &mut AsyncEnv<Function>| {
            let mut task: Call = Box::pin(independent.call_async(&mut other.clone(), &[]));
            assert!(
                task.as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            *output.borrow_mut() = Some(task);

            // Polling the independent task must also restore this host's
            // synchronous mode: its own pending child must trap immediately.
            let child = cx.with(|env| env.data().clone());
            let error = child.call_async(cx, &[]).await.unwrap_err();
            assert_eq!(
                error.to_trap(),
                Some(wasmer_types::TrapCode::YieldOutsideAsyncContext)
            );
            7i32
        },
    );
    assert_eq!(host.call(&mut store, &[]).unwrap()[0], Value::I32(7));
    settle().await;
    let mut task = slot.borrow_mut().take().unwrap();
    let outcome = task.as_mut().poll(&mut Context::from_waker(Waker::noop()));
    assert!(
        outcome.is_pending(),
        "the independent async task must be allowed to wait on its child: {outcome:?}"
    );
    drop(task);
    settle().await;
}

#[wasm_bindgen_test]
fn a_nested_ready_call_checks_the_async_signature() {
    use wasmer::js::AsJs;

    let mut store = Store::default();
    let unit = FunctionEnv::new(&mut store, ());
    let seven = Function::new_typed_async(&mut store, &unit, async |_: &mut AsyncEnv<()>| 7i32);
    let value = seven.as_jsvalue(&store);
    let wide = Function::from_jsvalue(
        &mut store,
        &wasmer::FunctionType::new([], [wasmer::Type::I64]),
        &value,
    )
    .unwrap();
    let pair = Function::from_jsvalue(
        &mut store,
        &wasmer::FunctionType::new([], [wasmer::Type::I32, wasmer::Type::I32]),
        &value,
    )
    .unwrap();
    let unary =
        Function::new_typed_async(&mut store, &unit, async |_: &mut AsyncEnv<()>, n: i32| n);
    let env = FunctionEnv::new(&mut store, [seven, wide, pair, unary]);
    let outer = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<[Function; 4]>| {
            let [seven, wide, pair, unary] = cx.with(|env| env.data().clone());
            assert!(wide.call_async(cx, &[]).await.is_err(), "result type");
            assert!(pair.call_async(cx, &[]).await.is_err(), "result count");
            assert!(
                seven.call_async(cx, &[Value::I32(1)]).await.is_err(),
                "parameter count"
            );
            assert!(
                unary.call_async(cx, &[Value::I64(1)]).await.is_err(),
                "parameter type"
            );
            7i32
        },
    );
    assert_eq!(
        outer.call(&mut store, &[]).unwrap()[0],
        Value::I32(7),
        "nested call_async must validate its signature before JavaScript coerces values"
    );
}

#[wasm_bindgen_test]
fn nested_ready_calls_preserve_host_reference_results() {
    use wasmer::js::AsJs;

    let mut store = Store::default();
    let module = Module::new(&store, r#"(module (func (export "f")))"#).unwrap();
    let instance = Instance::new(&mut store, &module, &wasmer::Imports::new()).unwrap();
    let wasm = instance.exports.get_function("f").unwrap().clone();
    let sync = Function::new_typed(&mut store, || {});
    let promise = Promise::resolve(&JsValue::from_str("not the reference"));
    let thenable = Value::from_jsvalue(&mut store, &wasmer::Type::ExternRef, &promise).unwrap();
    for value in [
        Value::FuncRef(None),
        Value::FuncRef(Some(wasm)),
        Value::FuncRef(Some(sync)),
        thenable,
    ] {
        let env = FunctionEnv::new(&mut store, value.clone());
        let host = Function::new_async(
            &mut store,
            &env,
            wasmer::FunctionType::new([], [value.ty()]),
            async |cx: &mut AsyncEnv<Value>, _: &[Value]| {
                Ok(vec![cx.with(|env| env.data().clone())])
            },
        );
        let env = FunctionEnv::new(&mut store, (host, value));
        let outer = Function::new_typed_async(
            &mut store,
            &env,
            async |cx: &mut AsyncEnv<(Function, Value)>| {
                let (host, expected) = cx.with(|env| env.data().clone());
                let results = host.call_async(cx, &[]).await.unwrap();
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].ty(), expected.ty());
                assert!(cx.with(|env| js_sys::Object::is(
                    &results[0].as_jsvalue(&env),
                    &expected.as_jsvalue(&env),
                )));
                7i32
            },
        );
        assert_eq!(outer.call(&mut store, &[]).unwrap()[0], Value::I32(7));
    }
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
    let ready = Function::new_async(
        &mut store,
        &env,
        wasmer::FunctionType::new([], [wasmer::Type::I32]),
        async |cx: &mut AsyncEnv<i32>, _: &[Value]| {
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

#[wasm_bindgen_test]
async fn a_throw_in_another_shared_store_restores_its_busy_flag() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    let other = Store::default().into_shared();
    let borrowed_other = other.clone();
    let host = Function::new_typed_async(&mut store, &env, async move |_: &mut AsyncEnv<()>| {
        borrowed_other
            .try_with(|_| wasm_bindgen::throw_str("other store escaped"))
            .unwrap();
    });
    let mut shared = store.into_shared();
    let error = host.call_async(&mut shared, &[]).await.unwrap_err();
    assert!(error.message().contains("other store escaped"), "{error}");
    assert!(
        shared.try_with(|_| ()).is_ok(),
        "the calling store is usable"
    );
    assert!(other.try_with(|_| ()).is_ok(), "the second store is usable");
}

#[wasm_bindgen_test]
async fn cancelling_own_call_retains_the_shared_state_until_its_slice_ends() {
    check_self_cancellation(false).await;
}

#[wasm_bindgen_test]
async fn throwing_after_self_cancellation_retains_state_through_finally() {
    check_self_cancellation(true).await;
}

async fn check_self_cancellation(throw_after_cancel: bool) {
    let slot: Rc<RefCell<Option<Call>>> = Rc::default();
    let ran = Rc::new(Cell::new(false));
    let retained = Rc::new(Cell::new(false));
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0u32);
    let mut shared = store.into_shared();
    let weak = shared.downgrade();
    let host = shared
        .try_with(|mut store| {
            Function::new_typed_async(&mut store, &env, {
                let (slot, ran, retained, weak) =
                    (slot.clone(), ran.clone(), retained.clone(), weak.clone());
                async move |cx: &mut AsyncEnv<u32>| {
                    cx.with(|mut env| {
                        drop(slot.borrow_mut().take());
                        retained.set(weak.upgrade().is_some());
                        *env.data_mut() += 1;
                        ran.set(true);
                        if throw_after_cancel {
                            wasm_bindgen::throw_str("raw throw after self cancellation");
                        }
                    });
                }
            })
        })
        .unwrap();
    let run = shared
        .try_with(|mut store| {
            let module = Module::new(
                &store,
                r#"(module
          (import "h" "host" (func $host))
          (func (export "run") (call $host)))"#,
            )
            .unwrap();
            let instance =
                Instance::new(&mut store, &module, &imports! { "h" => { "host" => host } })
                    .unwrap();
            instance.exports.get_function("run").unwrap().clone()
        })
        .unwrap();
    let mut call: Call = Box::pin(run.call_async(&mut shared, &[]));
    assert!(matches!(
        call.as_mut().poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    *slot.borrow_mut() = Some(call);
    drop(shared);
    for _ in 0..8 {
        wasm_bindgen_futures::JsFuture::from(js_sys::Promise::resolve(&JsValue::UNDEFINED))
            .await
            .unwrap();
    }
    assert!(ran.get(), "the host import ran from its check microtask");
    assert!(
        retained.get(),
        "SharedState was destroyed while its synchronous slice was live"
    );
    assert!(slot.borrow().is_none());
    assert!(
        weak.upgrade().is_none(),
        "the temporary owner was released after the callback"
    );
}

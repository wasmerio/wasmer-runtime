#![cfg(all(
    feature = "experimental-async",
    feature = "v8",
    any(feature = "v8-default", not(feature = "sys")),
    not(target_arch = "wasm32")
))]
//! Async calls on V8, which cannot switch stacks: `poll` runs the call, and
//! blocks while waiting for an import to become ready.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
    thread,
    time::{Duration, Instant},
};

use futures::channel::oneshot;
use wasmer::{
    AsyncEnv, Function, FunctionEnv, FunctionType, Instance, Module, RuntimeError, Store, Type,
    TypedFunction, Value, imports,
};

const GUEST: &str = r#"(module
  (import "h" "wait" (func $wait (param i32) (result i32)))
  (func (export "run") (param i32) (result i32)
    (i32.add (call $wait (local.get 0)) (i32.const 1))))"#;

fn poll_ready<F: Future + ?Sized>(future: &mut Pin<Box<F>>) -> F::Output {
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("a call on V8 finishes in its first poll"),
    }
}

/// `wait(x)` counts in the host state, waits for another thread, and returns
/// `x * 2`.
fn guest() -> (Store, Function, FunctionEnv<i32>) {
    let mut store = Store::default();
    let module = Module::new(&store, GUEST).unwrap();
    let env = FunctionEnv::new(&mut store, 0);
    let wait = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<i32>, x: i32| -> i32 {
            cx.with(|mut env| *env.data_mut() += 1);
            let (send, receive) = oneshot::channel();
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(5));
                send.send(()).unwrap();
            });
            receive.await.unwrap();
            cx.with(|mut env| *env.data_mut() += 10);
            x * 2
        },
    );
    let imports = imports! { "h" => { "wait" => wait } };
    let instance = Instance::new(&mut store, &module, &imports).unwrap();
    let run = instance.exports.get_function("run").unwrap().clone();
    (store, run, env)
}

#[test]
fn poll_blocks_while_an_import_is_driven() {
    let (mut store, run, env) = guest();
    let mut call = Box::pin(run.call_async(&mut store, &[Value::I32(4)]));
    let results = poll_ready(&mut call).unwrap();
    drop(call);
    assert_eq!(results[0].unwrap_i32(), 9);
    assert_eq!(*env.as_ref(&store), 11);
}

#[test]
fn typed_and_shared_calls_work() {
    let (store, run, env) = guest();
    let typed: TypedFunction<i32, i32> = run.typed(&store).unwrap();
    let mut shared = store.into_shared();
    let mut call = Box::pin(typed.call_async(&mut shared, 20));
    assert_eq!(poll_ready(&mut call).unwrap(), 41);
    drop(call);
    assert_eq!(shared.try_with(|store| *env.as_ref(&store)).unwrap(), 11);
    assert!(shared.into_store().is_ok());
}

/// A nested call runs lexically, inside the import that made it.
#[test]
fn a_nested_call_runs_inside_its_import() {
    struct Env(Function);
    let (mut store, inner, _) = guest();
    let env = FunctionEnv::new(&mut store, Env(inner));
    let outer = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<Env>, x: i32| -> Result<i32, RuntimeError> {
            let inner = cx.with(|env| env.data().0.clone());
            let results = inner.call_async(cx, &[Value::I32(x)]).await?;
            Ok(results[0].unwrap_i32() + 100)
        },
    );
    let mut call = Box::pin(outer.call_async(&mut store, &[Value::I32(1)]));
    let results = poll_ready(&mut call).unwrap();
    assert_eq!(results[0].unwrap_i32(), 103);
}

struct YieldOnce(bool);
impl Future for YieldOnce {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

#[test]
fn ready_imports_work_on_dynamic_and_typed_sync_entries() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0i32);
    let function = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<i32>, value: i32| -> i32 {
            cx.with(|mut env| *env.data_mut() += value);
            value + 1
        },
    );
    assert_eq!(
        function.call(&mut store, &[Value::I32(2)]).unwrap()[0],
        Value::I32(3)
    );
    let typed = function.typed::<i32, i32>(&store).unwrap();
    assert_eq!(typed.call(&mut store, 4).unwrap(), 5);
    assert_eq!(*env.as_ref(&store), 6);
}

#[test]
fn pending_imports_trap_on_sync_entries_including_nested_calls() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    let pending = Function::new_typed_async(&mut store, &env, async |_: &mut AsyncEnv<()>| {
        YieldOnce(false).await;
    });
    assert_eq!(
        pending.call(&mut store, &[]).unwrap_err().to_trap(),
        Some(wasmer_types::TrapCode::YieldOutsideAsyncContext),
    );
    assert!(
        pending
            .typed::<(), ()>(&store)
            .unwrap()
            .call(&mut store)
            .is_err()
    );

    // An asynchronous outer call must not make a synchronous nested entry
    // inherit permission to wait while its Rust caller still holds a borrow.
    let outer = Function::new_typed_async(
        &mut store,
        &env,
        async move |cx: &mut AsyncEnv<()>| -> Result<(), RuntimeError> {
            cx.with(|mut env| pending.call(&mut env, &[]))?;
            Ok(())
        },
    );
    let error = futures::executor::block_on(outer.call_async(&mut store, &[])).unwrap_err();
    assert_eq!(
        error.to_trap(),
        Some(wasmer_types::TrapCode::YieldOutsideAsyncContext)
    );
}

#[test]
fn start_functions_accept_ready_imports_and_reject_pending_imports() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0u32);
    let ready = Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<u32>| {
        cx.with(|mut env| *env.data_mut() += 1);
    });
    let pending = Function::new_typed_async(&mut store, &env, async |_: &mut AsyncEnv<u32>| {
        YieldOnce(false).await;
    });
    let module = Module::new(
        &store,
        r#"(module
        (import "h" "f" (func $f))
        (func $start (call $f)) (start $start))"#,
    )
    .unwrap();
    Instance::new(&mut store, &module, &imports! { "h" => { "f" => ready } }).unwrap();
    assert_eq!(*env.as_ref(&store), 1);
    let error =
        Instance::new(&mut store, &module, &imports! { "h" => { "f" => pending } }).unwrap_err();
    let wasmer::InstantiationError::Start(error) = error else {
        panic!("expected the start function to trap");
    };
    assert_eq!(
        error.to_trap(),
        Some(wasmer_types::TrapCode::YieldOutsideAsyncContext)
    );
}

#[test]
fn host_panics_resume_after_leaving_v8_frames() {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0u32);
    let function = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<u32>, _: i32| -> i32 {
            cx.with(|mut env| *env.data_mut() += 1);
            panic!("V8 async host panic");
        },
    );
    let module = Module::new(&store, GUEST).unwrap();
    let instance = Instance::new(
        &mut store,
        &module,
        &imports! { "h" => { "wait" => function } },
    )
    .unwrap();
    let run = instance.exports.get_function("run").unwrap();
    for _ in 0..2 {
        let panic = catch_unwind(AssertUnwindSafe(|| {
            futures::executor::block_on(run.call_async(&mut store, &[Value::I32(1)]))
        }))
        .unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"V8 async host panic"));
    }
    assert_eq!(*env.as_ref(&store), 2);
}

#[test]
fn dynamic_async_results_are_checked_before_returning_to_v8() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    for results in [
        vec![Value::I64(7)],
        vec![],
        vec![Value::I32(1), Value::I32(2)],
    ] {
        let function = Function::new_async(
            &mut store,
            &env,
            FunctionType::new(vec![], vec![Type::I32]),
            async move |_: &mut AsyncEnv<()>, _: &[Value]| Ok(results.clone()),
        );
        for asynchronous in [false, true] {
            let error = if asynchronous {
                futures::executor::block_on(function.call_async(&mut store, &[]))
            } else {
                function.call(&mut store, &[])
            }
            .unwrap_err();
            assert!(
                error.to_string().contains("result signature mismatch"),
                "{error}"
            );
        }
    }
}

#[test]
fn async_imports_reject_another_store() {
    let mut first = Store::default();
    let mut second = Store::default();
    let env = FunctionEnv::new(&mut first, ());
    let function = Function::new_typed_async(&mut first, &env, async |_: &mut AsyncEnv<()>| {});
    let error = futures::executor::block_on(function.call_async(&mut second, &[])).unwrap_err();
    assert!(error.to_string().contains("active guest entry"), "{error}");
}

#[test]
fn nested_driver_preserves_outer_wake() {
    const CHILD: &str = "WASMER_V8_NESTED_WAKE_PROBE";
    if std::env::var_os(CHILD).is_none() {
        // A lost notification blocks inside poll, so isolate the regression
        // in a subprocess with a deadline instead of hanging the test runner.
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "nested_driver_preserves_outer_wake",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                let output = child.wait_with_output().unwrap();
                assert!(
                    status.success(),
                    "nested wake probe failed: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                return;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("nested V8 driver lost its outer wake");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    let inner = Function::new_typed_async(&mut store, &env, async |_: &mut AsyncEnv<()>| {
        let (send, receive) = oneshot::channel();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            send.send(()).unwrap();
        });
        receive.await.unwrap();
    });
    let outer = Function::new_typed_async(
        &mut store,
        &env,
        async move |env: &mut AsyncEnv<()>| -> Result<(), RuntimeError> {
            let mut started = false;
            std::future::poll_fn(|cx| {
                if started {
                    return Poll::Ready(Ok(()));
                }
                started = true;
                cx.waker().wake_by_ref();
                let mut nested = Box::pin(inner.call_async(env, &[]));
                match nested.as_mut().poll(cx) {
                    Poll::Ready(Ok(_)) => {}
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => panic!("a V8 call finishes in its first poll"),
                }
                // A thread-wide unpark token would have been consumed by
                // the nested driver, leaving this valid wake unobserved.
                Poll::Pending
            })
            .await
        },
    );
    futures::executor::block_on(outer.call_async(&mut store, &[])).unwrap();
}

#[test]
fn async_host_errors_preserve_their_original_source() {
    #[derive(Debug)]
    struct HostError;
    impl std::fmt::Display for HostError {
        fn fmt(&self, _f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            panic!("a host error need not be formatted while crossing V8");
        }
    }
    impl std::error::Error for HostError {}

    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    let function = Function::new_async(
        &mut store,
        &env,
        FunctionType::new(vec![], vec![]),
        async |_: &mut AsyncEnv<()>, _: &[Value]| Err(RuntimeError::user(Box::new(HostError))),
    );
    for asynchronous in [false, true] {
        let error = if asynchronous {
            futures::executor::block_on(function.call_async(&mut store, &[]))
        } else {
            function.call(&mut store, &[])
        }
        .unwrap_err();
        assert!(error.downcast_ref::<HostError>().is_some());
    }
}

#[test]
fn synchronous_host_frames_relay_nested_async_panics() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use wasmer::FunctionEnvMut;

    let mut store = Store::default();
    let unit = FunctionEnv::new(&mut store, ());
    let inner = Function::new_typed_async(&mut store, &unit, async |_: &mut AsyncEnv<()>| -> () {
        panic!("nested V8 panic");
    });
    let env = FunctionEnv::new(&mut store, inner);
    let typed = Function::new_typed_with_env(
        &mut store,
        &env,
        |mut env: FunctionEnvMut<Function>| -> Result<(), RuntimeError> {
            let inner = env.data().clone();
            inner.call(&mut env, &[])?;
            Ok(())
        },
    );
    let dynamic = Function::new_with_env(
        &mut store,
        &env,
        FunctionType::new(vec![], vec![]),
        |mut env, _| {
            let inner = env.data().clone();
            inner.call(&mut env, &[]).map(Vec::from)
        },
    );
    let no_env = Function::new_typed(&mut store, || -> () { panic!("nested V8 panic") });
    for function in [typed, dynamic, no_env] {
        let panic = catch_unwind(AssertUnwindSafe(|| {
            futures::executor::block_on(function.call_async(&mut store, &[]))
        }))
        .unwrap_err();
        assert_eq!(panic.downcast_ref::<&str>(), Some(&"nested V8 panic"));
    }
}

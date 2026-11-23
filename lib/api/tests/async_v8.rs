#![cfg(all(
    feature = "experimental-async",
    feature = "v8",
    any(feature = "v8-default", not(feature = "sys")),
    not(target_arch = "wasm32")
))]
//! Async calls on V8, which cannot switch stacks: `poll` runs the call. An
//! exclusive call blocks while another thread drives an import's future.
//! Anything else that would have to suspend fails at once.

use std::{
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::mpsc,
    task::{Context, Poll, Waker},
    thread,
    time::Duration,
};

use futures::channel::oneshot;
use wasmer::{
    AsyncEnv, Deferred, Function, FunctionEnv, FunctionEnvMut, Instance, Module, RuntimeError,
    Store, TypedFunction, Value, imports,
};
use wasmer_types::TrapCode;

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
fn a_typed_exclusive_call_waits() {
    let (mut store, run, env) = guest();
    let typed: TypedFunction<i32, i32> = run.typed(&store).unwrap();
    let mut call = Box::pin(typed.call_async(&mut store, 20));
    assert_eq!(poll_ready(&mut call).unwrap(), 41);
    drop(call);
    assert_eq!(*env.as_ref(&store), 11);
}

/// A guest whose import is ready, and one whose import never is.
fn ready_and_pending() -> (Store, Function, Function) {
    let mut store = Store::default();
    let module = Module::new(&store, GUEST).unwrap();
    let env = FunctionEnv::new(&mut store, ());
    let ready = Function::new_typed_async(
        &mut store,
        &env,
        async |_: &mut AsyncEnv<()>, x: i32| -> i32 { x * 2 },
    );
    let pending = Function::new_typed_async(
        &mut store,
        &env,
        async |_: &mut AsyncEnv<()>, _: i32| -> i32 {
            std::future::pending::<()>().await;
            0
        },
    );
    let mut guest = |wait: Function| {
        let imports = imports! { "h" => { "wait" => wait } };
        let instance = Instance::new(&mut store, &module, &imports).unwrap();
        instance.exports.get_function("run").unwrap().clone()
    };
    let (ready, pending) = (guest(ready), guest(pending));
    (store, ready, pending)
}

/// Runs `f` on a thread of its own, failing if it does not finish.
fn finishes<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    let (send, receive) = mpsc::channel();
    thread::spawn(move || {
        let _ = send.send(catch_unwind(AssertUnwindSafe(f)));
    });
    match receive.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(value)) => value,
        Ok(Err(panic)) => std::panic::resume_unwind(panic),
        Err(_) => panic!("it blocked instead of failing"),
    }
}

fn assert_cannot_wait(error: &RuntimeError) {
    assert_eq!(
        error.clone().to_trap(),
        Some(TrapCode::YieldOutsideAsyncContext),
        "{error}"
    );
}

#[test]
fn a_sync_call_runs_a_ready_import_and_refuses_a_pending_one() {
    finishes(|| {
        let (mut store, ready, pending) = ready_and_pending();
        let results = ready.call(&mut store, &[Value::I32(4)]).unwrap();
        assert_eq!(results[0].unwrap_i32(), 9);
        let error = pending.call(&mut store, &[Value::I32(4)]).unwrap_err();
        assert_cannot_wait(&error);
    });
}

#[test]
fn a_start_function_runs_a_ready_import_and_refuses_a_pending_one() {
    const START: &str = r#"(module
      (import "h" "wait" (func $wait (param i32) (result i32)))
      (global (export "started") (mut i32) (i32.const 0))
      (func $start (global.set 0 (call $wait (i32.const 4))))
      (start $start))"#;
    finishes(|| {
        let mut store = Store::default();
        let env = FunctionEnv::new(&mut store, ());
        let module = Module::new(&store, START).unwrap();
        let ready = Function::new_typed_async(
            &mut store,
            &env,
            async |_: &mut AsyncEnv<()>, x: i32| -> i32 { x * 2 },
        );
        let imports = imports! { "h" => { "wait" => ready } };
        let instance = Instance::new(&mut store, &module, &imports).unwrap();
        let started = instance.exports.get_global("started").unwrap();
        assert_eq!(started.get(&mut store).unwrap_i32(), 8);

        let pending = Function::new_typed_async(
            &mut store,
            &env,
            async |_: &mut AsyncEnv<()>, _: i32| -> i32 {
                std::future::pending::<()>().await;
                0
            },
        );
        let imports = imports! { "h" => { "wait" => pending } };
        assert!(Instance::new(&mut store, &module, &imports).is_err());
    });
}

/// A task that waits could be waiting for one that cannot run until this one
/// returns, so it fails instead of blocking the thread for good.
#[test]
fn a_shared_task_runs_a_ready_import_and_refuses_to_wait() {
    finishes(|| {
        let (store, ready, pending) = ready_and_pending();
        let mut shared = store.into_shared();
        let mut call = Box::pin(ready.call_async(&mut shared, &[Value::I32(4)]));
        assert_eq!(poll_ready(&mut call).unwrap()[0].unwrap_i32(), 9);
        drop(call);
        let mut call = Box::pin(pending.call_async(&mut shared, &[Value::I32(4)]));
        assert_cannot_wait(&poll_ready(&mut call).unwrap_err());
        drop(call);
        assert!(shared.try_with(|_| ()).is_ok(), "the store is free");
    });
}

#[test]
fn a_host_future_panic_reaches_the_poller() {
    finishes(|| {
        let mut store = Store::default();
        let module = Module::new(&store, GUEST).unwrap();
        let env = FunctionEnv::new(&mut store, 5i32);
        let wait = Function::new_typed_async(
            &mut store,
            &env,
            async |_: &mut AsyncEnv<i32>, _: i32| -> i32 { panic!("host future panicked") },
        );
        let imports = imports! { "h" => { "wait" => wait } };
        let instance = Instance::new(&mut store, &module, &imports).unwrap();
        let run = instance.exports.get_function("run").unwrap().clone();

        let mut call = Box::pin(run.call_async(&mut store, &[Value::I32(1)]));
        let caught = catch_unwind(AssertUnwindSafe(|| {
            let _ = call.as_mut().poll(&mut Context::from_waker(Waker::noop()));
        }));
        drop(call);
        assert!(caught.is_err(), "the panic reaches the poller");
        assert_eq!(*env.as_ref(&store), 5, "the store stays usable");
    });
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

/// Not ready once, having woken whoever polled it.
struct YieldOnce(bool);

impl Future for YieldOnce {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if std::mem::replace(&mut self.0, true) {
            return Poll::Ready(());
        }
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// An async host function called synchronously, dynamically or typed, runs
/// if it is ready.
#[test]
fn a_ready_async_host_function_runs_when_called_synchronously() {
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
    let results = function.call(&mut store, &[Value::I32(2)]).unwrap();
    assert_eq!(results[0], Value::I32(3));
    let typed = function.typed::<i32, i32>(&store).unwrap();
    assert_eq!(typed.call(&mut store, 4).unwrap(), 5);
    assert_eq!(*env.as_ref(&store), 6);
}

/// A function that returns `Deferred` is an async host function here: it
/// runs if it is ready, with its environment, and fails if it would wait.
#[test]
fn a_deferred_function_runs_when_it_is_ready() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0i32);
    let function = Function::new_typed_deferred(
        &mut store,
        &env,
        |mut env: FunctionEnvMut<i32>, value: i32| -> Deferred<i32> {
            *env.data_mut() += value;
            if value < 0 {
                let mut cx = env.async_env();
                return Deferred::pending(async move {
                    cx.with(|mut env| *env.data_mut() += 100);
                    YieldOnce(false).await;
                    value
                });
            }
            Deferred::ready(value + 1)
        },
    );
    let results = function.call(&mut store, &[Value::I32(2)]).unwrap();
    assert_eq!(results[0], Value::I32(3));
    let typed = function.typed::<i32, i32>(&store).unwrap();
    assert_eq!(typed.call(&mut store, 4).unwrap(), 5);
    assert_eq!(*env.as_ref(&store), 6);

    let error = function.call(&mut store, &[Value::I32(-1)]).unwrap_err();
    assert_eq!(error.to_trap(), Some(TrapCode::YieldOutsideAsyncContext));
    assert_eq!(*env.as_ref(&store), 105);
}

/// A synchronous entry made inside an async call does not inherit its right
/// to wait: its Rust caller holds a borrow of the store meanwhile.
#[test]
fn a_sync_entry_nested_in_an_async_call_cannot_wait() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    let pending = Function::new_typed_async(&mut store, &env, async |_: &mut AsyncEnv<()>| {
        YieldOnce(false).await;
    });
    assert_cannot_wait(&pending.call(&mut store, &[]).unwrap_err());
    let typed = pending.typed::<(), ()>(&store).unwrap();
    assert_cannot_wait(&typed.call(&mut store).unwrap_err());

    let outer = Function::new_typed_async(
        &mut store,
        &env,
        async move |cx: &mut AsyncEnv<()>| -> Result<(), RuntimeError> {
            cx.with(|mut env| pending.call(&mut env, &[]))?;
            Ok(())
        },
    );
    let error = futures::executor::block_on(outer.call_async(&mut store, &[])).unwrap_err();
    assert_cannot_wait(&error);
}

/// What a dynamic async host function returns is checked against its
/// signature before V8 is given it: values of another type, too few or too
/// many fail the call.
#[test]
fn the_results_of_a_dynamic_async_host_function_are_checked() {
    use wasmer::{FunctionType, Type};

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
            let message = error.to_string();
            assert!(message.contains("wrong signature"), "{message}");
        }
    }
}

/// An async host function is refused with another store than its own, at
/// the entry, as any function is.
#[test]
fn an_async_host_function_is_refused_with_another_store() {
    let mut first = Store::default();
    let mut second = Store::default();
    let env = FunctionEnv::new(&mut first, ());
    let function = Function::new_typed_async(&mut first, &env, async |_: &mut AsyncEnv<()>| {});
    assert!(!function.is_from_store(&second));
    let panic = catch_unwind(AssertUnwindSafe(|| {
        futures::executor::block_on(function.call_async(&mut second, &[]))
    }))
    .unwrap_err();
    let message = panic.downcast_ref::<String>().expect("a message");
    assert!(message.contains("wrong context"), "{message}");
}

/// A wake of an import's future that arrives while a nested call drives its
/// own import is kept for the outer driver: each driver has its own
/// notification, where a thread's one park token would go to the nested one.
#[test]
fn a_nested_driver_keeps_the_wake_of_the_outer_one() {
    const CHILD: &str = "WASMER_V8_NESTED_WAKE_PROBE";
    if std::env::var_os(CHILD).is_none() {
        // A lost notification blocks inside `poll`: the scenario runs in a
        // subprocess with a deadline, instead of hanging the test runner.
        let name = "a_nested_driver_keeps_the_wake_of_the_outer_one";
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture"])
            .env(CHILD, "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                let output = child.wait_with_output().unwrap();
                assert!(
                    status.success(),
                    "the nested wake probe failed: {}{}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
                return;
            }
            if std::time::Instant::now() >= deadline {
                child.kill().unwrap();
                let _ = child.wait();
                panic!("a nested driver lost the wake of the outer one");
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
                // Woken during its own poll, before the nested call waits.
                cx.waker().wake_by_ref();
                let mut nested = Box::pin(inner.call_async(env, &[]));
                match nested.as_mut().poll(cx) {
                    Poll::Ready(Ok(_)) => {}
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Pending => panic!("a call on V8 finishes in its first poll"),
                }
                Poll::Pending
            })
            .await
        },
    );
    futures::executor::block_on(outer.call_async(&mut store, &[])).unwrap();
}

/// The error of an async host function reaches its caller as it is, with no
/// need to format it on the way through V8.
#[test]
fn an_async_host_error_keeps_its_source() {
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
        wasmer::FunctionType::new(vec![], vec![]),
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

/// A panic crosses every host function between it and its outermost Rust
/// caller, synchronous ones included: each one relays it past V8's frames.
#[test]
fn a_host_panic_crosses_synchronous_host_functions() {
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
        wasmer::FunctionType::new(vec![], vec![]),
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

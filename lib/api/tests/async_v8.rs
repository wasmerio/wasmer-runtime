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
    AsyncEnv, Function, FunctionEnv, Instance, Module, RuntimeError, Store, TypedFunction, Value,
    imports,
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

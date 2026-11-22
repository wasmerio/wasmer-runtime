#![cfg(all(
    feature = "experimental-async",
    feature = "v8",
    any(feature = "v8-default", not(feature = "sys")),
    not(target_arch = "wasm32")
))]
//! Async calls on V8, which cannot switch stacks: `poll` runs the call, and
//! blocks while an import's future is driven by another thread.

use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll, Waker},
    thread,
    time::Duration,
};

use futures::channel::oneshot;
use wasmer::{
    AsyncEnv, Function, FunctionEnv, Instance, Module, RuntimeError, Store, TypedFunction, Value,
    imports,
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

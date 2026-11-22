//! The V8 C API drives pending host futures on the calling thread.
#![cfg(all(feature = "experimental-async", feature = "v8", feature = "wat"))]

use std::{
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    task::{Context, Poll},
};

use futures::executor::block_on;
use wasmer::{
    AsyncEnv, Function, FunctionEnv, Instance, Module, RuntimeError, Store, Value, imports,
};

fn store() -> Store {
    Store::new(wasmer::v8::V8::default())
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
    let mut store = store();
    let env = FunctionEnv::new(&mut store, 0i32);
    let function = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<i32>, x: i32| -> i32 {
            cx.with(|mut env| *env.data_mut() += x);
            x + 1
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
fn pending_imports_are_driven_on_the_store_thread() {
    let mut store = store();
    let owner = std::thread::current().id();
    let env = FunctionEnv::new(&mut store, 0u32);
    let function = Function::new_typed_async(
        &mut store,
        &env,
        async move |cx: &mut AsyncEnv<u32>| -> i32 {
            let (send, receive) = futures::channel::oneshot::channel();
            let worker = std::thread::spawn(move || {
                send.send(42).unwrap();
            });
            let value = receive.await.unwrap();
            worker.join().unwrap();
            assert_eq!(std::thread::current().id(), owner);
            cx.with(|mut env| *env.data_mut() += 1);
            value
        },
    );
    assert_eq!(
        block_on(function.call_async(&mut store, &[])).unwrap()[0],
        Value::I32(42)
    );
    assert_eq!(*env.as_ref(&store), 1);
}

#[test]
fn nested_async_calls_complete_in_lexical_order() {
    let mut store = store();
    let env = FunctionEnv::new(&mut store, Vec::<i32>::new());
    let inner = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<Vec<i32>>, x: i32| -> i32 {
            cx.with(|mut env| env.data_mut().push(2));
            YieldOnce(false).await;
            x * 2
        },
    );
    let outer = Function::new_typed_async(
        &mut store,
        &env,
        async move |cx: &mut AsyncEnv<Vec<i32>>, x: i32| -> Result<i32, RuntimeError> {
            cx.with(|mut env| env.data_mut().push(1));
            let args = [Value::I32(x)];
            let result = inner.call_async(cx, &args).await?;
            cx.with(|mut env| env.data_mut().push(3));
            Ok(result[0].unwrap_i32() + 1)
        },
    );
    let typed = outer.typed::<i32, i32>(&store).unwrap();
    let mut shared = store.into_shared();
    assert_eq!(block_on(typed.call_async(&mut shared, 5)).unwrap(), 11);
    assert_eq!(
        shared.try_with(|store| env.as_ref(&store).clone()).unwrap(),
        vec![1, 2, 3]
    );
}

#[test]
fn pending_sync_import_is_a_runtime_error() {
    let mut store = store();
    let env = FunctionEnv::new(&mut store, ());
    let function = Function::new_typed_async(&mut store, &env, async |_: &mut AsyncEnv<()>| {
        std::future::pending::<()>().await;
    });
    let error = function.call(&mut store, &[]).unwrap_err();
    assert!(
        error.to_string().contains("synchronous V8 guest entry"),
        "{error}"
    );
}

#[test]
fn start_functions_poll_ready_imports_and_refuse_waiting_imports() {
    let mut store = store();
    let env = FunctionEnv::new(&mut store, 0u32);
    let ready = Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<u32>| {
        cx.with(|mut env| *env.data_mut() += 1);
    });
    let pending = Function::new_typed_async(&mut store, &env, async |_: &mut AsyncEnv<u32>| {
        std::future::pending::<()>().await;
    });
    let module = Module::new(
        &store,
        r#"(module (import "h" "f" (func $f))
        (func $start (call $f)) (start $start))"#,
    )
    .unwrap();
    Instance::new(&mut store, &module, &imports! { "h" => { "f" => ready } }).unwrap();
    assert_eq!(*env.as_ref(&store), 1);
    let error =
        Instance::new(&mut store, &module, &imports! { "h" => { "f" => pending } }).unwrap_err();
    assert!(
        error.to_string().contains("synchronous V8 guest entry"),
        "{error}"
    );
}

#[test]
fn host_panics_resume_after_leaving_v8_frames() {
    let mut store = store();
    let env = FunctionEnv::new(&mut store, 0u32);
    let function =
        Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<u32>| -> () {
            cx.with(|mut env| *env.data_mut() += 1);
            panic!("V8 host future panic");
        });
    assert!(
        catch_unwind(AssertUnwindSafe(|| block_on(
            function.call_async(&mut store, &[])
        )))
        .is_err()
    );
    assert_eq!(*env.as_ref(&store), 1);
}

#[test]
fn an_async_import_rejects_a_different_store() {
    let mut first = store();
    let mut second = store();
    let env = FunctionEnv::new(&mut first, ());
    let function = Function::new_typed_async(&mut first, &env, async |_: &mut AsyncEnv<()>| {});
    let error = block_on(function.call_async(&mut second, &[])).unwrap_err();
    assert!(error.to_string().contains("active guest entry"), "{error}");
}

#![cfg(all(
    feature = "experimental-async",
    feature = "sys",
    not(feature = "v8-default"),
    not(target_arch = "wasm32")
))]
//! The contract of the async API: what borrows prove, and the runtime checks
//! that remain where they cannot.

use std::{
    cell::{Cell, RefCell},
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
};

use wasmer::{
    AsStoreMut, AsStoreRef, AsyncEnv, Deferred, ExternRef, Function, FunctionEnv, FunctionEnvMut,
    FunctionType, Instance, Module, RuntimeError, SharedStore, Store, StoreBusy, StoreMut,
    StoreObjects, StoreRef, Type, TypedFunction, Value, imports,
};
use wasmer_vm::TrapCode;

/// Pending until its flag is set.
struct Gate(Rc<Cell<bool>>);
impl Future for Gate {
    type Output = ();
    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<()> {
        if self.0.get() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
}

struct OnOneThread<T>(T);
unsafe impl<T> Send for OnOneThread<T> {}
unsafe impl<T> Sync for OnOneThread<T> {}
impl<T> OnOneThread<T> {
    fn get(&self) -> &T {
        &self.0
    }
}

fn poll<F: Future + ?Sized>(future: &mut Pin<Box<F>>) -> Poll<F::Output> {
    future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
}

const GUEST: &str = r#"(module
  (import "h" "wait" (func $wait (param i32) (result i32)))
  (global (export "after") (mut i32) (i32.const 0))
  (func (export "run") (param i32) (result i32)
    (local.get 0)
    (call $wait)
    (global.set 0 (i32.add (global.get 0) (i32.const 1)))
    (i32.add (i32.const 1))))"#;

#[derive(Default)]
struct Counter {
    value: i32,
    gate: Option<OnOneThread<Rc<Cell<bool>>>>,
}

/// A guest whose `wait(x)` import adds to the host state, waits for the
/// gate, and returns `x * 2`.
fn guest() -> (Store, Function, Rc<Cell<bool>>, FunctionEnv<Counter>) {
    let (store, instance, gate, env) = instantiate(GUEST);
    let run = instance.exports.get_function("run").unwrap().clone();
    (store, run, gate, env)
}

/// An instance of `wat`, with the `wait` import of [`guest`].
fn instantiate(wat: &str) -> (Store, Instance, Rc<Cell<bool>>, FunctionEnv<Counter>) {
    let mut store = Store::default();
    let module = Module::new(&store, wat).unwrap();
    let gate = Rc::new(Cell::new(false));
    let env = FunctionEnv::new(
        &mut store,
        Counter {
            value: 0,
            gate: Some(OnOneThread(gate.clone())),
        },
    );
    let wait = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<Counter>, x: i32| -> i32 {
            let gate = cx.with(|mut env| {
                env.data_mut().value += 1;
                env.data().gate.as_ref().unwrap().get().clone()
            });
            Gate(gate).await;
            cx.with(|mut env| env.data_mut().value += 10);
            x * 2
        },
    );
    let instance =
        Instance::new(&mut store, &module, &imports! { "h" => { "wait" => wait } }).unwrap();
    (store, instance, gate, env)
}

#[test]
fn a_ready_typed_import_works_on_a_sync_entry() {
    let (mut store, run, gate, env) = guest();
    gate.set(true);
    let results = run.call(&mut store, &[Value::I32(4)]).unwrap();
    assert_eq!(results[0].unwrap_i32(), 9);
    assert_eq!(env.as_ref(&store).value, 11);
}

#[test]
fn a_pending_import_traps_on_a_sync_entry() {
    let (mut store, run, _gate, env) = guest();
    let error = run.call(&mut store, &[Value::I32(4)]).unwrap_err();
    assert_eq!(error.to_trap(), Some(TrapCode::YieldOutsideAsyncContext));
    // The host function ran its first slice, and the store stays usable.
    assert_eq!(env.as_ref(&store).value, 1);
    let results = run.call(&mut store, &[Value::I32(4)]);
    assert!(results.is_err());
}

#[test]
fn a_call_borrows_the_store_and_suspends() {
    let (mut store, run, gate, env) = guest();
    let params = [Value::I32(4)];
    let mut call = Box::pin(run.call_async(&mut store, &params));
    assert!(poll(&mut call).is_pending());
    gate.set(true);
    let Poll::Ready(results) = poll(&mut call) else {
        panic!("the call did not finish");
    };
    assert_eq!(results.unwrap()[0].unwrap_i32(), 9);
    drop(call);
    assert_eq!(env.as_ref(&store).value, 11);
}

#[test]
fn a_typed_call_converts_its_arguments_and_results() {
    let (mut store, run, gate, _env) = guest();
    gate.set(true);
    let typed: TypedFunction<i32, i32> = run.typed(&store).unwrap();
    let mut call = Box::pin(typed.call_async(&mut store, 20));
    let Poll::Ready(result) = poll(&mut call) else {
        panic!("the call did not finish");
    };
    assert_eq!(result.unwrap(), 41);
}

/// A typed call takes and returns more values than its future keeps inline,
/// of every numeric type, whether it waits or not.
#[test]
fn a_typed_call_takes_and_returns_many_values() {
    const MANY: &str = r#"(module
      (import "h" "wait" (func $wait (param i32) (result i32)))
      (func (export "rotate") (param i32 i64 f32 f64 i32) (result i64 f32 f64 i32 i32)
        (local.get 1) (local.get 2) (local.get 3) (local.get 4)
        (call $wait (local.get 0)))
      (func (export "spread") (param i32) (result i32 i32 i32 i32 i32)
        (local.get 0) (i32.const 2) (i32.const 3) (i32.const 4)
        (call $wait (local.get 0))))"#;
    type Rotated = (i64, f32, f64, i32, i32);
    let (mut store, instance, gate, _env) = instantiate(MANY);
    let exports = &instance.exports;
    let rotate: TypedFunction<(i32, i64, f32, f64, i32), Rotated> =
        exports.get_typed_function(&store, "rotate").unwrap();
    let spread: TypedFunction<i32, (i32, i32, i32, i32, i32)> =
        exports.get_typed_function(&store, "spread").unwrap();

    // It waits.
    let mut call = Box::pin(rotate.call_async(&mut store, 7, 1 << 40, 1.5, -2.5, 9));
    assert!(poll(&mut call).is_pending());
    gate.set(true);
    let Poll::Ready(result) = poll(&mut call) else {
        panic!("the call did not finish");
    };
    assert_eq!(result.unwrap(), (1 << 40, 1.5, -2.5, 9, 14));
    drop(call);

    // More results than arguments, ready at once.
    let mut call = Box::pin(spread.call_async(&mut store, 5));
    let Poll::Ready(result) = poll(&mut call) else {
        panic!("the call did not finish");
    };
    assert_eq!(result.unwrap(), (5, 2, 3, 4, 10));
}

/// A typed call refuses an argument of another store, as a synchronous one
/// does, and its store stays usable.
#[test]
fn a_typed_call_refuses_a_value_of_another_store() {
    let mut store = Store::default();
    let wat = r#"(module (func (export "take") (param externref)))"#;
    let module = Module::new(&store, wat).unwrap();
    let instance = Instance::new(&mut store, &module, &imports! {}).unwrap();
    let take: TypedFunction<Option<ExternRef>, ()> =
        instance.exports.get_typed_function(&store, "take").unwrap();
    let mut other = Store::default();
    let foreign = ExternRef::new(&mut other, 1u8);
    let mut call = Box::pin(take.call_async(&mut store, Some(foreign)));
    let Poll::Ready(Err(error)) = poll(&mut call) else {
        panic!("the call took a value of another store");
    };
    assert_eq!(error.message(), "cross-`Store` values are not supported");
    drop(call);

    let own = ExternRef::new(&mut store, 1u8);
    let mut call = Box::pin(take.call_async(&mut store, Some(own)));
    assert!(matches!(poll(&mut call), Poll::Ready(Ok(()))));
}

/// A typed import takes its arguments and gives its results as the
/// trampoline has them, whatever their types and their number, whether it is
/// ready at once or waits.
#[test]
fn a_typed_import_converts_its_arguments_and_results() {
    let mut store = Store::default();
    let gate = Rc::new(Cell::new(true));
    let counter = Counter {
        value: 0,
        gate: Some(OnOneThread(gate.clone())),
    };
    let env = FunctionEnv::new(&mut store, counter);
    let double = Function::new_typed(&mut store, |x: i32| x * 2);
    let import = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<Counter>,
               a: i32,
               b: i64,
               c: f32,
               d: f64,
               f: Option<Function>|
               -> (f64, f32, i64, i32, Option<Function>) {
            let gate = cx.with(|env| env.data().gate.as_ref().unwrap().get().clone());
            Gate(gate).await;
            (d, c, b, a, f)
        },
    );
    let params = [
        Value::I32(1),
        Value::I64(-2),
        Value::F32(3.5),
        Value::F64(-4.5),
        Value::FuncRef(Some(double)),
    ];
    let check = |store: &mut Store, results: &[Value]| {
        assert_eq!(results[0].unwrap_f64(), -4.5);
        assert_eq!(results[1].unwrap_f32(), 3.5);
        assert_eq!(results[2].unwrap_i64(), -2);
        assert_eq!(results[3].unwrap_i32(), 1);
        let double = results[4].unwrap_funcref().as_ref().unwrap();
        let doubled = double.call(store, &[Value::I32(21)]).unwrap();
        assert_eq!(doubled[0].unwrap_i32(), 42);
    };

    let results = import.call(&mut store, &params).unwrap();
    check(&mut store, &results);

    gate.set(false);
    let mut call = Box::pin(import.call_async(&mut store, &params));
    assert!(poll(&mut call).is_pending());
    gate.set(true);
    let Poll::Ready(results) = poll(&mut call) else {
        panic!("the call did not finish");
    };
    drop(call);
    check(&mut store, &results.unwrap());
}

/// A typed async host function is a static function, as a typed sync one
/// is: a guest can call it through a table, and wait in it.
#[test]
fn a_guest_waits_in_a_typed_import_it_calls_through_a_table() {
    const GUEST: &str = r#"(module
      (type $wait (func (param i32) (result i32)))
      (table (export "table") 1 funcref)
      (func (export "run") (param i32) (result i32)
        (call_indirect (type $wait) (local.get 0) (i32.const 0))))"#;
    let mut store = Store::default();
    let module = Module::new(&store, GUEST).unwrap();
    let gate = Rc::new(Cell::new(false));
    let counter = Counter {
        value: 0,
        gate: Some(OnOneThread(gate.clone())),
    };
    let env = FunctionEnv::new(&mut store, counter);
    let wait = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<Counter>, x: i32| -> i32 {
            let gate = cx.with(|env| env.data().gate.as_ref().unwrap().get().clone());
            Gate(gate).await;
            x * 2
        },
    );
    let instance = Instance::new(&mut store, &module, &imports! {}).unwrap();
    let table = instance.exports.get_table("table").unwrap();
    table
        .set(&mut store, 0, Value::FuncRef(Some(wait)))
        .unwrap();
    let run = instance.exports.get_function("run").unwrap().clone();

    let params = [Value::I32(4)];
    let mut call = Box::pin(run.call_async(&mut store, &params));
    assert!(poll(&mut call).is_pending());
    gate.set(true);
    let Poll::Ready(results) = poll(&mut call) else {
        panic!("the call did not finish");
    };
    assert_eq!(results.unwrap()[0].unwrap_i32(), 8);
}

/// A function that returns `Deferred::ready` completes its call at once,
/// wherever a sync host function would. It is made as an `async` host
/// function is, and reaches the store the same way.
#[test]
fn a_deferred_function_that_is_ready_never_waits() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, Counter::default());
    let double = Function::new_typed_async(
        &mut store,
        &env,
        |cx: &mut AsyncEnv<Counter>, x: i32| -> Deferred<Result<i32, RuntimeError>> {
            cx.with(|mut env| env.data_mut().value += 1);
            Deferred::ready(if x < 0 {
                Err(RuntimeError::new("negative"))
            } else {
                Ok(x * 2)
            })
        },
    );

    let params = [Value::I32(4)];
    assert_eq!(double.call(&mut store, &params).unwrap()[0].unwrap_i32(), 8);
    let mut call = Box::pin(double.call_async(&mut store, &params));
    let Poll::Ready(results) = poll(&mut call) else {
        panic!("the call waited");
    };
    assert_eq!(results.unwrap()[0].unwrap_i32(), 8);
    drop(call);

    let error = double.call(&mut store, &[Value::I32(-1)]).unwrap_err();
    assert_eq!(error.message(), "negative");
    assert_eq!(env.as_ref(&store).value, 3);
}

/// A guest waits for the future of a function that returns
/// `Deferred::pending`. The future reaches the store through a clone of the
/// `AsyncEnv` the function was called with.
#[test]
fn a_guest_waits_for_a_deferred_function() {
    let mut store = Store::default();
    let module = Module::new(&store, GUEST).unwrap();
    let gate = Rc::new(Cell::new(false));
    let counter = Counter {
        value: 0,
        gate: Some(OnOneThread(gate.clone())),
    };
    let env = FunctionEnv::new(&mut store, counter);
    let wait = Function::new_typed_async(
        &mut store,
        &env,
        |cx: &mut AsyncEnv<Counter>, x: i32| -> Deferred<i32> {
            let gate = cx.with(|mut env| {
                env.data_mut().value += 1;
                env.data().gate.as_ref().unwrap().get().clone()
            });
            if gate.get() {
                return Deferred::ready(x * 2);
            }
            let mut cx = cx.clone();
            Deferred::pending(async move {
                Gate(gate).await;
                cx.with(|mut env| env.data_mut().value += 10);
                x * 2
            })
        },
    );
    let instance =
        Instance::new(&mut store, &module, &imports! { "h" => { "wait" => wait } }).unwrap();
    let run = instance.exports.get_function("run").unwrap().clone();
    let params = [Value::I32(4)];

    // A guest entered synchronously cannot wait.
    let error = run.call(&mut store, &params).unwrap_err();
    assert_eq!(error.to_trap(), Some(TrapCode::YieldOutsideAsyncContext));
    assert_eq!(env.as_ref(&store).value, 1);

    let mut call = Box::pin(run.call_async(&mut store, &params));
    assert!(poll(&mut call).is_pending());
    gate.set(true);
    let Poll::Ready(results) = poll(&mut call) else {
        panic!("the call did not finish");
    };
    assert_eq!(results.unwrap()[0].unwrap_i32(), 9);
    drop(call);
    assert_eq!(env.as_ref(&store).value, 12);

    // The same function is ready once the gate is open, on a sync entry too.
    let results = run.call(&mut store, &params).unwrap();
    assert_eq!(results[0].unwrap_i32(), 9);
    assert_eq!(env.as_ref(&store).value, 13);
}

/// A function that returns `Deferred` holds no store while it runs: it
/// takes one slice after another, and a guest it calls in one of them runs.
#[test]
fn a_deferred_function_takes_the_store_in_slices() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, Counter::default());
    let double = Function::new_typed(&mut store, |x: i32| x * 2);
    let import = Function::new_typed_async(
        &mut store,
        &env,
        move |cx: &mut AsyncEnv<Counter>, x: i32| -> Deferred<i32> {
            cx.with(|mut env| env.data_mut().value += 1);
            let doubled =
                cx.with(|mut env| double.call(&mut env, &[Value::I32(x)]).unwrap()[0].unwrap_i32());
            cx.with(|mut env| env.data_mut().value += doubled);
            Deferred::ready(doubled)
        },
    );
    let results = import.call(&mut store, &[Value::I32(21)]).unwrap();
    assert_eq!(results[0].unwrap_i32(), 42);
    assert_eq!(env.as_ref(&store).value, 43);
}

/// A function that returns `Deferred` takes its arguments and gives its
/// results whatever their types and their number, at once or after waiting.
#[test]
fn a_deferred_function_converts_its_arguments_and_results() {
    type Results = (f64, f32, i64, i32, Option<Function>);

    let mut store = Store::default();
    let gate = Rc::new(Cell::new(true));
    let counter = Counter {
        value: 0,
        gate: Some(OnOneThread(gate.clone())),
    };
    let env = FunctionEnv::new(&mut store, counter);
    let double = Function::new_typed(&mut store, |x: i32| x * 2);
    let import = Function::new_typed_async(
        &mut store,
        &env,
        |cx: &mut AsyncEnv<Counter>,
         a: i32,
         b: i64,
         c: f32,
         d: f64,
         f: Option<Function>|
         -> Deferred<Results> {
            let gate = cx.with(|env| env.data().gate.as_ref().unwrap().get().clone());
            if gate.get() {
                return Deferred::ready((d, c, b, a, f));
            }
            Deferred::pending(async move {
                Gate(gate).await;
                (d, c, b, a, f)
            })
        },
    );
    let params = [
        Value::I32(1),
        Value::I64(-2),
        Value::F32(3.5),
        Value::F64(-4.5),
        Value::FuncRef(Some(double)),
    ];
    let check = |store: &mut Store, results: &[Value]| {
        assert_eq!(results[0].unwrap_f64(), -4.5);
        assert_eq!(results[1].unwrap_f32(), 3.5);
        assert_eq!(results[2].unwrap_i64(), -2);
        assert_eq!(results[3].unwrap_i32(), 1);
        let double = results[4].unwrap_funcref().as_ref().unwrap();
        let doubled = double.call(store, &[Value::I32(21)]).unwrap();
        assert_eq!(doubled[0].unwrap_i32(), 42);
    };

    let results = import.call(&mut store, &params).unwrap();
    check(&mut store, &results);

    gate.set(false);
    let mut call = Box::pin(import.call_async(&mut store, &params));
    assert!(poll(&mut call).is_pending());
    gate.set(true);
    let Poll::Ready(results) = poll(&mut call) else {
        panic!("the call did not finish");
    };
    drop(call);
    check(&mut store, &results.unwrap());
}

/// A dynamic function returns `Deferred` as a typed one does: it is ready at
/// once, or its guest waits.
#[test]
fn a_dynamic_function_returns_deferred() {
    let mut store = Store::default();
    let gate = Rc::new(Cell::new(true));
    let counter = Counter {
        value: 0,
        gate: Some(OnOneThread(gate.clone())),
    };
    let env = FunctionEnv::new(&mut store, counter);
    let double = Function::new_async(
        &mut store,
        &env,
        FunctionType::new([Type::I32], [Type::I32]),
        |cx: &mut AsyncEnv<Counter>,
         values: &[Value]|
         -> Deferred<Result<Vec<Value>, RuntimeError>> {
            let x = values[0].unwrap_i32();
            let gate = cx.with(|mut env| {
                env.data_mut().value += 1;
                env.data().gate.as_ref().unwrap().get().clone()
            });
            if x < 0 {
                return Deferred::ready(Err(RuntimeError::new("negative")));
            }
            if gate.get() {
                return Deferred::ready(Ok(vec![Value::I32(x * 2)]));
            }
            let mut cx = cx.clone();
            Deferred::pending(async move {
                Gate(gate).await;
                cx.with(|mut env| env.data_mut().value += 10);
                Ok(vec![Value::I32(x * 2)])
            })
        },
    );
    let params = [Value::I32(4)];
    assert_eq!(double.call(&mut store, &params).unwrap()[0].unwrap_i32(), 8);
    let error = double.call(&mut store, &[Value::I32(-1)]).unwrap_err();
    assert_eq!(error.message(), "negative");

    gate.set(false);
    let error = double.call(&mut store, &params).unwrap_err();
    assert_eq!(error.to_trap(), Some(TrapCode::YieldOutsideAsyncContext));
    let mut call = Box::pin(double.call_async(&mut store, &params));
    assert!(poll(&mut call).is_pending());
    gate.set(true);
    let Poll::Ready(results) = poll(&mut call) else {
        panic!("the call did not finish");
    };
    assert_eq!(results.unwrap()[0].unwrap_i32(), 8);
    drop(call);
    assert_eq!(env.as_ref(&store).value, 14);
}

/// An async host function waits for a guest through its own `AsyncEnv`. The
/// inner guest suspends, and both calls resume.
#[test]
fn a_nested_call_suspends_and_resumes() {
    struct Env {
        inner: Option<Function>,
    }
    let (mut store, inner, gate, _) = guest();
    let env = FunctionEnv::new(&mut store, Env { inner: Some(inner) });
    let outer = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<Env>, x: i32| -> Result<i32, RuntimeError> {
            let inner = cx.with(|env| env.data().inner.clone().unwrap());
            let results = inner.call_async(cx, &[Value::I32(x)]).await?;
            Ok(results[0].unwrap_i32() + 100)
        },
    );
    let mut call = Box::pin(outer.call_async(&mut store, &[Value::I32(1)]));
    assert!(poll(&mut call).is_pending());
    assert!(poll(&mut call).is_pending());
    gate.set(true);
    let Poll::Ready(results) = poll(&mut call) else {
        panic!("the call did not finish");
    };
    assert_eq!(results.unwrap()[0].unwrap_i32(), 103);
}

/// A typed call made through an `AsyncEnv` reads its arguments in a slice of
/// the host function that makes it, and suspends and resumes like any other.
#[test]
fn a_nested_typed_call_suspends_and_resumes() {
    struct Env {
        inner: Option<TypedFunction<i32, i32>>,
    }
    let (mut store, inner, gate, _) = guest();
    let inner = inner.typed(&store).unwrap();
    let env = FunctionEnv::new(&mut store, Env { inner: Some(inner) });
    let outer = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<Env>, x: i32| -> Result<i32, RuntimeError> {
            let inner = cx.with(|env| env.data().inner.clone().unwrap());
            Ok(inner.call_async(cx, x).await? + 100)
        },
    );
    let outer: TypedFunction<i32, i32> = outer.typed(&store).unwrap();
    let mut call = Box::pin(outer.call_async(&mut store, 1));
    assert!(poll(&mut call).is_pending());
    assert!(poll(&mut call).is_pending());
    gate.set(true);
    let Poll::Ready(result) = poll(&mut call) else {
        panic!("the call did not finish");
    };
    assert_eq!(result.unwrap(), 103);
}

#[test]
fn dropping_a_call_leaves_the_store_usable() {
    // Before it started.
    let (mut store, run, gate, env) = guest();
    drop(run.call_async(&mut store, &[Value::I32(1)]));
    assert_eq!(env.as_ref(&store).value, 0);

    // During a wait. No guest or host code runs afterwards.
    let mut call = Box::pin(run.call_async(&mut store, &[Value::I32(1)]));
    assert!(poll(&mut call).is_pending());
    drop(call);
    assert_eq!(env.as_ref(&store).value, 1);

    // After the wake, before the guest went on.
    let mut call = Box::pin(run.call_async(&mut store, &[Value::I32(1)]));
    assert!(poll(&mut call).is_pending());
    gate.set(true);
    drop(call);
    assert_eq!(env.as_ref(&store).value, 2);

    let results = run.call(&mut store, &[Value::I32(1)]).unwrap();
    assert_eq!(results[0].unwrap_i32(), 3);
}

/// A call dropped while its guest waits resets the guest's stack, which runs
/// no destructor: the trampoline of the import the guest waits in must hold
/// nothing by then. Whichever kind of async import it is, the future of the
/// import is dropped with the call, with what it captured, and the store has
/// no borrow left out, so that it runs the guest again.
#[test]
fn dropping_a_call_that_waits_releases_what_its_import_held() {
    fn gate_of(cx: &mut AsyncEnv<Counter>) -> Rc<Cell<bool>> {
        cx.with(|mut env| {
            env.data_mut().value += 1;
            env.data().gate.as_ref().unwrap().get().clone()
        })
    }
    type Import = fn(&mut Store, &FunctionEnv<Counter>) -> Function;
    let typed: Import = |store, env| {
        Function::new_typed_async(store, env, async |cx: &mut AsyncEnv<Counter>, x: i32| {
            Gate(gate_of(cx)).await;
            x * 2
        })
    };
    let deferred: Import = |store, env| {
        Function::new_typed_async(
            store,
            env,
            |cx: &mut AsyncEnv<Counter>, x: i32| -> Deferred<i32> {
                let gate = gate_of(cx);
                Deferred::pending(async move {
                    Gate(gate).await;
                    x * 2
                })
            },
        )
    };
    let dynamic: Import = |store, env| {
        Function::new_async(
            store,
            env,
            FunctionType::new([Type::I32], [Type::I32]),
            async |cx: &mut AsyncEnv<Counter>, values: &[Value]| {
                let x = values[0].unwrap_i32();
                Gate(gate_of(cx)).await;
                Ok(vec![Value::I32(x * 2)])
            },
        )
    };

    for (kind, import) in [
        ("typed", typed),
        ("deferred", deferred),
        ("dynamic", dynamic),
    ] {
        let mut store = Store::default();
        let module = Module::new(&store, GUEST).unwrap();
        let gate = Rc::new(Cell::new(false));
        let counter = Counter {
            value: 0,
            gate: Some(OnOneThread(gate.clone())),
        };
        let env = FunctionEnv::new(&mut store, counter);
        let wait = import(&mut store, &env);
        let instance =
            Instance::new(&mut store, &module, &imports! { "h" => { "wait" => wait } }).unwrap();
        let run = instance.exports.get_function("run").unwrap().clone();

        // This one and the host state's.
        assert_eq!(Rc::strong_count(&gate), 2, "{kind}");
        let mut call = Box::pin(run.call_async(&mut store, &[Value::I32(1)]));
        assert!(poll(&mut call).is_pending(), "{kind}");
        // The future of the import waits with its own.
        assert_eq!(Rc::strong_count(&gate), 3, "{kind}");
        drop(call);
        assert_eq!(Rc::strong_count(&gate), 2, "{kind}");
        assert_eq!(env.as_ref(&store).value, 1, "{kind}");

        gate.set(true);
        let results = run.call(&mut store, &[Value::I32(1)]).unwrap();
        assert_eq!(results[0].unwrap_i32(), 3, "{kind}");
        assert_eq!(Rc::strong_count(&gate), 2, "{kind}");
    }
}

/// The stack is leaked, and nothing refers to it: its state left the thread
/// when it suspended.
#[test]
fn forgetting_a_call_leaves_the_store_usable() {
    let (mut store, run, gate, env) = guest();
    let mut call = Box::pin(run.call_async(&mut store, &[Value::I32(1)]));
    assert!(poll(&mut call).is_pending());
    std::mem::forget(call);
    assert_eq!(env.as_ref(&store).value, 1);
    gate.set(true);
    let results = run.call(&mut store, &[Value::I32(2)]).unwrap();
    assert_eq!(results[0].unwrap_i32(), 5);
    drop(store);
}

/// `AsStoreMut` is a public trait, so a second `as_store_mut` could return
/// another store. A call asks once.
#[test]
fn a_call_derives_its_store_once() {
    struct Flip {
        store: Store,
        asked: u32,
    }
    impl AsStoreRef for Flip {
        fn as_store_ref(&self) -> StoreRef<'_> {
            self.store.as_store_ref()
        }
    }
    impl AsStoreMut for Flip {
        fn as_store_mut(&mut self) -> StoreMut<'_> {
            self.asked += 1;
            if self.asked > 1 {
                self.store = Store::default();
            }
            self.store.as_store_mut()
        }
        fn objects_mut(&mut self) -> &mut StoreObjects {
            self.store.objects_mut()
        }
    }

    let (store, run, gate, env) = guest();
    gate.set(true);
    let mut flip = Flip { store, asked: 0 };
    let mut call = Box::pin(run.call_async(&mut flip, &[Value::I32(1)]));
    assert!(matches!(poll(&mut call), Poll::Ready(Ok(_))));
    drop(call);
    assert_eq!(flip.asked, 1);
    assert_eq!(env.as_ref(&flip.store).value, 11);
}

#[test]
fn a_shared_store_runs_overlapping_tasks_out_of_order() {
    let (store, run, gate, env) = guest();
    let mut shared = store.into_shared();
    let mut first = Box::pin(run.call_async(&mut shared, &[Value::I32(1)]));
    let mut second = Box::pin(run.call_async(&mut shared, &[Value::I32(2)]));
    assert!(poll(&mut first).is_pending());
    assert!(poll(&mut second).is_pending());
    assert!(shared.try_with(|_| ()).is_ok(), "no task holds the store");

    gate.set(true);
    let Poll::Ready(second) = poll(&mut second) else {
        panic!("the second task did not finish");
    };
    assert_eq!(second.unwrap()[0].unwrap_i32(), 5);
    let Poll::Ready(first) = poll(&mut first) else {
        panic!("the first task did not finish");
    };
    assert_eq!(first.unwrap()[0].unwrap_i32(), 3);
    let value = shared.try_with(|store| env.as_ref(&store).value).unwrap();
    assert_eq!(value, 22);
}

#[test]
fn a_shared_store_comes_back_once_its_tasks_are_gone() {
    let (store, run, _gate, _env) = guest();
    let mut shared = store.into_shared();
    let weak = shared.downgrade();
    let mut task = Box::pin(run.call_async(&mut shared, &[Value::I32(1)]));
    assert!(poll(&mut task).is_pending());
    let shared = shared.into_store().expect_err("a suspended task holds it");
    drop(task);
    let store = shared.into_store().expect("no task holds it");
    assert!(weak.upgrade().is_none());
    drop(store);
}

/// The back-reference does not keep the store alive.
#[test]
fn a_host_function_finds_the_shared_handle_of_its_store() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    assert!(env.clone().into_mut(&mut store).shared().is_none());
    let shared = store.into_shared();
    let found = shared
        .try_with(|mut store| env.clone().into_mut(&mut store).shared().is_some())
        .unwrap();
    assert!(found);
    assert!(shared.into_store().is_ok());
}

/// Two handles are compared without borrowing the store, so inside a slice
/// too, where `try_with` on either would fail.
#[test]
fn handles_of_a_shared_store_are_compared_without_borrowing_it() {
    let shared = Store::default().into_shared();
    let other = Store::default().into_shared();
    let clone = shared.clone();
    assert!(shared.ptr_eq(&clone));
    assert!(!shared.ptr_eq(&other));
    assert!(shared.downgrade().ptr_eq(&clone.downgrade()));
    assert!(!shared.downgrade().ptr_eq(&other.downgrade()));

    shared
        .try_with(|_store| {
            assert!(clone.try_with(|_| ()).is_err());
            assert!(shared.ptr_eq(&clone));
            assert!(!shared.ptr_eq(&other));
        })
        .unwrap();

    // A weak handle still answers once its store is gone.
    let weak = shared.downgrade();
    drop((shared, clone));
    assert!(weak.upgrade().is_none());
    assert!(weak.ptr_eq(&weak.clone()));
    assert!(!weak.ptr_eq(&other.downgrade()));
}

/// `release` lends the store to a `SharedStore` handle while a host function
/// runs, one holder at a time. Without it the store is busy.
#[test]
fn release_lends_the_store_one_slice_at_a_time() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    let seen: Rc<RefCell<Vec<&str>>> = Rc::default();
    let host = Function::new_typed_with_env(&mut store, &env, {
        let seen = OnOneThread(seen.clone());
        move |mut env: FunctionEnvMut<()>| {
            let shared = env.shared().expect("a shared store");
            let mut log = seen.get().borrow_mut();
            log.push(match shared.try_with(|_| ()) {
                Ok(()) => "free",
                Err(_) => "busy",
            });
            env.release(|| {
                let inner = shared.try_with(|_| {
                    log.push("released");
                    shared.try_with(|_| ()).is_err()
                });
                assert_eq!(inner, Ok(true), "the store is lent to one holder");
            });
        }
    });
    let shared = store.into_shared();
    let mut handle = shared.clone();
    let mut call = Box::pin(host.call_async(&mut handle, &[]));
    assert!(matches!(poll(&mut call), Poll::Ready(Ok(_))));
    assert_eq!(*seen.borrow(), ["busy", "released"]);
}

/// A waker that polls inline must defer, because the holder is below it.
#[test]
fn polling_a_task_inside_a_slice_of_its_store_panics() {
    let (store, run, _gate, _env) = guest();
    let mut shared = store.into_shared();
    let mut task = Box::pin(run.call_async(&mut shared, &[Value::I32(1)]));
    let refused = shared
        .try_with(|_| catch_unwind(AssertUnwindSafe(|| poll(&mut task))).is_err())
        .unwrap();
    assert!(refused);
    assert!(poll(&mut task).is_pending(), "the task is not harmed");
}

/// A host future's destructor runs when its task is dropped. If that happens
/// inside another slice of the shared store, `with` fails safely instead of
/// aliasing the store.
#[test]
fn with_panics_in_a_destructor_run_inside_another_slice() {
    thread_local! {
        static PANICKED: Cell<Option<bool>> = const { Cell::new(None) };
    }
    struct UsesStore<'a>(&'a mut AsyncEnv<()>);
    impl Drop for UsesStore<'_> {
        fn drop(&mut self) {
            let panicked = catch_unwind(AssertUnwindSafe(|| self.0.with(|_| ()))).is_err();
            PANICKED.set(Some(panicked));
        }
    }

    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    let host = Function::new_async(
        &mut store,
        &env,
        FunctionType::new(vec![], vec![]),
        async |cx: &mut AsyncEnv<()>, _: &[Value]| {
            let _uses = UsesStore(cx);
            Gate(Rc::new(Cell::new(false))).await;
            Ok(vec![])
        },
    );
    let mut shared = store.into_shared();
    let mut task = Box::pin(host.call_async(&mut shared, &[]));
    assert!(poll(&mut task).is_pending());
    shared.try_with(|_| drop(task)).unwrap();
    assert_eq!(PANICKED.get(), Some(true));
    assert!(shared.try_with(|_| ()).is_ok(), "the store is free again");
}

type Task = Pin<Box<dyn Future<Output = Result<Box<[Value]>, RuntimeError>>>>;

thread_local! {
    /// What each destructor of a [`TriesStore`] got from `try_with`.
    static TRIED: RefCell<Vec<Result<i32, StoreBusy>>> = const { RefCell::new(Vec::new()) };
    /// A task for the `drop_task` import to drop.
    static TO_DROP: RefCell<Option<Task>> = const { RefCell::new(None) };
}

/// What a host future holds while it waits. Its destructor asks for the store
/// with `try_with`, adds 100 to the host state if it gets it, and notes the
/// outcome.
struct TriesStore<'a>(&'a mut AsyncEnv<i32>);
impl Drop for TriesStore<'_> {
    fn drop(&mut self) {
        let tried = self.0.try_with(|mut env| {
            *env.data_mut() += 100;
            *env.data()
        });
        // Where `try_with` has the store, `with` has it too.
        if let Ok(value) = tried {
            assert_eq!(self.0.with(|env| *env.data()), value);
        }
        TRIED.with_borrow_mut(|tried_so_far| tried_so_far.push(tried));
    }
}

/// A guest on a shared store, whose exports are each one import:
///
/// - `waits` waits for good, in a host future that holds a [`TriesStore`];
/// - `relays` waits for `waits`, which it calls with its `AsyncEnv`;
/// - `drops` drops the task in `TO_DROP`, in a synchronous host function;
/// - `drops_in_slice` drops it in a `with` slice of an async one.
fn dropping_guest() -> (SharedStore, FunctionEnv<i32>, Instance) {
    TRIED.take();
    let mut store = Store::default();
    let module = Module::new(
        &store,
        r#"(module
          (import "h" "wait" (func $wait))
          (import "h" "relay" (func $relay))
          (import "h" "drop" (func $drop))
          (import "h" "drop_in_slice" (func $drop_in_slice))
          (func (export "waits") (call $wait))
          (func (export "relays") (call $relay))
          (func (export "drops") (call $drop))
          (func (export "drops_in_slice") (call $drop_in_slice)))"#,
    )
    .unwrap();
    let env = FunctionEnv::new(&mut store, 0);
    let wait = Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<i32>| {
        let _tries = TriesStore(cx);
        std::future::pending::<()>().await;
    });
    let relay = Function::new_typed_async(&mut store, &env, {
        let wait = wait.clone();
        async move |cx: &mut AsyncEnv<i32>| {
            let _ = wait.call_async(cx, &[]).await;
        }
    });
    let drop_task = Function::new_typed(&mut store, || drop(TO_DROP.take()));
    let drop_in_slice =
        Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<i32>| {
            cx.with(|_| drop(TO_DROP.take()));
        });
    let imports = imports! { "h" => {
        "wait" => wait,
        "relay" => relay,
        "drop" => drop_task,
        "drop_in_slice" => drop_in_slice,
    } };
    let instance = Instance::new(&mut store, &module, &imports).unwrap();
    (store.into_shared(), env, instance)
}

fn export(instance: &Instance, name: &str) -> Function {
    instance.exports.get_function(name).unwrap().clone()
}

/// A task of `shared` whose guest waits in `waits`.
fn waiting_task(shared: &SharedStore, waits: &Function) -> Task {
    let mut task: Task = Box::pin(waits.call_async(&mut shared.clone(), &[]));
    assert!(poll(&mut task).is_pending());
    task
}

/// The destructor of `with_panics_in_a_destructor_run_inside_another_slice`,
/// asking with `try_with`: the store is busy, whoever holds it and however
/// deep the host future is, and nothing panics.
#[test]
fn try_with_fails_in_a_destructor_run_inside_another_slice() {
    let (shared, env, instance) = dropping_guest();
    let drops = export(&instance, "drops");
    for waits in ["waits", "relays"].map(|name| export(&instance, name)) {
        // Dropped by the holder of a slice.
        let task = waiting_task(&shared, &waits);
        shared.try_with(|_| drop(task)).unwrap();
        assert_eq!(TRIED.take(), [Err(StoreBusy)]);

        // Dropped by an import of a guest that a synchronous call runs.
        TO_DROP.set(Some(waiting_task(&shared, &waits)));
        let dropped = shared.try_with(|mut store| drops.call(&mut store, &[]));
        assert!(dropped.unwrap().is_ok());
        assert_eq!(TRIED.take(), [Err(StoreBusy)]);

        // Dropped by an import of another task: by a synchronous host
        // function, and in a slice of an async one.
        for drops in ["drops", "drops_in_slice"].map(|name| export(&instance, name)) {
            TO_DROP.set(Some(waiting_task(&shared, &waits)));
            let mut dropper: Task = Box::pin(drops.call_async(&mut shared.clone(), &[]));
            assert!(matches!(poll(&mut dropper), Poll::Ready(Ok(_))));
            assert_eq!(TRIED.take(), [Err(StoreBusy)]);
        }
    }

    // No destructor reached the store, which is free again.
    let value = shared.try_with(|mut store| *env.as_mut(&mut store));
    assert_eq!(value, Ok(0));
}

/// A call drops its host future with the store in reach, so a destructor
/// gets the store from `try_with` wherever it gets it from `with`: when a
/// task is dropped outside every slice, when a synchronous entry drops a
/// future that cannot wait, and when a call that borrows the store is
/// dropped.
#[test]
fn try_with_takes_the_store_in_a_destructor_run_with_the_store_in_reach() {
    let (shared, env, instance) = dropping_guest();
    let (waits, relays) = (export(&instance, "waits"), export(&instance, "relays"));

    drop(waiting_task(&shared, &waits));
    assert_eq!(TRIED.take(), [Ok(100)]);
    drop(waiting_task(&shared, &relays));
    assert_eq!(TRIED.take(), [Ok(200)]);

    let waited = shared.try_with(|mut store| waits.call(&mut store, &[]));
    assert!(waited.unwrap().is_err(), "the import cannot wait");
    assert_eq!(TRIED.take(), [Ok(300)]);

    let mut store = shared.into_store().expect("the last handle");
    let mut call = Box::pin(waits.call_async(&mut store, &[]));
    assert!(poll(&mut call).is_pending());
    drop(call);
    assert_eq!(TRIED.take(), [Ok(400)]);
    assert_eq!(*env.as_ref(&store), 400);
}

/// Inside a poll of its own call, `try_with` is `with`.
#[test]
fn try_with_takes_the_store_in_a_poll_as_with_does() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0);
    let add = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<i32>, x: i32| -> i32 {
            let tried = cx.try_with(|mut env| {
                *env.data_mut() += x;
                *env.data()
            });
            let with = cx.with(|env| *env.data());
            assert_eq!(tried, Ok(with));
            with
        },
    );
    let results = add.call(&mut store, &[Value::I32(1)]).unwrap();
    assert_eq!(results[0].unwrap_i32(), 1);

    let mut call = Box::pin(add.call_async(&mut store, &[Value::I32(2)]));
    let Poll::Ready(results) = poll(&mut call) else {
        panic!("the call did not finish");
    };
    assert_eq!(results.unwrap()[0].unwrap_i32(), 3);
    drop(call);

    let mut shared = store.into_shared();
    let mut task = Box::pin(add.call_async(&mut shared, &[Value::I32(4)]));
    let Poll::Ready(results) = poll(&mut task) else {
        panic!("the task did not finish");
    };
    assert_eq!(results.unwrap()[0].unwrap_i32(), 7);
}

/// Asks for the store as code that no reference reaches does, such as a
/// callback of an embedded engine: with a handle made from the environment.
/// Adds one to the host state if it gets the store.
fn reaches(env: &FunctionEnv<i32>) -> bool {
    env.async_env()
        .with_if_in_reach(|mut env| *env.data_mut() += 1)
        .is_some()
}

/// A guest whose exports are each one import, which returns whether
/// [`reaches`] got the store:
///
/// - `async` and `deferred` ask in an async host function, outside a slice;
/// - `in_slice` asks in a `with` slice of one;
/// - `sync` asks in a synchronous host function;
/// - `nested` calls `sync` with a handle made from the environment, and
///   returns its answer in the second bit and its own, asked afterwards, in
///   the first.
fn reaching_guest() -> (Store, FunctionEnv<i32>, Instance) {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0);
    let sync = Function::new_typed(&mut store, {
        let env = env.clone();
        move || i32::from(reaches(&env))
    });
    let calls_sync = Module::new(
        &store,
        r#"(module
          (import "h" "sync" (func $sync (result i32)))
          (func (export "sync") (result i32) (call $sync)))"#,
    )
    .unwrap();
    let calls_sync = Instance::new(
        &mut store,
        &calls_sync,
        &imports! { "h" => { "sync" => sync } },
    )
    .unwrap();
    let calls_sync = export(&calls_sync, "sync");

    let module = Module::new(
        &store,
        r#"(module
          (import "h" "async" (func $async (result i32)))
          (import "h" "deferred" (func $deferred (result i32)))
          (import "h" "in_slice" (func $in_slice (result i32)))
          (import "h" "sync" (func $sync (result i32)))
          (import "h" "nested" (func $nested (result i32)))
          (func (export "async") (result i32) (call $async))
          (func (export "deferred") (result i32) (call $deferred))
          (func (export "in_slice") (result i32) (call $in_slice))
          (func (export "sync") (result i32) (call $sync))
          (func (export "nested") (result i32) (call $nested)))"#,
    )
    .unwrap();
    let asks = Function::new_typed_async(&mut store, &env, {
        let env = env.clone();
        async move |_: &mut AsyncEnv<i32>| i32::from(reaches(&env))
    });
    let deferred = Function::new_typed_async(&mut store, &env, {
        let env = env.clone();
        move |_: &mut AsyncEnv<i32>| Deferred::ready(i32::from(reaches(&env)))
    });
    let in_slice = Function::new_typed_async(&mut store, &env, {
        let env = env.clone();
        async move |cx: &mut AsyncEnv<i32>| i32::from(cx.with(|_| reaches(&env)))
    });
    let nested = Function::new_typed_async(&mut store, &env, {
        let (env, calls_sync) = (env.clone(), calls_sync.clone());
        async move |_: &mut AsyncEnv<i32>| {
            let mut handle = env.async_env();
            let inner = calls_sync.call_async(&mut handle, &[]).await.unwrap()[0].unwrap_i32();
            inner << 1 | i32::from(reaches(&env))
        }
    });
    let imports = imports! { "h" => {
        "async" => asks,
        "deferred" => deferred,
        "in_slice" => in_slice,
        "sync" => calls_sync,
        "nested" => nested,
    } };
    let instance = Instance::new(&mut store, &module, &imports).unwrap();
    (store, env, instance)
}

/// `with_if_in_reach` has the store where `with` has it, in a poll of an
/// async host function of the store, however the guest was called and
/// whichever handle asks. Such a handle also makes a nested call.
#[test]
fn with_if_in_reach_takes_the_store_in_a_poll_as_with_does() {
    let (mut store, env, instance) = reaching_guest();
    let expected = [("async", 1), ("deferred", 1), ("nested", 0b01)];
    let mut reached = 0;
    for (name, bits) in expected {
        let function = export(&instance, name);
        let results = function.call(&mut store, &[]).unwrap();
        assert_eq!(results[0].unwrap_i32(), bits, "{name}, called");

        let mut call = Box::pin(function.call_async(&mut store, &[]));
        let Poll::Ready(results) = poll(&mut call) else {
            panic!("the call did not finish");
        };
        assert_eq!(results.unwrap()[0].unwrap_i32(), bits, "{name}, awaited");
        drop(call);
        reached += 2;
        assert_eq!(*env.as_ref(&store), reached);
    }

    let mut shared = store.into_shared();
    for (name, bits) in expected {
        let mut task = Box::pin(export(&instance, name).call_async(&mut shared, &[]));
        let Poll::Ready(results) = poll(&mut task) else {
            panic!("the task did not finish");
        };
        assert_eq!(results.unwrap()[0].unwrap_i32(), bits, "{name}, as a task");
    }
    assert_eq!(
        shared.try_with(|store| *env.as_ref(&store)),
        Ok(reached + 3)
    );
}

/// Where `with` panics, `with_if_in_reach` returns `None` and leaves the
/// store to whoever has it: outside every call, in a synchronous host
/// function, in a slice, and in a call of another store.
#[test]
fn with_if_in_reach_returns_none_where_the_store_is_out_of_reach() {
    let (mut store, env, instance) = reaching_guest();
    assert!(!reaches(&env), "no call is being polled");

    for name in ["sync", "in_slice"] {
        let function = export(&instance, name);
        let results = function.call(&mut store, &[]).unwrap();
        assert_eq!(results[0].unwrap_i32(), 0, "{name}, called");

        let mut call = Box::pin(function.call_async(&mut store, &[]));
        let Poll::Ready(results) = poll(&mut call) else {
            panic!("the call did not finish");
        };
        assert_eq!(results.unwrap()[0].unwrap_i32(), 0, "{name}, awaited");
    }

    // A call of another store is being polled, and it has its own store.
    let mut other = Store::default();
    let other_env = FunctionEnv::new(&mut other, 0);
    let asks_both = Function::new_typed_async(&mut other, &other_env, {
        let (env, other_env) = (env.clone(), other_env.clone());
        async move |_: &mut AsyncEnv<i32>| {
            i32::from(reaches(&env)) << 1 | i32::from(reaches(&other_env))
        }
    });
    let results = asks_both.call(&mut other, &[]).unwrap();
    assert_eq!(results[0].unwrap_i32(), 0b01);
    assert_eq!(*other_env.as_ref(&other), 1);

    // A slice of a shared store is not a poll of a call.
    let shared = store.into_shared();
    assert_eq!(shared.try_with(|_| reaches(&env)), Ok(false));
    assert_eq!(shared.try_with(|store| *env.as_ref(&store)), Ok(0));
}

/// The destructor of `try_with_fails_in_a_destructor_run_inside_another_slice`
/// asking with `with_if_in_reach`: `None` where the store is busy, and the
/// store where it is in reach.
#[test]
fn with_if_in_reach_asks_in_a_destructor_as_try_with_does() {
    struct Asks<'a>(&'a mut AsyncEnv<i32>);
    impl Drop for Asks<'_> {
        fn drop(&mut self) {
            let reached = self.0.with_if_in_reach(|mut env| *env.data_mut() += 1);
            REACHED.with_borrow_mut(|so_far| so_far.push(reached.is_some()));
        }
    }
    thread_local! {
        static REACHED: RefCell<Vec<bool>> = const { RefCell::new(Vec::new()) };
    }

    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0);
    let waits = Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<i32>| {
        let _asks = Asks(cx);
        std::future::pending::<()>().await;
    });
    let shared = store.into_shared();

    // Dropped outside every slice: the call drops its host future with the
    // store in reach.
    drop(waiting_task(&shared, &waits));
    assert_eq!(REACHED.take(), [true]);

    // Dropped by the holder of a slice.
    let task = waiting_task(&shared, &waits);
    shared.try_with(|_| drop(task)).unwrap();
    assert_eq!(REACHED.take(), [false]);
    assert_eq!(shared.try_with(|store| *env.as_ref(&store)), Ok(1));
}

/// Keeps the helper types above honest: `SharedStore` is `Clone`.
#[test]
fn a_shared_handle_is_a_cheap_clone() {
    let shared: SharedStore = Store::default().into_shared();
    let second = shared.clone();
    assert!(second.try_with(|_| ()).is_ok());
}

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
    AsStoreMut, AsStoreRef, AsyncEnv, Function, FunctionEnv, FunctionEnvMut, FunctionType,
    Instance, Module, RuntimeError, SharedStore, Store, StoreMut, StoreObjects, StoreRef,
    TypedFunction, Value, imports,
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
    let mut store = Store::default();
    let module = Module::new(&store, GUEST).unwrap();
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
    let run = instance.exports.get_function("run").unwrap().clone();
    (store, run, gate, env)
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

/// Keeps the helper types above honest: `SharedStore` is `Clone`.
#[test]
fn a_shared_handle_is_a_cheap_clone() {
    let shared: SharedStore = Store::default().into_shared();
    let second = shared.clone();
    assert!(second.try_with(|_| ()).is_ok());
}

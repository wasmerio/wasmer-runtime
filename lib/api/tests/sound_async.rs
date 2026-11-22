//! Acceptance tests for the proposal's borrowing async API.
#![cfg(all(
    feature = "experimental-async",
    feature = "sys",
    feature = "wat",
    not(feature = "v8-default"),
    not(target_arch = "wasm32")
))]

use std::{
    cell::Cell,
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

use futures::{executor::block_on, task::noop_waker};
use wasmer::{
    AsStoreMut, AsStoreRef, AsyncEnv, Function, FunctionEnv, FunctionEnvMut, Instance, Module,
    RuntimeError, Store, StoreMut, StoreObjects, StoreRef, Value, imports,
};
use wasmer_vm::TrapCode;

fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(&noop_waker()))
}

struct Gate(Rc<Cell<bool>>);
impl Future for Gate {
    type Output = ();

    fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
        if self.0.get() {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    }
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

struct CountDrop(Rc<Cell<usize>>);
impl Drop for CountDrop {
    fn drop(&mut self) {
        self.0.set(self.0.get() + 1);
    }
}

fn guest(store: &mut Store, import: Function) -> Function {
    let module = Module::new(
        store,
        r#"(module
          (import "h" "step" (func $step (param i32) (result i32)))
          (func (export "run") (param i32) (result i32)
            (i32.add (call $step (local.get 0)) (i32.const 1))))"#,
    )
    .unwrap();
    Instance::new(store, &module, &imports! { "h" => { "step" => import } })
        .unwrap()
        .exports
        .get_function("run")
        .unwrap()
        .clone()
}

#[test]
fn exclusive_calls_borrow_only_until_completion() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0i32);
    let import = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<i32>, value: i32| -> i32 {
            cx.with(|mut env| *env.data_mut() += value);
            YieldOnce(false).await;
            cx.with(|env| *env.data())
        },
    );
    let run = guest(&mut store, import);
    assert_eq!(
        block_on(run.call_async(&mut store, &[Value::I32(6)])).unwrap()[0],
        Value::I32(7)
    );
    assert_eq!(*env.as_ref(&store), 6);
    let typed = run.typed::<i32, i32>(&store).unwrap();
    assert_eq!(block_on(typed.call_async(&mut store, 2)).unwrap(), 9);
    assert_eq!(*env.as_ref(&store), 8);
}

#[test]
fn nested_guest_calls_resume_the_same_async_environment() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0usize);
    let inner = Function::new_typed_async(
        &mut store,
        &env,
        async |cx: &mut AsyncEnv<usize>, value: i32| -> i32 {
            cx.with(|mut env| *env.data_mut() += 1);
            YieldOnce(false).await;
            value * 2
        },
    );
    let inner_guest = guest(&mut store, inner);
    let outer = Function::new_typed_async(
        &mut store,
        &env,
        async move |cx: &mut AsyncEnv<usize>, value: i32| -> Result<i32, RuntimeError> {
            let args = [Value::I32(value)];
            let result = inner_guest.call_async(cx, &args).await?;
            cx.with(|mut env| *env.data_mut() += 1);
            Ok(result[0].unwrap_i32() + 3)
        },
    );
    let outer_guest = guest(&mut store, outer);
    assert_eq!(
        block_on(outer_guest.call_async(&mut store, &[Value::I32(5)])).unwrap()[0],
        Value::I32(15)
    );
    assert_eq!(*env.as_ref(&store), 2);
}

#[test]
fn ready_typed_import_runs_on_sync_and_start_entries() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0u32);
    let ready = Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<u32>| {
        cx.with(|mut env| *env.data_mut() += 1);
    });
    ready.call(&mut store, &[]).unwrap();
    let module = Module::new(
        &store,
        r#"(module (import "h" "ready" (func $ready))
            (func $start (call $ready)) (start $start))"#,
    )
    .unwrap();
    Instance::new(
        &mut store,
        &module,
        &imports! { "h" => { "ready" => ready } },
    )
    .unwrap();
    assert_eq!(*env.as_ref(&store), 2);
}

#[test]
fn pending_import_traps_on_sync_and_start_entries() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    let pending = Function::new_typed_async(&mut store, &env, async |_: &mut AsyncEnv<()>| {
        std::future::pending::<()>().await;
    });
    let error = pending.call(&mut store, &[]).unwrap_err();
    assert_eq!(error.to_trap(), Some(TrapCode::YieldOutsideAsyncContext));
    let module = Module::new(
        &store,
        r#"(module (import "h" "wait" (func $wait))
            (func $start (call $wait)) (start $start))"#,
    )
    .unwrap();
    let error = Instance::new(
        &mut store,
        &module,
        &imports! { "h" => { "wait" => pending } },
    )
    .unwrap_err();
    let wasmer::InstantiationError::Start(error) = error else {
        panic!("unexpected instantiation error: {error}");
    };
    assert_eq!(error.to_trap(), Some(TrapCode::YieldOutsideAsyncContext));
}

#[test]
fn dropping_before_start_or_during_wait_releases_the_store() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0usize);
    let gate = Rc::new(Cell::new(false));
    let drops = Rc::new(Cell::new(0));
    let import = Function::new_typed_async(&mut store, &env, {
        let (gate, drops) = (gate.clone(), drops.clone());
        async move |cx: &mut AsyncEnv<usize>, value: i32| -> i32 {
            let _drop = CountDrop(drops.clone());
            cx.with(|mut env| *env.data_mut() += 1);
            Gate(gate.clone()).await;
            cx.with(|mut env| *env.data_mut() += 100);
            value
        }
    });
    let run = guest(&mut store, import);
    drop(run.call_async(&mut store, &[Value::I32(0)]));
    assert_eq!(*env.as_ref(&store), 0);
    let params = [Value::I32(2)];
    let mut call = Box::pin(run.call_async(&mut store, &params));
    assert!(poll(call.as_mut()).is_pending());
    drop(call);
    assert_eq!(drops.get(), 1);
    assert_eq!(*env.as_ref(&store), 1);
    gate.set(true);
    assert_eq!(
        block_on(run.call_async(&mut store, &params)).unwrap()[0],
        Value::I32(3)
    );
    assert_eq!(*env.as_ref(&store), 102);
    assert_eq!(drops.get(), 2);
}

#[test]
fn forgetting_a_suspended_call_leaves_no_thread_state_or_store_dependency() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0usize);
    let wait = Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<usize>| {
        cx.with(|mut env| *env.data_mut() += 1);
        std::future::pending::<()>().await;
    });
    let mut call = Box::pin(wait.call_async(&mut store, &[]));
    assert!(poll(call.as_mut()).is_pending());
    std::mem::forget(call);
    *env.as_mut(&mut store) += 1;
    let ready =
        Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<usize>| -> i32 {
            cx.with(|env| *env.data() as i32)
        });
    assert_eq!(
        block_on(ready.call_async(&mut store, &[])).unwrap()[0],
        Value::I32(2)
    );
    drop(store);
    // A second store may run on the same thread after the first was freed.
    let mut other = Store::default();
    let function = Function::new_typed(&mut other, || 42i32);
    assert_eq!(function.call(&mut other, &[]).unwrap()[0], Value::I32(42));
}

#[test]
fn shared_calls_borrow_nothing_and_resume_out_of_order() {
    fn requires_static<T: 'static>(_: &T) {}
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, Vec::<i32>::new());
    let gates = [Rc::new(Cell::new(false)), Rc::new(Cell::new(false))];
    let wait = Function::new_typed_async(&mut store, &env, {
        let gates = gates.clone();
        async move |cx: &mut AsyncEnv<Vec<i32>>, index: i32| -> i32 {
            Gate(gates[index as usize].clone()).await;
            cx.with(|mut env| env.data_mut().push(index));
            index
        }
    });
    let run = guest(&mut store, wait);
    let mut shared = store.into_shared();
    let mut first = Box::pin(run.call_async(&mut shared, &[Value::I32(0)]));
    let mut second = Box::pin(run.call_async(&mut shared, &[Value::I32(1)]));
    requires_static(&first);
    requires_static(&second);
    assert!(poll(first.as_mut()).is_pending());
    assert!(poll(second.as_mut()).is_pending());
    gates[1].set(true);
    assert!(matches!(poll(second.as_mut()), Poll::Ready(Ok(_))));
    gates[0].set(true);
    assert!(matches!(poll(first.as_mut()), Poll::Ready(Ok(_))));
    drop((first, second));
    assert_eq!(
        shared.try_with(|store| env.as_ref(&store).clone()).unwrap(),
        vec![1, 0]
    );
    assert!(shared.into_store().is_ok());
}

#[test]
fn shared_store_rejects_reentry_and_release_lends_the_current_borrow() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0i32);
    let function =
        Function::new_typed_with_env(&mut store, &env, |mut env: FunctionEnvMut<i32>| {
            let shared = env.shared().expect("a shared call has a back-reference");
            assert!(shared.try_with(|_| ()).is_err());
            let handle = env.as_ref();
            env.release(|| {
                shared
                    .try_with(|mut store| *handle.as_mut(&mut store) = 42)
                    .unwrap();
            });
            assert_eq!(*env.data(), 42);
            assert!(shared.try_with(|_| ()).is_err());
        });
    let shared = store.into_shared();
    let weak = shared.downgrade();
    shared
        .try_with(|mut store| function.call(&mut store, &[]))
        .unwrap()
        .unwrap();
    shared
        .try_with(|_| assert!(shared.try_with(|_| ()).is_err()))
        .unwrap();
    let strong = weak.upgrade().unwrap();
    assert!(shared.into_store().is_err());
    let store = strong.into_store().unwrap();
    assert!(weak.upgrade().is_none());
    assert_eq!(*env.as_ref(&store), 42);
}

#[test]
fn a_public_store_adapter_is_derived_exactly_once() {
    struct Alternating {
        first: Store,
        second: Store,
        calls: usize,
    }
    impl AsStoreRef for Alternating {
        fn as_store_ref(&self) -> StoreRef<'_> {
            self.first.as_store_ref()
        }
    }
    impl AsStoreMut for Alternating {
        fn as_store_mut(&mut self) -> StoreMut<'_> {
            self.calls += 1;
            if self.calls == 1 {
                self.first.as_store_mut()
            } else {
                self.second.as_store_mut()
            }
        }
        fn objects_mut(&mut self) -> &mut StoreObjects {
            panic!("the public adapter must be captured through as_store_mut once")
        }
    }
    let mut first = Store::default();
    let env = FunctionEnv::new(&mut first, 41i32);
    let function =
        Function::new_typed_async(&mut first, &env, async |cx: &mut AsyncEnv<i32>| -> i32 {
            YieldOnce(false).await;
            cx.with(|env| *env.data() + 1)
        });
    let mut adapter = Alternating {
        first,
        second: Store::default(),
        calls: 0,
    };
    assert_eq!(
        block_on(function.call_async(&mut adapter, &[])).unwrap()[0],
        Value::I32(42)
    );
    assert_eq!(adapter.calls, 1);
}

#[test]
fn panicking_host_future_leaves_the_store_usable_after_drop() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0usize);
    let function =
        Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<usize>| -> () {
            cx.with(|mut env| *env.data_mut() += 1);
            panic!("host future panic");
        });
    let mut call = Box::pin(function.call_async(&mut store, &[]));
    assert!(catch_unwind(AssertUnwindSafe(|| poll(call.as_mut()))).is_err());
    drop(call);
    assert_eq!(*env.as_ref(&store), 1);
    *env.as_mut(&mut store) += 1;
}

#[test]
fn a_panicking_host_destructor_still_discards_the_guest_stack_safely() {
    struct PanicDrop;
    impl Drop for PanicDrop {
        fn drop(&mut self) {
            panic!("host destructor panic");
        }
    }
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    let wait = Function::new_typed_async(&mut store, &env, async |_: &mut AsyncEnv<()>| {
        let _drop = PanicDrop;
        std::future::pending::<()>().await;
    });
    let mut call = Box::pin(wait.call_async(&mut store, &[]));
    assert!(poll(call.as_mut()).is_pending());
    assert!(catch_unwind(AssertUnwindSafe(|| drop(call))).is_err());
    let ready = Function::new_typed(&mut store, || 42i32);
    assert_eq!(ready.call(&mut store, &[]).unwrap()[0], Value::I32(42));
}

#[test]
fn shared_cancel_and_forget_leave_other_tasks_runnable() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0usize);
    let gate = Rc::new(Cell::new(false));
    let drops = Rc::new(Cell::new(0));
    let wait = Function::new_typed_async(&mut store, &env, {
        let (gate, drops) = (gate.clone(), drops.clone());
        async move |cx: &mut AsyncEnv<usize>| {
            let _drop = CountDrop(drops.clone());
            Gate(gate.clone()).await;
            cx.with(|mut env| *env.data_mut() += 1);
        }
    });
    let mut shared = store.into_shared();
    let mut forgotten = Box::pin(wait.call_async(&mut shared, &[]));
    let mut cancelled = Box::pin(wait.call_async(&mut shared, &[]));
    let mut completes = Box::pin(wait.call_async(&mut shared, &[]));
    assert!(poll(forgotten.as_mut()).is_pending());
    assert!(poll(cancelled.as_mut()).is_pending());
    assert!(poll(completes.as_mut()).is_pending());
    std::mem::forget(forgotten);
    gate.set(true);
    // Cancellation after the import became ready must not resume its guest.
    drop(cancelled);
    assert_eq!(drops.get(), 1);
    assert!(matches!(poll(completes.as_mut()), Poll::Ready(Ok(_))));
    drop(completes);
    assert_eq!(drops.get(), 2);
    assert_eq!(shared.try_with(|store| *env.as_ref(&store)).unwrap(), 1);
}

#[test]
fn polling_from_a_slice_of_the_same_store_panics_without_aliasing() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    let ready = Function::new_typed_async(&mut store, &env, async |_: &mut AsyncEnv<()>| {});
    let mut shared = store.into_shared();
    let mut call = Box::pin(ready.call_async(&mut shared, &[]));
    shared
        .try_with(|_| {
            assert!(catch_unwind(AssertUnwindSafe(|| poll(call.as_mut()))).is_err());
        })
        .unwrap();
    // The rejected poll did not start the coroutine or poison the handle.
    assert!(matches!(poll(call.as_mut()), Poll::Ready(Ok(_))));
}

/// SharedStore permits interleaving; it does not give each Wasm stack a private
/// copy of the guest's globals. This is the proposal's P3 shadow-stack probe.
#[test]
fn shared_guests_must_manage_their_own_shadow_stacks() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    let gates = [Rc::new(Cell::new(false)), Rc::new(Cell::new(false))];
    let wait = Function::new_typed_async(&mut store, &env, {
        let gates = gates.clone();
        async move |_: &mut AsyncEnv<()>, slot: i32| {
            Gate(gates[slot as usize].clone()).await;
        }
    });
    let module = Module::new(
        &store,
        r#"(module
      (import "h" "wait" (func $wait (param i32)))
      (global $sp (mut i32) (i32.const 4096))
      (func (export "run") (param $slot i32) (result i32)
        (local $saved i32)
        (local.set $saved (global.get $sp))
        (global.set $sp (i32.sub (local.get $saved) (i32.const 16)))
        (call $wait (local.get $slot))
        (if (i32.ne (global.get $sp) (i32.sub (local.get $saved) (i32.const 16)))
          (then unreachable))
        (global.set $sp (local.get $saved))
        (local.get $slot)))"#,
    )
    .unwrap();
    let run = Instance::new(&mut store, &module, &imports! { "h" => { "wait" => wait } })
        .unwrap()
        .exports
        .get_function("run")
        .unwrap()
        .clone();
    let mut shared = store.into_shared();
    let mut first = Box::pin(run.call_async(&mut shared, &[Value::I32(0)]));
    let mut second = Box::pin(run.call_async(&mut shared, &[Value::I32(1)]));
    assert!(poll(first.as_mut()).is_pending());
    assert!(poll(second.as_mut()).is_pending());
    gates[0].set(true);
    assert!(
        matches!(poll(first.as_mut()), Poll::Ready(Err(_))),
        "the first guest detects that the second guest changed its shadow stack"
    );
    gates[1].set(true);
    assert!(matches!(poll(second.as_mut()), Poll::Ready(Ok(values)) if values[0] == Value::I32(1)));
    drop((first, second));
    assert!(shared.into_store().is_ok());
}

#![cfg(all(
    feature = "v8",
    feature = "experimental-async",
    not(target_arch = "wasm32")
))]

use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    process::{Child, Command},
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};
use wasmer::{
    AsStoreAsync, AsStoreMut, AsyncFunctionEnvMut, Function, FunctionEnv, FunctionEnvMut,
    FunctionType, Instance, Module, RuntimeError, Store, Type, Value, imports,
};

fn store() -> Store {
    Store::new(wasmer::v8::V8::new())
}

// Same owned-child watchdog as async_sync_borrow: a blocked wee8 OS stack
// cannot prevent the parent from reaping this one finite proof.
fn run_in_subprocess(test_name: &str) -> bool {
    const CHILD: &str = "WASMER_V8_ASYNC_PROOF_CHILD";
    if std::env::var_os(CHILD).is_some() {
        return true;
    }
    struct OwnedChild(Child);
    impl Drop for OwnedChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let mut child = OwnedChild(
        Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env(CHILD, "1")
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        match child.0.try_wait() {
            Ok(Some(status)) => {
                assert!(status.success(), "V8 proof failed: {status}");
                return false;
            }
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            result => panic!("V8 proof did not finish: {result:?}"),
        }
    }
}

fn poll_once<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

#[test]
fn v8_async_ready_guest_and_both_host_adapters_release_real_guards() {
    if !run_in_subprocess("v8_async_ready_guest_and_both_host_adapters_release_real_guards") {
        return;
    }
    struct State {
        value: i32,
        drops: Arc<AtomicUsize>,
    }
    impl Drop for State {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
        }
    }
    let drops = Arc::new(AtomicUsize::new(0));
    let weak_slot = Rc::new(RefCell::new(None));
    let mut store = store();
    let environment = FunctionEnv::new(
        &mut store,
        State {
            value: 2,
            drops: drops.clone(),
        },
    );
    let saved = weak_slot.clone();
    let dynamic = Function::new_with_env_async(
        &mut store,
        &environment,
        FunctionType::new([Type::I32], [Type::I32]),
        move |env: AsyncFunctionEnvMut<State>, values| {
            *saved.borrow_mut() = env.downgrade();
            let input = values[0].unwrap_i32();
            async move {
                let mut write = env
                    .try_write()
                    .expect("factory has no retained Store guard");
                write.data_mut().value += input;
                Ok(vec![Value::I32(write.data_mut().value)])
            }
        },
    );
    let typed = Function::new_typed_with_env_async(
        &mut store,
        &environment,
        |env: AsyncFunctionEnvMut<State>, input: i32| async move {
            let read = env
                .try_read()
                .expect("typed factory has no retained Store guard");
            input + read.data().value
        },
    );
    let module = Module::new(
        &store,
        wat::parse_str(
            r#"(module
        (import "host" "dynamic" (func $dynamic (param i32) (result i32)))
        (import "host" "typed" (func $typed (param i32) (result i32)))
        (func (export "run") (param i32) (result i32)
          local.get 0 call $dynamic call $typed))"#,
        )
        .unwrap(),
    )
    .unwrap();
    let instance = Instance::new(
        &mut store,
        &module,
        &imports! { "host" => { "dynamic" => dynamic, "typed" => typed } },
    )
    .unwrap();
    let run = instance.exports.get_function("run").unwrap().clone();
    let store = store.into_async();
    assert_eq!(
        futures::executor::block_on(run.call_async(&store, vec![Value::I32(5)])).unwrap()[0].i32(),
        Some(14)
    );
    let weak = weak_slot.borrow().as_ref().unwrap().clone();
    assert!(weak.upgrade().is_some());
    drop(instance);
    drop(store);
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(weak.upgrade().is_none());
    let _stale_copy = run.clone();
    assert_eq!(drops.load(Ordering::SeqCst), 1);
}

#[test]
fn v8_async_external_timer_wakes_the_same_stack_after_actual_pending() {
    if !run_in_subprocess("v8_async_external_timer_wakes_the_same_stack_after_actual_pending") {
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (pending_send, pending_receive) = tokio::sync::oneshot::channel();
    let (complete_send, complete_receive) = tokio::sync::oneshot::channel();
    let sender = runtime.spawn(async move {
        pending_receive.await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        complete_send.send(37i32).unwrap();
    });
    let pending_send = Rc::new(RefCell::new(Some(pending_send)));
    let complete_receive = Rc::new(RefCell::new(Some(complete_receive)));
    let polls = Rc::new(Cell::new(0));
    let mut store = store();
    let environment = FunctionEnv::new(&mut store, 0i32);
    let observed_polls = polls.clone();
    let function = Function::new_with_env_async(
        &mut store,
        &environment,
        FunctionType::new([], [Type::I32]),
        move |env: AsyncFunctionEnvMut<i32>, _| {
            let mut pending_send = pending_send.borrow_mut().take();
            let mut receive = complete_receive.borrow_mut().take().unwrap();
            let polls = observed_polls.clone();
            async move {
                let value = std::future::poll_fn(|cx| {
                    polls.set(polls.get() + 1);
                    assert!(
                        env.try_write().is_some(),
                        "the guest OS stack holds no Rust Store guard"
                    );
                    let result = Pin::new(&mut receive).poll(cx);
                    if result.is_pending()
                        && let Some(send) = pending_send.take()
                    {
                        send.send(()).unwrap();
                    }
                    result
                })
                .await
                .unwrap();
                *env.write().await.data_mut() = value;
                Ok(vec![Value::I32(value)])
            }
        },
    );
    let store = store.into_async();
    let result = futures::executor::block_on(function.call_async(&store, vec![])).unwrap();
    assert_eq!(result[0].i32(), Some(37));
    assert!(
        polls.get() >= 2,
        "the actual import returned Pending before the timer wake"
    );
    runtime.block_on(sender).unwrap();
    let read = futures::executor::block_on(store.read_lock());
    assert_eq!(*environment.as_ref(&read), 37);
}

#[test]
fn v8_async_guest_import_timer_resumes_its_live_wasm_activation() {
    if !run_in_subprocess("v8_async_guest_import_timer_resumes_its_live_wasm_activation") {
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (pending_send, pending_receive) = tokio::sync::oneshot::channel();
    let (complete_send, complete_receive) = tokio::sync::oneshot::channel();
    let sender = runtime.spawn(async move {
        pending_receive.await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        complete_send.send(37i32).unwrap();
    });
    let pending_send = Rc::new(RefCell::new(Some(pending_send)));
    let complete_receive = Rc::new(RefCell::new(Some(complete_receive)));
    let polls = Rc::new(Cell::new(0));
    let phase = Rc::new(RefCell::new(None::<wasmer::Global>));
    let observed_phase = phase.clone();
    let mut store = store();
    let environment = FunctionEnv::new(&mut store, 0i32);
    let observed_polls = polls.clone();
    let function = Function::new_with_env_async(
        &mut store,
        &environment,
        FunctionType::new([], [Type::I32]),
        move |env: AsyncFunctionEnvMut<i32>, _| {
            let mut pending_send = pending_send.borrow_mut().take();
            let mut receive = complete_receive.borrow_mut().take().unwrap();
            let polls = observed_polls.clone();
            let phase = observed_phase.clone();
            async move {
                let value = std::future::poll_fn(|cx| {
                    polls.set(polls.get() + 1);
                    {
                        let mut guard = env
                            .try_write()
                            .expect("the live guest stack holds no Rust Store guard");
                        let marker = phase.borrow().as_ref().unwrap().clone();
                        assert_eq!(
                            marker.get(&mut guard).i32(),
                            Some(1),
                            "the actual guest entered and has not resumed past its import"
                        );
                    }
                    let result = Pin::new(&mut receive).poll(cx);
                    if result.is_pending()
                        && let Some(send) = pending_send.take()
                    {
                        send.send(()).unwrap();
                    }
                    result
                })
                .await
                .unwrap();
                *env.write().await.data_mut() = value;
                Ok(vec![Value::I32(value)])
            }
        },
    );
    // This is the guest/import variant, distinct from the direct host-call
    // sibling. Before/after markers and result+1 require an actual guest
    // activation to remain on the same OS stack across the import Pending.
    let module = Module::new(
        &store,
        wat::parse_str(
            r#"(module
        (import "host" "timer" (func $timer (result i32)))
        (global $phase (export "phase") (mut i32) (i32.const 0))
        (func (export "run") (result i32)
            i32.const 1 global.set $phase
            call $timer
            i32.const 2 global.set $phase
            i32.const 1 i32.add))"#,
        )
        .unwrap(),
    )
    .unwrap();
    let instance = Instance::new(
        &mut store,
        &module,
        &imports! { "host" => { "timer" => function } },
    )
    .unwrap();
    let marker = instance.exports.get_global("phase").unwrap().clone();
    *phase.borrow_mut() = Some(marker.clone());
    let guest = instance.exports.get_function("run").unwrap().clone();
    let store = store.into_async();
    let result = futures::executor::block_on(guest.call_async(&store, vec![])).unwrap();
    assert_eq!(result[0].i32(), Some(38));
    assert!(
        polls.get() >= 2,
        "the actual import returned Pending before the timer wake"
    );
    runtime.block_on(sender).unwrap();
    let mut guard = futures::executor::block_on(store.write_lock());
    assert_eq!(*environment.as_ref(&guard), 37);
    assert_eq!(marker.get(&mut guard).i32(), Some(2));
}

#[test]
fn v8_async_construction_is_inert_and_first_poll_busy_preserves_reuse() {
    if !run_in_subprocess("v8_async_construction_is_inert_and_first_poll_busy_preserves_reuse") {
        return;
    }
    let factories = Rc::new(Cell::new(0));
    let polled = Rc::new(Cell::new(0));
    let mut store = store();
    let factory_count = factories.clone();
    let poll_count = polled.clone();
    let function = Function::new_async(&mut store, FunctionType::new([], [Type::I32]), move |_| {
        factory_count.set(factory_count.get() + 1);
        let polls = poll_count.clone();
        async move {
            polls.set(polls.get() + 1);
            Ok(vec![Value::I32(42)])
        }
    });
    let store = store.into_async();
    let held = futures::executor::block_on(store.write_lock());
    drop(function.call_async(&store, vec![]));
    let mut busy = Box::pin(function.call_async(&store, vec![]));
    assert_eq!((factories.get(), polled.get()), (0, 0));
    let Poll::Ready(Err(error)) = poll_once(busy.as_mut()) else {
        panic!("busy first poll must finish with an error")
    };
    assert!(error.to_string().contains("Store is busy"));
    assert_eq!((factories.get(), polled.get()), (0, 0));
    drop(busy);
    let mut other = self::store();
    let wrong = Function::new_typed(&mut other, || 99i32);
    let mut rejected = Box::pin(wrong.call_async(&store, vec![]));
    let Poll::Ready(Err(error)) = poll_once(rejected.as_mut()) else {
        panic!("busy wrong-origin first poll must finish")
    };
    assert!(error.to_string().contains("Store is busy"));
    drop(rejected);
    drop(held);
    let mut rejected = Box::pin(wrong.call_async(&store, vec![]));
    let Poll::Ready(Err(error)) = poll_once(rejected.as_mut()) else {
        panic!("wrong origin must finish before guest work")
    };
    assert!(error.to_string().contains("another Store backing"));
    drop(rejected);
    assert_eq!(
        futures::executor::block_on(function.call_async(&store, vec![])).unwrap()[0].i32(),
        Some(42)
    );
    assert_eq!((factories.get(), polled.get()), (1, 1));
}

#[test]
fn v8_async_mixed_sync_callbacks_reject_same_store_and_allow_different_store() {
    if !run_in_subprocess(
        "v8_async_mixed_sync_callbacks_reject_same_store_and_allow_different_store",
    ) {
        return;
    }
    let mut store = store();
    let same = Function::new_typed_async(&mut store, || async { 13i32 });
    let environment = FunctionEnv::new(&mut store, same);
    let function = Function::new_typed_with_env(
        &mut store,
        &environment,
        |env: FunctionEnvMut<Function>| -> Result<i32, RuntimeError> {
            let target = env.data().clone();
            let asynchronous = env
                .as_async_mut()
                .expect("actual V8 callback has its checked async cap");
            let cap = asynchronous.as_store_async();
            let mut same = Box::pin(target.call_async(&cap, vec![]));
            let Poll::Ready(Err(error)) = poll_once(same.as_mut()) else {
                panic!("same-Store sync callback cannot retain its parent guard")
            };
            assert!(error.to_string().contains("Store is busy"));
            drop(same);
            let mut other = self::store();
            let ready = Function::new_typed_async(&mut other, || async { 29i32 });
            let cap = other.into_async();
            let mut different = Box::pin(ready.call_async(&cap, vec![]));
            let Poll::Ready(Ok(result)) = poll_once(different.as_mut()) else {
                panic!("different Store Ready call must complete")
            };
            Ok(13 + result[0].unwrap_i32())
        },
    );
    let store = store.into_async();
    assert_eq!(
        futures::executor::block_on(function.call_async(&store, vec![])).unwrap()[0].i32(),
        Some(42)
    );
}

#[test]
fn v8_async_first_panic_prevents_on_called_replay() {
    if !run_in_subprocess("v8_async_first_panic_prevents_on_called_replay") {
        return;
    }
    let mut store = store();
    let first = Box::new(47usize);
    let address = (&*first) as *const usize as usize;
    let first = Rc::new(RefCell::new(Some(first)));
    let observed = Rc::new(Cell::new(0));
    let counter = observed.clone();
    let function = Function::new_async(&mut store, FunctionType::new([], []), move |_| {
        counter.set(counter.get() + 1);
        let mut first = first.borrow_mut().take();
        std::future::poll_fn(move |_| -> Poll<Result<Vec<Value>, RuntimeError>> {
            std::panic::resume_unwind(first.take().unwrap())
        })
    });
    let callbacks = Arc::new(AtomicUsize::new(0));
    let callback_count = callbacks.clone();
    store.as_store_mut().on_called(move |_| {
        callback_count.fetch_add(1, Ordering::SeqCst);
        Ok(wasmer_types::OnCalledAction::InvokeAgain)
    });
    let store = store.into_async();
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        futures::executor::block_on(function.call_async(&store, vec![]))
    }))
    .unwrap_err();
    let same = caught.downcast::<usize>().unwrap();
    assert_eq!((&*same) as *const usize as usize, address);
    assert_eq!(*same, 47);
    assert_eq!(observed.get(), 1);
    assert_eq!(callbacks.load(Ordering::SeqCst), 0);
}

#[test]
fn v8_guard_free_instantiation_selects_dual_callback_without_losing_borrowed_start() {
    if !run_in_subprocess(
        "v8_guard_free_instantiation_selects_dual_callback_without_losing_borrowed_start",
    ) {
        return;
    }
    let mut store = store();
    let environment = FunctionEnv::new(&mut store, 123i32);
    let synchronous = Arc::new(AtomicUsize::new(0));
    let asynchronous = Rc::new(Cell::new(0));
    let sync_count = synchronous.clone();
    let async_count = asynchronous.clone();
    let function = wasmer::v8::new_with_env_async_and_sync(
        &mut store,
        &environment,
        FunctionType::new([], [Type::I32]),
        move |env: FunctionEnvMut<'_, i32>, _| {
            assert_eq!(*env.data(), 123);
            sync_count.fetch_add(1, Ordering::SeqCst);
            Ok(vec![Value::I32(31)])
        },
        move |env: AsyncFunctionEnvMut<i32>, _| {
            let count = async_count.clone();
            async move {
                assert_eq!(*env.try_read().expect("no real outer guard").data(), 123);
                count.set(count.get() + 1);
                Ok(vec![Value::I32(47)])
            }
        },
    );
    let module = Module::new(
        &store,
        wat::parse_str(
            r#"(module
        (import "host" "value" (func $value (result i32)))
        (global $answer (mut i32) (i32.const 0))
        (func $start call $value global.set $answer)
        (start $start)
        (func (export "answer") (result i32) global.get $answer)
        (func (export "call") (result i32) call $value))"#,
        )
        .unwrap(),
    )
    .unwrap();
    let instance = Instance::new(
        &mut store,
        &module,
        &imports! { "host" => { "value" => function.clone() } },
    )
    .unwrap();
    assert_eq!(synchronous.load(Ordering::SeqCst), 1);
    assert_eq!(
        instance
            .exports
            .get_function("answer")
            .unwrap()
            .call(&mut store, &[])
            .unwrap()[0]
            .i32(),
        Some(31)
    );
    assert_eq!(
        instance
            .exports
            .get_function("call")
            .unwrap()
            .call(&mut store, &[])
            .unwrap()[0]
            .i32(),
        Some(31)
    );
    assert_eq!(synchronous.load(Ordering::SeqCst), 2);
    assert_eq!(asynchronous.get(), 0);
    let store = store.into_async();
    let held = futures::executor::block_on(store.write_lock());
    drop(wasmer::v8::instantiate_async(
        &store,
        module.clone(),
        vec![function.clone().into()],
    ));
    let mut busy =
        wasmer::v8::instantiate_async(&store, module.clone(), vec![function.clone().into()]);
    assert!(matches!(poll_once(busy.as_mut()), Poll::Ready(Err(_))));
    assert_eq!(
        asynchronous.get(),
        0,
        "busy/lazy construction cannot run start"
    );
    drop(busy);
    drop(held);
    let instance = futures::executor::block_on(wasmer::v8::instantiate_async(
        &store,
        module,
        vec![function.into()],
    ))
    .unwrap();
    assert_eq!(asynchronous.get(), 1);
    assert_eq!(synchronous.load(Ordering::SeqCst), 2);
    let answer = instance.exports.get_function("answer").unwrap();
    assert_eq!(
        futures::executor::block_on(answer.call_async(&store, vec![])).unwrap()[0].i32(),
        Some(47)
    );
    let call = instance.exports.get_function("call").unwrap();
    assert_eq!(
        futures::executor::block_on(call.call_async(&store, vec![])).unwrap()[0].i32(),
        Some(47)
    );
    assert_eq!(asynchronous.get(), 2);
}

#[test]
fn v8_guard_free_instance_start_drives_a_real_pending_timer_import() {
    if !run_in_subprocess("v8_guard_free_instance_start_drives_a_real_pending_timer_import") {
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap();
    let (pending_send, pending_receive) = tokio::sync::oneshot::channel();
    let (complete_send, complete_receive) = tokio::sync::oneshot::channel();
    let sender = runtime.spawn(async move {
        pending_receive.await.unwrap();
        tokio::time::sleep(Duration::from_millis(5)).await;
        complete_send.send(37i32).unwrap();
    });
    let pending_send = Rc::new(RefCell::new(Some(pending_send)));
    let complete_receive = Rc::new(RefCell::new(Some(complete_receive)));
    let polls = Rc::new(Cell::new(0));
    let mut store = store();
    let environment = FunctionEnv::new(&mut store, 0i32);
    let observed = polls.clone();
    let function = Function::new_with_env_async(
        &mut store,
        &environment,
        FunctionType::new([], [Type::I32]),
        move |env: AsyncFunctionEnvMut<i32>, _| {
            let mut send = pending_send.borrow_mut().take();
            let mut receive = complete_receive.borrow_mut().take().unwrap();
            let polls = observed.clone();
            async move {
                let value = std::future::poll_fn(|cx| {
                    polls.set(polls.get() + 1);
                    assert!(
                        env.try_write().is_some(),
                        "Wasm start must not retain its real Store guard"
                    );
                    let result = Pin::new(&mut receive).poll(cx);
                    if result.is_pending()
                        && let Some(send) = send.take()
                    {
                        send.send(()).unwrap();
                    }
                    result
                })
                .await
                .unwrap();
                *env.write().await.data_mut() = value;
                Ok(vec![Value::I32(value)])
            }
        },
    );
    let module = Module::new(
        &store,
        wat::parse_str(
            r#"(module
        (import "host" "timer" (func $timer (result i32)))
        (global $answer (mut i32) (i32.const 0))
        (func $start call $timer global.set $answer)
        (start $start)
        (func (export "answer") (result i32) global.get $answer))"#,
        )
        .unwrap(),
    )
    .unwrap();
    let store = store.into_async();
    let instance = futures::executor::block_on(wasmer::v8::instantiate_async(
        &store,
        module,
        vec![function.into()],
    ))
    .unwrap();
    assert!(
        polls.get() >= 2,
        "actual start import must have returned Pending"
    );
    let answer = instance.exports.get_function("answer").unwrap();
    assert_eq!(
        futures::executor::block_on(answer.call_async(&store, vec![])).unwrap()[0].i32(),
        Some(37)
    );
    assert_eq!(
        *environment.as_ref(&futures::executor::block_on(store.read_lock())),
        37
    );
    runtime.block_on(sender).unwrap();
}

#[test]
fn v8_checked_borrow_reenters_exact_lock_and_restores_plain_foreign_store() {
    if !run_in_subprocess("v8_checked_borrow_reenters_exact_lock_and_restores_plain_foreign_store")
    {
        return;
    }
    let first = store();
    let first_id = first.id();
    let first = first.into_async();
    let mut foreign = store();
    let foreign_id = foreign.id();
    let mut wrong = store();
    wrong
        .as_store_mut()
        .objects_mut()
        .as_v8_mut()
        .set_id(first_id);
    let wrong = wrong.into_async();
    assert!(
        wasmer::v8::with_borrowed_store(&first, |_| ())
            .unwrap()
            .is_none()
    );
    let mut guard = futures::executor::block_on(first.write_lock());
    guard.as_store_mut().parked(|| {
        let error = wasmer::v8::with_borrowed_store(&wrong, |_| panic!("wrong backing admitted"))
            .unwrap_err();
        assert!(error.to_string().contains("backing changed"));
        wasmer::v8::with_borrowed_store(&first, |current| {
            current.parked(|| {
                foreign.as_store_mut().parked(|| {
                    assert_eq!(
                        wasmer::v8::with_borrowed_store(&first, |first| first.objects_mut().id())
                            .unwrap(),
                        Some(first_id)
                    );
                    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        wasmer::v8::with_borrowed_store(&first, |_| {
                            std::panic::resume_unwind(Box::new(71u64))
                        })
                        .unwrap();
                    }))
                    .unwrap_err();
                    assert_eq!(*panic.downcast::<u64>().unwrap(), 71);
                    assert_eq!(
                        Store::with_current(|current| current.objects_mut().id()),
                        Some(foreign_id)
                    );
                });
            });
        })
        .unwrap()
        .unwrap();
        assert_eq!(
            Store::with_current(|current| current.objects_mut().id()),
            Some(first_id)
        );
    });
    drop(guard);
    assert!(Store::with_current(|_| ()).is_none());
    assert!(
        wasmer::v8::with_borrowed_store(&first, |_| ())
            .unwrap()
            .is_none()
    );
    assert!(poll_once(Box::pin(first.write_lock()).as_mut()).is_ready());
}

#[test]
fn v8_dual_callback_factory_disposal_preserves_first_panic_during_normal_and_active_unwind() {
    if !run_in_subprocess(
        "v8_dual_callback_factory_disposal_preserves_first_panic_during_normal_and_active_unwind",
    ) {
        return;
    }
    struct Capture {
        panic: std::sync::Mutex<Option<Box<dyn std::any::Any + Send>>>,
        drops: Arc<AtomicUsize>,
        calls: Arc<AtomicUsize>,
    }
    impl Capture {
        fn called(&self) {
            self.calls.fetch_add(1, Ordering::SeqCst);
        }
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            let panic = self.panic.get_mut().unwrap().take().unwrap();
            std::panic::resume_unwind(panic);
        }
    }
    struct DropStore(Option<Store>);
    impl Drop for DropStore {
        fn drop(&mut self) {
            drop(self.0.take());
        }
    }
    for active in [false, true] {
        let mut store = store();
        let environment = FunctionEnv::new(&mut store, ());
        let first = Box::new(37usize);
        let first_address = (&*first) as *const usize as usize;
        let first_drops = Arc::new(AtomicUsize::new(0));
        let second_drops = Arc::new(AtomicUsize::new(0));
        let calls = Arc::new(AtomicUsize::new(0));
        let asynchronous = Capture {
            panic: std::sync::Mutex::new(Some(first)),
            drops: first_drops.clone(),
            calls: calls.clone(),
        };
        let synchronous = Capture {
            panic: std::sync::Mutex::new(Some(Box::new(47usize))),
            drops: second_drops.clone(),
            calls: calls.clone(),
        };
        let function = wasmer::v8::new_with_env_async_and_sync(
            &mut store,
            &environment,
            FunctionType::new([], []),
            move |_: FunctionEnvMut<'_, ()>, _| {
                synchronous.called();
                Ok(vec![])
            },
            move |_: AsyncFunctionEnvMut<()>, _| {
                asynchronous.called();
                std::future::ready(Ok(vec![]))
            },
        );
        if active {
            let primary = Box::new(71usize);
            let primary_address = (&*primary) as *const usize as usize;
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _store = DropStore(Some(store));
                std::panic::resume_unwind(primary);
            }))
            .unwrap_err()
            .downcast::<usize>()
            .unwrap();
            assert_eq!((&*caught) as *const usize as usize, primary_address);
            assert_eq!(*caught, 71);
        } else {
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(store)))
                .unwrap_err()
                .downcast::<usize>()
                .unwrap();
            assert_eq!((&*caught) as *const usize as usize, first_address);
            assert_eq!(*caught, 37);
        }
        assert_eq!(first_drops.load(Ordering::SeqCst), 1);
        assert_eq!(second_drops.load(Ordering::SeqCst), 1);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "factory construction/teardown cannot execute either callback"
        );
        drop(function); // expired inert metadata remains safe after actual arena Drop
    }
}

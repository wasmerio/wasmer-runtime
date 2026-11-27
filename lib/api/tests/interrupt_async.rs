#![cfg(all(unix, feature = "experimental-host-interrupt", feature = "async"))]

// TODO: tests for recursive function calls across different stores

use std::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    sync::{Arc, Barrier, Mutex, atomic::AtomicU32},
    task::{Context, Poll, Wake, Waker},
    thread,
    time::Duration,
};

use anyhow::Result;
use futures::task::noop_waker;
use wasmer::{
    AsyncEnv, Function, FunctionEnv, FunctionEnvMut, Instance, Interrupter, Module, RuntimeError,
    Store, Value, imports,
};
use wasmer_vm::{TrapCode, interrupt_registry};

const WAT: &str = r#"
    (module
      (import "env" "f" (func $f))
      (func (export "async")
        call $f
      )
      (func (export "plain") (result i32)
        i32.const 1
      )
    )"#;

/// A store whose export `async` waits for good, and its export `plain`.
/// Untyped: the future of a typed call drops the untyped call it awaits as
/// soon as that returns, and a caller keeps an untyped one as long as it likes.
fn waiting_guest() -> Result<(Store, Function, Function)> {
    let wasm = wat::parse_str(WAT)?;
    let mut store = Store::default();
    let module = Module::new(&store, &wasm)?;
    let env = FunctionEnv::new(&mut store, ());
    let f = Function::new_typed_async(&mut store, &env, async |_: &mut AsyncEnv<()>| {
        std::future::pending::<()>().await;
    });
    let instance = Instance::new(&mut store, &module, &imports! { "env" => { "f" => f } })?;
    let waits = instance.exports.get_function("async")?.clone();
    let plain = instance.exports.get_function("plain")?.clone();
    Ok((store, waits, plain))
}

#[test]
fn async_function_can_be_interrupted() -> Result<()> {
    let interrupter_slot = Arc::new(Mutex::new(None));
    let barrier = Arc::new(Barrier::new(2));

    let worker = thread::spawn({
        let barrier = barrier.clone();
        let interrupter_slot = interrupter_slot.clone();
        move || {
            let wasm = wat::parse_str(WAT)?;

            let mut store = Store::default();
            let interrupter = store.interrupter();
            interrupter_slot.lock().unwrap().replace(interrupter);
            let module = Module::new(&store, &wasm)?;

            let env = FunctionEnv::new(&mut store, ());
            let f = Function::new_typed_async(&mut store, &env, async |_: &mut AsyncEnv<()>| {
                std::future::pending::<()>().await;
            });
            let imports = imports! {
                "env" => {
                    "f" => f
                }
            };

            let instance = Instance::new(&mut store, &module, &imports)?;
            let f = instance
                .exports
                .get_typed_function::<(), ()>(&store, "async")?;

            barrier.wait();
            let res = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(f.call_async(&mut store));
            anyhow::Ok(res)
        }
    });

    barrier.wait();
    // Give the function time to suspend before requesting interruption.
    thread::sleep(Duration::from_millis(500));

    interrupter_slot
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .interrupt();
    let result = worker.join().unwrap().unwrap().unwrap_err();
    assert_eq!(result.to_trap().unwrap(), TrapCode::HostInterrupt);

    Ok(())
}

#[test]
fn all_futures_from_store_are_interrupted() -> Result<()> {
    let wasm = wat::parse_str(WAT)?;

    let mut store = Store::default();
    let interrupter = store.interrupter();
    let module = Module::new(&store, &wasm)?;

    let pending_count = Arc::new(AtomicU32::new(0));

    let env = FunctionEnv::new(&mut store, ());
    let f = Function::new_typed_async(&mut store, &env, {
        let pending_count = pending_count.clone();
        async move |_: &mut AsyncEnv<()>| {
            pending_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::pending::<()>().await;
        }
    });
    let imports = imports! {
        "env" => {
            "f" => f
        }
    };

    let instance = Instance::new(&mut store, &module, &imports)?;
    let f = instance
        .exports
        .get_typed_function::<(), ()>(&store, "async")?;

    let mut shared = store.into_shared();
    let mut futures = [
        Box::pin(f.call_async(&mut shared)),
        Box::pin(f.call_async(&mut shared)),
        Box::pin(f.call_async(&mut shared)),
        Box::pin(f.call_async(&mut shared)),
    ];

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    // All futures should be in pending state for now
    for f in &mut futures {
        assert!(f.as_mut().poll(&mut cx).is_pending());
    }

    // Since we polled everything already, there should be 4 calls to the imported function
    assert_eq!(pending_count.load(std::sync::atomic::Ordering::SeqCst), 4);

    interrupter.interrupt();

    for pending in &mut futures {
        // Until the last pending call exits, a new call must be refused.
        let mut new_call = Box::pin(f.call_async(&mut shared));
        let Poll::Ready(Err(error)) = new_call.as_mut().poll(&mut cx) else {
            panic!("new call entered an interrupted store")
        };
        assert_eq!(error.to_trap(), Some(TrapCode::HostInterrupt));

        let Poll::Ready(Err(error)) = pending.as_mut().poll(&mut cx) else {
            panic!("pending call was not interrupted")
        };
        assert_eq!(error.to_trap(), Some(TrapCode::HostInterrupt));
    }

    let mut new_call = Box::pin(f.call_async(&mut shared));
    assert!(new_call.as_mut().poll(&mut cx).is_pending());
    assert_eq!(pending_count.load(std::sync::atomic::Ordering::SeqCst), 5);
    Ok(())
}

/// An interrupted call releases its registration on completion, so it does
/// not keep the store interrupted even if its future has not been dropped.
#[test]
fn an_interrupted_call_does_not_keep_its_store_interrupted() -> Result<()> {
    let (store, waits, plain) = waiting_guest()?;
    let interrupter = store.interrupter();
    let mut store = store.into_shared();
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);

    let mut call = Box::pin(waits.call_async(&mut store, &[]));
    assert!(call.as_mut().poll(&mut cx).is_pending());
    interrupter.interrupt();
    let Poll::Ready(Err(error)) = call.as_mut().poll(&mut cx) else {
        panic!("the call was not interrupted");
    };
    assert_eq!(error.to_trap(), Some(TrapCode::HostInterrupt));

    // `call` is kept: it finished, and holds nothing.
    let mut next = Box::pin(plain.call_async(&mut store, &[]));
    let Poll::Ready(result) = next.as_mut().poll(&mut cx) else {
        panic!("the next call did not finish");
    };
    assert_eq!(result?.to_vec(), vec![Value::I32(1)]);
    drop(call);
    Ok(())
}

type CallFuture = Pin<Box<dyn Future<Output = Result<Box<[Value]>, RuntimeError>>>>;

thread_local! {
    /// The call `PollsAtOnce` polls.
    static POLLED_AT_ONCE: RefCell<Option<CallFuture>> = const { RefCell::new(None) };
}

/// A waker that polls its call as soon as it is woken.
struct PollsAtOnce;

impl Wake for PollsAtOnce {
    fn wake(self: Arc<Self>) {
        let waker = Waker::from(self);
        POLLED_AT_ONCE.with_borrow_mut(|call| {
            if let Some(call) = call {
                let _ = call.as_mut().poll(&mut Context::from_waker(&waker));
            }
        });
    }
}

/// The interrupt wakes a call whose waker polls it at once: the poll can
/// take the interrupt's bookkeeping again.
#[test]
fn an_interrupt_wakes_a_waker_that_polls_at_once() -> Result<()> {
    let (store, waits, _) = waiting_guest()?;
    let interrupter = store.interrupter();
    let mut store = store.into_shared();
    let waker = Waker::from(Arc::new(PollsAtOnce));
    let mut call: CallFuture = Box::pin(waits.call_async(&mut store, &[]));
    assert!(
        call.as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    POLLED_AT_ONCE.set(Some(call));
    interrupter.interrupt();
    drop(POLLED_AT_ONCE.take());
    Ok(())
}

/// A forgotten suspended call may keep an interrupt registration, but it
/// cannot resume once its thread is gone.
#[test]
fn a_forgotten_call_on_an_exited_thread_is_not_interruptible() -> Result<()> {
    let id = thread::spawn(|| -> Result<_> {
        let (store, waits, _) = waiting_guest()?;
        let id = store.id();
        let mut store = store.into_shared();
        let mut call = Box::pin(waits.call_async(&mut store, &[]));
        assert!(
            call.as_mut()
                .poll(&mut Context::from_waker(&noop_waker()))
                .is_pending()
        );
        std::mem::forget(call);
        Ok(id)
    })
    .join()
    .unwrap()?;
    assert!(matches!(
        interrupt_registry::interrupt(id),
        Err(interrupt_registry::InterruptError::StoreNotRunning)
    ));
    Ok(())
}

/// A waker is the caller's: its destructor is user code, and may panic. A
/// call dropped while its guest waits drops the waker it registered last,
/// when nothing of the call is left to put in order, so the panic leaves the
/// store usable.
#[test]
fn a_waker_whose_destructor_panics_leaves_a_cancelled_call_in_order() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use wasmer::{AsyncEnv, FunctionEnv};

    struct PanicOnDrop;
    impl Wake for PanicOnDrop {
        fn wake(self: Arc<Self>) {}
    }
    impl Drop for PanicOnDrop {
        fn drop(&mut self) {
            panic!("waker destructor panic");
        }
    }

    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, ());
    let wait = Function::new_typed_async(&mut store, &env, async |_: &mut AsyncEnv<()>| {
        std::future::pending::<()>().await;
    });
    let module = Module::new(
        &store,
        r#"(module
          (import "h" "wait" (func $wait))
          (func (export "run") call $wait)
          (func (export "plain")))"#,
    )
    .unwrap();
    let imports = imports! { "h" => { "wait" => wait } };
    let instance = Instance::new(&mut store, &module, &imports).unwrap();
    let run = instance.exports.get_function("run").unwrap();
    let waker = Waker::from(Arc::new(PanicOnDrop));
    let mut call = Box::pin(run.call_async(&mut store, &[]));
    let poll = call.as_mut().poll(&mut Context::from_waker(&waker));
    assert!(poll.is_pending());
    // The call holds the last handle to the waker now.
    drop(waker);
    assert!(catch_unwind(AssertUnwindSafe(|| drop(call))).is_err());
    let plain = instance.exports.get_function("plain").unwrap();
    plain.call(&mut store, &[]).unwrap();
}

/// The state of the import of `relaying_guest`.
struct Relay {
    interrupter: Interrupter,
    /// The guest's export `plain`.
    plain: Option<Function>,
    /// What the import's call of `plain` returned.
    nested: Option<Result<(), Option<TrapCode>>>,
}

/// A store whose export `async` calls the import that `make_relay` builds.
/// Returns the export, and the environment of the import.
fn relaying_guest(
    make_relay: impl FnOnce(&mut Store, &FunctionEnv<Relay>) -> Function,
) -> Result<(Store, FunctionEnv<Relay>, Function)> {
    let wasm = wat::parse_str(WAT)?;
    let mut store = Store::default();
    let module = Module::new(&store, &wasm)?;
    let relay = Relay {
        interrupter: store.interrupter(),
        plain: None,
        nested: None,
    };
    let env = FunctionEnv::new(&mut store, relay);
    let f = make_relay(&mut store, &env);
    let instance = Instance::new(&mut store, &module, &imports! { "env" => { "f" => f } })?;
    env.as_mut(&mut store).plain = Some(instance.exports.get_function("plain")?.clone());
    let run = instance.exports.get_function("async")?.clone();
    Ok((store, env, run))
}

/// A sync import interrupts its store, then calls into it again. The nested
/// call is refused: the store was interrupted before it entered.
#[test]
fn a_sync_call_nested_after_an_interrupt_is_interrupted() -> Result<()> {
    let (mut store, env, run) = relaying_guest(|store, env| {
        Function::new_typed_with_env(
            store,
            env,
            |mut env: FunctionEnvMut<Relay>| -> Result<(), RuntimeError> {
                env.data().interrupter.interrupt();
                let plain = env.data().plain.clone().unwrap();
                let nested = plain.call(&mut env, &[]);
                env.data_mut().nested = Some(nested.map(drop).map_err(|e| e.to_trap()));
                Ok(())
            },
        )
    })?;

    let result = run.call(&mut store, &[]);
    assert_eq!(result.unwrap_err().to_trap(), Some(TrapCode::HostInterrupt));
    assert_eq!(
        env.as_ref(&store).nested,
        Some(Err(Some(TrapCode::HostInterrupt)))
    );
    Ok(())
}

/// The same through an async import, whose nested call is a `call_async`
/// made in the first poll of the import's future.
#[test]
fn an_async_call_nested_after_an_interrupt_is_interrupted() -> Result<()> {
    let (mut store, env, run) = relaying_guest(|store, env| {
        Function::new_typed_async(
            store,
            env,
            async |cx: &mut AsyncEnv<Relay>| -> Result<(), RuntimeError> {
                let (interrupter, plain) = cx.with(|env| {
                    let relay = env.data();
                    (relay.interrupter.clone(), relay.plain.clone().unwrap())
                });
                interrupter.interrupt();
                let nested = plain.call_async(cx, &[]).await;
                let nested = nested.map(drop).map_err(|e| e.to_trap());
                cx.with(|mut env| env.data_mut().nested = Some(nested));
                Ok(())
            },
        )
    })?;

    let result = futures::executor::block_on(run.call_async(&mut store, &[]));
    assert_eq!(result.unwrap_err().to_trap(), Some(TrapCode::HostInterrupt));
    assert_eq!(
        env.as_ref(&store).nested,
        Some(Err(Some(TrapCode::HostInterrupt)))
    );
    Ok(())
}

/// Pending on its first poll, which wakes it, and ready on the next.
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

/// The same, with the import's future waiting once before it interrupts the
/// store and makes the nested call.
#[test]
fn an_async_call_nested_after_waiting_is_interrupted() -> Result<()> {
    let (mut store, env, run) = relaying_guest(|store, env| {
        Function::new_typed_async(
            store,
            env,
            async |cx: &mut AsyncEnv<Relay>| -> Result<(), RuntimeError> {
                YieldOnce(false).await;
                let (interrupter, plain) = cx.with(|env| {
                    let relay = env.data();
                    (relay.interrupter.clone(), relay.plain.clone().unwrap())
                });
                interrupter.interrupt();
                let nested = plain.call_async(cx, &[]).await;
                let nested = nested.map(drop).map_err(|e| e.to_trap());
                cx.with(|mut env| env.data_mut().nested = Some(nested));
                Ok(())
            },
        )
    })?;

    let result = futures::executor::block_on(run.call_async(&mut store, &[]));
    assert_eq!(result.unwrap_err().to_trap(), Some(TrapCode::HostInterrupt));
    assert_eq!(
        env.as_ref(&store).nested,
        Some(Err(Some(TrapCode::HostInterrupt)))
    );
    Ok(())
}

#![cfg(all(
    feature = "async",
    feature = "sys",
    not(feature = "v8-default"),
    not(target_arch = "wasm32")
))]
#![forbid(unsafe_code)]
//! Regressions from adversarial reviews of the native async API. Each test
//! uses only safe public API.

use std::{
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    task::{Context, Waker},
};

use wasmer::{
    AsStoreMut, AsStoreRef, AsyncEnv, Function, FunctionEnv, FunctionEnvMut, Instance, Module,
    Store, StoreMut, StoreObjects, StoreRef, imports,
};

/// `AsStoreMut` is a public trait, so a wrapper can present the view of
/// `data_and_store` as a mutable store. The view still cannot reach the `T`
/// whose `&mut` is live.
#[test]
#[should_panic(expected = "cannot read host state")]
fn a_wrapped_store_view_cannot_read_host_state() {
    struct ViewAsMut<'a>(StoreRef<'a>);
    impl AsStoreRef for ViewAsMut<'_> {
        fn as_store_ref(&self) -> StoreRef<'_> {
            self.0.as_store_ref()
        }
    }
    impl AsStoreMut for ViewAsMut<'_> {
        fn as_store_mut(&mut self) -> StoreMut<'_> {
            unreachable!("never called")
        }
        fn objects_mut(&mut self) -> &mut StoreObjects {
            unreachable!("never called")
        }
    }

    let mut store = Store::default();
    let handle = FunctionEnv::new(&mut store, 1u64);
    let mut env = handle.clone().into_mut(&mut store);
    let (data, view) = env.data_and_store();
    let wrapped = ViewAsMut(view);
    let alias: &u64 = handle.as_ref(&wrapped);
    *data = 2;
    assert_eq!(*alias, 2);
}

#[derive(Default)]
struct Host {
    pending: Option<Function>,
    inner: Option<Function>,
    drops: u32,
}

struct UsesStoreOnDrop<'a>(&'a mut AsyncEnv<Host>);
impl Drop for UsesStoreOnDrop<'_> {
    fn drop(&mut self) {
        self.0.with(|mut env| env.data_mut().drops += 1);
    }
}

/// A guest whose exports reach a host future that waits for good and uses
/// the store in its destructor, directly and through nested calls.
fn destructor_guest() -> (Store, FunctionEnv<Host>, Function, Function) {
    let mut store = Store::default();
    let module = Module::new(
        &store,
        r#"(module
          (import "h" "outer" (func $outer))
          (import "h" "sync" (func $sync))
          (import "h" "pend" (func $pend))
          (func (export "top") call $outer)
          (func (export "inner") call $sync)
          (func (export "pending") call $pend))"#,
    )
    .unwrap();
    let env = FunctionEnv::new(&mut store, Host::default());
    let outer = Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<Host>| {
        let inner = cx.with(|env| env.data().inner.clone().unwrap());
        let _ = inner.call_async(cx, &[]).await;
    });
    let sync = Function::new_typed_with_env(&mut store, &env, |mut env: FunctionEnvMut<Host>| {
        let pending = env.data().pending.clone().unwrap();
        assert!(pending.call(&mut env, &[]).is_err());
    });
    let pend = Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<Host>| {
        let _guard = UsesStoreOnDrop(cx);
        std::future::pending::<()>().await;
    });
    let imports = imports! { "h" => { "outer" => outer, "sync" => sync, "pend" => pend } };
    let instance = Instance::new(&mut store, &module, &imports).unwrap();
    let get = |name: &str| instance.exports.get_function(name).unwrap().clone();
    let (top, inner, pending) = (get("top"), get("inner"), get("pending"));
    let data = env.as_mut(&mut store);
    data.pending = Some(pending.clone());
    data.inner = Some(inner);
    (store, env, top, pending)
}

/// A host future that cannot wait on a sync entry is dropped with the store of
/// that entry in reach.
#[test]
fn a_pending_future_on_a_sync_entry_is_dropped_with_its_store() {
    let (mut store, env, _, pending) = destructor_guest();
    let outcome = catch_unwind(AssertUnwindSafe(|| pending.call(&mut store, &[])));
    assert!(outcome.unwrap().is_err(), "the import cannot wait");
    assert_eq!(env.as_ref(&store).drops, 1);
}

/// The same under an outer poll, whose published store must not be the one
/// the destructor gets: a sync host function holds that store.
#[test]
fn a_pending_future_under_a_sync_host_function_uses_that_entry() {
    let (mut store, env, top, _) = destructor_guest();
    let mut call = Box::pin(top.call_async(&mut store, &[]));
    assert!(
        call.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
    drop(call);
    assert_eq!(env.as_ref(&store).drops, 1);
}

#[cfg(all(unix, feature = "experimental-host-interrupt"))]
mod interrupts {
    use std::{
        cell::RefCell,
        pin::Pin,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
        task::{Poll, Wake},
        time::Duration,
    };

    use wasmer::{Interrupter, RuntimeError, Value};

    use super::*;

    type Call = Pin<Box<dyn Future<Output = Result<Box<[Value]>, RuntimeError>>>>;

    fn poll<F: Future + ?Sized>(future: Pin<&mut F>) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(Waker::noop()))
    }

    /// `run` waits for good, and `plain` returns 7.
    fn guest(store: &mut Store) -> (Function, Function) {
        let env = FunctionEnv::new(store, ());
        let wait = Function::new_typed_async(&mut *store, &env, async |_: &mut AsyncEnv<()>| {
            std::future::pending::<()>().await;
        });
        let module = Module::new(
            &*store,
            r#"(module
              (import "h" "wait" (func $wait))
              (func (export "run") (call $wait))
              (func (export "plain") (result i32) i32.const 7))"#,
        )
        .unwrap();
        let imports = imports! { "h" => { "wait" => wait } };
        let instance = Instance::new(store, &module, &imports).unwrap();
        let get = |name: &str| instance.exports.get_function(name).unwrap().clone();
        (get("run"), get("plain"))
    }

    #[derive(Default)]
    struct Flag(AtomicBool);
    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// Runs `f` on its own thread, failing if it deadlocks.
    fn finishes(f: impl FnOnce() + Send + 'static) {
        let (done, finished) = mpsc::channel();
        std::thread::spawn(move || {
            f();
            done.send(()).unwrap();
        });
        finished
            .recv_timeout(Duration::from_secs(10))
            .expect("the thread deadlocked");
    }

    #[test]
    fn a_forgotten_call_does_not_keep_the_store_interrupted() {
        let mut store = Store::default();
        let interrupter = store.interrupter();
        let (run, plain) = guest(&mut store);
        let mut call = Box::pin(run.call_async(&mut store, &[]));
        assert!(poll(call.as_mut()).is_pending());
        std::mem::forget(call);

        interrupter.interrupt();
        for _ in 0..3 {
            let results = plain.call(&mut store, &[]).unwrap();
            assert_eq!(results[0], Value::I32(7));
        }
    }

    /// A live call stays interrupted, while a new call on the store runs.
    #[test]
    fn an_interrupt_reaches_a_suspended_call_after_a_new_one_ran() {
        let mut store = Store::default();
        let interrupter = store.interrupter();
        let (run, plain) = guest(&mut store);
        let mut shared = store.into_shared();
        let mut suspended = Box::pin(run.call_async(&mut shared, &[]));
        assert!(poll(suspended.as_mut()).is_pending());

        interrupter.interrupt();
        let results = shared
            .try_with(|mut store| plain.call(&mut store, &[]))
            .unwrap()
            .unwrap();
        assert_eq!(results[0], Value::I32(7));
        let Poll::Ready(Err(error)) = poll(suspended.as_mut()) else {
            panic!("the suspended call was not interrupted");
        };
        assert_eq!(error.to_trap(), Some(wasmer_vm::TrapCode::HostInterrupt));
    }

    #[test]
    fn an_interrupt_wakes_the_waker_of_the_latest_poll() {
        let mut store = Store::default();
        let interrupter = store.interrupter();
        let (run, _) = guest(&mut store);
        let (old, new) = (Arc::new(Flag::default()), Arc::new(Flag::default()));
        let (old_waker, new_waker) = (Waker::from(old.clone()), Waker::from(new.clone()));

        let mut call = Box::pin(run.call_async(&mut store, &[]));
        assert!(
            call.as_mut()
                .poll(&mut Context::from_waker(&old_waker))
                .is_pending()
        );
        assert!(
            call.as_mut()
                .poll(&mut Context::from_waker(&new_waker))
                .is_pending()
        );
        interrupter.interrupt();
        assert!(
            new.0.load(Ordering::SeqCst),
            "the current waker was not woken"
        );
    }

    #[test]
    fn a_forgotten_call_does_not_keep_its_waker_past_the_store() {
        let flag = Arc::new(Flag::default());
        let waker = Waker::from(flag.clone());
        let mut store = Store::default();
        let (run, _) = guest(&mut store);
        let mut call = Box::pin(run.call_async(&mut store, &[]));
        assert!(
            call.as_mut()
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );
        std::mem::forget(call);
        drop(store);
        drop(waker);
        assert_eq!(Arc::strong_count(&flag), 1, "a global map owns the waker");
    }

    /// A waker's destructor is user code, and may interrupt the store.
    #[test]
    fn a_waker_destructor_can_interrupt_its_store() {
        struct InterruptOnDrop(Interrupter);
        impl Wake for InterruptOnDrop {
            fn wake(self: Arc<Self>) {
                drop(self);
            }
        }
        impl Drop for InterruptOnDrop {
            fn drop(&mut self) {
                self.0.interrupt();
            }
        }

        finishes(|| {
            let mut store = Store::default();
            let interrupter = store.interrupter();
            let (run, _) = guest(&mut store);
            let mut call = Box::pin(run.call_async(&mut store, &[]));
            let waker = Waker::from(Arc::new(InterruptOnDrop(interrupter)));
            assert!(
                call.as_mut()
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
            drop(waker);
            drop(call);
        });
    }

    /// A waker's destructor is user code, and may drop another call.
    #[test]
    fn a_waker_destructor_can_drop_another_call() {
        thread_local! {
            static HELD: RefCell<Option<Call>> = const { RefCell::new(None) };
        }
        struct DropsCall;
        impl Wake for DropsCall {
            fn wake(self: Arc<Self>) {
                drop(self);
            }
        }
        impl Drop for DropsCall {
            fn drop(&mut self) {
                let _ = HELD.try_with(|held| held.borrow_mut().take());
            }
        }

        finishes(|| {
            let mut store = Store::default();
            let (run, _) = guest(&mut store);
            let mut shared = store.into_shared();
            let mut first: Call = Box::pin(run.call_async(&mut shared, &[]));
            let mut second: Call = Box::pin(run.call_async(&mut shared, &[]));
            assert!(poll(second.as_mut()).is_pending());
            HELD.with(|held| *held.borrow_mut() = Some(second));
            let owning = Waker::from(Arc::new(DropsCall));
            assert!(
                first
                    .as_mut()
                    .poll(&mut Context::from_waker(&owning))
                    .is_pending()
            );
            drop(owning);
            drop(first);
        });
    }
}

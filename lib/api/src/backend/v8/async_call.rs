//! Async calls on the V8 backend.
//!
//! V8's C API cannot switch stacks, so a guest stays on the calling OS stack
//! and is never suspended. `poll` runs the whole call, and blocks the thread
//! while the future of an async import is driven. The awaited work must not
//! depend on the blocked executor; self-wakes and wakes from another thread
//! work. Nested calls run lexically, and tasks on a `SharedStore` that need
//! an out-of-order resume cannot run here.

use std::{
    any::Any,
    cell::{Cell, RefCell},
    future::Future,
    panic::resume_unwind,
    pin::Pin,
    rc::Rc,
    sync::{Arc, Condvar, Mutex},
    task::{Context, Poll, Wake, Waker},
};

use crate::{
    AsyncCallRequest, CallStore, PollScope, PollStore, RuntimeError, StoreInner, StoreMut, Value,
    entities::function::async_host::HostFuture,
};
use wasmer_types::StoreId;

type Panic = Box<dyn Any + Send>;

enum Failure {
    Error(RuntimeError),
    Panic(Panic),
}

struct Entry {
    id: StoreId,
    ptr: *mut StoreInner,
    asynchronous: bool,
    failure: RefCell<Option<Failure>>,
}

thread_local! {
    static ENTRIES: RefCell<Vec<Rc<Entry>>> = const { RefCell::new(Vec::new()) };
    // An async marker is consumed by exactly one guest entry. A synchronous
    // guest call made by a host callback must not inherit its parent's marker.
    static ASYNC_ENTRY: Cell<Option<StoreId>> = const { Cell::new(None) };
}

pub(crate) fn with_async_entry<R>(id: StoreId, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<StoreId>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ASYNC_ENTRY.set(self.0);
        }
    }
    let _restore = Restore(ASYNC_ENTRY.replace(Some(id)));
    f()
}

/// Lends the caller's exact store borrow until the C API returns. The frame is
/// also used by synchronous calls and instantiation's start function.
pub(crate) fn enter<R>(
    store: &mut StoreMut<'_>,
    f: impl FnOnce() -> Result<R, RuntimeError>,
) -> Result<R, RuntimeError> {
    let id = store.inner.objects.id();
    let asynchronous = ASYNC_ENTRY.take().is_some_and(|expected| expected == id);
    let entry = Rc::new(Entry {
        id,
        ptr: store.inner as *mut StoreInner,
        asynchronous,
        failure: RefCell::new(None),
    });
    struct Leave;
    impl Drop for Leave {
        fn drop(&mut self) {
            ENTRIES.with(|entries| {
                entries.borrow_mut().pop();
            });
        }
    }
    ENTRIES.with(|entries| entries.borrow_mut().push(entry.clone()));
    let leave = Leave;
    let result = f();
    let failure = entry.failure.borrow_mut().take();
    drop(leave);
    match failure {
        Some(Failure::Panic(panic)) => resume_unwind(panic),
        Some(Failure::Error(error)) => Err(error),
        None => result,
    }
}

fn current(id: StoreId) -> Result<Rc<Entry>, RuntimeError> {
    ENTRIES.with(|entries| {
        entries
            .borrow()
            .last()
            .filter(|entry| entry.id == id)
            .cloned()
            .ok_or_else(|| {
                RuntimeError::new("V8 async import called without its store's active guest entry")
            })
    })
}

pub(crate) fn save_panic(id: StoreId, panic: Panic) {
    if let Ok(entry) = current(id) {
        *entry.failure.borrow_mut() = Some(Failure::Panic(panic));
    }
}

/// Preserve the original host error, including its source and trap code,
/// while the C API carries a trap back to the matching entry.
pub(crate) fn save_error(error: RuntimeError) {
    ENTRIES.with(|entries| {
        if let Some(entry) = entries.borrow().last() {
            *entry.failure.borrow_mut() = Some(Failure::Error(error));
        }
    });
}

pub(crate) fn run_host(
    id: StoreId,
    factory: &dyn Fn(Vec<Value>) -> HostFuture,
    args: Vec<Value>,
) -> Result<Vec<Value>, RuntimeError> {
    let entry = current(id)?;
    // `enter` retains the exact caller's loan until this C callback returns.
    // Publish it before constructing the future, and keep it published through
    // dropping the future, so both polling and destructors can take slices.
    let _scope = PollScope::enter(Some(PollStore::exclusive(id, entry.ptr)));
    let mut future = factory(args);
    if entry.asynchronous {
        block_on(future.as_mut())
    } else {
        let waker = futures::task::noop_waker();
        match future.as_mut().poll(&mut Context::from_waker(&waker)) {
            Poll::Ready(result) => result,
            Poll::Pending => Err(RuntimeError::new_from_source(
                crate::BackendTrap::V8(super::error::Trap::user(
                    "an async import cannot suspend a synchronous V8 guest entry; use Function::call_async".into(),
                )),
                Vec::new(),
                Some(wasmer_types::TrapCode::YieldOutsideAsyncContext),
            )),
        }
    }
}

#[derive(Default)]
struct Parker {
    notified: Mutex<bool>,
    ready: Condvar,
}
impl Wake for Parker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Arc<Self>) {
        *self.notified.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.ready.notify_one();
    }
}

/// Unlike executor `block_on`, this driver allows lexical nesting. Its waker
/// wakes only the owning thread; it never polls a future on a foreign thread.
fn block_on<F: Future + ?Sized>(mut future: Pin<&mut F>) -> F::Output {
    let parker = Arc::new(Parker::default());
    let waker = Waker::from(parker.clone());
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        let mut notified = parker.notified.lock().unwrap_or_else(|e| e.into_inner());
        while !*notified {
            notified = parker
                .ready
                .wait(notified)
                .unwrap_or_else(|e| e.into_inner());
        }
        *notified = false;
    }
}

/// `Function::call_async` on this backend. The call runs in its first poll.
pub(crate) async fn call<X: 'static>(
    store: CallStore<'_>,
    request: AsyncCallRequest<X>,
) -> Result<X, RuntimeError> {
    let AsyncCallRequest {
        function,
        params,
        finish,
    } = request;
    let slice = store.resolve().enter_slice();
    // Safety: the slice is the only holder of the store until it is dropped,
    // which is after every borrow made here.
    let mut store_mut = unsafe { StoreMut::from_raw(slice.ptr()) };
    let params = params(&mut store_mut)?;
    let results = with_async_entry(store.id(), || function.call(&mut store_mut, &params))?;
    finish(&mut store_mut, &results)
}

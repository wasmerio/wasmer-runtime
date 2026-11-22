//! V8's C API cannot suspend a guest stack. Async imports are driven on the
//! calling OS thread, and nested guest calls must finish in lexical order.

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

use crate::{RuntimeError, StoreInner, StoreMut, Value, async_runtime::HostFuture};
use wasmer_types::StoreId;

type Panic = Box<dyn Any + Send>;

struct Entry {
    id: StoreId,
    ptr: *mut StoreInner,
    asynchronous: bool,
    panic: RefCell<Option<Panic>>,
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
pub(crate) fn enter<R>(store: &mut StoreMut<'_>, f: impl FnOnce() -> R) -> R {
    let id = store.inner.objects.id();
    let asynchronous = ASYNC_ENTRY.take().is_some_and(|expected| expected == id);
    let entry = Rc::new(Entry {
        id,
        ptr: store.inner as *mut StoreInner,
        asynchronous,
        panic: RefCell::new(None),
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
    let panic = entry.panic.borrow_mut().take();
    drop(leave);
    if let Some(panic) = panic {
        resume_unwind(panic);
    }
    result
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
        *entry.panic.borrow_mut() = Some(panic);
    }
}

pub(crate) fn run_host(
    id: StoreId,
    factory: &dyn Fn(Vec<Value>) -> HostFuture,
    args: Vec<Value>,
) -> Result<Vec<Value>, RuntimeError> {
    let entry = current(id)?;
    // SAFETY: `enter` keeps the store borrow alive and unreachable to Rust
    // callers until this C callback returns. Each with-slice reborrows it.
    unsafe {
        crate::async_runtime::with_sync_store(entry.ptr, || {
            let mut future = factory(args);
            if entry.asynchronous {
                Ok(block_on(future.as_mut())?)
            } else {
                let waker = futures::task::noop_waker();
                match future.as_mut().poll(&mut Context::from_waker(&waker)) {
                    Poll::Ready(result) => result,
                    Poll::Pending => Err(RuntimeError::new(
                        "an async import cannot suspend a synchronous V8 guest entry; use Function::call_async",
                    )),
                }
            }
        })
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

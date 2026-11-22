//! Async calls on the V8 backend.
//!
//! V8's C API cannot switch stacks, so a guest stays on the calling OS stack
//! and is never suspended. `poll` runs the whole call. An exclusive call
//! blocks its thread while an import's future is driven, so that future must
//! be woken by another thread, or by itself. Everything else that would have
//! to suspend fails at once: a synchronous entry, and a task on a
//! `SharedStore`, which could be waiting for another task that cannot run
//! until this one returns.

use std::{
    any::Any,
    cell::{Cell, RefCell},
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::{Arc, Condvar, Mutex},
    task::{Context, Poll, Wake, Waker},
};

use wasmer_types::{StoreId, TrapCode};

use super::error::Trap;
use crate::{
    AsStoreMut, AsyncCallRequest, BackendTrap, CallStore, PollScope, PollStore, RuntimeError,
    StoreInner, StoreMut, Value, entities::function::async_host::HostFuture,
};

/// One guest entry through V8's C API, while it runs.
#[derive(Clone, Copy)]
struct Entry {
    id: StoreId,
    /// The store, as the caller of the entry lends it until V8 returns.
    store: *mut StoreInner,
    /// The entry was made by a call that may wait for an import.
    may_wait: bool,
}

thread_local! {
    /// Set by a call that may wait, for the guest entry it is about to make.
    static NEXT_ENTRY_MAY_WAIT: Cell<Option<StoreId>> = const { Cell::new(None) };
    /// The innermost guest entry on this thread.
    static ENTRY: Cell<Option<Entry>> = const { Cell::new(None) };
    /// A host function's panic, on its way past V8's frames.
    static PANIC: RefCell<Option<Box<dyn Any + Send>>> = const { RefCell::new(None) };
}

/// Runs one guest entry through V8's C API. Every entry goes through here, so
/// that a synchronous one never inherits the right to wait from its caller,
/// an async import finds the store its caller lends, and a host function's
/// panic resumes once V8's frames are gone.
pub(crate) fn guest_entry<R>(store: &mut impl AsStoreMut, enter: impl FnOnce() -> R) -> R {
    struct Restore(Option<Entry>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ENTRY.set(self.0);
        }
    }
    let store = store.as_store_mut();
    let id = store.inner.objects.id();
    let entry = Entry {
        id,
        store: &mut *store.inner,
        may_wait: NEXT_ENTRY_MAY_WAIT.take() == Some(id),
    };
    let result = {
        let _restore = Restore(ENTRY.replace(Some(entry)));
        enter()
    };
    if let Some(panic) = PANIC.take() {
        resume_unwind(panic);
    }
    result
}

/// Keeps the panic of a host function, which cannot unwind through V8's
/// frames, for the guest entry it ran in to resume.
pub(crate) fn keep_panic(panic: Box<dyn Any + Send>) {
    PANIC.set(Some(panic));
}

/// What wakes the driver of one import. Each driver has its own: the one
/// park token of a thread would go to whichever driver parks next, and a call
/// nested in an import's future drives imports of its own.
#[derive(Default)]
struct Notification {
    woken: Mutex<bool>,
    changed: Condvar,
}

impl Notification {
    /// Blocks the thread until a wake that came since the last wait.
    fn wait(&self) {
        let mut woken = self.woken.lock().unwrap_or_else(|error| error.into_inner());
        while !*woken {
            woken = self.changed.wait(woken).unwrap_or_else(|error| error.into_inner());
        }
        *woken = false;
    }
}

impl Wake for Notification {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        *self.woken.lock().unwrap_or_else(|error| error.into_inner()) = true;
        self.changed.notify_one();
    }
}

/// The error of an import that would have to suspend.
fn cannot_wait(message: &str) -> RuntimeError {
    let source = BackendTrap::V8(Trap::user(message.to_string().into()));
    RuntimeError::new_from_source(
        source,
        Vec::new(),
        Some(TrapCode::YieldOutsideAsyncContext),
    )
}

/// Drives the future of an async import of the store `id` on this stack. The
/// store is published for its slices, as the caller of the guest entry lends
/// it: an import called with another store than its own fails. A panic
/// cannot unwind through V8's frames, so it is kept for [`guest_entry`] to
/// resume.
pub(crate) fn run_import(
    id: StoreId,
    future: impl FnOnce() -> HostFuture,
) -> Result<Vec<Value>, RuntimeError> {
    let Some(entry) = ENTRY.get().filter(|entry| entry.id == id) else {
        return Err(RuntimeError::new(
            "a host function was called with a store it does not belong to",
        ));
    };
    let outcome = catch_unwind(AssertUnwindSafe(|| drive(entry, future)));
    outcome.unwrap_or_else(|panic| {
        keep_panic(panic);
        Err(RuntimeError::new("an async host function panicked"))
    })
}

fn drive(entry: Entry, future: impl FnOnce() -> HostFuture) -> Result<Vec<Value>, RuntimeError> {
    let _scope = PollScope::enter(Some(PollStore::exclusive(entry.id, entry.store)));
    // After the scope, so that it is made and dropped with the store in reach.
    let mut future = future();
    if !entry.may_wait {
        // Nothing will wait for a wake: there is no waker to make.
        return match future.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(result) => result,
            Poll::Pending => Err(cannot_wait(
                "an async import cannot wait here on the V8 backend: its guest was \
                 entered synchronously, or is a task on a `SharedStore`, and V8 \
                 cannot suspend it",
            )),
        };
    }
    let notification = Arc::new(Notification::default());
    let waker = Waker::from(Arc::clone(&notification));
    let mut cx = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(result) = future.as_mut().poll(&mut cx) {
            return result;
        }
        notification.wait();
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
    let id = store.id();
    let may_wait = match &store {
        CallStore::Exclusive { .. } => true,
        CallStore::Env { .. } => ENTRY.get().is_some_and(|entry| entry.id == id && entry.may_wait),
        CallStore::Shared(_) => false,
    };
    let slice = store.resolve().enter_slice();
    // Safety: the slice is the only holder of the store until it is dropped,
    // which is after every borrow made here.
    let mut store_mut = unsafe { StoreMut::from_raw(slice.ptr()) };
    let params = params(&mut store_mut)?;
    NEXT_ENTRY_MAY_WAIT.set(may_wait.then_some(id));
    let results = {
        // A sync host function of this guest holds the store, and must not
        // find it published for a host future as well.
        let _hidden = PollScope::enter(None);
        function.call(&mut store_mut, &params)
    };
    NEXT_ENTRY_MAY_WAIT.set(None);
    finish(&mut store_mut, &results?)
}

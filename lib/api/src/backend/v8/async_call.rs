//! Async calls on the V8 backend.
//!
//! V8's C API cannot switch stacks, so a guest stays on the calling OS stack
//! and is never suspended. `poll` runs the whole call. An exclusive call
//! blocks its thread while an import's future is driven, so that future must
//! be woken by another thread. Everything else that would have to suspend
//! fails at once: a synchronous entry, and a task on a `SharedStore`, which
//! could be waiting for another task that cannot run until this one returns.

use std::{
    any::Any,
    cell::{Cell, RefCell},
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
    thread::{self, Thread},
};

use wasmer_types::{StoreId, TrapCode};

use super::error::Trap;
use crate::{
    AsStoreMut, AsyncCallRequest, BackendTrap, CallStore, PollScope, PollStore, RuntimeError,
    StoreInner, StoreMut, Value, entities::function::async_host::HostFuture,
};

thread_local! {
    /// Set by a call that may wait, for the guest entry it is about to make.
    static NEXT_ENTRY_MAY_WAIT: Cell<Option<StoreId>> = const { Cell::new(None) };
    /// The store whose innermost guest entry may wait, if it may.
    static ENTRY_MAY_WAIT: Cell<Option<StoreId>> = const { Cell::new(None) };
    /// A host future's panic, on its way past V8's frames.
    static PANIC: RefCell<Option<Box<dyn Any + Send>>> = const { RefCell::new(None) };
}

/// Runs one guest entry through V8's C API. Every entry goes through here, so
/// that a synchronous one never inherits the right to wait from its caller,
/// and a host future's panic resumes once V8's frames are gone.
pub(crate) fn guest_entry<R>(id: StoreId, enter: impl FnOnce() -> R) -> R {
    struct Restore(Option<StoreId>);
    impl Drop for Restore {
        fn drop(&mut self) {
            ENTRY_MAY_WAIT.set(self.0);
        }
    }
    let may_wait = NEXT_ENTRY_MAY_WAIT.take().filter(|next| *next == id);
    let result = {
        let _restore = Restore(ENTRY_MAY_WAIT.replace(may_wait));
        enter()
    };
    if let Some(panic) = PANIC.take() {
        resume_unwind(panic);
    }
    result
}

struct ThreadWaker(Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
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

/// Drives the future of an async import on this stack. The store is published
/// for its slices, from the borrow the import holds. A panic cannot unwind
/// through V8's frames, so it is kept for [`guest_entry`] to resume.
pub(crate) fn run_import(
    store: &mut StoreMut<'_>,
    future: HostFuture,
) -> Result<Vec<Value>, RuntimeError> {
    let id = store.objects_mut().id();
    let ptr: *mut StoreInner = &mut *store.inner;
    let may_wait = ENTRY_MAY_WAIT.get() == Some(id);
    let outcome = catch_unwind(AssertUnwindSafe(|| drive(id, ptr, future, may_wait)));
    outcome.unwrap_or_else(|panic| {
        PANIC.set(Some(panic));
        Err(RuntimeError::new("an async host function panicked"))
    })
}

fn drive(
    id: StoreId,
    ptr: *mut StoreInner,
    future: HostFuture,
    may_wait: bool,
) -> Result<Vec<Value>, RuntimeError> {
    let _scope = PollScope::enter(Some(PollStore::exclusive(id, ptr)));
    // After the scope, so that it is dropped with the store in reach.
    let mut future = future;
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(result) => return result,
            Poll::Pending if may_wait => thread::park(),
            Poll::Pending => {
                return Err(cannot_wait(
                    "an async import cannot wait here on the V8 backend: its guest was \
                     entered synchronously, or is a task on a `SharedStore`, and V8 \
                     cannot suspend it",
                ));
            }
        }
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
        CallStore::Env { .. } => ENTRY_MAY_WAIT.get() == Some(id),
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

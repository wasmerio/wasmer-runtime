//! Async calls on the V8 backend.
//!
//! V8's C API cannot switch stacks, so a guest stays on the calling OS stack
//! and is never suspended. `poll` runs the whole call, and blocks the thread
//! while the future of an async import is driven. So that future must be
//! woken by another thread: one that waits for the executor polling this call
//! never finishes. Nested calls work lexically, and tasks on a `SharedStore`
//! that need an out-of-order resume cannot run here.

use std::{
    sync::Arc,
    task::{Context, Poll, Wake, Waker},
    thread::{self, Thread},
};

use crate::{
    AsStoreMut, AsyncCallRequest, CallStore, PollScope, PollStore, RuntimeError, StoreInner,
    StoreMut, Value, entities::function::async_host::HostFuture,
};

struct ThreadWaker(Thread);

impl Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Drives the future of an async import to completion on this stack. The
/// store is published for its slices, from the borrow the import holds.
pub(crate) fn block_on_host(
    store: &mut StoreMut<'_>,
    future: HostFuture,
) -> Result<Vec<Value>, RuntimeError> {
    let id = store.objects_mut().id();
    let ptr: *mut StoreInner = &mut *store.inner;
    let _scope = PollScope::enter(Some(PollStore::exclusive(id, ptr)));
    // After the scope, so that it is dropped with the store in reach.
    let mut future = future;
    let waker = Waker::from(Arc::new(ThreadWaker(thread::current())));
    let mut cx = Context::from_waker(&waker);
    loop {
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(result) => return result,
            Poll::Pending => thread::park(),
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
    let slice = store.resolve().enter_slice();
    // Safety: the slice is the only holder of the store until it is dropped,
    // which is after every borrow made here.
    let mut store_mut = unsafe { StoreMut::from_raw(slice.ptr()) };
    let params = params(&mut store_mut)?;
    let results = function.call(&mut store_mut, &params)?;
    finish(&mut store_mut, &results)
}

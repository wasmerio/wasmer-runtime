//! [`Deferred`]: the results of a call, or a future for them.

use std::{future::Future, pin::Pin};

/// What a host function made with
/// [`Function::new_typed_async`](crate::Function::new_typed_async) or
/// [`Function::new_async`](crate::Function::new_async) returns when it is
/// not an `async` function: the results of its call, or a future for them.
///
/// An `async` host function pays for being able to wait on every call: its
/// call is a future, which is made, given a place to live and polled before
/// the runtime knows whether it waits. A function that returns `Deferred`
/// says for itself whether it waits. It is called as a sync host function
/// is, with the [`AsyncEnv`](crate::AsyncEnv) an `async` one has, and reaches
/// the store with [`AsyncEnv::with`](crate::AsyncEnv::with).
///
/// - [`Deferred::ready`] ends the call. On `sys`, such a call of a typed
///   function costs what a call of a sync host function costs.
/// - [`Deferred::pending`] has the guest wait for a future, as it does in an
///   `async` host function. The future is `'static`: it keeps a clone of the
///   `AsyncEnv` to reach the store with.
///
/// ```ignore
/// fn read(cx: &mut AsyncEnv<State>, fd: i32) -> Deferred<i32> {
///     if let Some(byte) = cx.with(|mut env| env.data_mut().buffered(fd)) {
///         return Deferred::ready(byte);
///     }
///     let mut cx = cx.clone();
///     Deferred::pending(async move {
///         let byte = receive(fd).await;
///         cx.with(|mut env| env.data_mut().count(fd));
///         byte
///     })
/// }
///
/// let read = Function::new_typed_async(&mut store, &env, read);
/// ```
pub struct Deferred<R>(pub(crate) DeferredState<R>);

pub(crate) enum DeferredState<R> {
    Ready(R),
    Pending(Pin<Box<dyn Future<Output = R>>>),
}

impl<R> Deferred<R> {
    /// The call is over, with these results.
    pub fn ready(results: R) -> Self {
        Self(DeferredState::Ready(results))
    }

    /// The guest waits for `future`, which gives the results.
    ///
    /// Called through [`Function::call_async`](crate::Function::call_async),
    /// the guest may wait. Called synchronously, the future must be ready at
    /// once, or the guest traps with `YieldOutsideAsyncContext`.
    pub fn pending(future: impl Future<Output = R> + 'static) -> Self {
        Self(DeferredState::Pending(Box::pin(future)))
    }

    /// The results, at once or once the future has them.
    pub(crate) async fn into_results(self) -> R {
        match self.0 {
            DeferredState::Ready(results) => results,
            DeferredState::Pending(future) => future.await,
        }
    }
}

impl<R> std::fmt::Debug for Deferred<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self.0 {
            DeferredState::Ready(_) => "Deferred::ready",
            DeferredState::Pending(_) => "Deferred::pending",
        })
    }
}

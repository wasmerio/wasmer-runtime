//! Async host functions: [`AsyncEnv`], [`AsyncHostFunction`] and
//! [`AsyncDynamicHostFunction`].

use std::{future::Future, marker::PhantomData};

use wasmer_types::StoreId;

use super::deferred::Deferred;
use crate::{
    FunctionEnv, FunctionEnvMut, RuntimeError, StoreBusy, StoreMut, Value, WasmTypeList, WithSlice,
    utils::{FromToNativeWasmType, IntoResult},
};

/// What an async host function holds across `.await`. It contains no borrow of
/// the store.
///
/// The store is reachable only through [`with`](Self::with), a synchronous
/// closure, so no store borrow can cross an `.await`:
///
/// ```
/// # use wasmer::{AsyncEnv, Function, FunctionEnv, RuntimeError, Store};
/// struct State {
///     offset: i32,
///     last: i32,
/// }
///
/// # async fn lookup(key: i32) -> i32 { key }
/// async fn fetch(cx: &mut AsyncEnv<State>, key: i32) -> Result<i32, RuntimeError> {
///     let offset = cx.with(|env| env.data().offset);
///     // The store is not borrowed here, so other calls can use it.
///     let value = lookup(key + offset).await;
///     cx.with(|mut env| env.data_mut().last = value);
///     Ok(value)
/// }
///
/// let mut store = Store::default();
/// let env = FunctionEnv::new(&mut store, State { offset: 1, last: 0 });
/// let fetch = Function::new_typed_async(&mut store, &env, fetch);
/// ```
///
/// To call a guest and wait for it, pass `cx` to
/// [`Function::call_async`](crate::Function::call_async). A guest called
/// synchronously, with `cx.with(|mut env| f.call(&mut env, &params))`, cannot
/// suspend.
///
/// A clone is another handle to the same environment, for a future that
/// outlives the borrow its function was given: the one of
/// [`Deferred::pending`]. No two handles have the store at once: `with`
/// panics inside another `with`.
pub struct AsyncEnv<T> {
    env: FunctionEnv<T>,
    store_id: StoreId,
    _not_send: PhantomData<*mut ()>,
}

impl<T> Clone for AsyncEnv<T> {
    fn clone(&self) -> Self {
        Self::new(self.env.clone(), self.store_id)
    }
}

/// How a host function made with
/// [`Function::new_typed_async`](crate::Function::new_typed_async) or
/// [`Function::new_async`](crate::Function::new_async) gives the results of
/// a call: the function is `async`, and its call is the future it returns.
///
/// This and [`ReturnsDeferred`] are the last type parameter of
/// [`AsyncHostFunction`] and [`AsyncDynamicHostFunction`], which the compiler
/// infers from the function.
#[derive(Debug, Clone, Copy)]
pub struct ReturnsFuture;

/// How a host function made with
/// [`Function::new_typed_async`](crate::Function::new_typed_async) or
/// [`Function::new_async`](crate::Function::new_async) gives the results of
/// a call: the function returns a [`Deferred`], the results or a future for
/// them. See [`ReturnsFuture`].
#[derive(Debug, Clone, Copy)]
pub struct ReturnsDeferred;

impl<T> AsyncEnv<T> {
    pub(crate) fn new(env: FunctionEnv<T>, store_id: StoreId) -> Self {
        Self {
            env,
            store_id,
            _not_send: PhantomData,
        }
    }

    pub(crate) fn store_id(&self) -> StoreId {
        self.store_id
    }

    /// Borrows the store for one synchronous slice of a poll.
    ///
    /// # Panics
    /// Panics outside a poll of the call this host function belongs to: for
    /// example from a destructor that runs after the call was dropped inside
    /// another slice of a shared store. A destructor uses
    /// [`try_with`](Self::try_with), which fails there instead.
    pub fn with<R>(&mut self, f: impl FnOnce(FunctionEnvMut<'_, T>) -> R) -> R {
        self.in_slice(WithSlice::enter(self.store_id), f)
    }

    /// [`with`](Self::with), for where the store may be busy: it fails with
    /// [`StoreBusy`] instead of panicking if the store is a shared one that
    /// is already running a slice on this thread.
    ///
    /// That is where the destructor of a host future can find itself. A call
    /// drops its host future with the store in reach, but a task of a
    /// [`SharedStore`](crate::SharedStore) can be dropped from inside a call
    /// of its store, for example by the import that ends the guest. The
    /// frames below hold the store then, and there is nothing to wait for:
    /// what still needs the store is left for when they have returned.
    ///
    /// ```
    /// # use std::cell::RefCell;
    /// # use wasmer::AsyncEnv;
    /// # struct State;
    /// # impl State {
    /// #     fn close(&mut self, _id: u32) {}
    /// # }
    /// # thread_local! {
    /// #     static CLOSE_LATER: RefCell<Vec<u32>> = const { RefCell::new(Vec::new()) };
    /// # }
    /// /// What a host function holds while it waits.
    /// struct Connection<'a> {
    ///     cx: &'a mut AsyncEnv<State>,
    ///     id: u32,
    /// }
    ///
    /// impl Drop for Connection<'_> {
    ///     fn drop(&mut self) {
    ///         let id = self.id;
    ///         if self.cx.try_with(|mut env| env.data_mut().close(id)).is_err() {
    ///             // For the embedder to close, once its call has returned.
    ///             CLOSE_LATER.with_borrow_mut(|ids| ids.push(id));
    ///         }
    ///     }
    /// }
    /// ```
    ///
    /// # Panics
    /// Panics, as `with` does, if no call on this environment's store is
    /// being polled or dropped on this thread, and inside another `with`. The
    /// future of a host function does not get there through the `AsyncEnv`
    /// it was given: it is only polled and dropped by its own call, which
    /// publishes the store for it. A handle that outlives that borrow can: a
    /// clone, or the one of
    /// [`FunctionEnvMut::async_env`](crate::FunctionEnvMut::async_env).
    pub fn try_with<R>(
        &mut self,
        f: impl FnOnce(FunctionEnvMut<'_, T>) -> R,
    ) -> Result<R, StoreBusy> {
        let slice = WithSlice::try_enter(self.store_id).ok_or(StoreBusy)?;
        Ok(self.in_slice(slice, f))
    }

    /// [`with`](Self::with), for code that cannot tell whether the store is
    /// in its reach: it returns `None` wherever `with` panics.
    ///
    /// That is code a host function reaches through frames that carry no
    /// reference, such as a callback of an embedded engine. It keeps a
    /// handle of its own, the one of
    /// [`FunctionEnv::async_env`](crate::FunctionEnv::async_env) or a clone,
    /// and what is beneath it on the stack decides whether the store can be
    /// had. It can while an async host function of the store is being
    /// polled on this thread and holds no slice. It cannot while
    ///
    /// - no call on the store is being polled on this thread, or the call
    ///   that is being polled is on another store;
    /// - a guest of the store runs, also in a synchronous host function of
    ///   that guest, which has the store itself;
    /// - a slice has the store: a `with`, or one of a
    ///   [`SharedStore`](crate::SharedStore).
    ///
    /// Where this returns `Some`, the same code can call a guest and wait
    /// for it: it passes this handle to
    /// [`Function::call_async`](crate::Function::call_async), and polls the
    /// call there.
    ///
    /// ```
    /// # use wasmer::{FunctionEnv, Store};
    /// # #[derive(Default)]
    /// # struct State {
    /// #     events: Vec<u32>,
    /// # }
    /// /// Called by an embedded engine, with nothing of the store: from
    /// /// inside a host function of any kind, or from none. Returns whether
    /// /// the event was noted.
    /// fn on_event(env: &FunctionEnv<State>, event: u32) -> bool {
    ///     env.async_env()
    ///         .with_if_in_reach(|mut env| env.data_mut().events.push(event))
    ///         .is_some()
    /// }
    ///
    /// let mut store = Store::default();
    /// let env = FunctionEnv::new(&mut store, State::default());
    /// // No call is being polled here: the store is out of reach.
    /// assert!(!on_event(&env, 1));
    /// ```
    pub fn with_if_in_reach<R>(&mut self, f: impl FnOnce(FunctionEnvMut<'_, T>) -> R) -> Option<R> {
        let slice = WithSlice::enter_if_in_reach(self.store_id)?;
        Some(self.in_slice(slice, f))
    }

    /// Runs `f` with the store, which `slice` holds until `f` has returned.
    fn in_slice<R>(&self, slice: WithSlice, f: impl FnOnce(FunctionEnvMut<'_, T>) -> R) -> R {
        // Safety: the slice is the only holder of the store until it drops,
        // after `f`, and `f` cannot keep a borrow it was given.
        let store = unsafe { StoreMut::from_raw(slice.ptr()) };
        f(self.env.0.on_store(store))
    }
}

mod sealed {
    /// On `sys`, a typed async host function is a static function, and the
    /// backend has the trampolines of every one, of either kind.
    #[cfg(feature = "sys")]
    #[allow(private_bounds)]
    pub trait Sealed<T, Args, Rets, Kind>:
        crate::backend::sys::function::AsyncHostTrampolines<T, Args, Rets, Kind>
    {
    }

    #[cfg(not(feature = "sys"))]
    pub trait Sealed<T, Args, Rets, Kind> {}

    /// The functions [`Function::new_async`](crate::Function::new_async)
    /// takes.
    pub trait SealedDynamic<T, Kind> {}
}

/// A function usable as a typed async host function: what
/// [`Function::new_typed_async`](crate::Function::new_typed_async) takes.
///
/// Implemented for two kinds of function, where `Rets` may be a `Result`:
///
/// - `for<'e> AsyncFn(&'e mut AsyncEnv<T>, Args..) -> Rets`, an `async`
///   function ([`ReturnsFuture`]);
/// - `for<'e> Fn(&'e mut AsyncEnv<T>, Args..) -> Deferred<Rets>`, a function
///   that gives its results at once or a future for them
///   ([`ReturnsDeferred`]).
///
/// The trait is sealed.
pub trait AsyncHostFunction<T, Args, Rets, Kind = ReturnsFuture>:
    sealed::Sealed<T, Args, Rets, Kind> + 'static
where
    Args: WasmTypeList,
    Rets: WasmTypeList,
{
    #[doc(hidden)]
    fn invoke(
        &self,
        env: &mut AsyncEnv<T>,
        args: Args,
    ) -> impl Future<Output = Result<Rets, RuntimeError>>;
}

macro_rules! impl_async_host_function {
    ( $( $x:ident ),* ) => {
        // A single argument is `(A1)`, which is `A1` itself.
        #[allow(unused_parens)]
        impl<$( $x, )* Rets, RetsAsResult, T, F>
            sealed::Sealed<T, ( $( $x ),* ), Rets, ReturnsFuture> for F
        where
            T: 'static,
            F: for<'e> AsyncFn(&'e mut AsyncEnv<T>, $( $x ),*) -> RetsAsResult + 'static,
            RetsAsResult: IntoResult<Rets> + 'static,
            Rets: WasmTypeList + 'static,
            ( $( $x ),* ): WasmTypeList + 'static,
            $( $x: FromToNativeWasmType + 'static, )*
        {
        }

        // A single argument is `(A1)`, which is `A1` itself.
        #[allow(unused_parens)]
        impl<$( $x, )* Rets, RetsAsResult, T, F>
            sealed::Sealed<T, ( $( $x ),* ), Rets, ReturnsDeferred> for F
        where
            T: 'static,
            F: for<'e> Fn(&'e mut AsyncEnv<T>, $( $x ),*) -> Deferred<RetsAsResult> + 'static,
            RetsAsResult: IntoResult<Rets> + 'static,
            Rets: WasmTypeList + 'static,
            ( $( $x ),* ): WasmTypeList + 'static,
            $( $x: FromToNativeWasmType + 'static, )*
        {
        }

        // A single argument is `(A1)`, which is `A1` itself.
        #[allow(unused_parens)]
        impl<$( $x, )* Rets, RetsAsResult, T, F>
            AsyncHostFunction<T, ( $( $x ),* ), Rets, ReturnsDeferred> for F
        where
            T: 'static,
            F: for<'e> Fn(&'e mut AsyncEnv<T>, $( $x ),*) -> Deferred<RetsAsResult> + 'static,
            RetsAsResult: IntoResult<Rets> + 'static,
            Rets: WasmTypeList + 'static,
            ( $( $x ),* ): WasmTypeList + 'static,
            $( $x: FromToNativeWasmType + 'static, )*
        {
            // The function runs in the first poll, where it has the store.
            // On `sys` its trampoline calls it instead, without a future.
            #[allow(non_snake_case)]
            fn invoke(
                &self,
                env: &mut AsyncEnv<T>,
                args: ( $( $x ),* ),
            ) -> impl Future<Output = Result<Rets, RuntimeError>> {
                let ( $( $x ),* ) = args;
                async move {
                    self(env, $( $x ),*)
                        .into_results()
                        .await
                        .into_result()
                        .map_err(|err| RuntimeError::from_dyn(Box::new(err)))
                }
            }
        }

        // A single argument is `(A1)`, which is `A1` itself.
        #[allow(unused_parens)]
        impl<$( $x, )* Rets, RetsAsResult, T, F>
            AsyncHostFunction<T, ( $( $x ),* ), Rets, ReturnsFuture> for F
        where
            T: 'static,
            F: for<'e> AsyncFn(&'e mut AsyncEnv<T>, $( $x ),*) -> RetsAsResult + 'static,
            RetsAsResult: IntoResult<Rets> + 'static,
            Rets: WasmTypeList + 'static,
            ( $( $x ),* ): WasmTypeList + 'static,
            $( $x: FromToNativeWasmType + 'static, )*
        {
            #[allow(non_snake_case)]
            fn invoke(
                &self,
                env: &mut AsyncEnv<T>,
                args: ( $( $x ),* ),
            ) -> impl Future<Output = Result<Rets, RuntimeError>> {
                let ( $( $x ),* ) = args;
                async move {
                    self(env, $( $x ),*)
                        .await
                        .into_result()
                        .map_err(|err| RuntimeError::from_dyn(Box::new(err)))
                }
            }
        }
    };
}

impl_async_host_function!();
impl_async_host_function!(A1);
impl_async_host_function!(A1, A2);
impl_async_host_function!(A1, A2, A3);
impl_async_host_function!(A1, A2, A3, A4);
impl_async_host_function!(A1, A2, A3, A4, A5);
impl_async_host_function!(A1, A2, A3, A4, A5, A6);
impl_async_host_function!(A1, A2, A3, A4, A5, A6, A7);
impl_async_host_function!(A1, A2, A3, A4, A5, A6, A7, A8);
impl_async_host_function!(A1, A2, A3, A4, A5, A6, A7, A8, A9);
impl_async_host_function!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10);
impl_async_host_function!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11);
impl_async_host_function!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12);
impl_async_host_function!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13);
impl_async_host_function!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14);
impl_async_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15
);
impl_async_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16
);
impl_async_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17
);
impl_async_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18
);
impl_async_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19
);
impl_async_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20
);
impl_async_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21
);
impl_async_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21,
    A22
);
impl_async_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21,
    A22, A23
);
impl_async_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21,
    A22, A23, A24
);
impl_async_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21,
    A22, A23, A24, A25
);
impl_async_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21,
    A22, A23, A24, A25, A26
);

/// A function usable as a dynamic async host function: what
/// [`Function::new_async`](crate::Function::new_async) takes.
///
/// Implemented for two kinds of function:
///
/// - `for<'e> AsyncFn(&'e mut AsyncEnv<T>, &'e [Value]) -> Result<Vec<Value>,
///   RuntimeError>`, an `async` function ([`ReturnsFuture`]);
/// - `for<'e> Fn(&'e mut AsyncEnv<T>, &'e [Value]) ->
///   Deferred<Result<Vec<Value>, RuntimeError>>`, a function that gives its
///   results at once or a future for them ([`ReturnsDeferred`]).
///
/// The trait is sealed.
pub trait AsyncDynamicHostFunction<T, Kind = ReturnsFuture>:
    sealed::SealedDynamic<T, Kind> + 'static
{
    #[doc(hidden)]
    fn invoke<'e>(
        &'e self,
        env: &'e mut AsyncEnv<T>,
        values: &'e [Value],
    ) -> impl Future<Output = Result<Vec<Value>, RuntimeError>> + 'e;
}

impl<T, F> sealed::SealedDynamic<T, ReturnsFuture> for F
where
    T: 'static,
    F: for<'e> AsyncFn(&'e mut AsyncEnv<T>, &'e [Value]) -> Result<Vec<Value>, RuntimeError>
        + 'static,
{
}

impl<T, F> AsyncDynamicHostFunction<T, ReturnsFuture> for F
where
    T: 'static,
    F: for<'e> AsyncFn(&'e mut AsyncEnv<T>, &'e [Value]) -> Result<Vec<Value>, RuntimeError>
        + 'static,
{
    fn invoke<'e>(
        &'e self,
        env: &'e mut AsyncEnv<T>,
        values: &'e [Value],
    ) -> impl Future<Output = Result<Vec<Value>, RuntimeError>> + 'e {
        self(env, values)
    }
}

impl<T, F> sealed::SealedDynamic<T, ReturnsDeferred> for F
where
    T: 'static,
    F: for<'e> Fn(&'e mut AsyncEnv<T>, &'e [Value]) -> Deferred<Result<Vec<Value>, RuntimeError>>
        + 'static,
{
}

impl<T, F> AsyncDynamicHostFunction<T, ReturnsDeferred> for F
where
    T: 'static,
    F: for<'e> Fn(&'e mut AsyncEnv<T>, &'e [Value]) -> Deferred<Result<Vec<Value>, RuntimeError>>
        + 'static,
{
    // Called in the first poll of the call, where the function has the
    // store. A dynamic function is called through a future on every backend:
    // one that is ready at once does not skip it, as a typed one does.
    fn invoke<'e>(
        &'e self,
        env: &'e mut AsyncEnv<T>,
        values: &'e [Value],
    ) -> impl Future<Output = Result<Vec<Value>, RuntimeError>> + 'e {
        self(env, values).into_results()
    }
}

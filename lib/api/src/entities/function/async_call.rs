//! The future of an async call, and what can make one.

use std::{
    future::Future,
    marker::PhantomData,
    pin::Pin,
    task::{Context, Poll},
};

use wasmer_types::RawValue;

#[cfg(feature = "sys")]
use crate::WithSlice;
use crate::{
    AsStoreMut, AsyncEnv, BackendFunction, CallStore, Function, RuntimeError, SharedStore,
    StoreInner, StoreMut, Value, WasmTypeList,
};

/// The future of every async call.
///
/// `'a` is the exclusive borrow of the store, or `'static` for a call on a
/// [`SharedStore`]. Dropping the call cancels it. Its guest stack is discarded
/// without running guest or host code.
#[must_use = "an async call does nothing until it is polled"]
pub struct AsyncCall<'a, R> {
    inner: Inner<'a, R>,
}

// The larger variant is the one that is there not to be boxed.
#[allow(clippy::large_enum_variant)]
enum Inner<'a, R> {
    Boxed(Pin<Box<dyn Future<Output = R> + 'a>>),
    /// A typed call on the native backend, which keeps nothing on the heap.
    #[cfg(feature = "sys")]
    Typed(sys::TypedCall<'a, R>),
}

#[cfg(feature = "sys")]
mod sys {
    pub(super) use crate::backend::sys::{
        async_runtime::{AsyncCallFuture, Converted, Typed},
        entities::function::{Function, RawValues},
    };

    pub(super) type TypedCall<'a, R> = AsyncCallFuture<'a, Typed<R>>;
}

impl<'a, R> AsyncCall<'a, R> {
    pub(crate) fn new(future: impl Future<Output = R> + 'a) -> Self {
        Self {
            inner: Inner::Boxed(Box::pin(future)),
        }
    }
}

impl<R> Future for AsyncCall<'_, R> {
    type Output = R;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<R> {
        // Safety: the call is not moved out of its variant.
        match unsafe { &mut self.get_unchecked_mut().inner } {
            Inner::Boxed(call) => call.as_mut().poll(cx),
            #[cfg(feature = "sys")]
            Inner::Typed(call) => unsafe { Pin::new_unchecked(call) }.poll(cx),
        }
    }
}

impl<R> std::fmt::Debug for AsyncCall<'_, R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AsyncCall")
    }
}

pub(crate) type CallParams = Box<dyn FnOnce(&mut StoreMut<'_>) -> Result<Vec<Value>, RuntimeError>>;
pub(crate) type CallFinish<X> =
    Box<dyn FnOnce(&mut StoreMut<'_>, &[Value]) -> Result<X, RuntimeError>>;

/// What a typed or dynamic call needs its backend to run.
///
/// The arguments and results are converted in a slice of the call, because
/// that is where the store can be reached.
#[doc(hidden)]
pub struct AsyncCallRequest<X> {
    pub(crate) function: Function,
    pub(crate) params: CallParams,
    pub(crate) finish: CallFinish<X>,
}

impl AsyncCallRequest<Box<[Value]>> {
    pub(crate) fn dynamic(function: Function, params: &[Value]) -> Self {
        let params = params.to_vec();
        Self {
            function,
            params: Box::new(move |_| Ok(params)),
            finish: Box::new(|_, results| Ok(results.into())),
        }
    }
}

impl<X> AsyncCallRequest<X> {
    /// A typed call, whose arguments `args` makes as raw values.
    pub(crate) fn typed<A: AsRef<[RawValue]>>(
        function: Function,
        args: impl FnOnce(&mut StoreMut<'_>) -> Result<A, RuntimeError> + 'static,
    ) -> Self
    where
        X: WasmTypeList,
    {
        let params_source = function.clone();
        let results_source = function.clone();
        Self {
            function,
            params: Box::new(move |store| {
                let args = args(store)?;
                let ty = params_source.ty(store);
                let params = args.as_ref().iter().zip(ty.params());
                // Safety: each raw value was made from an argument of this
                // function's parameter type.
                Ok(params
                    .map(|(raw, ty)| unsafe { Value::from_raw(store, *ty, *raw) })
                    .collect())
            }),
            finish: Box::new(move |store, results| {
                let ty = results_source.ty(store);
                convert_results::<X>(store, ty.results(), results)
            }),
        }
    }
}

/// Reads typed results from dynamic ones, refusing values of another type.
pub(crate) fn convert_results<Rets: WasmTypeList>(
    store: &mut impl AsStoreMut,
    types: &[wasmer_types::Type],
    results: &[Value],
) -> Result<Rets, RuntimeError> {
    if results.len() != types.len() {
        return Err(RuntimeError::new("result arity mismatch"));
    }
    let mut raw_array = Rets::empty_array();
    for ((slot, ty), value) in raw_array.as_mut().iter_mut().zip(types).zip(results) {
        // `Rets::from_array` reads each raw value as the field of its type.
        if value.ty() != *ty {
            return Err(RuntimeError::new(format!(
                "a result of type {} where the signature has {ty}",
                value.ty()
            )));
        }
        *slot = value.as_raw(store);
    }
    // Safety: the raw values were read from values of the result types.
    Ok(unsafe { Rets::from_array(store, raw_array) })
}

mod sealed {
    use super::*;

    pub trait Sealed {}
    impl<S: AsStoreMut> Sealed for S {}
    impl<T> Sealed for AsyncEnv<T> {}
    impl Sealed for SharedStore {}
}

/// What [`Function::call_async`] can run on: a store, the [`AsyncEnv`] of an
/// async host function, or a [`SharedStore`].
///
/// The trait is sealed. `Call<'a, R>` is [`AsyncCall<'a, R>`], or
/// `AsyncCall<'static, R>` for a `SharedStore`, whose calls keep their own
/// handle.
pub trait AsyncCallStore: sealed::Sealed {
    /// The future of a call on this store.
    type Call<'a, R: 'static>: Future<Output = R>
    where
        Self: 'a;

    #[doc(hidden)]
    fn start_call<'a, X: 'static>(
        &'a mut self,
        request: AsyncCallRequest<X>,
    ) -> Self::Call<'a, Result<X, RuntimeError>>;

    /// Starts a typed call of `function`, whose arguments `args` makes as raw
    /// values. A store that can be reached now makes them now.
    ///
    /// # Safety
    ///
    /// `args` must make one valid raw value per parameter of `function`, of
    /// its type, and `Rets` must be the function's results. Nothing checks
    /// either: [`TypedFunction`](crate::TypedFunction) is what guarantees them.
    #[doc(hidden)]
    unsafe fn start_typed_call<'a, A, Rets>(
        &'a mut self,
        function: &Function,
        args: impl FnOnce(&mut StoreMut<'_>) -> Result<A, RuntimeError> + 'static,
    ) -> Self::Call<'a, Result<Rets, RuntimeError>>
    where
        A: AsRef<[RawValue]>,
        Rets: WasmTypeList + 'static,
    {
        self.start_call(AsyncCallRequest::typed(function.clone(), args))
    }
}

impl<S: AsStoreMut> AsyncCallStore for S {
    type Call<'a, R: 'static>
        = AsyncCall<'a, R>
    where
        Self: 'a;

    fn start_call<'a, X: 'static>(
        &'a mut self,
        request: AsyncCallRequest<X>,
    ) -> AsyncCall<'a, Result<X, RuntimeError>> {
        start(exclusive(self.as_store_mut()), request)
    }

    unsafe fn start_typed_call<'a, A, Rets>(
        &'a mut self,
        function: &Function,
        args: impl FnOnce(&mut StoreMut<'_>) -> Result<A, RuntimeError> + 'static,
    ) -> AsyncCall<'a, Result<Rets, RuntimeError>>
    where
        A: AsRef<[RawValue]>,
        Rets: WasmTypeList + 'static,
    {
        match &function.0 {
            #[cfg(feature = "sys")]
            BackendFunction::Sys(sys) => {
                let mut store: StoreMut<'a> = self.as_store_mut();
                let args = args(&mut store);
                typed(exclusive(store), sys, args)
            }
            #[allow(unreachable_patterns)]
            _ => self.start_call(AsyncCallRequest::typed(function.clone(), args)),
        }
    }
}

/// The store of a call that holds `&'a mut` of it.
fn exclusive<'a>(store: StoreMut<'a>) -> CallStore<'a> {
    // `AsStoreMut` is a public trait, and a later call could return another
    // store: the pointer is derived here, once, and `'a` keeps the borrow it
    // comes from alive and unreachable.
    let id = store.inner.objects.id();
    let ptr: *mut StoreInner = store.inner;
    CallStore::Exclusive {
        id,
        ptr,
        _borrow: PhantomData,
    }
}

impl<T> AsyncCallStore for AsyncEnv<T> {
    type Call<'a, R: 'static>
        = AsyncCall<'a, R>
    where
        Self: 'a;

    fn start_call<'a, X: 'static>(
        &'a mut self,
        request: AsyncCallRequest<X>,
    ) -> AsyncCall<'a, Result<X, RuntimeError>> {
        start(
            CallStore::Env {
                id: self.store_id(),
                _borrow: PhantomData,
            },
            request,
        )
    }

    unsafe fn start_typed_call<'a, A, Rets>(
        &'a mut self,
        function: &Function,
        args: impl FnOnce(&mut StoreMut<'_>) -> Result<A, RuntimeError> + 'static,
    ) -> AsyncCall<'a, Result<Rets, RuntimeError>>
    where
        A: AsRef<[RawValue]>,
        Rets: WasmTypeList + 'static,
    {
        // Where the store is in reach, the arguments are made now: the host
        // function that makes the call is being polled, and holds no slice.
        // Elsewhere, in a destructor or inside a slice, they are left for
        // the first poll of the call, as those of a dynamic call are.
        #[cfg(feature = "sys")]
        if let BackendFunction::Sys(sys) = &function.0
            && let Some(slice) = WithSlice::enter_if_in_reach(self.store_id())
        {
            // Safety: the slice is the only holder of the store.
            let args = args(&mut unsafe { StoreMut::from_raw(slice.ptr()) });
            drop(slice);
            let store = CallStore::Env {
                id: self.store_id(),
                _borrow: PhantomData,
            };
            return typed(store, sys, args);
        }
        self.start_call(AsyncCallRequest::typed(function.clone(), args))
    }
}

impl AsyncCallStore for SharedStore {
    type Call<'a, R: 'static>
        = AsyncCall<'static, R>
    where
        Self: 'a;

    fn start_call<X: 'static>(
        &mut self,
        request: AsyncCallRequest<X>,
    ) -> AsyncCall<'static, Result<X, RuntimeError>> {
        start(CallStore::Shared(self.clone()), request)
    }
}

fn start<'a, X: 'static>(
    store: CallStore<'a>,
    request: AsyncCallRequest<X>,
) -> AsyncCall<'a, Result<X, RuntimeError>> {
    match &request.function.0 {
        #[cfg(feature = "sys")]
        BackendFunction::Sys(_) => AsyncCall::new(sys::AsyncCallFuture::new(
            store,
            sys::Converted::new(request),
            sys::RawValues::new(),
        )),
        #[cfg(feature = "v8")]
        BackendFunction::V8(_) => {
            AsyncCall::new(crate::backend::v8::async_call::call(store, request))
        }
        #[cfg(feature = "js")]
        BackendFunction::Js(_) => AsyncCall::new(crate::backend::js::jspi::call(store, request)),
    }
}

/// A typed call of a native function, whose arguments are raw values already:
/// its future is the call itself, with nothing on the heap.
#[cfg(feature = "sys")]
fn typed<'a, A: AsRef<[RawValue]>, Rets: WasmTypeList + 'static>(
    store: CallStore<'a>,
    function: &sys::Function,
    args: Result<A, RuntimeError>,
) -> AsyncCall<'a, Result<Rets, RuntimeError>> {
    let (values, refused) = match args {
        Ok(args) => {
            // With room for the results, which the trampoline writes over them.
            let args = args.as_ref();
            let mut values = sys::RawValues::from_slice(args);
            values.resize(args.len().max(Rets::size() as usize), RawValue { i32: 0 });
            (values, None)
        }
        Err(refused) => (sys::RawValues::new(), Some(refused)),
    };
    let entry = sys::Typed::new(function.clone(), refused, typed_results::<Rets>);
    AsyncCall {
        inner: Inner::Typed(sys::AsyncCallFuture::new(store, entry, values)),
    }
}

/// Reads the typed results of a guest that returned from its values, of which
/// they are the first.
#[cfg(feature = "sys")]
fn typed_results<Rets: WasmTypeList>(
    outcome: Result<(&mut StoreMut<'_>, &[RawValue]), RuntimeError>,
) -> Result<Rets, RuntimeError> {
    let (store, values) = outcome?;
    let mut array = Rets::empty_array();
    let results = array.as_mut();
    results.copy_from_slice(&values[..results.len()]);
    // Safety: the function has the results of its `TypedFunction`, which the
    // trampoline wrote over the first values.
    Ok(unsafe { Rets::from_array(store, array) })
}

/// The misuses the type contract rejects at compile time.
///
/// A second call while one is live:
///
/// ```compile_fail,E0499
/// # use wasmer::*;
/// # fn f(mut store: Store, f: Function) {
/// let first = f.call_async(&mut store, &[]);
/// let second = f.call_async(&mut store, &[]);
/// drop((first, second));
/// # }
/// ```
///
/// Using the store while a call is live:
///
/// ```compile_fail,E0499
/// # use wasmer::*;
/// # fn f(mut store: Store, f: Function) {
/// let call = f.call_async(&mut store, &[]);
/// let _ = f.call(&mut store, &[]);
/// drop(call);
/// # }
/// ```
///
/// Dropping the store while a call is live:
///
/// ```compile_fail,E0505
/// # use wasmer::*;
/// # fn f(mut store: Store, f: Function) {
/// let call = f.call_async(&mut store, &[]);
/// drop(store);
/// drop(call);
/// # }
/// ```
///
/// Sending a call to another thread:
///
/// ```compile_fail,E0277
/// # use wasmer::*;
/// # fn f(mut store: Store, f: Function) {
/// fn send<T: Send>(_: T) {}
/// send(f.call_async(&mut store, &[]));
/// # }
/// ```
///
/// Moving a store to another thread than the one that made it:
///
/// ```compile_fail,E0277
/// # use wasmer::*;
/// # fn f(store: Store) {
/// fn send<T: Send>(_: T) {}
/// send(store);
/// # }
/// ```
///
/// Moving a handle of a shared store to another thread:
///
/// ```compile_fail,E0277
/// # use wasmer::*;
/// # fn f(shared: SharedStore) {
/// fn send<T: Send>(_: T) {}
/// send(shared);
/// # }
/// ```
///
/// Moving a weak handle of a shared store to another thread:
///
/// ```compile_fail,E0277
/// # use wasmer::*;
/// # fn f(weak: WeakSharedStore) {
/// fn send<T: Send>(_: T) {}
/// send(weak);
/// # }
/// ```
///
/// Holding a store borrow across an `.await` in a host function:
///
/// ```compile_fail,E0728
/// # use wasmer::*;
/// async fn host(cx: &mut AsyncEnv<()>) {
///     cx.with(|_env| { std::future::ready(()).await; });
/// }
/// ```
///
/// Keeping the store past a `with` slice:
///
/// ```compile_fail
/// # use wasmer::*;
/// async fn host(cx: &mut AsyncEnv<()>) {
///     let env = cx.with(|env| env);
///     std::future::ready(()).await;
///     drop(env);
/// }
/// ```
///
/// Two overlapping nested calls through one `AsyncEnv`:
///
/// ```compile_fail,E0499
/// # use wasmer::*;
/// async fn host(cx: &mut AsyncEnv<()>, f: Function) {
///     let first = f.call_async(cx, &[]);
///     let second = f.call_async(cx, &[]);
///     drop((first, second));
/// }
/// ```
///
/// Reaching the host state through the `StoreRef` of `data_and_store`:
///
/// ```compile_fail,E0277
/// # use wasmer::*;
/// fn host(mut env: FunctionEnvMut<'_, i32>, handle: FunctionEnv<i32>) {
///     let (data, store) = env.data_and_store();
///     let _alias = handle.as_ref(&store);
///     *data += 1;
/// }
/// ```
///
/// A call on a shared store borrows nothing, so it can outlive the handle:
///
/// ```
/// # use wasmer::*;
/// fn task(mut shared: SharedStore, f: Function) -> impl std::future::Future + 'static {
///     f.call_async(&mut shared, &[])
/// }
/// ```
#[doc(hidden)]
#[allow(dead_code)]
pub(crate) const COMPILE_FAIL_CONTRACT: () = ();

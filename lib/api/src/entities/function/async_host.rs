//! Async host functions: [`AsyncEnv`] and [`AsyncHostFunction`].

use std::{future::Future, marker::PhantomData};
#[cfg(any(feature = "js", feature = "v8"))]
use std::{pin::Pin, rc::Rc};

#[cfg(any(feature = "js", feature = "v8"))]
use wasmer_types::FunctionType;
use wasmer_types::StoreId;

#[cfg(any(feature = "js", feature = "v8"))]
use crate::{AsStoreMut, Value};
use crate::{
    FunctionEnv, FunctionEnvMut, RuntimeError, StoreMut, WasmTypeList, WithSlice,
    utils::{FromToNativeWasmType, IntoResult},
};

/// The future of one call of an async host function, for a backend whose
/// host functions take and return [`Value`]s.
#[cfg(any(feature = "js", feature = "v8"))]
pub(crate) type HostFuture =
    Pin<Box<dyn Future<Output = Result<Vec<Value>, RuntimeError>> + 'static>>;

/// What an async host function holds across `.await`. It contains no borrow of
/// the store.
///
/// The store is reachable only through [`with`](Self::with), a synchronous
/// closure, so no store borrow can cross an `.await`:
///
/// ```ignore
/// async fn fetch(cx: &mut AsyncEnv<State>, key: i32) -> Result<i32, RuntimeError> {
///     let client = cx.with(|env| env.data().client.clone());
///     let value = client.get(key).await?;
///     cx.with(|mut env| env.data_mut().last = value);
///     Ok(value)
/// }
/// ```
///
/// To call a guest and wait for it, pass `cx` to
/// [`Function::call_async`](crate::Function::call_async). A guest called
/// synchronously, with `cx.with(|mut env| f.call(&mut env, &params))`, cannot
/// suspend.
pub struct AsyncEnv<T> {
    env: FunctionEnv<T>,
    store_id: StoreId,
    _not_send: PhantomData<*mut ()>,
}

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
    /// another slice of a shared store.
    pub fn with<R>(&mut self, f: impl FnOnce(FunctionEnvMut<'_, T>) -> R) -> R {
        let slice = WithSlice::enter(self.store_id);
        // Safety: the slice is the only holder of the store until it drops,
        // after `f`, and `f` cannot keep a borrow it was given.
        let store = unsafe { StoreMut::from_raw(slice.ptr()) };
        f(self.env.0.on_store(store))
    }
}

mod sealed {
    /// On `sys`, a typed async host function is a static function, and the
    /// backend has the trampolines of every one.
    #[cfg(feature = "sys")]
    #[allow(private_bounds)]
    pub trait Sealed<T, Args, Rets>:
        crate::backend::sys::function::AsyncHostTrampolines<T, Args, Rets>
    {
    }

    #[cfg(not(feature = "sys"))]
    pub trait Sealed<T, Args, Rets> {}
}

/// An async function usable as a typed host function.
///
/// Implemented for `for<'e> AsyncFn(&'e mut AsyncEnv<T>, Args..) -> Rets`,
/// where `Rets` may be a `Result`. The trait is sealed.
pub trait AsyncHostFunction<T, Args, Rets>: sealed::Sealed<T, Args, Rets> + 'static
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
        impl<$( $x, )* Rets, RetsAsResult, T, F> sealed::Sealed<T, ( $( $x ),* ), Rets> for F
        where
            T: 'static,
            F: for<'e> AsyncFn(&'e mut AsyncEnv<T>, $( $x ),*) -> RetsAsResult + 'static,
            RetsAsResult: IntoResult<Rets> + 'static,
            Rets: WasmTypeList + 'static,
            ( $( $x ),* ): WasmTypeList + 'static,
            $( $x: FromToNativeWasmType + 'static, )*
        {
        }

        impl<$( $x, )* Rets, RetsAsResult, T, F> AsyncHostFunction<T, ( $( $x ),* ), Rets> for F
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

/// Boxes the futures of the body of an async host function, for a backend
/// that keeps them as [`HostFuture`]s.
#[cfg(any(feature = "js", feature = "v8"))]
pub(crate) fn boxed_body<T, F, Fut>(body: F) -> impl Fn(AsyncEnv<T>, Vec<Value>) -> HostFuture
where
    F: Fn(AsyncEnv<T>, Vec<Value>) -> Fut,
    Fut: Future<Output = Result<Vec<Value>, RuntimeError>> + 'static,
{
    move |cx, values| -> HostFuture { Box::pin(body(cx, values)) }
}

/// Turns a typed async host function into the body of a dynamic one, for a
/// backend whose host functions take and return [`Value`]s.
#[cfg(any(feature = "js", feature = "v8"))]
pub(crate) fn typed_host_body<T, F, Args, Rets>(
    func: F,
    ty: FunctionType,
) -> impl Fn(AsyncEnv<T>, Vec<Value>) -> HostFuture + 'static
where
    T: 'static,
    F: AsyncHostFunction<T, Args, Rets>,
    Args: WasmTypeList + 'static,
    Rets: WasmTypeList + 'static,
{
    let func = Rc::new(func);
    let ty = Rc::new(ty);
    move |mut cx, values| -> HostFuture {
        let func = Rc::clone(&func);
        let ty = Rc::clone(&ty);
        Box::pin(async move {
            let args = cx.with(|mut env| args_from_values::<Args>(&mut env, &ty, &values))?;
            let rets = func.invoke(&mut cx, args).await?;
            Ok(cx.with(|mut env| results_to_values::<Rets>(&mut env, &ty, rets)))
        })
    }
}

#[cfg(any(feature = "js", feature = "v8"))]
fn args_from_values<Args: WasmTypeList>(
    store: &mut impl AsStoreMut,
    ty: &FunctionType,
    values: &[Value],
) -> Result<Args, RuntimeError> {
    if values.len() != ty.params().len() {
        return Err(RuntimeError::new(
            "typed host function received wrong number of parameters",
        ));
    }
    let mut raw_array = Args::empty_array();
    for ((slot, value), expected) in raw_array.as_mut().iter_mut().zip(values).zip(ty.params()) {
        debug_assert_eq!(
            value.ty(),
            *expected,
            "wasm should only call host functions with matching signatures"
        );
        *slot = value.as_raw(store);
    }
    // Safety: the raw values were read from values of the parameter types.
    Ok(unsafe { Args::from_array(store, raw_array) })
}

#[cfg(any(feature = "js", feature = "v8"))]
fn results_to_values<Rets: WasmTypeList>(
    store: &mut impl AsStoreMut,
    ty: &FunctionType,
    rets: Rets,
) -> Vec<Value> {
    // Safety: the raw values are read back as the result types of `Rets`.
    let mut raw_array = unsafe { rets.into_array(store) };
    let results = raw_array.as_mut().iter().zip(ty.results());
    results
        .map(|(raw, ty)| unsafe { Value::from_raw(store, *ty, *raw) })
        .collect()
}

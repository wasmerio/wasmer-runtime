//! Host functions that complete at once or have their guest wait:
//! [`Deferred`] and [`DeferredHostFunction`].

use std::{future::Future, pin::Pin};

use crate::{
    FunctionEnvMut, WasmTypeList,
    utils::{FromToNativeWasmType, IntoResult},
};

/// What a host function made with
/// [`Function::new_typed_deferred`](crate::Function::new_typed_deferred)
/// returns: the results of its call, or a future for them.
///
/// The function runs as a sync host function does, with its
/// [`FunctionEnvMut`]. If it returns [`Deferred::ready`], its call is over,
/// and costs what a call of a sync host function costs. If it returns
/// [`Deferred::pending`], the guest waits for the future, as it does in an
/// async host function: the future reaches the store with
/// [`AsyncEnv::with`](crate::AsyncEnv::with), through the
/// [`AsyncEnv`](crate::AsyncEnv) the function took with
/// [`FunctionEnvMut::async_env`].
///
/// ```ignore
/// fn read(mut env: FunctionEnvMut<State>, fd: i32) -> Deferred<i32> {
///     if let Some(byte) = env.data_mut().buffered(fd) {
///         return Deferred::ready(byte);
///     }
///     let mut cx = env.async_env();
///     Deferred::pending(async move {
///         let byte = receive(fd).await;
///         cx.with(|mut env| env.data_mut().count(fd));
///         byte
///     })
/// }
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
}

mod sealed {
    /// On `sys`, a host function that returns [`Deferred`](super::Deferred)
    /// is a static function, and the backend has the trampolines of every
    /// one.
    #[cfg(feature = "sys")]
    #[allow(private_bounds)]
    pub trait Sealed<T, Args, Rets>:
        crate::backend::sys::function::DeferredHostTrampolines<T, Args, Rets>
    {
    }

    #[cfg(not(feature = "sys"))]
    pub trait Sealed<T, Args, Rets> {}
}

/// A function usable as a typed host function that returns [`Deferred`].
///
/// Implemented for `Fn(FunctionEnvMut<T>, Args..) -> Deferred<Rets>`, where
/// `Rets` may be a `Result`. The trait is sealed.
pub trait DeferredHostFunction<T, Args, Rets>: sealed::Sealed<T, Args, Rets> + 'static
where
    Args: WasmTypeList,
    Rets: WasmTypeList,
{
    /// What the function gives, at once or later: the results, or a `Result`
    /// of them.
    #[doc(hidden)]
    type Output: IntoResult<Rets> + 'static;

    #[doc(hidden)]
    fn invoke(&self, env: FunctionEnvMut<'_, T>, args: Args) -> Deferred<Self::Output>;
}

macro_rules! impl_deferred_host_function {
    ( $( $x:ident ),* ) => {
        impl<$( $x, )* Rets, RetsAsResult, T, F> sealed::Sealed<T, ( $( $x ),* ), Rets> for F
        where
            T: Send + 'static,
            F: Fn(FunctionEnvMut<'_, T>, $( $x ),*) -> Deferred<RetsAsResult> + 'static,
            RetsAsResult: IntoResult<Rets> + 'static,
            Rets: WasmTypeList + 'static,
            ( $( $x ),* ): WasmTypeList + 'static,
            $( $x: FromToNativeWasmType + 'static, )*
        {
        }

        impl<$( $x, )* Rets, RetsAsResult, T, F> DeferredHostFunction<T, ( $( $x ),* ), Rets> for F
        where
            T: Send + 'static,
            F: Fn(FunctionEnvMut<'_, T>, $( $x ),*) -> Deferred<RetsAsResult> + 'static,
            RetsAsResult: IntoResult<Rets> + 'static,
            Rets: WasmTypeList + 'static,
            ( $( $x ),* ): WasmTypeList + 'static,
            $( $x: FromToNativeWasmType + 'static, )*
        {
            type Output = RetsAsResult;

            #[allow(non_snake_case)]
            fn invoke(
                &self,
                env: FunctionEnvMut<'_, T>,
                args: ( $( $x ),* ),
            ) -> Deferred<RetsAsResult> {
                let ( $( $x ),* ) = args;
                self(env, $( $x ),*)
            }
        }
    };
}

impl_deferred_host_function!();
impl_deferred_host_function!(A1);
impl_deferred_host_function!(A1, A2);
impl_deferred_host_function!(A1, A2, A3);
impl_deferred_host_function!(A1, A2, A3, A4);
impl_deferred_host_function!(A1, A2, A3, A4, A5);
impl_deferred_host_function!(A1, A2, A3, A4, A5, A6);
impl_deferred_host_function!(A1, A2, A3, A4, A5, A6, A7);
impl_deferred_host_function!(A1, A2, A3, A4, A5, A6, A7, A8);
impl_deferred_host_function!(A1, A2, A3, A4, A5, A6, A7, A8, A9);
impl_deferred_host_function!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10);
impl_deferred_host_function!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11);
impl_deferred_host_function!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12);
impl_deferred_host_function!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13);
impl_deferred_host_function!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14);
impl_deferred_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15
);
impl_deferred_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16
);
impl_deferred_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17
);
impl_deferred_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18
);
impl_deferred_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19
);
impl_deferred_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20
);
impl_deferred_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21
);
impl_deferred_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21,
    A22
);
impl_deferred_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21,
    A22, A23
);
impl_deferred_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21,
    A22, A23, A24
);
impl_deferred_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21,
    A22, A23, A24, A25
);
impl_deferred_host_function!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21,
    A22, A23, A24, A25, A26
);

/// Turns a host function that returns [`Deferred`] into the body of a
/// dynamic async one, for a backend whose host functions take and return
/// [`Value`](crate::Value)s. The function runs in the first slice of the
/// first poll, where it has the store.
#[cfg(any(feature = "js", feature = "v8"))]
pub(crate) fn deferred_host_body<T, F, Args, Rets>(
    func: F,
    ty: wasmer_types::FunctionType,
) -> impl Fn(crate::AsyncEnv<T>, Vec<crate::Value>) -> super::async_host::HostFuture + 'static
where
    T: Send + 'static,
    F: DeferredHostFunction<T, Args, Rets>,
    Args: WasmTypeList + 'static,
    Rets: WasmTypeList + 'static,
{
    use super::async_host::{HostFuture, args_from_values, results_to_values};
    use crate::RuntimeError;
    use std::rc::Rc;

    let func = Rc::new(func);
    let ty = Rc::new(ty);
    move |mut cx, values| -> HostFuture {
        let func = Rc::clone(&func);
        let ty = Rc::clone(&ty);
        Box::pin(async move {
            let deferred = cx.with(|mut env| {
                let args = args_from_values::<Args>(&mut env, &ty, &values)?;
                Ok::<_, RuntimeError>(func.invoke(env, args))
            })?;
            let rets = match deferred.0 {
                DeferredState::Ready(rets) => rets,
                DeferredState::Pending(future) => future.await,
            };
            let rets = rets
                .into_result()
                .map_err(|err| RuntimeError::from_dyn(Box::new(err)))?;
            Ok(cx.with(|mut env| results_to_values::<Rets>(&mut env, &ty, rets)))
        })
    }
}

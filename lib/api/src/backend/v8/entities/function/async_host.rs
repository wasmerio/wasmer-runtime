//! Async host functions on the V8 backend, whose host functions take
//! and return [`Value`]s: their futures, and how a typed one is called as a dynamic one.

use std::{future::Future, marker::PhantomData, pin::Pin, rc::Rc};

use wasmer_types::FunctionType;

use crate::{AsStoreMut, AsyncEnv, AsyncHostFunction, RuntimeError, Value, WasmTypeList};

/// The future of one call of an async host function.
pub(crate) type HostFuture =
    Pin<Box<dyn Future<Output = Result<Vec<Value>, RuntimeError>> + 'static>>;

/// The call of a typed async host function, with the type of the function
/// erased: what this backend keeps of one.
///
/// Only [`call`](Self::call) is instantiated per host function, and it does
/// no more than box the function's future. The rest of the body, converting
/// arguments and results and awaiting that future, is instantiated once per
/// environment type and signature, at the cost of one more allocation per
/// call.
pub(crate) trait TypedCall<T, Args, Rets> {
    fn call<'a>(
        &'a self,
        cx: &'a mut AsyncEnv<T>,
        args: Args,
    ) -> Pin<Box<dyn Future<Output = Result<Rets, RuntimeError>> + 'a>>;
}

struct Typed<F, Kind>(F, PhantomData<fn() -> Kind>);

impl<T, F, Args, Rets, Kind> TypedCall<T, Args, Rets> for Typed<F, Kind>
where
    F: AsyncHostFunction<T, Args, Rets, Kind>,
    Args: WasmTypeList + 'static,
    Rets: WasmTypeList + 'static,
{
    fn call<'a>(
        &'a self,
        cx: &'a mut AsyncEnv<T>,
        args: Args,
    ) -> Pin<Box<dyn Future<Output = Result<Rets, RuntimeError>> + 'a>> {
        Box::pin(self.0.invoke(cx, args))
    }
}

/// `func`, with its type erased.
pub(crate) fn typed_call<T, F, Args, Rets, Kind>(func: F) -> Rc<dyn TypedCall<T, Args, Rets>>
where
    T: 'static,
    Kind: 'static,
    F: AsyncHostFunction<T, Args, Rets, Kind>,
    Args: WasmTypeList + 'static,
    Rets: WasmTypeList + 'static,
{
    Rc::new(Typed(func, PhantomData))
}

/// Turns a typed async host function into the body of a dynamic one.
pub(crate) fn typed_host_body<T, Args, Rets>(
    func: Rc<dyn TypedCall<T, Args, Rets>>,
    ty: FunctionType,
) -> impl Fn(AsyncEnv<T>, Vec<Value>) -> HostFuture + 'static
where
    T: 'static,
    Args: WasmTypeList + 'static,
    Rets: WasmTypeList + 'static,
{
    let ty = Rc::new(ty);
    move |mut cx, values| -> HostFuture {
        let func = Rc::clone(&func);
        let ty = Rc::clone(&ty);
        Box::pin(async move {
            let args = typed_args::<T, Args>(&mut cx, &ty, &values)?;
            let rets = func.call(&mut cx, args).await?;
            typed_results::<T, Rets>(&mut cx, &ty, rets)
        })
    }
}

#[inline(never)]
fn typed_args<T, Args: WasmTypeList>(
    cx: &mut AsyncEnv<T>,
    ty: &FunctionType,
    values: &[Value],
) -> Result<Args, RuntimeError> {
    cx.with(|mut env| args_from_values::<Args>(&mut env, ty, values))
}

#[inline(never)]
fn typed_results<T, Rets: WasmTypeList>(
    cx: &mut AsyncEnv<T>,
    ty: &FunctionType,
    rets: Rets,
) -> Result<Vec<Value>, RuntimeError> {
    cx.with(|mut env| results_to_values::<Rets>(&mut env, ty, rets))
}

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

fn results_to_values<Rets: WasmTypeList>(
    store: &mut impl AsStoreMut,
    ty: &FunctionType,
    rets: Rets,
) -> Result<Vec<Value>, RuntimeError> {
    // The raw form of a reference does not say which store it is of, so one
    // of another store would be read back below as one of this store.
    if !rets.is_from_store(store) {
        return Err(RuntimeError::new("cross-`Store` values are not supported"));
    }
    // Safety: the raw values are read back as the result types of `Rets`.
    let mut raw_array = unsafe { rets.into_array(store) };
    let results = raw_array.as_mut().iter().zip(ty.results());
    Ok(results
        .map(|(raw, ty)| unsafe { Value::from_raw(store, *ty, *raw) })
        .collect())
}

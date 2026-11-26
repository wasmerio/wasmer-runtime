pub(crate) mod env;
pub(crate) mod numeric;
pub(crate) mod typed;
use std::marker::PhantomData;
#[cfg(feature = "async")]
use std::rc::Rc;


#[wasm_bindgen(inline_js = r#"
export function variadic(f) {
  return function () {
    return f(Array.prototype.slice.call(arguments));
  };
}
"#)]
extern "C" {
    /// A function that calls `f` with an array of the arguments it is
    /// called with. Static JavaScript, not made from a string: some hosts
    /// forbid that.
    fn variadic(f: &JsValue) -> JsFunction;
}

#[cfg(feature = "async")]
thread_local! {
    /// Whether the innermost entry into a guest on this thread is a
    /// synchronous call, rather than `call_async`. Saved and restored around
    /// each entry.
    static ENTERED_SYNCHRONOUSLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Marks the guest about to be entered as entered synchronously, until the
/// guard is dropped: one that cannot be suspended. Some engines run the body
/// of an async import before they refuse to suspend such a guest; the body
/// refuses itself instead (see `jspi`). Without `async` there is
/// nothing to mark.
///
/// Synchronous calls of this crate mark the guest they enter, and the host
/// functions it makes mark whatever they run: a guest entered from inside
/// one, also by JavaScript that calls an export directly, is beneath a
/// JavaScript frame.
#[inline]
pub(crate) fn enter_sync() -> EnteredGuest {
    EnteredGuest::new(true)
}

/// Marks the guest about to be entered by `call_async` as not entered
/// synchronously, until the guard is dropped, whatever called `call_async`.
#[cfg(feature = "async")]
pub(crate) fn enter_async() -> EnteredGuest {
    EnteredGuest::new(false)
}

/// Whether the innermost entry into the running guest was a synchronous call.
#[cfg(feature = "async")]
pub(crate) fn entered_synchronously() -> bool {
    ENTERED_SYNCHRONOUSLY.get()
}

/// Marks what runs from now on as entered synchronously, or not, and returns
/// how it was marked before: for a guest the engine resumes, and for what an
/// exception boundary puts back (see `jspi` and `boundary`).
#[cfg(feature = "async")]
pub(crate) fn mark_entered(synchronously: bool) -> bool {
    ENTERED_SYNCHRONOUSLY.replace(synchronously)
}

/// `function`, a host function made for the store `id`, recorded as that
/// store's (see `Function::is_from_store`).
fn owned(function: JsFunction, id: wasmer_types::StoreId) -> JsFunction {
    #[cfg(feature = "async")]
    jspi::own(&function, id);
    #[cfg(not(feature = "async"))]
    let _ = id;
    function
}

/// Restores how the guest before was entered when dropped.
pub(crate) struct EnteredGuest {
    #[cfg(feature = "async")]
    previous: bool,
}

impl EnteredGuest {
    #[inline]
    fn new(synchronously: bool) -> Self {
        #[cfg(feature = "async")]
        {
            Self {
                previous: ENTERED_SYNCHRONOUSLY.replace(synchronously),
            }
        }
        #[cfg(not(feature = "async"))]
        {
            let _ = synchronously;
            Self {}
        }
    }
}

impl Drop for EnteredGuest {
    #[inline]
    fn drop(&mut self) {
        #[cfg(feature = "async")]
        ENTERED_SYNCHRONOUSLY.set(self.previous);
    }
}

use js_sys::{Array, Function as JsFunction};
use wasm_bindgen::{JsCast, prelude::*};
use wasmer_types::{FunctionType, RawValue, Type};

use crate::{
    AsStoreMut, AsStoreRef, BackendFunction, BackendFunctionEnvMut,
    CallWrapper, FromToNativeWasmType, FunctionEnv, FunctionEnvMut, HostFunction,
    HostFunctionKind, IntoResult, NativeWasmType, NativeWasmTypeInto, RuntimeError, StoreMut,
    Value, WasmTypeList, WithEnv, WithoutEnv,
    entities::function::call_wrapper::call_wrapped,
    js::{
        error::trap_value,
        lent,
        utils::convert::{AsJs as _, js_value_to_wasmer, wasmer_value_to_js},
        vm::{VMFuncRef, VMFunctionCallback, function::VMFunction},
    },
    vm::{VMExtern, VMExternFunction},
};
#[cfg(feature = "async")]
use crate::{AsyncEnv, entities::function::async_host::HostFuture, js::jspi};

use std::panic::{self, AssertUnwindSafe};

#[derive(Debug)]
struct HostFunctionPanic(String);

impl std::fmt::Display for HostFunctionPanic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HostFunctionPanic {}

fn raise_host_function_panic(payload: Box<dyn std::any::Any + Send>) -> ! {
    let message = if let Some(message) = payload.downcast_ref::<&str>() {
        (*message).to_owned()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "host function panicked with a non-string payload".to_owned()
    };
    crate::backend::js::error::raise(Box::new(HostFunctionPanic(message)))
}

#[inline]
/// What a dynamic host function returns for a result of type `ty`, as the
/// engine takes it: an `i64` as a BigInt, which the engine requires of one,
/// and anything else as a number or a reference, which the engine converts
/// to the result's type if it is of another.
fn result_to_js(value: &Value, ty: Option<&Type>) -> JsValue {
    match (value, ty) {
        (Value::I64(value), Some(Type::I64)) => JsValue::from(*value),
        (value, _) => wasmer_value_to_js(value),
    }
}

/// Whether `values` are of the types `types`, as many as there are.
#[cfg(feature = "async")]
pub(crate) fn values_match(values: &[Value], types: &[Type]) -> bool {
    values.len() == types.len() && values.iter().zip(types).all(|(value, ty)| value.ty() == *ty)
}

/// [`result_to_js`] for each of several results of the types `types`.
fn results_to_js_array(values: &[Value], types: &[Type]) -> Array {
    let values = values.iter().enumerate();
    Array::from_iter(values.map(|(index, value)| result_to_js(value, types.get(index))))
}

#[derive(Clone, PartialEq, Eq)]
pub struct Function {
    pub(crate) handle: VMFunction,
}

// Function can't be Send in js because it doesn't support `structuredClone`
// https://developer.mozilla.org/en-US/docs/Web/API/structuredClone
// unsafe impl Send for Function {}

impl From<VMFunction> for Function {
    fn from(handle: VMFunction) -> Self {
        Self { handle }
    }
}

impl Function {
    /// To `VMExtern`.
    pub(crate) fn to_vm_extern(&self) -> VMExtern {
        VMExtern::Js(crate::js::vm::external::VMExtern::Function(
            self.handle.clone(),
        ))
    }

    #[cfg(feature = "async")]
    pub(crate) fn new_async<FT, F, T>(
        store: &mut impl AsStoreMut,
        env: &FunctionEnv<T>,
        ty: FT,
        func: F,
    ) -> Self
    where
        FT: Into<FunctionType>,
        F: Fn(AsyncEnv<T>, Vec<Value>) -> HostFuture + 'static,
        T: Send + 'static,
    {
        assert!(
            jspi::is_supported(),
            "the JavaScript host does not support WebAssembly JSPI"
        );

        let func_ty: FunctionType = ty.into();
        let host = Rc::new(AsyncHost {
            func,
            env: env.clone(),
            ty: func_ty.clone(),
            store_id: store.objects_mut().id(),
        });
        let store_id = host.store_id;
        let (start, takes_array) = host.start();
        let function = jspi::host_function(&func_ty, store_id, &start, takes_array);
        let vm_function = VMFunction::new(function, func_ty);
        Self::from_vm_extern(
            &mut store.as_store_mut(),
            VMExternFunction::Js(vm_function),
        )
    }

    #[allow(clippy::cast_ptr_alignment)]
    pub fn new_with_env<FT, F, T: Send + 'static>(
        store: &mut impl AsStoreMut,
        env: &FunctionEnv<T>,
        ty: FT,
        func: F,
    ) -> Self
    where
        FT: Into<FunctionType>,
        F: Fn(FunctionEnvMut<'_, T>, &[Value]) -> Result<Vec<Value>, RuntimeError>
            + 'static
            + Send
            + Sync,
    {
        let mut store = store.as_store_mut();
        let function_type = ty.into();
        let func_ty = function_type.clone();
        // The host function runs with its store as the call it runs in lends
        // it, which it finds by the store's id, and holds it until it returns
        // (see `lent`).
        let store_id = store.as_store_ref().objects().id().as_raw().get();
        let raw_env = env.clone();
        if numeric::is_numeric(&function_type) {
            // Nothing but numbers: the guest calls the closure itself, with
            // no array of arguments in between.
            let arity = function_type.params().len();
            let function = numeric::closure(arity, move |numbers| {
                let (store, _hold) = lent::hold(store_id).map_err(trap_value)?;
                let mut store: StoreMut = unsafe { StoreMut::from_raw(store) };
                let _entered = enter_sync();
                let arguments = numeric::values(function_type.params(), numbers);
                let env: FunctionEnvMut<T> = raw_env.clone().into_mut(&mut store);
                let results = func(env, &arguments[..numbers.len()]).map_err(trap_value)?;
                Ok(match function_type.results() {
                    [] => 0.0,
                    _ => numeric::result(&results)?,
                })
            });
            let function = owned(function, store.as_store_ref().objects().id());
            let vm_function = VMFunction::new(function, func_ty);
            return Self::from_vm_extern(&mut store, VMExternFunction::Js(vm_function));
        }
        let wrapped_func: JsValue = match function_type.results().len() {
            0 => Closure::wrap(Box::new(move |args: &Array| {
                let (store, _hold) = lent::hold(store_id).map_err(trap_value)?;
                let mut store: StoreMut = unsafe { StoreMut::from_raw(store) };
                let _entered = enter_sync();
                let wasm_arguments = function_type
                    .params()
                    .iter()
                    .enumerate()
                    .map(|(i, param)| {
                        js_value_to_wasmer(&mut store, param, &args.get(i as u32))
                    })
                    .collect::<Vec<_>>();
                let env: FunctionEnvMut<T> = raw_env.clone().into_mut(&mut store);
                let _results = func(env, &wasm_arguments).map_err(trap_value)?;
                Ok(())
            })
                as Box<dyn Fn(&Array) -> Result<(), JsValue>>)
            .into_js_value(),
            1 => Closure::wrap(Box::new(move |args: &Array| {
                let (store, _hold) = lent::hold(store_id).map_err(trap_value)?;
                let mut store: StoreMut = unsafe { StoreMut::from_raw(store) };
                let _entered = enter_sync();
                let wasm_arguments = function_type
                    .params()
                    .iter()
                    .enumerate()
                    .map(|(i, param)| {
                        js_value_to_wasmer(&mut store, param, &args.get(i as u32))
                    })
                    .collect::<Vec<_>>();
                let env: FunctionEnvMut<T> = raw_env.clone().into_mut(&mut store);
                let results = func(env, &wasm_arguments).map_err(trap_value)?;
                Ok(result_to_js(&results[0], function_type.results().first()))
            })
                as Box<dyn Fn(&Array) -> Result<JsValue, JsValue>>)
            .into_js_value(),
            _n => Closure::wrap(Box::new(move |args: &Array| {
                let (store, _hold) = lent::hold(store_id).map_err(trap_value)?;
                let mut store: StoreMut = unsafe { StoreMut::from_raw(store) };
                let _entered = enter_sync();
                let wasm_arguments = function_type
                    .params()
                    .iter()
                    .enumerate()
                    .map(|(i, param)| {
                        js_value_to_wasmer(&mut store, param, &args.get(i as u32))
                    })
                    .collect::<Vec<_>>();
                let env: FunctionEnvMut<T> = raw_env.clone().into_mut(&mut store);
                let results = func(env, &wasm_arguments).map_err(trap_value)?;
                Ok(results_to_js_array(&results, function_type.results()))
            })
                as Box<dyn Fn(&Array) -> Result<Array, JsValue>>)
            .into_js_value(),
        };

        let function = owned(variadic(&wrapped_func), store.as_store_ref().objects().id());
        let vm_function = VMFunction::new(function, func_ty);
        Self::from_vm_extern(&mut store, VMExternFunction::Js(vm_function))
    }

    /// Creates a new host `Function` from a native function.
    pub fn new_typed<F, Args, Rets>(store: &mut impl AsStoreMut, func: F) -> Self
    where
        F: HostFunction<(), Args, Rets, WithoutEnv> + 'static + Send + Sync,
        Args: WasmTypeList,
        Rets: WasmTypeList,
    {
        let store = store.as_store_mut();
        if std::mem::size_of::<F>() != 0 {
            Self::closures_unsupported_panic();
        }
        let function = WasmFunction::<Args, Rets>::new(func);
        let address = function.address() as usize as u32;

        let ft = wasm_bindgen::function_table();
        let as_table = ft.unchecked_ref::<js_sys::WebAssembly::Table>();
        let func = as_table.get(address).unwrap();

        let ty = function.ty();
        let id = store.as_store_ref().objects().id();
        // The store's id, by which the host function finds the store the
        // call it runs in lends it (see `lent`).
        let store = JsValue::from_f64(id.as_raw().get() as f64);
        let binded_func = with_leading_arguments(func.unchecked_ref(), &[store], ty.params().len());
        let vm_function = VMFunction::new(owned(binded_func, id), ty);
        Self {
            handle: vm_function,
        }
    }

    pub fn new_typed_with_env<T, F, Args, Rets>(
        store: &mut impl AsStoreMut,
        env: &FunctionEnv<T>,
        func: F,
    ) -> Self
    where
        F: HostFunction<T, Args, Rets, WithEnv>,
        Args: WasmTypeList,
        Rets: WasmTypeList,
    {
        let store = store.as_store_mut();
        if std::mem::size_of::<F>() != 0 {
            Self::closures_unsupported_panic();
        }
        let function = WasmFunction::<Args, Rets>::new(func);
        let address = function.address() as usize as u32;

        let ft = wasm_bindgen::function_table();
        let as_table = ft.unchecked_ref::<js_sys::WebAssembly::Table>();
        let func = as_table.get(address).unwrap();

        let ty = function.ty();
        let id = store.as_store_ref().objects().id();
        // As for `new_typed`, with the index of the environment.
        let leading = [
            JsValue::from_f64(id.as_raw().get() as f64),
            JsValue::from_f64(env.as_js().handle.internal_handle().index() as f64),
        ];
        let binded_func = with_leading_arguments(func.unchecked_ref(), &leading, ty.params().len());
        let vm_function = VMFunction::new(owned(binded_func, id), ty);
        Self {
            handle: vm_function,
        }
    }

    pub fn ty(&self, _store: &impl AsStoreRef) -> FunctionType {
        self.handle.ty.clone()
    }

    pub(crate) unsafe fn call_raw(
        &self,
        _store: &mut impl AsStoreMut,
        _params: Vec<RawValue>,
    ) -> Result<Box<[Value]>, RuntimeError> {
        Err(RuntimeError::new(
            "raw calls are not supported by the js backend",
        ))
    }

    pub fn call(
        &self,
        store: &mut impl AsStoreMut,
        params: &[Value],
    ) -> Result<Box<[Value]>, RuntimeError> {
        self.call_wrapped(store, None, params)
    }

    pub(crate) fn call_wrapped(
        &self,
        store: &mut impl AsStoreMut,
        wrapper: Option<&mut dyn CallWrapper>,
        params: &[Value],
    ) -> Result<Box<[Value]>, RuntimeError> {
        let ty = &self.handle.ty;
        if numeric::is_numeric(ty)
            && let Some(numbers) = numeric::numbers(ty, params)
        {
            // Nothing but numbers: they are passed as they are. The function
            // is called as `Reflect::apply` calls it below.
            let mut result = 0.0;
            let returns = !ty.results().is_empty();
            call_wrapped(store, wrapper, |store| {
                let _lent = lent::lend(&mut store.as_store_mut())?;
                let _entered = enter_sync();
                let function = &self.handle.function;
                result = numeric::call(function, &JsValue::NULL, &numbers, returns)?;
                Ok(())
            })?;
            let results = ty.results().iter();
            return Ok(results.map(|ty| numeric::value(ty, result)).collect());
        }

        // Annotation is here to prevent spurious IDE warnings.
        let arr = js_sys::Array::new_with_length(params.len() as u32);

        // let raw_env = env.as_raw() as *mut u8;
        // let mut env = unsafe { FunctionEnvMut::from_raw(raw_env as *mut StoreInner<()>) };

        for (i, param) in params.iter().enumerate() {
            let js_value = param.as_jsvalue(&store.as_store_ref());
            arr.set(i as u32, js_value);
        }

        let mut result = JsValue::UNDEFINED;
        call_wrapped(store, wrapper, |store| {
            let _lent = lent::lend(&mut store.as_store_mut())?;
            let _entered = enter_sync();
            result = js_sys::Reflect::apply(&self.handle.function, &JsValue::NULL, &arr)?;
            Ok(())
        })?;

        let result_types = self.handle.ty.results();
        match result_types.len() {
            0 => Ok(Box::new([])),
            1 => {
                let value = js_value_to_wasmer(store, &result_types[0], &result);
                Ok(vec![value].into_boxed_slice())
            }
            _n => {
                let result_array: Array = result.into();
                Ok(result_array
                    .iter()
                    .enumerate()
                    .map(|(i, js_val)| {
                        js_value_to_wasmer(store, &result_types[i], &js_val)
                    })
                    .collect::<Vec<_>>()
                    .into_boxed_slice())
            }
        }
    }

    pub(crate) fn from_vm_extern(_store: &mut impl AsStoreMut, internal: VMExternFunction) -> Self {
        Self {
            handle: internal.unwrap_js(),
        }
    }

    pub(crate) fn vm_funcref(&self, _store: &impl AsStoreRef) -> VMFuncRef {
        unimplemented!();
    }

    pub(crate) unsafe fn from_vm_funcref(
        _store: &mut impl AsStoreMut,
        _funcref: VMFuncRef,
    ) -> Self {
        unimplemented!();
    }

    #[track_caller]
    fn closures_unsupported_panic() -> ! {
        unimplemented!(
            "Closures (functions with captured environments) are currently unsupported with native functions. See: https://github.com/wasmerio/wasmer/issues/1840"
        )
    }

    /// Checks whether this `Function` can be used with the given context.
    pub fn is_from_store(&self, _store: &impl AsStoreRef) -> bool {
        #[cfg(feature = "async")]
        return jspi::belongs_to(
            &self.handle.function,
            _store.as_store_ref().objects().id(),
        );
        #[cfg(not(feature = "async"))]
        true
    }
}

#[wasm_bindgen(inline_js = r#"
export function leading1(f, l, arity) {
  switch (arity) {
    case 0: return () => f(l);
    case 1: return (a) => f(l, a);
    case 2: return (a, b) => f(l, a, b);
    case 3: return (a, b, c) => f(l, a, b, c);
    case 4: return (a, b, c, d) => f(l, a, b, c, d);
    case 5: return (a, b, c, d, e) => f(l, a, b, c, d, e);
    case 6: return (a, b, c, d, e, g) => f(l, a, b, c, d, e, g);
    case 7: return (a, b, c, d, e, g, h) => f(l, a, b, c, d, e, g, h);
    case 8: return (a, b, c, d, e, g, h, i) => f(l, a, b, c, d, e, g, h, i);
    case 9: return (a, b, c, d, e, g, h, i, j) => f(l, a, b, c, d, e, g, h, i, j);
    case 10: return (a, b, c, d, e, g, h, i, j, k) => f(l, a, b, c, d, e, g, h, i, j, k);
    case 11: return (a, b, c, d, e, g, h, i, j, k, n) => f(l, a, b, c, d, e, g, h, i, j, k, n);
    case 12: return (a, b, c, d, e, g, h, i, j, k, n, o) =>
      f(l, a, b, c, d, e, g, h, i, j, k, n, o);
    case 13: return (a, b, c, d, e, g, h, i, j, k, n, o, p) =>
      f(l, a, b, c, d, e, g, h, i, j, k, n, o, p);
    case 14: return (a, b, c, d, e, g, h, i, j, k, n, o, p, q) =>
      f(l, a, b, c, d, e, g, h, i, j, k, n, o, p, q);
    case 15: return (a, b, c, d, e, g, h, i, j, k, n, o, p, q, r) =>
      f(l, a, b, c, d, e, g, h, i, j, k, n, o, p, q, r);
    case 16: return (a, b, c, d, e, g, h, i, j, k, n, o, p, q, r, s) =>
      f(l, a, b, c, d, e, g, h, i, j, k, n, o, p, q, r, s);
  }
}
export function leading2(f, l, m, arity) {
  switch (arity) {
    case 0: return () => f(l, m);
    case 1: return (a) => f(l, m, a);
    case 2: return (a, b) => f(l, m, a, b);
    case 3: return (a, b, c) => f(l, m, a, b, c);
    case 4: return (a, b, c, d) => f(l, m, a, b, c, d);
    case 5: return (a, b, c, d, e) => f(l, m, a, b, c, d, e);
    case 6: return (a, b, c, d, e, g) => f(l, m, a, b, c, d, e, g);
    case 7: return (a, b, c, d, e, g, h) => f(l, m, a, b, c, d, e, g, h);
    case 8: return (a, b, c, d, e, g, h, i) => f(l, m, a, b, c, d, e, g, h, i);
    case 9: return (a, b, c, d, e, g, h, i, j) => f(l, m, a, b, c, d, e, g, h, i, j);
    case 10: return (a, b, c, d, e, g, h, i, j, k) => f(l, m, a, b, c, d, e, g, h, i, j, k);
    case 11: return (a, b, c, d, e, g, h, i, j, k, n) =>
      f(l, m, a, b, c, d, e, g, h, i, j, k, n);
    case 12: return (a, b, c, d, e, g, h, i, j, k, n, o) =>
      f(l, m, a, b, c, d, e, g, h, i, j, k, n, o);
    case 13: return (a, b, c, d, e, g, h, i, j, k, n, o, p) =>
      f(l, m, a, b, c, d, e, g, h, i, j, k, n, o, p);
    case 14: return (a, b, c, d, e, g, h, i, j, k, n, o, p, q) =>
      f(l, m, a, b, c, d, e, g, h, i, j, k, n, o, p, q);
    case 15: return (a, b, c, d, e, g, h, i, j, k, n, o, p, q, r) =>
      f(l, m, a, b, c, d, e, g, h, i, j, k, n, o, p, q, r);
    case 16: return (a, b, c, d, e, g, h, i, j, k, n, o, p, q, r, s) =>
      f(l, m, a, b, c, d, e, g, h, i, j, k, n, o, p, q, r, s);
  }
}
"#)]
extern "C" {
    /// A function of `arity` parameters, up to [`ARROW_ARITY`], that calls
    /// `f` with `l` and then its own arguments; `undefined` for more. Static
    /// JavaScript, not made from a string: some hosts forbid that.
    fn leading1(f: &JsFunction, l: &JsValue, arity: usize) -> JsValue;
    /// [`leading1`], with two leading arguments.
    fn leading2(f: &JsFunction, l: &JsValue, m: &JsValue, arity: usize) -> JsValue;
}

/// The most parameters [`leading1`] and [`leading2`] make a function of.
const ARROW_ARITY: usize = 16;

/// A function of `arity` parameters that calls `function` with `leading` and
/// then its own arguments: `function.bind(undefined, ...leading)`, as a plain
/// arrow function of that many parameters for up to [`ARROW_ARITY`] of them.
/// Engines call such an arrow faster than a bound function when the caller is
/// WebAssembly and `function` is too: JavaScriptCore several times faster.
fn with_leading_arguments(function: &JsFunction, leading: &[JsValue], arity: usize) -> JsFunction {
    arrow_with_leading_arguments(function, leading, arity)
        .unwrap_or_else(|| bound_with_leading_arguments(function, leading))
}

fn arrow_with_leading_arguments(
    function: &JsFunction,
    leading: &[JsValue],
    arity: usize,
) -> Option<JsFunction> {
    let made = match leading {
        [l] => leading1(function, l, arity),
        [l, m] => leading2(function, l, m, arity),
        _ => unreachable!("typed host functions have one or two leading arguments"),
    };
    made.dyn_into().ok()
}

fn bound_with_leading_arguments(function: &JsFunction, leading: &[JsValue]) -> JsFunction {
    let receiver = &JsValue::UNDEFINED;
    match leading {
        [first] => function.bind1(receiver, first).unchecked_into(),
        [first, second] => function.bind2(receiver, first, second).unchecked_into(),
        _ => unreachable!("typed host functions have one or two leading arguments"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasm_bindgen_test::wasm_bindgen_test;

    #[wasm_bindgen(inline_js = r#"
    export function countFunctionsFromText() {
      const original = globalThis.Function;
      const counter = { count: 0, restore() { globalThis.Function = original; } };
      globalThis.Function = new Proxy(original, {
        construct(target, args, newTarget) {
          counter.count++;
          return Reflect.construct(target, args, newTarget);
        },
        apply(target, receiver, args) {
          counter.count++;
          return Reflect.apply(target, receiver, args);
        },
      });
      return counter;
    }
    "#)]
    extern "C" {
        /// Counts the functions made from text, with `Function`, until
        /// `restore` is called.
        #[wasm_bindgen(js_name = countFunctionsFromText)]
        fn count_functions_from_text() -> JsValue;
    }

    /// The arrow that stands in for a bound function passes the same
    /// arguments in the same order, whatever their number and kind, up to
    /// the most it is made for, and one is made for every number up to that;
    /// for more, the bound function stands for itself. Nothing is made from
    /// text: the hosts that forbid it, and report every attempt, make the
    /// same arrows.
    #[wasm_bindgen_test]
    fn an_arrow_with_leading_arguments_calls_as_a_bound_function_does() {
        let to_string: JsFunction = js_sys::Reflect::get(&js_sys::global(), &"String".into())
            .unwrap()
            .unchecked_into();
        let join = Closure::wrap(Box::new(move |all: Array| -> JsValue {
            let all = all.iter().map(|value| {
                let value = to_string.call1(&JsValue::UNDEFINED, &value).unwrap();
                value.as_string().unwrap()
            });
            all.collect::<Vec<_>>().join(" ").into()
        }) as Box<dyn Fn(Array) -> JsValue>)
        .into_js_value();
        let join = variadic(&join);
        let counter = count_functions_from_text();
        let leading = [JsValue::from(-1), JsValue::from(2.5)];
        for leading in [&leading[..1], &leading[..]] {
            for arity in 0..=ARROW_ARITY + 2 {
                let arguments = Array::new();
                for index in 0..arity {
                    arguments.push(&match index % 3 {
                        0 => JsValue::from(index as i32),
                        1 => JsValue::from(js_sys::BigInt::from(index as i64)),
                        _ => JsValue::NULL,
                    });
                }
                let call = |function: &JsFunction| {
                    let joined = function.apply(&JsValue::UNDEFINED, &arguments);
                    joined.unwrap().as_string().unwrap()
                };
                let Some(arrow) = arrow_with_leading_arguments(&join, leading, arity) else {
                    assert!(arity > ARROW_ARITY, "no arrow of {arity} parameters");
                    continue;
                };
                assert!(arity <= ARROW_ARITY, "an arrow of {arity} parameters");
                assert_eq!(arrow.length() as usize, arity);
                let arrow = call(&arrow);
                assert_eq!(arrow, call(&bound_with_leading_arguments(&join, leading)));
                assert_eq!(arrow.split(' ').count(), leading.len() + arity);
                assert!(arrow.starts_with("-1"), "{arrow}");
            }
        }
        let made = js_sys::Reflect::get(&counter, &"count".into()).unwrap();
        let restore = js_sys::Reflect::get(&counter, &"restore".into()).unwrap();
        restore.unchecked_into::<JsFunction>().call0(&counter).unwrap();
        assert_eq!(made.as_f64(), Some(0.0), "functions were made from text");
    }
}

/// The arguments of a call of an async host function, as they arrived.
#[cfg(feature = "async")]
enum Arguments {
    Array(Array),
    /// Parameters that are all numbers.
    Numbers(numeric::Numbers),
}

/// An async host function, as the body of its import needs it.
#[cfg(feature = "async")]
struct AsyncHost<T, F> {
    func: F,
    env: FunctionEnv<T>,
    ty: FunctionType,
    store_id: wasmer_types::StoreId,
}

#[cfg(feature = "async")]
impl<T, F> AsyncHost<T, F>
where
    T: 'static,
    F: Fn(AsyncEnv<T>, Vec<Value>) -> HostFuture + 'static,
{
    /// The body of the `WebAssembly.Suspending` import a guest calls, given
    /// the arguments in an array: it returns the promise the guest waits on.
    /// Nothing of the host function runs here: its future runs on the
    /// microtask queue, once the guest waits.
    ///
    /// Parameters that are all numbers arrive as they are, with no array in
    /// between; the body is then the function itself. Returns the body, and
    /// whether it takes an array.
    fn start(self: Rc<Self>) -> (JsValue, bool) {
        let id = self.store_id;
        if numeric::takes_numbers(&self.ty) {
            let arity = self.ty.params().len();
            let start = numeric::closure_returning_any(arity, move |numbers| {
                let (host, args) = (Rc::clone(&self), Arguments::Numbers(numeric::copy(numbers)));
                jspi::start(id, move || host.import(args))
            });
            return (start.into(), false);
        }
        let start = Closure::wrap(Box::new(move |args: Array| {
            let (host, args) = (Rc::clone(&self), Arguments::Array(args));
            jspi::start(id, move || host.import(args))
        }) as Box<dyn Fn(Array) -> Result<JsValue, JsValue>>);
        (start.into_js_value(), true)
    }

    /// The host's half of one call of the import: the host function, and
    /// what it awaits. `store` is the store of the call the guest belongs to.
    fn import(self: Rc<Self>, args: Arguments) -> jspi::ImportFuture {
        Box::pin(async move {
            let mut cx = AsyncEnv::new(self.env.clone(), self.store_id);
            let values = cx.with(|mut env| self.arguments(&mut env, &args));
            let results = (self.func)(cx, values).await?;
            // Checked here: values of other types would reach the guest as
            // an exception it can catch.
            if !values_match(&results, self.ty.results()) {
                return Err(RuntimeError::new(
                    "an async host function returned values that do not match its signature",
                ));
            }
            Ok(results)
        })
    }

    fn arguments(&self, store: &mut impl AsStoreMut, args: &Arguments) -> Vec<Value> {
        let types = self.ty.params();
        match args {
            Arguments::Array(args) => {
                let types = types.iter().enumerate();
                types
                    .map(|(index, ty)| js_value_to_wasmer(store, ty, &args.get(index as u32)))
                    .collect()
            }
            Arguments::Numbers((numbers, arity)) => {
                numeric::values(types, &numbers[..*arity])[..*arity].to_vec()
            }
        }
    }

}

impl std::fmt::Debug for Function {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.debug_struct("Function").finish()
    }
}

/// Represents a low-level Wasm static host function. See
/// `super::Function::new` and `super::Function::new_env` to learn
/// more.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct WasmFunction<Args = (), Rets = ()> {
    address: VMFunctionCallback,
    _phantom: PhantomData<(Args, Rets)>,
}

unsafe impl<Args, Rets> Send for WasmFunction<Args, Rets> {}

impl<Args, Rets> WasmFunction<Args, Rets>
where
    Args: WasmTypeList,
    Rets: WasmTypeList,
{
    /// Creates a new `WasmFunction`.
    #[allow(dead_code)]
    pub fn new<F, T, Kind: HostFunctionKind>(function: F) -> Self
    where
        F: HostFunction<T, Args, Rets, Kind>,
        T: Sized,
    {
        Self {
            address: function.function_callback(crate::BackendKind::Js).unwrap_js(),
            _phantom: PhantomData,
        }
    }

    /// Get the function type of this `WasmFunction`.
    #[allow(dead_code)]
    pub fn ty(&self) -> FunctionType {
        FunctionType::new(Args::wasm_types(), Rets::wasm_types())
    }

    /// Get the address of this `WasmFunction`.
    #[allow(dead_code)]
    pub fn address(&self) -> VMFunctionCallback {
        self.address
    }
}

impl crate::Function {
    /// Consume [`self`] into [`crate::backend::js::function::Function`].
    pub fn into_js(self) -> crate::backend::js::function::Function {
        match self.0 {
            BackendFunction::Js(s) => s,
            _ => panic!("Not a `js` function!"),
        }
    }

    /// Convert a reference to [`self`] into a reference to [`crate::backend::js::function::Function`].
    pub fn as_js(&self) -> &crate::backend::js::function::Function {
        match self.0 {
            BackendFunction::Js(ref s) => s,
            _ => panic!("Not a `js` function!"),
        }
    }

    /// Convert a mutable reference to [`self`] into a mutable reference [`crate::backend::js::function::Function`].
    pub fn as_js_mut(&mut self) -> &mut crate::backend::js::function::Function {
        match self.0 {
            BackendFunction::Js(ref mut s) => s,
            _ => panic!("Not a `js` function!"),
        }
    }
}

macro_rules! impl_host_function {
    ([$c_struct_representation:ident] $c_struct_name:ident, $( $x:ident ),* ) => {
        paste::paste! {
        #[allow(non_snake_case)]
        pub(crate) fn [<gen_fn_callback_ $c_struct_name:lower _no_env>]
            <$( $x: FromToNativeWasmType, )* Rets: WasmTypeList, RetsAsResult: IntoResult<Rets>, Func: Fn($( $x , )*) -> RetsAsResult + 'static>
            (_this: &Func) -> crate::backend::js::vm::VMFunctionCallback {

            /// This is a function that wraps the real host
            /// function. Its address will be used inside the
            /// runtime.
            unsafe extern "C" fn func_wrapper<$( $x, )* Rets, RetsAsResult, Func>( store_id: usize, $( $x: <$x::Native as NativeWasmType>::Abi, )* ) -> Rets::CStruct
            where
                $( $x: FromToNativeWasmType, )*
                Rets: WasmTypeList,
                RetsAsResult: IntoResult<Rets>,
                Func: Fn($( $x , )*) -> RetsAsResult + 'static,
            {
                // let env: &Env = unsafe { &*(ptr as *const u8 as *const Env) };
                let func: &Func = unsafe { &*(&() as *const () as *const Func) };
                let (store, hold) = match crate::backend::js::lent::hold(store_id) {
                    Ok(held) => held,
                    Err(error) => crate::backend::js::error::raise(Box::new(error)),
                };
                let mut store = unsafe { StoreMut::from_raw(store) };
                let entered = enter_sync();

                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    func($(
                        {
                            let native = unsafe { NativeWasmTypeInto::from_abi(&mut store, $x) };
                            FromToNativeWasmType::from_native(native)
                        }
                    ),* ).into_result()
                }));
                // Before anything is thrown below: a throw skips destructors.
                drop(entered);

                let result = result.map(|result| {
                    result.map(|result| unsafe { result.into_c_struct(&mut store) })
                });
                drop(hold);

                match result {
                    Ok(Ok(c_struct)) => c_struct,
                    Ok(Err(trap)) => crate::backend::js::error::raise(Box::new(trap)),
                    Err(panic) => raise_host_function_panic(panic),
                }
            }

            func_wrapper::< $( $x, )* Rets, RetsAsResult, Func> as _

        }


        #[allow(non_snake_case)]
        pub(crate) fn [<gen_fn_callback_ $c_struct_name:lower>]
            <$( $x: FromToNativeWasmType, )* Rets: WasmTypeList, RetsAsResult: IntoResult<Rets>, T: Send + 'static,  Func: Fn(FunctionEnvMut<T>, $( $x , )*) -> RetsAsResult + 'static>
            (_this: &Func) -> crate::backend::js::vm::VMFunctionCallback {

            /// This is a function that wraps the real host
            /// function. Its address will be used inside the
            /// runtime.
            unsafe extern "C" fn func_wrapper<T, $( $x, )* Rets, RetsAsResult, Func>( store_id: usize, handle_index: usize, $( $x: <$x::Native as NativeWasmType>::Abi, )* ) -> Rets::CStruct
            where
                $( $x: FromToNativeWasmType, )*
                Rets: WasmTypeList,
                RetsAsResult: IntoResult<Rets>,
                T: Send + 'static,
                Func: Fn(FunctionEnvMut<'_, T>, $( $x , )*) -> RetsAsResult + 'static,
            {
                let (store_ptr, hold) = match crate::backend::js::lent::hold(store_id) {
                    Ok(held) => held,
                    Err(error) => crate::backend::js::error::raise(Box::new(error)),
                };
                let mut store = unsafe { StoreMut::from_raw(store_ptr) };
                let entered = enter_sync();

                let result = {
                    let func: &Func = unsafe { &*(&() as *const () as *const Func) };
                    panic::catch_unwind(AssertUnwindSafe(|| {
                        // The arguments first, and then the environment, which
                        // borrows the store until the function returns: one
                        // borrow of the store at a time.
                        $(
                            let $x: $x = {
                                let native = unsafe { NativeWasmTypeInto::from_abi(&mut store, $x) };
                                FromToNativeWasmType::from_native(native)
                            };
                        )*
                        let handle: crate::backend::js::store::StoreHandle<crate::backend::js::vm::VMFunctionEnvironment> =
                          unsafe {
                              crate::backend::js::store::StoreHandle::from_internal(
                                  store.objects_mut().id(),
                                  crate::backend::js::store::InternalStoreHandle::from_index(handle_index).unwrap(),
                              )
                          };
                        let env: crate::backend::js::function::env::FunctionEnvMut<T> = crate::backend::js::function::env::FunctionEnv::from_handle(handle).into_mut(&mut store);
                        func(BackendFunctionEnvMut::Js(env).into(), $( $x ),* ).into_result()
                    }))
                };
                // Before anything is thrown below: a throw skips destructors.
                drop(entered);

                let result = result.map(|result| {
                    result.map(|result| unsafe { result.into_c_struct(&mut store) })
                });
                drop(hold);

                match result {
                    Ok(Ok(c_struct)) => c_struct,
                    #[allow(deprecated)]
                    Ok(Err(trap)) => crate::js::error::raise(Box::new(trap)),
                    Err(panic) => raise_host_function_panic(panic),
                }
            }

            func_wrapper::< T, $( $x, )* Rets, RetsAsResult, Func > as _
        }

        }
    };
}

// Here we go! Let's generate all the C struct, `WasmTypeList`
// implementations and `HostFunction` implementations.
impl_host_function!([C] S0,);
impl_host_function!([transparent] S1, A1);
impl_host_function!([C] S2, A1, A2);
impl_host_function!([C] S3, A1, A2, A3);
impl_host_function!([C] S4, A1, A2, A3, A4);
impl_host_function!([C] S5, A1, A2, A3, A4, A5);
impl_host_function!([C] S6, A1, A2, A3, A4, A5, A6);
impl_host_function!([C] S7, A1, A2, A3, A4, A5, A6, A7);
impl_host_function!([C] S8, A1, A2, A3, A4, A5, A6, A7, A8);
impl_host_function!([C] S9, A1, A2, A3, A4, A5, A6, A7, A8, A9);
impl_host_function!([C] S10, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10);
impl_host_function!([C] S11, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11);
impl_host_function!([C] S12, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12);
impl_host_function!([C] S13, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13);
impl_host_function!([C] S14, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14);
impl_host_function!([C] S15, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15);
impl_host_function!([C] S16, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16);
impl_host_function!([C] S17, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17);
impl_host_function!([C] S18, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18);
impl_host_function!([C] S19, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19);
impl_host_function!([C] S20, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20);
impl_host_function!([C] S21, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21);
impl_host_function!([C] S22, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22);
impl_host_function!([C] S23, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22, A23);
impl_host_function!([C] S24, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22, A23, A24);
impl_host_function!([C] S25, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22, A23, A24, A25);
impl_host_function!([C] S26, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22, A23, A24, A25, A26);

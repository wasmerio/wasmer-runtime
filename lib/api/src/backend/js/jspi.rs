//! JavaScript ownership, type conversion, and trap helpers shared by JSPI.
use crate::{
    AsStoreRef, BackendTrap, RuntimeError, Value,
    js::{error::Trap, utils::convert::AsJs},
};
use js_sys::{Array, Function, Reflect};
use std::cell::Cell;
use wasm_bindgen::{
    JsValue,
    prelude::{Closure, wasm_bindgen},
};
use wasmer_types::{FunctionType, StoreId, TrapCode, Type};

thread_local! {
    static TRAP_READY: Cell<bool> = const { Cell::new(false) };
    static SHIM: () = shim_init(
        &Closure::wrap(Box::new(crate::js::function::mark_entered) as Box<dyn Fn(bool) -> bool>).into_js_value(),
        &Closure::wrap(Box::new(crate::js::function::entered_synchronously) as Box<dyn Fn() -> bool>).into_js_value(),
        &Closure::wrap(Box::new(crate::js::async_runtime::enter_host) as Box<dyn Fn(usize) -> u32>).into_js_value(),
        &Closure::wrap(Box::new(crate::js::async_runtime::leave_host) as Box<dyn Fn(u32)>).into_js_value(),
    );
}

#[cfg(test)]
pub(crate) fn forget_trap() {
    shim_forget_trap();
    TRAP_READY.set(false);
}

#[wasm_bindgen(inline_js = r#"
let mark, entered, enterHost, leaveHost, trapError;
export function jspiInit(m, s, e, l) { mark = m; entered = s; enterHost = e; leaveHost = l; }
function hostBoundary(value, store, synchronous) {
  const host = function (...args) {
    const checkpoint = enterHost(store), previous = entered();
    if (synchronous) mark(true);
    try { return apply(value, this, args); }
    finally { leaveHost(checkpoint); mark(previous); }
  };
  storeOf.set(host, store);
  return host;
}
export function jspiSyncHost(value, store) { return hostBoundary(value, store, true); }
export function jspiHostCallback(store, callback) {
  const checkpoint = enterHost(store), previous = mark(true);
  try { callback(); }
  finally { leaveHost(checkpoint); mark(previous); }
}
export function jspiPrepareTrap() {
  if (trapError !== undefined) return;
  const bytes = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0, 1, 4, 1, 96, 0, 0, 3, 2, 1, 0,
    7, 8, 1, 4, 116, 114, 97, 112, 0, 0, 10, 5, 1, 3, 0, 0, 11]);
  const { trap } = new WebAssembly.Instance(new WebAssembly.Module(bytes)).exports;
  try { trap(); } catch (error) { trapError = error; }
}
export function jspiTrapError() {
  return trapError;
}
export function jspiForgetTrap() {
  trapError = undefined;
}
const suspendingOf = new WeakMap(), hostOf = new WeakMap(), storeOf = new WeakMap();
export function jspiHost(start, array, store) {
  // This boundary returns its Promise before Suspending suspends the guest;
  // no JavaScript frame is retained across the suspension.
  const host = hostBoundary(array ? (...args) => start(args) : start, store, false);
  const suspending = new WebAssembly.Suspending(host);
  suspendingOf.set(host, suspending);
  hostOf.set(suspending, host);
  storeOf.set(host, store);
  return host;
}
export function jspiOwn(value, store) {
  storeOf.set(value, store);
}
export function jspiOwner(value) {
  return storeOf.get(value);
}
export function jspiHostOf(value) {
  return hostOf.get(value) ?? embedderOf.get(value) ?? value;
}
export function jspiIsHost(value) {
  return suspendingOf.has(value);
}
export function jspiImport(value, kinds) {
  return suspendingOf.get(value) ?? embedderFunction(value, kinds);
}
const apply = Reflect.apply, markedOf = new WeakMap(), embedderOf = new WeakMap();
const wasmFunctions = new WeakSet();
let probe;
function isWasmFunction(value) {
  if (wasmFunctions.has(value)) return true;
  probe ??= new WebAssembly.Table({ element: "anyfunc", initial: 1 });
  try { probe.set(0, value); } catch { return false; }
  probe.set(0, null);
  wasmFunctions.add(value);
  return true;
}
const toResult = (kind, value) =>
  kind === "n" ? +value : kind === "b" ? BigInt.asIntN(64, value) : value;
const iterator = Symbol.iterator;
// Results the engine iterates without running anything of the embedder's:
// no prototype to look anything up on, and an iterator of its own.
function tuple(values) {
  let index = 0;
  const next = () => index < values.length
    ? { __proto__: null, value: values[index++], done: false }
    : { __proto__: null, value: undefined, done: true };
  const results = { __proto__: null, next };
  return { __proto__: null, [iterator]: () => results };
}
// What a guest imports for a JavaScript function an embedder gave: one that
// marks what it runs, converting the results inside the marked region. Only
// imports are given it: a direct call of the function reads what the
// function returns itself.
function embedderFunction(value, kinds) {
  // The functions this crate makes mark what they run themselves.
  if (typeof value !== "function" || storeOf.has(value)) return value;
  let byKinds = markedOf.get(value);
  let marked = byKinds?.get(kinds);
  if (marked === undefined) {
    if (isWasmFunction(value)) return value;
    marked = function (...args) {
      const previous = mark(true);
      try {
        const result = apply(value, this, args);
        if (kinds.length === 0) return result;
        if (kinds.length === 1) return toResult(kinds, result);
        // As the engine does with what a host function returns: iterated
        // once, its length checked, and then each value converted.
        const values = [];
        for (const item of result) values[values.length] = item;
        if (values.length !== kinds.length) {
          throw new TypeError(`expected ${kinds.length} results, got ${values.length}`);
        }
        for (let i = 0; i < kinds.length; i++) values[i] = toResult(kinds[i], values[i]);
        return tuple(values);
      } finally {
        mark(previous);
      }
    };
    if (byKinds === undefined) markedOf.set(value, byKinds = new Map());
    byKinds.set(kinds, marked);
    embedderOf.set(marked, value);
  }
  return marked;
}
export function jspiIsWasmFunction(value) {
  return isWasmFunction(value);
}
export function jspiPublic(value) {
  return suspendingOf.get(value) ?? value;
}
export function jspiIsThenable(value) {
  return (typeof value === "object" || typeof value === "function") && value !== null &&
    typeof value.then === "function";
}
export function jspiIsSuspendError(value) {
  const SuspendError = WebAssembly.SuspendError;
  try {
    return typeof SuspendError === "function" && value instanceof SuspendError;
  } catch {
    // A revoked `Proxy` has no prototype to look at: it is not one.
    return false;
  }
}
// The promising form of each function, made once. Kept here rather than on
// the function, where any JavaScript could replace it; and the reaction is
// attached with the `then` of the promises the engine makes, as it was when
// this module loaded.
const promisingOf = new WeakMap(), then = Promise.prototype.then;
export function jspiPromising(fn) {
  let promising = promisingOf.get(fn);
  if (promising === undefined) {
    promising = WebAssembly.promising(fn);
    promisingOf.set(fn, promising);
  }
  return promising;
}
"#)]
extern "C" {
    #[wasm_bindgen(js_name = jspiInit)]
    fn shim_init(mark: &JsValue, entered: &JsValue, enter_host: &JsValue, leave_host: &JsValue);
    #[wasm_bindgen(js_name = jspiSyncHost)]
    fn shim_sync_host(value: &Function, store: usize) -> Function;
    #[wasm_bindgen(catch, js_name = jspiHostCallback)]
    fn shim_host_callback(store: usize, callback: &JsValue) -> Result<(), JsValue>;
    #[wasm_bindgen(js_name = jspiPrepareTrap, catch)]
    fn shim_prepare_trap() -> Result<(), JsValue>;
    #[wasm_bindgen(js_name = jspiTrapError)]
    pub(crate) fn trap_error() -> JsValue;
    /// Forgets the error of a trap, for it to be made again.
    #[cfg(test)]
    #[wasm_bindgen(js_name = jspiForgetTrap)]
    fn shim_forget_trap();
    #[wasm_bindgen(js_name = jspiHost)]
    fn shim_host(start: &JsValue, array: bool, store: f64) -> Function;
    #[wasm_bindgen(js_name = jspiOwn)]
    fn shim_own(value: &JsValue, store: f64);
    #[wasm_bindgen(js_name = jspiOwner)]
    fn shim_owner(value: &JsValue) -> Option<f64>;
    #[wasm_bindgen(js_name = jspiIsWasmFunction)]
    fn shim_is_wasm_function(value: &JsValue) -> bool;
    #[wasm_bindgen(js_name = jspiIsHost)]
    fn shim_is_host(value: &JsValue) -> bool;
    #[wasm_bindgen(js_name = jspiImport)]
    fn shim_import(value: &JsValue, kinds: &str) -> JsValue;
    #[wasm_bindgen(js_name = jspiHostOf)]
    fn shim_host_of(value: &JsValue) -> JsValue;
    #[wasm_bindgen(js_name = jspiPublic)]
    fn shim_public(value: &JsValue) -> JsValue;
    #[wasm_bindgen(js_name = jspiIsThenable, catch)]
    fn shim_is_thenable(value: &JsValue) -> Result<bool, JsValue>;
    #[wasm_bindgen(js_name = jspiIsSuspendError)]
    fn shim_is_suspend_error(value: &JsValue) -> bool;
    #[wasm_bindgen(js_name = jspiPromising, catch)]
    fn shim_promising(function: &Function) -> Result<Function, JsValue>;
}

/// The results of an import, as a `WebAssembly.Suspending` function returns
/// them, or as its promise resolves with them.
pub(crate) fn results(values: &[Value], store: &impl AsStoreRef) -> JsValue {
    match values {
        [] => JsValue::UNDEFINED,
        [value] => value.as_jsvalue(store),
        values => Array::from_iter(values.iter().map(|value| value.as_jsvalue(store))).into(),
    }
}

/// Whether the results of an import can reach the guest as they are: a
/// promise takes a thenable it is resolved with for a promise, waits for it,
/// and gives the guest what it settles with. So an import whose result is one
/// fails instead, as one whose result cannot be read: reading `then` runs
/// JavaScript, which may throw. It runs before the store is taken back for
/// the guest, and may drop the call. The promise reads `then` again when it
/// is resolved; an object that answers differently, as a `Proxy` can, is
/// taken for what it answers then. If that is a thenable, the guest waits for
/// it with its store, also once its call is dropped: the engine resumes the
/// guest when the thenable settles, and nothing can stop that, so the store
/// is not given up before.
///
/// The engine gives a guest no function as a `funcref` but a WebAssembly
/// one, and throws an exception at the guest that the guest can catch
/// otherwise. That fails the import too.
pub(crate) fn deliverable(values: &[Value]) -> Result<(), RuntimeError> {
    for value in values {
        if let Value::FuncRef(Some(function)) = value
            && !shim_is_wasm_function(&function.as_js().handle.function)
        {
            return Err(RuntimeError::new(
                "an async host function returned a function that is not a WebAssembly \
                 function, which a guest cannot be given as a `funcref`",
            ));
        }
    }
    let value: JsValue = match values {
        [Value::ExternRef(Some(reference))] => match &reference.0 {
            crate::BackendExternRef::Js(reference) => reference.as_js_value(),
            #[allow(unreachable_patterns)]
            _ => return Ok(()),
        },
        [Value::FuncRef(Some(function))] => function.as_js().handle.function.clone().into(),
        // Nothing else is ever a thenable: several results reach the guest
        // as an array.
        _ => return Ok(()),
    };
    match shim_is_thenable(&value) {
        Ok(false) => Ok(()),
        Ok(true) => Err(RuntimeError::new(
            "an async host function returned a thenable, which a guest cannot be given \
             through JSPI as it is",
        )),
        Err(error) => Err(RuntimeError::new(format!(
            "reading `then` of what an async host function returned threw: {}",
            RuntimeError::from(error).message()
        ))),
    }
}

/// The function of an async host function of the store `store`: `start`,
/// which returns the promise the guest waits on, or, if it takes an `array`,
/// a function that calls it with its arguments in one. A guest imports it as
/// a `WebAssembly.Suspending` function, which it keeps (see [`import`]).
///
/// # Panics
/// Panics if the signature has a type JavaScript has no value of, or takes a
/// `funcref`, which this backend cannot make a function of.
pub(crate) fn host_function(
    ty: &FunctionType,
    store: StoreId,
    start: &JsValue,
    array: bool,
) -> Function {
    for ty in ty.params().iter().chain(ty.results()) {
        if matches!(ty, Type::V128 | Type::ExceptionRef) {
            panic!(
                "an async host function cannot take or return a `{ty}` on the JavaScript \
                 backend: JavaScript has no value of that type"
            );
        }
    }
    if ty.params().contains(&Type::FuncRef) {
        panic!(
            "an async host function cannot take a `funcref` on the JavaScript backend: \
             it cannot make a function of what a guest passes"
        );
    }
    SHIM.with(|_| ());
    shim_host(start, array, store.as_raw().get() as f64)
}

/// Records that `function`, a host function this crate made for the store
/// `id`, belongs to it: what it runs reaches that store through a pointer.
pub(crate) fn own(function: &JsValue, id: StoreId) {
    shim_own(function, id.as_raw().get() as f64);
}

/// Whether `function` can be used with the store `id`: a host function this
/// crate made, also given back as an embedder's function, with the store it
/// was made for only, which, being the caller's, is alive; any other
/// function with any store.
pub(crate) fn belongs_to(function: &JsValue, id: StoreId) -> bool {
    shim_owner(function).is_none_or(|store| store == id.as_raw().get() as f64)
}

/// The error of a host function called with another store than its own.
pub(crate) fn foreign_store_error() -> RuntimeError {
    RuntimeError::new("a host function was called with a store it does not belong to")
}

/// What a guest imports for `value`, a function of the type `ty`: the
/// `WebAssembly.Suspending` form of an async host function; for a JavaScript
/// function an embedder gave, a function that marks what it runs as entered
/// synchronously, calls `value` with its receiver and
/// arguments, and makes of what it returns the primitives the engine takes
/// for the results of `ty`, so that all the JavaScript the call runs, the
/// engine's conversion of the results included (`valueOf`, iteration), runs
/// marked; anything else, a WebAssembly function or a host function of this
/// crate, which marks what it runs itself, as it is.
///
/// Only a guest is given that function, the same one for the same function
/// and results: a direct call of `value` reads what it returns itself, as an
/// array of several results, and converts it as a direct call does.
pub(crate) fn import(value: &JsValue, ty: &FunctionType) -> JsValue {
    SHIM.with(|_| ());
    // The conversion each result takes from the engine: to a number, to a
    // BigInt, or none, which runs no JavaScript.
    let kinds: String = ty
        .results()
        .iter()
        .map(|ty| match ty {
            Type::I32 | Type::F32 | Type::F64 => 'n',
            Type::I64 => 'b',
            _ => 'r',
        })
        .collect();
    shim_import(value, &kinds)
}

/// The function `value` stands for, given to this crate: the async host
/// function whose `WebAssembly.Suspending` form it is, as [`public`] gives
/// it, or the embedder's own function for one [`import`] made; anything else
/// as it is.
pub(crate) fn host_of(value: &JsValue) -> JsValue {
    shim_host_of(value)
}

/// Whether `function` is a WebAssembly function, or an async host function:
/// one `call_async` enters a guest through. Anything else is JavaScript that
/// runs to the end.
pub(crate) fn enters_a_guest(function: &JsValue) -> bool {
    shim_is_host(function) || shim_is_wasm_function(function)
}

/// What a function is given to JavaScript as: the `WebAssembly.Suspending`
/// form of an async host function, which is what works outside a guest
/// Wasmer instantiated, and anything else as it is.
pub(crate) fn public(value: &JsValue) -> JsValue {
    shim_public(value)
}

pub(crate) fn cannot_suspend_error() -> RuntimeError {
    let code = TrapCode::YieldOutsideAsyncContext;
    let source = BackendTrap::Js(Trap::user(code.message().into()));
    RuntimeError::new_from_source(source, Vec::new(), Some(code))
}

/// [`cannot_suspend_error`], thrown as host functions throw their errors, so
/// that the caller gets this very error back.
fn cannot_suspend() -> JsValue {
    Trap::user(Box::new(cannot_suspend_error())).into()
}

/// The error of a guest the engine refused to suspend, if `error` is the
/// engine's `WebAssembly.SuspendError`: the engine refuses before the body
/// of an async host function runs, or after, on some engines.
pub(crate) fn refusal(error: &JsValue) -> Option<RuntimeError> {
    shim_is_suspend_error(error).then(cannot_suspend_error)
}

pub(crate) fn prepare_trap() -> Result<(), JsValue> {
    if !TRAP_READY.get() {
        shim_prepare_trap()?;
        TRAP_READY.set(true);
    }
    Ok(())
}

pub(crate) fn is_supported() -> bool {
    let Ok(webassembly) = Reflect::get(&js_sys::global(), &JsValue::from_str("WebAssembly")) else {
        return false;
    };
    Reflect::get(&webassembly, &JsValue::from_str("promising"))
        .is_ok_and(|value| value.is_function())
        && Reflect::get(&webassembly, &JsValue::from_str("Suspending"))
            .is_ok_and(|value| value.is_function())
}

/// The function `call_async` calls to enter `function`: its promising form,
/// or, for an async host function, the function itself.
pub(crate) fn promising(function: &Function) -> Result<Function, JsValue> {
    if shim_is_host(function) {
        return Ok(function.clone());
    }
    shim_promising(function)
}

/// A JavaScript boundary recovers holds and entry state even when a raw JS
/// exception skips every Rust destructor in a synchronous host invocation.
pub(crate) fn sync_host(value: &Function, id: StoreId) -> Function {
    SHIM.with(|_| ());
    shim_sync_host(value, id.as_raw().get())
}

/// The callback is borrowed for this synchronous call and never retained by JS.
/// Exceptions are caught before returning to the Rust executor stack.
pub(crate) fn host_callback(id: StoreId, callback: &JsValue) -> Result<(), JsValue> {
    SHIM.with(|_| ());
    shim_host_callback(id.as_raw().get(), callback)
}

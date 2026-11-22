//! Async calls on the JavaScript backend, through JSPI.
//!
//! The engine runs them. `Function::call_async` calls the
//! `WebAssembly.promising` form of a function and waits for its promise. An
//! async host function is a `WebAssembly.Suspending` import that returns a
//! promise of its host future, which runs on the microtask queue: the engine
//! suspends the guest on that promise, and resumes it when it settles.
//!
//! JavaScript runs one thing at a time, and a microtask only on an empty
//! stack, so a resumed guest overlaps nothing else that uses its store. A
//! guest therefore holds nothing. Only host code takes the store, for one
//! synchronous slice at a time: a host function while it runs, a host future
//! inside `AsyncEnv::with`.
//!
//! What is kept here is what the engine does not know:
//!
//! * Which call a guest belongs to ([`CURRENT`]): the one that entered it,
//!   and after a wait the one whose promise resumed it. A reaction attached
//!   to that promise before the engine attaches its own runs right before
//!   the guest does.
//! * How long a call's borrow of the store lasts. The engine resumes a guest
//!   with no `poll` on the stack, also once its call was dropped or
//!   forgotten. A dropped call gives the store up at once: its host future is
//!   dropped, and its guest is woken with the error of a trap, which no
//!   exception handler of the guest catches, so that its stack unwinds
//!   without running guest code and the engine frees it. A guest that was
//!   already resumed runs on until its next import, which fails. A forgotten
//!   call goes on to its end, and the
//!   [guest guard](crate::backend::js::guard) keeps its store alive and out
//!   of its owner's reach until then.
//!
//! After its first poll a call makes progress whether or not it is polled
//! again, as a promise does.
//!
//! The engine refuses to suspend a guest that was entered by a synchronous
//! call, or that has a JavaScript frame on the stack since it was entered: an
//! async host function fails there, ready or not, with the error other
//! backends give ([`refusal`]).

use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll, Waker},
};

use js_sys::{Array, Function, Reflect};
use wasm_bindgen::{
    JsCast, JsValue,
    prelude::{Closure, wasm_bindgen},
};
use wasm_bindgen_futures::spawn_local;
use wasmer_types::{FunctionType, StoreId, TrapCode, Type};

use crate::{
    AsStoreRef, AsyncCallRequest, BackendTrap, CallStore, PollScope, PollStore, RuntimeError,
    SharedStore, StoreInner, StoreMut, StoreSlice, Value,
    js::{
        boundary,
        error::Trap,
        function::{enter_async, entered_synchronously, mark_entered},
        guard::GuestGuard,
        lent,
        utils::convert::{AsJs as _, js_value_to_wasmer},
    },
};

/// The host's half of an async import: the host function and what it awaits.
/// Nothing of it has run until it is polled.
pub(crate) type ImportFuture = Pin<Box<dyn Future<Output = Result<Vec<Value>, RuntimeError>>>>;

// The JavaScript half. It is static JavaScript, not made from a string: some
// hosts forbid that.
//
// An async host function and its `WebAssembly.Suspending` form, which a guest
// imports, find each other in the maps below.
#[wasm_bindgen(inline_js = r#"
const suspendingOf = new WeakMap(), hostOf = new WeakMap();
const storeOf = new WeakMap(), promisingOf = new WeakMap();
const noop = () => {};
let trapError;
export function jspiHost(start, array, store) {
  const host = array ? (...args) => start(args) : start;
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
export function jspiPublic(value) {
  return suspendingOf.get(value) ?? value;
}
export function jspiHostOf(value) {
  return hostOf.get(value) ?? value;
}
export function jspiIsHost(value) {
  return suspendingOf.has(value);
}
// The promising form of each function, made once; `null` for a function that
// is not a WebAssembly one, which the engine refuses.
export function jspiPromising(fn) {
  let promising = promisingOf.get(fn);
  if (promising === undefined) {
    try { promising = WebAssembly.promising(fn); } catch { promising = null; }
    promisingOf.set(fn, promising);
  }
  return promising;
}
// The promise a guest waits on, and what settles it. `resumed` is attached
// before the engine attaches its own reaction, so it runs right before the
// guest is resumed. A rejection has a handler too: a wait the engine refused
// is rejected with nobody waiting on it.
export function jspiWait(resumed) {
  let resolve, reject;
  const promise = new Promise((s, j) => { resolve = s; reject = j; });
  promise.then(resumed, noop);
  return [promise, resolve, reject];
}
export function jspiSettle(promise, done) {
  promise.then((value) => done(false, value), (error) => done(true, error));
}
// The error of a trap: the one thing a guest's exception handlers do not
// catch, also when JavaScript throws it. It is made by trapping in
// `(module (func (export "trap") unreachable))`. A host that refuses to
// compile that gives its own error instead, which a guest may catch.
export function jspiTrapError() {
  if (trapError === undefined) {
    try {
      const bytes = new Uint8Array([0, 97, 115, 109, 1, 0, 0, 0, 1, 4, 1, 96, 0, 0, 3, 2, 1, 0,
        7, 8, 1, 4, 116, 114, 97, 112, 0, 0, 10, 5, 1, 3, 0, 0, 11]);
      new WebAssembly.Instance(new WebAssembly.Module(bytes)).exports.trap();
    } catch (error) {
      trapError = error;
    }
  }
  return trapError;
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
"#)]
extern "C" {
    #[wasm_bindgen(js_name = jspiHost)]
    fn shim_host(start: &JsValue, array: bool, store: f64) -> Function;
    #[wasm_bindgen(js_name = jspiOwn)]
    fn shim_own(value: &JsValue, store: f64);
    #[wasm_bindgen(js_name = jspiOwner)]
    fn shim_owner(value: &JsValue) -> Option<f64>;
    #[wasm_bindgen(js_name = jspiPublic)]
    fn shim_public(value: &JsValue) -> JsValue;
    #[wasm_bindgen(js_name = jspiHostOf)]
    fn shim_host_of(value: &JsValue) -> JsValue;
    #[wasm_bindgen(js_name = jspiIsHost)]
    fn shim_is_host(value: &JsValue) -> bool;
    #[wasm_bindgen(js_name = jspiPromising)]
    fn shim_promising(function: &Function) -> Option<Function>;
    #[wasm_bindgen(js_name = jspiWait)]
    fn shim_wait(resumed: &JsValue) -> Array;
    #[wasm_bindgen(js_name = jspiSettle)]
    fn shim_settle(promise: &JsValue, done: &JsValue);
    #[wasm_bindgen(js_name = jspiTrapError)]
    fn trap_error() -> JsValue;
    #[wasm_bindgen(js_name = jspiIsSuspendError)]
    fn shim_is_suspend_error(value: &JsValue) -> bool;
}

/// The results of an import, as a `WebAssembly.Suspending` function returns
/// them, or as its promise resolves with them.
fn results(values: &[Value], store: &impl AsStoreRef) -> JsValue {
    match values {
        [] => JsValue::UNDEFINED,
        [value] => value.as_jsvalue(store),
        values => Array::from_iter(values.iter().map(|value| value.as_jsvalue(store))).into(),
    }
}

/// The function of an async host function of the store `store`: `start`,
/// which returns the promise the guest waits on, or, if it takes an `array`,
/// a function that calls it with its arguments in one. A guest imports it as
/// a `WebAssembly.Suspending` function (see [`public`]).
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
    shim_host(start, array, store.as_raw().get() as f64)
}

/// Records that `function`, a host function this crate made for the store
/// `id`, belongs to it: what it runs reaches that store through a pointer.
pub(crate) fn own(function: &JsValue, id: StoreId) {
    shim_own(function, id.as_raw().get() as f64);
}

/// Whether `function` can be used with the store `id`: a host function this
/// crate made, also given back as an embedder's function, with the store it
/// was made for only; any other function with any store.
pub(crate) fn belongs_to(function: &JsValue, id: StoreId) -> bool {
    shim_owner(function).is_none_or(|store| store == id.as_raw().get() as f64)
}

/// The error of a host function called with another store than its own.
pub(crate) fn foreign_store_error() -> RuntimeError {
    RuntimeError::new("a host function was called with a store it does not belong to")
}

/// What a function is given to JavaScript as, and what a guest imports for
/// it: the `WebAssembly.Suspending` form of an async host function, and
/// anything else as it is.
pub(crate) fn public(value: &JsValue) -> JsValue {
    shim_public(value)
}

/// The function `value` stands for, given to this crate: the async host
/// function whose public form it is, and anything else as it is.
pub(crate) fn host_of(value: &JsValue) -> JsValue {
    shim_host_of(value)
}

/// The promise a guest waits on in an async import, and what it waits for.
struct Wait {
    /// Which wait of its call this is: a new one takes the place of one the
    /// engine refused.
    id: u32,
    /// The host future, but while it is being polled.
    import: Option<ImportFuture>,
    resolve: Function,
    reject: Function,
}

/// One `Function::call_async` that entered a guest.
struct Activation {
    store_id: StoreId,
    guard: Rc<GuestGuard>,
    /// How the host code of the call reaches the store, until the call is
    /// over: finished or dropped.
    store: Cell<Option<PollStore>>,
    /// Keeps a shared store alive for as long.
    shared: RefCell<Option<SharedStore>>,
    /// The call counts in the guard as holding the store through a borrow.
    counted: Cell<bool>,
    /// The call was dropped: its guest must not use the store again.
    cancelled: Cell<bool>,
    /// The guest's promise settled.
    done: Cell<bool>,
    /// What the guest waits for, while it is suspended in an async import.
    wait: RefCell<Option<Wait>>,
    waits: Cell<u32>,
    /// The error of a host function, kept as it is across the guest's trap.
    error: RefCell<Option<RuntimeError>>,
    /// How the guest's promise settled.
    result: RefCell<Option<Result<JsValue, JsValue>>>,
    /// Whoever awaits the call: the waker of its last poll.
    caller: RefCell<Option<Waker>>,
    /// The reaction that runs right before the engine resumes the guest.
    resumed: RefCell<Option<Reaction>>,
}

/// A promise reaction: it is given what the promise settled with.
type Reaction = Closure<dyn FnMut(JsValue)>;

thread_local! {
    /// The guest of each store that entered or was resumed last: the one that
    /// is running, if one of that store is. There are few.
    static CURRENT: RefCell<Vec<Rc<Activation>>> = const { RefCell::new(Vec::new()) };
    /// An async host function is being called directly: see [`Direct`].
    static DIRECT: RefCell<Option<Direct>> = const { RefCell::new(None) };
}

/// The call of the guest of the store `id` that is running.
fn current(id: usize) -> Option<Rc<Activation>> {
    CURRENT.with(|current| {
        let current = current.borrow();
        let mut current = current.iter();
        current.find(|activation| activation.store_id.as_raw().get() == id).cloned()
    })
}

/// Makes `activation` the running guest of its store. Returns the one before.
fn set_current(activation: &Rc<Activation>) -> Option<Rc<Activation>> {
    CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        let slot = current.iter_mut().find(|other| other.store_id == activation.store_id);
        match slot {
            Some(slot) => Some(std::mem::replace(slot, Rc::clone(activation))),
            None => {
                current.push(Rc::clone(activation));
                None
            }
        }
    })
}

/// `activation` is not running any more, if it was.
fn unset_current(activation: &Rc<Activation>) {
    // Dropped outside the borrow.
    let _was = CURRENT.with(|current| {
        let mut current = current.borrow_mut();
        let index = current.iter().position(|other| Rc::ptr_eq(other, activation))?;
        Some(current.swap_remove(index))
    });
}

impl Activation {
    /// The reaction of every wait of this call: the engine resumes its guest
    /// next, from a microtask, so nothing else is on the stack.
    fn resumed(self: &Rc<Self>) -> JsValue {
        let mut resumed = self.resumed.borrow_mut();
        let closure = resumed.get_or_insert_with(|| {
            let this = Rc::downgrade(self);
            Closure::new(move |_: JsValue| {
                if let Some(this) = this.upgrade() {
                    this.clear_holds();
                    // The guest is on the stack `call_async` gave it.
                    mark_entered(false);
                    set_current(&this);
                }
            })
        });
        closure.as_ref().clone()
    }

    /// Nothing is on the stack, so nothing holds the store: a JavaScript
    /// exception may have skipped the frame of a host function that did.
    fn clear_holds(&self) {
        self.guard.set_held(false);
        if let Some(store) = self.store.get() {
            store.set_busy(false);
        }
    }

    /// The guest is to wait for `import`. Returns the promise it waits on,
    /// and has the host future run on the microtask queue.
    fn wait_for(self: &Rc<Self>, import: ImportFuture) -> JsValue {
        // A wait is still there: the engine ran this body for it and then
        // refused to suspend the guest, which went on to here.
        self.cancel_wait();
        let id = self.waits.get().wrapping_add(1);
        self.waits.set(id);
        let parts = shim_wait(&self.resumed());
        *self.wait.borrow_mut() = Some(Wait {
            id,
            import: Some(import),
            resolve: parts.get(1).unchecked_into(),
            reject: parts.get(2).unchecked_into(),
        });
        let this = Rc::clone(self);
        spawn_local(std::future::poll_fn(move |cx| this.drive(id, cx)));
        parts.get(0)
    }

    /// Polls what the guest waits for in its wait `id`, from a microtask: the
    /// guest is suspended, and the store is free.
    fn drive(self: &Rc<Self>, id: u32, cx: &mut Context<'_>) -> Poll<()> {
        let import = {
            let mut wait = self.wait.borrow_mut();
            let wait = wait.as_mut().filter(|wait| wait.id == id);
            wait.and_then(|wait| wait.import.take())
        };
        // No import: the wait is over, cancelled or replaced. A wait has its
        // call's store.
        let (Some(mut import), Some(store)) = (import, self.store.get()) else {
            return Poll::Ready(());
        };
        unset_current(self);
        self.clear_holds();
        // The host future may drop its own call.
        let _shared = self.shared.borrow().clone();
        // No host function that JavaScript reaches from the host future runs
        // with the store meanwhile (see `hold_running`).
        self.guard.set_held(true);
        let outcome = {
            let _scope = PollScope::enter(Some(store));
            boundary::catching(|| import.as_mut().poll(cx))
        };
        self.guard.set_held(false);
        let result = match outcome {
            Ok(Poll::Pending) => {
                let mut wait = self.wait.borrow_mut();
                if let Some(wait) = wait.as_mut().filter(|wait| wait.id == id) {
                    wait.import = Some(import);
                    return Poll::Pending;
                }
                drop(wait);
                // The poll dropped the call.
                dispose(Some(store), import);
                return Poll::Ready(());
            }
            Ok(Poll::Ready(result)) => {
                dispose(Some(store), import);
                result
            }
            Err(error) => {
                // Left part way through: it can be neither resumed nor
                // dropped. It holds no store borrow.
                std::mem::forget(import);
                Err(error.into())
            }
        };
        self.finish_wait(id, result);
        Poll::Ready(())
    }

    /// Settles the promise of the wait `id`, if the guest still waits on it:
    /// with the results of its import, or with a trap, which unwinds it.
    fn finish_wait(&self, id: u32, result: Result<Vec<Value>, RuntimeError>) {
        let wait = {
            let mut wait = self.wait.borrow_mut();
            wait.take_if(|wait| wait.id == id)
        };
        let (Some(wait), Some(store)) = (wait, self.store.get()) else {
            return;
        };
        match result {
            Ok(values) => {
                let slice = store.enter_slice();
                // Safety: the slice is the only holder of the store.
                let store = unsafe { StoreMut::from_raw(slice.ptr()) };
                let _ = wait.resolve.call1(&JsValue::UNDEFINED, &results(&values, &store));
            }
            Err(error) => {
                *self.error.borrow_mut() = Some(error);
                let _ = wait.reject.call1(&JsValue::UNDEFINED, &trap_error());
            }
        }
    }

    /// The guest is not to be given what it waits for: its host future is
    /// dropped, and it is woken with a trap.
    fn cancel_wait(&self) {
        let Some(mut wait) = self.wait.take() else {
            return;
        };
        if let Some(import) = wait.import.take() {
            dispose(self.store.get(), import);
        }
        let _ = wait.reject.call1(&JsValue::UNDEFINED, &trap_error());
    }

    /// The call is over for whoever made it, finished or dropped: its borrow
    /// of the store ends here.
    fn release(&self) {
        self.store.set(None);
        drop(self.shared.take());
        if self.counted.replace(false) {
            self.guard.end_call();
        }
    }

    /// The end of the activation: its guest's promise settled.
    fn retire(self: &Rc<Self>, result: Result<JsValue, JsValue>) {
        self.done.set(true);
        unset_current(self);
        self.cancel_wait();
        self.release();
        drop(self.resumed.take());
        *self.result.borrow_mut() = Some(result);
        // Not woken under the borrow: a waker may do anything.
        let caller = self.caller.take();
        if let Some(caller) = caller {
            caller.wake();
        }
    }
}

/// Drops a host future with its store in reach of its destructor. What
/// JavaScript throws from there has nowhere to go.
fn dispose(store: Option<PollStore>, future: ImportFuture) {
    let _scope = PollScope::enter(store);
    let mut future = Some(future);
    let _ = boundary::catching(|| drop(future.take()));
}

/// A host function's hold on the store of a guest of `call_async` that the
/// engine resumed: see [`hold_running`].
pub(crate) struct Held {
    // Dropped in this order: the slice is of the store the handle keeps.
    _slice: StoreSlice,
    guard: Rc<GuestGuard>,
    _shared: Option<SharedStore>,
}

impl Drop for Held {
    fn drop(&mut self) {
        self.guard.set_held(false);
    }
}

/// The store whose raw id is `id`, for one of its host functions to run with
/// in a guest of `call_async` that the engine resumed, which no call lends
/// the store (see `lent`), and a hold on it. `None` if no such guest runs in
/// that store; an error if its call was dropped, or if host code holds the
/// store already.
pub(crate) fn hold_running(id: usize) -> Option<Result<(*mut StoreInner, Held), RuntimeError>> {
    let activation = current(id)?;
    // The call was dropped: nothing may touch the store on its guest's behalf.
    if activation.cancelled.get() {
        return Some(Err(RuntimeError::new(
            "a host function of a guest whose call was dropped cannot use the store",
        )));
    }
    let store = activation.store.get()?;
    let guard = &activation.guard;
    let slice = (!guard.is_held()).then(|| store.try_enter_slice()).flatten();
    let Some(slice) = slice else {
        return Some(Err(lent::held()));
    };
    guard.set_held(true);
    let held = Held {
        guard: Rc::clone(guard),
        _shared: activation.shared.borrow().clone(),
        _slice: slice,
    };
    Some(Ok((held._slice.ptr(), held)))
}

/// The error of an async host function whose guest cannot be suspended: it
/// was not entered by `call_async`, or it was, with a synchronous call or
/// another JavaScript frame on the stack since. The same error as on other
/// backends.
fn cannot_suspend_error() -> RuntimeError {
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
/// engine's `WebAssembly.SuspendError`.
pub(crate) fn refusal(error: &JsValue) -> Option<RuntimeError> {
    shim_is_suspend_error(error).then(cannot_suspend_error)
}

/// A direct call of an async host function, by `call_async`: no guest, and
/// nothing for the engine to suspend. The body hands its host future over,
/// and the call runs it itself.
enum Direct {
    /// The function is being called.
    Called,
    /// Its body ran: this is its host future.
    Started(ImportFuture),
}

/// The body of an async host function of the store `id`. Returns the promise
/// its guest waits on; nothing of the host function runs here.
///
/// A guest that was entered synchronously cannot be suspended, and some
/// engines run this body before they refuse to: it refuses first. Synchronous
/// calls of this crate mark the guest they enter, and so do its host
/// functions for whatever they run.
pub(crate) fn start(
    id: StoreId,
    import: impl FnOnce() -> ImportFuture,
) -> Result<JsValue, JsValue> {
    if DIRECT.with(|direct| direct.borrow().is_some()) {
        DIRECT.with(|direct| *direct.borrow_mut() = Some(Direct::Started(import())));
        return Ok(JsValue::UNDEFINED);
    }
    if entered_synchronously() {
        return Err(cannot_suspend());
    }
    let activation = current(id.as_raw().get()).ok_or_else(cannot_suspend)?;
    // A guest whose call is over is unwound instead.
    if activation.cancelled.get() || activation.store.get().is_none() {
        return Err(trap_error());
    }
    Ok(activation.wait_for(import()))
}

/// The caller's side of an activation. Dropping it cancels the call.
struct Call(Rc<Activation>);

impl Call {
    /// Enters a guest of the store `store` through `enter`, which calls a
    /// promising function. The guest's first slice runs here, with `slice`
    /// lent to its host functions as a synchronous call lends its store.
    fn enter(
        store_id: StoreId,
        store: PollStore,
        slice: StoreSlice,
        enter: impl FnOnce() -> Result<JsValue, JsValue>,
    ) -> Self {
        let guard = store.keeper().unwrap_js().0;
        let counted = store.is_exclusive();
        if counted {
            guard.begin_call();
        }
        let activation = Rc::new(Activation {
            store_id,
            guard,
            store: Cell::new(Some(store)),
            shared: RefCell::new(store.shared()),
            counted: Cell::new(counted),
            cancelled: Cell::new(false),
            done: Cell::new(false),
            wait: RefCell::new(None),
            waits: Cell::new(0),
            error: RefCell::new(None),
            result: RefCell::new(None),
            caller: RefCell::new(None),
            resumed: RefCell::new(None),
        });
        let entered = match lent::lend_raw(store_id, slice.ptr()) {
            Ok(_lent) => {
                // A sync host function of this guest holds the store, and
                // must not find it published for a host future as well.
                let _hidden = PollScope::enter(None);
                let _entered = enter_async();
                let before = set_current(&activation);
                let entered = enter();
                // A guest this call was made in, from a host function, goes
                // on. With none, this one stays: it may be suspended where
                // this module does not see it resumed.
                if let Some(before) = before {
                    set_current(&before);
                }
                entered
            }
            Err(error) => {
                *activation.error.borrow_mut() = Some(error);
                Err(JsValue::UNDEFINED)
            }
        };
        // The guest left the stack.
        drop(slice);
        match entered {
            // A promising function returns a promise, or throws before it
            // enters the guest. The reaction is attached here, before
            // anything else can run: a rejection nobody handles ends the
            // process on some hosts.
            Ok(promise) => {
                let this = Rc::clone(&activation);
                let done = Closure::once_into_js(move |rejected: bool, value: JsValue| {
                    this.retire(if rejected { Err(value) } else { Ok(value) });
                });
                shim_settle(&promise, &done);
            }
            Err(error) => activation.retire(Err(error)),
        }
        Self(activation)
    }
}

impl Future for Call {
    type Output = Result<JsValue, RuntimeError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let activation = &self.0;
        if let Some(result) = activation.result.take() {
            // The trap of a failed host function stands for its error.
            let error = |error: JsValue| activation.error.take().unwrap_or_else(|| error.into());
            return Poll::Ready(result.map_err(error));
        }
        let mut caller = activation.caller.borrow_mut();
        if !caller.as_ref().is_some_and(|caller| caller.will_wake(cx.waker())) {
            *caller = Some(cx.waker().clone());
        }
        Poll::Pending
    }
}

impl Drop for Call {
    fn drop(&mut self) {
        let activation = &self.0;
        drop(activation.caller.take());
        if activation.done.get() {
            return;
        }
        // A guest suspended in an async import is unwound. One the engine
        // resumed already runs on until its next import, which fails.
        activation.cancelled.set(true);
        activation.cancel_wait();
        activation.release();
    }
}

/// The host future of an async host function that `call_async` calls
/// directly, run by the future of that call.
struct HostCall<'s, 'a> {
    store: &'s CallStore<'a>,
    future: Option<ImportFuture>,
}

impl Future for HostCall<'_, '_> {
    type Output = Result<Vec<Value>, RuntimeError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let store = self.store.resolve();
        let mut future = self.future.take().expect("polled after it finished");
        let outcome = {
            let _scope = PollScope::enter(Some(store));
            boundary::catching(|| future.as_mut().poll(cx))
        };
        match outcome {
            Ok(Poll::Pending) => {
                self.future = Some(future);
                Poll::Pending
            }
            Ok(Poll::Ready(result)) => {
                dispose(Some(store), future);
                Poll::Ready(result)
            }
            Err(error) => {
                // Left part way through: it can be neither resumed nor
                // dropped.
                std::mem::forget(future);
                Poll::Ready(Err(error.into()))
            }
        }
    }
}

impl Drop for HostCall<'_, '_> {
    fn drop(&mut self) {
        if let Some(future) = self.future.take() {
            dispose(self.store.try_resolve(), future);
        }
    }
}

/// `Function::call_async` on this backend.
pub(crate) async fn call<'a, X: 'static>(
    store: CallStore<'a>,
    request: AsyncCallRequest<X>,
) -> Result<X, RuntimeError> {
    let AsyncCallRequest {
        function,
        params,
        finish,
    } = request;
    let handle = function.as_js().handle.clone();
    let id = store.id();
    // A host function runs with the store it was made for only.
    if !belongs_to(&handle.function, id) {
        return Err(foreign_store_error());
    }

    // The store, for the slice that converts the arguments and enters.
    let poll = store.resolve();
    let slice = poll.enter_slice();
    // Safety: the slice is the only holder of the store until it is dropped.
    let mut store_mut = unsafe { StoreMut::from_raw(slice.ptr()) };
    let params = params(&mut store_mut)?;
    // As on other backends: an async host function, which this calls
    // directly, could not take them, and nothing converts them.
    let signature = &handle.ty;
    if !crate::js::function::values_match(&params, signature.params()) {
        let types: Vec<String> = params.iter().map(|param| param.ty().to_string()).collect();
        return Err(RuntimeError::new(format!(
            "Parameters of type [{}] did not match signature {signature}",
            types.join(", "),
        )));
    }
    let arguments = Array::new_with_length(params.len() as u32);
    for (index, param) in params.iter().enumerate() {
        arguments.set(index as u32, param.as_jsvalue(&store_mut));
    }

    let results: Vec<Value> = if shim_is_host(&handle.function) {
        // An async host function: its body hands its host future over, and
        // this call runs it. Its results are what it returned, as they are.
        DIRECT.with(|direct| *direct.borrow_mut() = Some(Direct::Called));
        let called = Reflect::apply(&handle.function, &JsValue::NULL, &arguments);
        let started = DIRECT.with(|direct| direct.borrow_mut().take());
        called?;
        let Some(Direct::Started(future)) = started else {
            unreachable!("the body of an async host function runs when it is called");
        };
        drop(slice);
        let future = Some(future);
        let values = HostCall { store: &store, future }.await?;
        // They were checked against the signature the host function was made
        // with; this handle may give it another (`Function::from_jsvalue`),
        // by which its caller reads them.
        if !crate::js::function::values_match(&values, signature.results()) {
            let types: Vec<String> = values.iter().map(|value| value.ty().to_string()).collect();
            return Err(RuntimeError::new(format!(
                "an async host function returned values of type [{}], which do not match \
                 the signature it is called with, {signature}",
                types.join(", "),
            )));
        }
        values
    } else if let Some(promising) = handle.promising() {
        // A guest. Dropping this future while it waits cancels the call.
        let enter = || Reflect::apply(&promising, &JsValue::NULL, &arguments);
        let result = Call::enter(id, poll, slice, enter).await?;
        let slice = store.resolve().enter_slice();
        // Safety: as above.
        let mut store_mut = unsafe { StoreMut::from_raw(slice.ptr()) };
        let types = signature.results();
        match types.len() {
            0 => Vec::new(),
            1 => vec![js_value_to_wasmer(&mut store_mut, &types[0], &result)],
            _ => {
                let result: Array = result.into();
                let types = types.iter().enumerate();
                types
                    .map(|(index, ty)| {
                        js_value_to_wasmer(&mut store_mut, ty, &result.get(index as u32))
                    })
                    .collect()
            }
        }
    } else {
        // JavaScript that runs to the end: a host function, or one an
        // embedder gave. It is called as `call` calls it.
        let results = function.call(&mut store_mut, &params)?;
        return finish(&mut store_mut, &results);
    };

    let slice = store.resolve().enter_slice();
    // Safety: as above.
    let mut store_mut = unsafe { StoreMut::from_raw(slice.ptr()) };
    finish(&mut store_mut, &results)
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
/// made once. `None` for a function that is not a WebAssembly one.
pub(crate) fn promising(function: &Function) -> Option<Function> {
    shim_promising(function)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Function, Store, Type};
    use js_sys::Promise;
    use wasm_bindgen_futures::JsFuture;
    use wasm_bindgen_test::wasm_bindgen_test;

    async fn tick() {
        JsFuture::from(Promise::resolve(&JsValue::UNDEFINED))
            .await
            .unwrap();
    }

    /// The guests this module takes for running.
    fn running() -> usize {
        CURRENT.with(|current| current.borrow().len())
    }

    /// However a call ends, nothing of it stays in this module's state.
    #[cfg(feature = "wat")]
    #[wasm_bindgen_test]
    async fn a_call_that_ended_leaves_nothing_behind() {
        let mut store = Store::default();
        // Fails when asked to, after waiting for a tick.
        let ty = FunctionType::new([Type::I32], []);
        let env = crate::FunctionEnv::new(&mut store, ());
        let host = Function::new_async(
            &mut store,
            &env,
            ty,
            async |_: &mut crate::AsyncEnv<()>, arguments: &[Value]| {
                let fail = arguments[0].unwrap_i32() != 0;
                tick().await;
                if fail {
                    return Err(RuntimeError::new("failed"));
                }
                Ok(vec![])
            },
        );
        let module = crate::Module::new(
            &store,
            r#"(module
              (import "h" "f" (func $f (param i32)))
              (func (export "run") (param i32) (call $f (local.get 0))))"#,
        )
        .unwrap();
        let imports = crate::imports! { "h" => { "f" => host.clone() } };
        let instance = crate::Instance::new(&mut store, &module, &imports).unwrap();
        let run = instance.exports.get_function("run").unwrap().clone();
        let store = store.into_shared();
        assert_eq!(running(), 0);

        for function in [&run, &host] {
            let call =
                |fail: i32| Box::pin(function.call_async(&mut store.clone(), &[Value::I32(fail)]));

            // Returns.
            call(0).await.unwrap();
            assert_eq!(running(), 0, "returned");

            // Fails.
            let error = call(1).await.unwrap_err();
            assert!(error.message().contains("failed"), "{error}");
            assert_eq!(running(), 0, "failed");

            // Is cancelled: its guest is unwound in a microtask.
            let mut cancelled = call(0);
            let mut cx = Context::from_waker(Waker::noop());
            assert!(cancelled.as_mut().poll(&mut cx).is_pending());
            drop(cancelled);
            for _ in 0..3 {
                tick().await;
            }
            assert_eq!(running(), 0, "cancelled");

            // Cannot be suspended.
            let refused = store.try_with(|mut lock| function.call(&mut lock, &[Value::I32(0)]));
            refused.unwrap().unwrap_err();
            assert_eq!(running(), 0, "refused");
        }
        assert!(store.into_store().is_ok());
    }

    /// `WebAssembly.promising` is called for the first `call_async` of a
    /// function, and never again: every function of the same JavaScript
    /// function gets the same promising form.
    #[cfg(feature = "wat")]
    #[wasm_bindgen_test]
    async fn the_promising_wrapper_of_a_function_is_kept() {
        let mut store = Store::default();
        let module = crate::Module::new(
            &store,
            r#"(module (func (export "id") (param i32) (result i32) (local.get 0)))"#,
        )
        .unwrap();
        let instance = crate::Instance::new(&mut store, &module, &crate::Imports::new()).unwrap();
        let host = instance.exports.get_function("id").unwrap().clone();
        let again = instance.exports.get_function("id").unwrap().clone();
        let form = |function: &Function| promising(&function.as_js().handle.function).unwrap();

        host.call_async(&mut store, &[Value::I32(1)]).await.unwrap();
        let first = form(&host);
        again.call_async(&mut store, &[Value::I32(2)]).await.unwrap();
        assert!(js_sys::Object::is(&first, &form(&host)));
        assert!(js_sys::Object::is(&first, &form(&again)));
    }
}

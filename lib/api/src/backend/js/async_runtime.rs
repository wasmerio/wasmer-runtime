//! JSPI implementation of the borrowing async API.
//!
//! Rust drives host futures only from `Call::poll`. JavaScript owns the guest
//! stack, so every started activation owns the store allocation until its
//! promise settles. An exclusive activation also closes public store entry;
//! this remains necessary when a caller forgets its Rust future.

use std::{
    cell::{Cell, RefCell},
    future::Future,
    pin::Pin,
    rc::{Rc, Weak},
    task::{Context, Poll, Waker},
};

use js_sys::{Array, Reflect};
use wasm_bindgen::{
    JsCast, JsValue,
    closure::ScopedClosure,
    prelude::{Closure, wasm_bindgen},
};
use wasmer_types::{FunctionType, StoreId};

use crate::{
    AsStoreMut, Function, RuntimeError, StoreInner, Value,
    async_runtime::{HostFuture, StoreAccess, with_access},
    js::{
        guest_guard::Guest,
        jspi, lent,
        utils::convert::{AsJs, js_value_to_wasmer},
    },
};

type Factory = Rc<dyn Fn(Vec<Value>) -> HostFuture>;
type ResultFuture = Pin<Box<dyn Future<Output = Result<Box<[Value]>, RuntimeError>>>>;

#[derive(Clone, Copy, PartialEq)]
enum State {
    Running,
    Waiting,
    Woken,
    Unwinding,
    Done,
}

struct Activation {
    token: u32,
    access: StoreAccess,
    guest: Guest,
    direct: bool,
    state: Cell<State>,
    cancelled: Cell<bool>,
    accepted: Cell<bool>,
    held: Cell<bool>,
    import: RefCell<Option<HostFuture>>,
    error: RefCell<Option<RuntimeError>>,
    returned: RefCell<Option<Vec<Value>>>,
    result: RefCell<Option<Result<JsValue, JsValue>>>,
    caller: RefCell<Option<Waker>>,
    driving: Cell<bool>,
}

thread_local! {
    static ACTIVATIONS: RefCell<Vec<Rc<Activation>>> = const { RefCell::new(Vec::new()) };
    static NEXT: Cell<u32> = const { Cell::new(0) };
    static HOSTS: RefCell<Vec<HostCheckpoint>> = const { RefCell::new(Vec::new()) };
    static DIRECT: RefCell<Option<Rc<Activation>>> = const { RefCell::new(None) };
    static ENTRY_WAITERS: RefCell<Vec<Weak<RefCell<Option<Waker>>>>> = const { RefCell::new(Vec::new()) };
    static SHIM: () = shim_init(
        &Closure::wrap(Box::new(done) as Box<dyn Fn(u32, bool, JsValue)>).into_js_value(),
        &Closure::wrap(Box::new(check) as Box<dyn Fn(u32)>).into_js_value(),
        &Closure::wrap(Box::new(woken) as Box<dyn Fn(u32)>).into_js_value(),
    );
}

#[wasm_bindgen(inline_js = r#"
let done, check, woken;
const waits = new Map(), wakes = new Map(), hosts = new WeakMap();
export function scopedOwnAdapter(adapter, host) { hosts.set(adapter, host); }
export function scopedAdapterHost(adapter) { return hosts.get(adapter); }
const apply = Reflect.apply, then = Promise.prototype.then;
export function scopedInit(d, c, w) { done = d; check = c; woken = w; }
export function scopedWait(token) {
  return new Promise((resolve, reject) => { waits.set(token, {resolve, reject}); });
}
export function scopedCheck(token) { queueMicrotask(() => queueMicrotask(() => check(token))); }
export function scopedSettle(promise, token) {
  apply(then, promise, [(value) => done(token, false, value), (error) => done(token, true, error)]);
}
export function scopedWake(token, value) {
  const wake = {};
  wakes.set(token, wake);
  queueMicrotask(() => {
    if (wakes.get(token) !== wake) return;
    wakes.delete(token);
    const wait = waits.get(token);
    waits.delete(token);
    if (wait) { woken(token); wait.resolve(value); }
  });
}
export function scopedUnwind(token, error) {
  wakes.delete(token);
  const wait = waits.get(token);
  waits.delete(token);
  if (wait) wait.reject(error);
}
"#)]
extern "C" {
    #[wasm_bindgen(js_name = scopedOwnAdapter)]
    fn own_adapter(adapter: &JsValue, host: &JsValue);
    #[wasm_bindgen(js_name = scopedAdapterHost)]
    fn adapter_host(adapter: &JsValue) -> JsValue;
    #[wasm_bindgen(js_name = scopedInit)]
    fn shim_init(done: &JsValue, check: &JsValue, woken: &JsValue);
    #[wasm_bindgen(js_name = scopedWait)]
    fn shim_wait(token: u32) -> JsValue;
    #[wasm_bindgen(js_name = scopedCheck)]
    fn shim_check(token: u32);
    #[wasm_bindgen(js_name = scopedSettle)]
    fn shim_settle(promise: &JsValue, token: u32);
    #[wasm_bindgen(js_name = scopedWake)]
    fn shim_wake(token: u32, value: &JsValue);
    #[wasm_bindgen(js_name = scopedUnwind)]
    fn shim_unwind(token: u32, error: &JsValue);
}

fn activation(token: u32) -> Option<Rc<Activation>> {
    ACTIVATIONS.with(|all| all.borrow().iter().find(|a| a.token == token).cloned())
}

fn running(id: StoreId) -> Option<Rc<Activation>> {
    ACTIVATIONS.with(|all| {
        all.borrow()
            .iter()
            .rev()
            .find(|a| {
                a.access.store_id() == id
                    && match a.state.get() {
                        State::Running => true,
                        State::Waiting => !a.accepted.get(),
                        _ => false,
                    }
            })
            .cloned()
    })
}

fn wake_entries() {
    let waiters = ENTRY_WAITERS.with(|waiters| std::mem::take(&mut *waiters.borrow_mut()));
    for waiter in waiters {
        if let Some(waiter) = waiter.upgrade() {
            let waker = waiter.borrow_mut().take();
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }
}

fn entry_blocked(id: StoreId) -> bool {
    ACTIVATIONS.with(|all| {
        all.borrow().iter().any(|a| {
            a.access.store_id() == id
                && a.state.get() != State::Done
                && (a.cancelled.get()
                    || matches!(a.state.get(), State::Running | State::Woken)
                    || (a.state.get() == State::Waiting && !a.accepted.get()))
        })
    })
}

/// A cancelled engine stack may still resume. A new synchronous guest cannot
/// wait for it to leave; it fails before entering the engine instead.
pub(crate) fn check_sync_entry(id: StoreId) -> Result<(), RuntimeError> {
    let cancelled = ACTIVATIONS.with(|all| {
        all.borrow()
            .iter()
            .any(|a| a.access.store_id() == id && a.cancelled.get() && a.state.get() != State::Done)
    });
    if cancelled {
        Err(RuntimeError::new(
            "a cancelled async JavaScript guest has not left its stack",
        ))
    } else {
        Ok(())
    }
}

fn poll_entry(id: StoreId, waiter: &Rc<RefCell<Option<Waker>>>, cx: &mut Context<'_>) -> Poll<()> {
    if !entry_blocked(id) {
        return Poll::Ready(());
    }
    ENTRY_WAITERS.with(|waiters| {
        let mut waiters = waiters.borrow_mut();
        // The waiting future owns its waker. A cancelled entry must release
        // that waker immediately even while the older guest is unwinding.
        waiters.retain(|waiter| waiter.strong_count() != 0);
        let weak = Rc::downgrade(waiter);
        if !waiters.iter().any(|entry| Weak::ptr_eq(entry, &weak)) {
            waiters.push(weak);
        }
        *waiter.borrow_mut() = Some(cx.waker().clone());
    });
    Poll::Pending
}

/// Keep the callback, result slot, and access publication outside the caught
/// JavaScript boundary. Only the borrowed host invocation can be unwound.
fn invoke_host<R>(access: &StoreAccess, mut f: impl FnMut() -> R) -> Result<R, JsValue> {
    let mut result = None;
    let mut invoke = || result = Some(f());
    let callback = ScopedClosure::borrow_mut_assert_unwind_safe(&mut invoke);
    let called = with_access(access, || {
        jspi::host_callback(access.store_id(), callback.as_ref())
    });
    drop(callback);
    called.map(|()| result.expect("host callback was invoked synchronously"))
}

fn dispose_host(access: &StoreAccess, future: HostFuture) -> Result<(), JsValue> {
    let mut future = Some(future);
    // Taking before Drop ensures a throwing destructor is never retried.
    invoke_host(access, || drop(future.take()))
}

fn done(token: u32, rejected: bool, value: JsValue) {
    let Some(a) = activation(token) else {
        return;
    };
    a.state.set(State::Done);
    a.guest.finish();
    let dropped = a.drop_import();
    *a.result.borrow_mut() = Some(if rejected {
        Err(value)
    } else if let Err(error) = dropped {
        Err(error)
    } else {
        Ok(value)
    });
    ACTIVATIONS.with(|all| all.borrow_mut().retain(|other| !Rc::ptr_eq(other, &a)));
    wake_entries();
    a.wake_caller();
}

fn check(token: u32) {
    let Some(a) = activation(token) else {
        return;
    };
    if a.state.get() != State::Waiting || a.accepted.replace(true) {
        return;
    }
    a.guest.suspend();
    if a.cancelled.get() {
        a.unwind();
    }
    // The guest is now off the engine stack. Only the Rust caller's next
    // poll may run its host future, so AsyncEnv never depends on a microtask.
    wake_entries();
    a.wake_caller();
}

fn woken(token: u32) {
    if let Some(a) = activation(token) {
        a.state.set(State::Running);
    }
}

impl Activation {
    fn wake_caller(&self) {
        let waker = self.caller.borrow().clone();
        if let Some(waker) = waker {
            waker.wake();
        }
    }

    fn drop_import(&self) -> Result<(), JsValue> {
        match self.import.take() {
            Some(future) => dispose_host(&self.access, future),
            None => Ok(()),
        }
    }

    fn unwind(&self) {
        let _ = self.drop_import();
        self.guest.suspend();
        self.state.set(State::Unwinding);
        shim_unwind(self.token, &jspi::trap_error());
    }

    fn drive(&self, cx: &mut Context<'_>) {
        if self.state.get() != State::Waiting || !self.accepted.get() || self.cancelled.get() {
            return;
        }
        assert!(
            !self.driving.replace(true),
            "an async call cannot be polled recursively"
        );
        struct Reset<'a>(&'a Cell<bool>);
        impl Drop for Reset<'_> {
            fn drop(&mut self) {
                self.0.set(false);
            }
        }
        let _reset = Reset(&self.driving);
        let Some(mut future) = self.import.take() else {
            return;
        };
        let mut outcome = match invoke_host(&self.access, || future.as_mut().poll(cx)) {
            Ok(outcome) => outcome,
            Err(error) => {
                // Foreign unwinding can leave a generator partially moved.
                // It is no longer safe to resume or drop it. Its captured
                // access is borrowed and its published scope is revoked, so
                // forgetting it cannot retain or expose the store allocation.
                std::mem::forget(future);
                *self.error.borrow_mut() = Some(error.into());
                self.unwind();
                return;
            }
        };
        if self.cancelled.get() {
            let _ = dispose_host(&self.access, future);
            return;
        }
        if outcome.is_ready() {
            if let Err(error) = dispose_host(&self.access, future)
                && matches!(outcome, Poll::Ready(Ok(_)))
            {
                outcome = Poll::Ready(Err(error.into()));
            }
        } else {
            *self.import.borrow_mut() = Some(future);
        }
        match outcome {
            Poll::Pending => {}
            Poll::Ready(Err(error)) => {
                *self.error.borrow_mut() = Some(error);
                self.unwind();
            }
            Poll::Ready(Ok(values)) => {
                if !self.direct
                    && let Err(error) = jspi::deliverable(&values)
                {
                    *self.error.borrow_mut() = Some(error);
                    self.unwind();
                    return;
                }
                let value = if self.direct {
                    *self.returned.borrow_mut() = Some(values);
                    JsValue::UNDEFINED
                } else {
                    self.access.with(|store| jspi::results(&values, &store))
                };
                if self.cancelled.get() {
                    return;
                }
                self.guest.resume();
                self.state.set(State::Woken);
                self.accepted.set(false);
                shim_wake(self.token, &value);
            }
        }
    }
}

/// The registry owns an activation until its guest leaves the engine. This
/// guard must not own an Rc: JavaScript exceptions can skip its destructor.
pub(crate) struct Held {
    token: u32,
}

impl Drop for Held {
    fn drop(&mut self) {
        restore_host_hold(self.token);
    }
}

/// A host boundary restores only a hold acquired inside it. A nested host
/// must leave the enclosing host's hold intact.
fn unheld_host(id: usize) -> u32 {
    ACTIVATIONS.with(|all| {
        all.borrow()
            .iter()
            .rev()
            .find(|a| {
                a.access.store_id().as_raw().get() == id
                    && !a.held.get()
                    && (a.state.get() == State::Running
                        || (a.state.get() == State::Waiting && !a.accepted.get()))
            })
            .map_or(0, |a| a.token)
    })
}

struct AllocationCheckpoint {
    allocation: *const crate::store::AllocationInner,
    busy: bool,
    lent: *mut StoreInner,
    released: usize,
}

struct HostCheckpoint {
    lends: u32,
    context: (usize, u32),
    scope: usize,
    token: u32,
    allocation: Option<AllocationCheckpoint>,
}

/// A host invocation cannot suspend, so its original lend or registered guest
/// keeps this metadata alive through the JavaScript finally boundary. The
/// snapshot owns no store reference that a skipped Rust destructor could leak.
pub(crate) fn enter_host(id: usize) -> u32 {
    let allocation = lent::allocation(id).or_else(|| {
        ACTIVATIONS.with(|all| {
            all.borrow()
                .iter()
                .rev()
                .find(|a| a.access.store_id().as_raw().get() == id)
                .map(|a| Rc::as_ptr(&a.access.allocation().0))
        })
    });
    let allocation = allocation.map(|ptr| {
        // SAFETY: the lend or activation keeps its allocation alive until the
        // synchronous host boundary exits. Only disjoint metadata is read.
        let allocation = unsafe { &*ptr };
        AllocationCheckpoint {
            allocation: ptr,
            busy: allocation.busy.get(),
            lent: allocation.lent.get(),
            released: allocation.guest.released(),
        }
    });
    let snapshot = HostCheckpoint {
        lends: lent::checkpoint(),
        context: crate::entities::store::StoreContext::checkpoint(),
        scope: crate::async_runtime::scope_checkpoint(),
        token: unheld_host(id),
        allocation,
    };
    HOSTS.with(|hosts| {
        let mut hosts = hosts.borrow_mut();
        let depth = hosts.len() as u32;
        hosts.push(snapshot);
        depth
    })
}

pub(crate) fn leave_host(depth: u32) {
    let snapshot = HOSTS.with(|hosts| {
        let mut hosts = hosts.borrow_mut();
        assert_eq!(hosts.len(), depth as usize + 1, "host boundaries must nest");
        hosts.pop().unwrap()
    });
    // SAFETY: JavaScript finally runs only after every interior Rust frame has
    // returned or been unwound. No references from those frames remain usable.
    unsafe {
        crate::entities::store::StoreContext::restore_checkpoint(
            snapshot.context.0,
            snapshot.context.1,
        );
        crate::async_runtime::restore_scope_checkpoint(snapshot.scope);
    }
    lent::restore(snapshot.lends);
    restore_host_hold(snapshot.token);
    if let Some(snapshot) = snapshot.allocation {
        // SAFETY: the enclosing lend/activation still owns this metadata.
        let allocation = unsafe { &*snapshot.allocation };
        allocation.busy.set(snapshot.busy);
        allocation.lent.set(snapshot.lent);
        allocation.guest.restore_released(snapshot.released);
    }
}

pub(crate) fn restore_host_hold(token: u32) {
    if let Some(a) = activation(token)
        && a.held.replace(false)
    {
        a.access.allocation().0.busy.set(false);
    }
}

pub(crate) fn hold_running(id: usize) -> Option<Result<(*mut StoreInner, Held), RuntimeError>> {
    let a = ACTIVATIONS.with(|all| {
        all.borrow()
            .iter()
            .rev()
            .find(|a| {
                a.access.store_id().as_raw().get() == id
                    && match a.state.get() {
                        State::Running => true,
                        State::Waiting => !a.accepted.get(),
                        _ => false,
                    }
            })
            .cloned()
    })?;
    if a.cancelled.get() {
        return Some(Err(RuntimeError::new("the async guest call was cancelled")));
    }
    let allocation = a.access.allocation();
    if a.held.get() || allocation.0.busy.replace(true) {
        return Some(Err(lent::held()));
    }
    a.held.set(true);
    Some(Ok((a.access.ptr, Held { token: a.token })))
}

fn arguments(access: &StoreAccess, ty: &FunctionType, args: &Array) -> Vec<Value> {
    access.with(|mut store| {
        ty.params()
            .iter()
            .enumerate()
            .map(|(index, ty)| js_value_to_wasmer(&mut store, ty, &args.get(index as u32)))
            .collect()
    })
}

fn start(
    id: StoreId,
    ty: &FunctionType,
    factory: &Factory,
    args: Array,
) -> Result<JsValue, JsValue> {
    let direct = DIRECT.with(|direct| direct.borrow_mut().take());
    let a = direct.or_else(|| running(id));
    if crate::js::function::entered_synchronously() || a.is_none() {
        return immediate(id, ty, factory, args);
    }
    let a = a.unwrap();
    if a.access.store_id() != id {
        return Err(crate::js::error::trap_value(jspi::foreign_store_error()));
    }
    if a.cancelled.get() {
        return Err(jspi::trap_error());
    }
    // SAFETY: the activation owns the allocation for every usable poll/drop
    // of this future. A future poisoned by a foreign throw is forgotten after
    // its publication is revoked; it must not retain allocation ownership.
    let access = unsafe { a.access.borrowed() };
    let factory = factory.clone();
    let ty = ty.clone();
    // Conversion and host construction happen on the executor stack too.
    *a.import.borrow_mut() = Some(Box::pin(async move {
        let values = arguments(&access, &ty, &args);
        let future = (factory)(values);
        let results = future.await?;
        if results.len() != ty.results().len()
            || results
                .iter()
                .zip(ty.results())
                .any(|(value, ty)| value.ty() != *ty)
        {
            return Err(RuntimeError::new(
                "an async host function returned values that do not match its signature",
            ));
        }
        Ok(results)
    }));
    a.state.set(State::Waiting);
    a.accepted.set(a.direct);
    if a.direct {
        a.guest.suspend();
    }
    let promise = shim_wait(a.token);
    if !a.direct {
        shim_check(a.token);
    }
    Ok(promise)
}

fn immediate(
    id: StoreId,
    ty: &FunctionType,
    factory: &Factory,
    args: Array,
) -> Result<JsValue, JsValue> {
    let (ptr, _hold) = lent::hold(id.as_raw().get())
        .map_err(|_| crate::js::error::trap_value(jspi::cannot_suspend_error()))?;
    // SAFETY: the host-function lend owns the store for this synchronous call.
    unsafe {
        crate::async_runtime::with_sync_store(ptr, || {
            let access = StoreAccess::borrowed_from_ptr(ptr);
            let values = arguments(&access, ty, &args);
            let mut future = with_access(&access, || (factory)(values));
            let mut cx = Context::from_waker(Waker::noop());
            let outcome = with_access(&access, || future.as_mut().poll(&mut cx));
            with_access(&access, || drop(future));
            match outcome {
                Poll::Ready(Ok(values)) => {
                    if values.len() != ty.results().len()
                        || values
                            .iter()
                            .zip(ty.results())
                            .any(|(value, ty)| value.ty() != *ty)
                    {
                        return Err(crate::js::error::trap_value(RuntimeError::new(
                            "async import result type mismatch",
                        )));
                    }
                    Ok(access.with(|store| jspi::results(&values, &store)))
                }
                Poll::Ready(Err(error)) => Err(crate::js::error::trap_value(error)),
                Poll::Pending => Err(crate::js::error::trap_value(jspi::cannot_suspend_error())),
            }
        })
    }
}

pub(crate) fn new_function(
    store: &mut impl AsStoreMut,
    ty: FunctionType,
    factory: Factory,
) -> Function {
    assert!(
        jspi::is_supported(),
        "the JavaScript host does not support WebAssembly JSPI"
    );
    let id = store.objects_mut().id();
    let host_ty = ty.clone();
    let body = Closure::wrap(
        Box::new(move |args: Array| start(id, &host_ty, &factory, args))
            as Box<dyn Fn(Array) -> Result<JsValue, JsValue>>,
    )
    .into_js_value();
    let host = jspi::host_function(&ty, id, &body, true);
    let function = dispatch_import(&ty, &host);
    jspi::own(&function, id);
    let vm = crate::js::vm::function::VMFunction::new(function, ty);
    Function(crate::BackendFunction::Js(
        crate::js::function::Function::from_vm_extern(store, crate::vm::VMExternFunction::Js(vm)),
    ))
}

/// Engines may reject a Suspending import before calling its body on a sync
/// entry, even if the host future is ready. A Wasm frame selects the plain
/// import on sync entry and the suspending import on async entry. A JavaScript
/// dispatcher cannot do this: its frame would itself prevent suspension.
fn dispatch_import(ty: &FunctionType, host: &js_sys::Function) -> js_sys::Function {
    fn u32(out: &mut Vec<u8>, mut value: usize) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            out.push(byte | if value == 0 { 0 } else { 0x80 });
            if value == 0 {
                break;
            }
        }
    }
    fn section(wasm: &mut Vec<u8>, id: u8, contents: &[u8]) {
        wasm.push(id);
        u32(wasm, contents.len());
        wasm.extend_from_slice(contents);
    }
    fn types(out: &mut Vec<u8>, types: &[wasmer_types::Type]) {
        u32(out, types.len());
        for ty in types {
            out.push(match ty {
                wasmer_types::Type::I32 => 0x7f,
                wasmer_types::Type::I64 => 0x7e,
                wasmer_types::Type::F32 => 0x7d,
                wasmer_types::Type::F64 => 0x7c,
                wasmer_types::Type::FuncRef => 0x70,
                wasmer_types::Type::ExternRef => 0x6f,
                _ => unreachable!("host_function checked JS-supported types"),
            });
        }
    }
    let mut wasm = b"\0asm\x01\0\0\0".to_vec();
    let mut signatures = vec![2, 0x60];
    types(&mut signatures, ty.params());
    types(&mut signatures, ty.results());
    signatures.extend_from_slice(&[0x60, 0, 1, 0x7f]);
    section(&mut wasm, 1, &signatures);
    // (import "h" "mode" (func (type 1)))
    // (import "h" "sync" (func (type 0)))
    // (import "h" "wait" (func (type 0)))
    let mut imports = vec![3];
    for (name, ty) in [(b"mode", 1), (b"sync", 0), (b"wait", 0)] {
        imports.extend_from_slice(&[1, b'h', 4]);
        imports.extend_from_slice(name);
        imports.extend_from_slice(&[0, ty]);
    }
    section(&mut wasm, 2, &imports);
    section(&mut wasm, 3, &[1, 0]);
    section(&mut wasm, 7, &[1, 3, b'r', b'u', b'n', 0, 3]);
    let mut body = vec![0, 0x10, 0, 0x04, 0x40];
    for index in 0..ty.params().len() {
        body.push(0x20);
        u32(&mut body, index);
    }
    body.extend_from_slice(&[0x10, 1, 0x0f, 0x0b]);
    for index in 0..ty.params().len() {
        body.push(0x20);
        u32(&mut body, index);
    }
    body.extend_from_slice(&[0x10, 2, 0x0b]);
    let mut code = vec![1];
    u32(&mut code, body.len());
    code.extend(body);
    section(&mut wasm, 10, &code);
    let module =
        js_sys::WebAssembly::Module::new(&js_sys::Uint8Array::from(wasm.as_slice()).into())
            .expect("valid async import adapter");
    let imports = js_sys::Object::new();
    let namespace = js_sys::Object::new();
    let mode = Closure::wrap(
        Box::new(|| i32::from(crate::js::function::entered_synchronously()))
            as Box<dyn Fn() -> i32>,
    )
    .into_js_value();
    Reflect::set(&namespace, &"mode".into(), &mode).unwrap();
    Reflect::set(&namespace, &"sync".into(), host).unwrap();
    Reflect::set(&namespace, &"wait".into(), &jspi::public(host)).unwrap();
    Reflect::set(&imports, &"h".into(), &namespace).unwrap();
    let instance = js_sys::WebAssembly::Instance::new(&module, &imports)
        .expect("valid async import adapter imports");
    let adapter = Reflect::get(&instance.exports(), &"run".into()).unwrap();
    own_adapter(&adapter, host);
    adapter.unchecked_into()
}

struct Call(Rc<Activation>);

impl Future for Call {
    type Output = Result<Returned, RuntimeError>;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let a = &self.0;
        *a.caller.borrow_mut() = Some(cx.waker().clone());
        a.drive(cx);
        match a.result.take() {
            Some(Ok(value)) => Poll::Ready(Ok(match a.returned.take() {
                Some(values) => Returned::Host(values),
                None => Returned::Guest(value),
            })),
            Some(Err(error)) => Poll::Ready(Err(a.error.take().unwrap_or_else(|| error.into()))),
            None => Poll::Pending,
        }
    }
}

enum Returned {
    Guest(JsValue),
    Host(Vec<Value>),
}

impl Drop for Call {
    fn drop(&mut self) {
        let a = &self.0;
        if a.state.get() == State::Done {
            return;
        }
        a.cancelled.set(true);
        a.guest.cancel();
        drop(a.caller.take());
        let _ = a.drop_import();
        match a.state.get() {
            State::Waiting if a.accepted.get() => a.unwind(),
            State::Woken => a.unwind(),
            _ => {}
        }
    }
}

pub(crate) fn call(
    function: crate::js::function::Function,
    access: StoreAccess,
    params: Vec<Value>,
) -> ResultFuture {
    Box::pin(async move {
        if !jspi::belongs_to(&function.handle.function, access.store_id()) {
            return Err(jspi::foreign_store_error());
        }
        let ty = &function.handle.ty;
        if params.len() != ty.params().len()
            || params
                .iter()
                .zip(ty.params())
                .any(|(value, ty)| value.ty() != *ty)
        {
            return Err(RuntimeError::new("Parameters did not match signature"));
        }
        // A ready async import reached through a synchronous entry has only
        // that entry's loan. Nested calls must finish on this stack as well:
        // ready imports work, while a waiting import traps through call().
        if access.is_borrowed() {
            return access.with(|mut store| function.call(&mut store, &params));
        }
        {
            let waiter = Rc::new(RefCell::new(None));
            std::future::poll_fn(|cx| poll_entry(access.store_id(), &waiter, cx)).await;
        }
        if !jspi::enters_a_guest(&function.handle.function) {
            return access.with(|mut store| function.call(&mut store, &params));
        }
        SHIM.with(|_| ());
        jspi::prepare_trap().map_err(|error| {
            RuntimeError::new(format!(
                "the error of a trap, which unwinds a guest, cannot be made: {}",
                RuntimeError::from(error).message()
            ))
        })?;
        let host = adapter_host(&function.handle.function);
        let direct = host.is_function();
        let promising: js_sys::Function = if direct {
            host.unchecked_into()
        } else {
            function.handle.promising().map_err(RuntimeError::from)?
        };
        let arguments =
            access.with(|store| Array::from_iter(params.iter().map(|v| v.as_jsvalue(&store))));
        let token = NEXT.with(|next| {
            let token = next.get().checked_add(1).expect("too many async calls");
            next.set(token);
            token
        });
        let a = Rc::new(Activation {
            token,
            guest: access.allocation().0.guest.begin(!access.is_shared()),
            access: access.clone(),
            direct,
            state: Cell::new(State::Running),
            cancelled: Cell::new(false),
            accepted: Cell::new(false),
            held: Cell::new(false),
            import: RefCell::new(None),
            error: RefCell::new(None),
            returned: RefCell::new(None),
            result: RefCell::new(None),
            caller: RefCell::new(None),
            driving: Cell::new(false),
        });
        ACTIVATIONS.with(|all| all.borrow_mut().push(a.clone()));
        let call = Call(a.clone());
        let entered = access.with(|mut store| {
            let _lend = lent::lend(&mut store).map_err(crate::js::error::trap_value)?;
            let _entered = crate::js::function::enter_async();
            if direct {
                DIRECT.with(|slot| *slot.borrow_mut() = Some(a.clone()));
            }
            let result = Reflect::apply(&promising, &JsValue::NULL, &arguments);
            DIRECT.with(|slot| slot.borrow_mut().take());
            result
        });
        match entered {
            Ok(promise) => shim_settle(&promise, token),
            Err(error) => done(token, true, error),
        }
        let returned = call.await?;
        match returned {
            Returned::Host(values) => {
                if values.len() != ty.results().len()
                    || values
                        .iter()
                        .zip(ty.results())
                        .any(|(value, ty)| value.ty() != *ty)
                {
                    return Err(RuntimeError::new(
                        "an async host function returned values that do not match the signature it is called with",
                    ));
                }
                Ok(values.into_boxed_slice())
            }
            Returned::Guest(result) => access.with(|mut store| {
                Ok(match ty.results() {
                    [] => Box::default(),
                    [ty] => vec![js_value_to_wasmer(&mut store, ty, &result)].into_boxed_slice(),
                    types => {
                        let results = Array::from(&result);
                        types
                            .iter()
                            .enumerate()
                            .map(|(index, ty)| {
                                js_value_to_wasmer(&mut store, ty, &results.get(index as u32))
                            })
                            .collect()
                    }
                })
            }),
        }
    })
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

    /// The activations there are, and how many of them hold their store.
    fn live() -> (usize, usize) {
        ACTIVATIONS.with(|all| {
            let all = all.borrow();
            (
                all.len(),
                all.iter()
                    .filter(|a| a.access.allocation().0.guest.is_running())
                    .count(),
            )
        })
    }

    /// However a call ends, nothing of it stays in this module's state, and
    /// its token is used again. A call that never waits holds its store until
    /// the engine has settled its promise.
    #[wasm_bindgen_test]
    async fn a_call_that_ended_leaves_nothing_behind() {
        let mut store = Store::default();
        // Fails when asked to, after waiting for a tick.
        let ty = FunctionType::new([Type::I32], []);
        let host = Function::new_async(&mut store, ty, |arguments| {
            let fail = arguments[0].unwrap_i32() != 0;
            async move {
                tick().await;
                if fail {
                    return Err(RuntimeError::new("failed"));
                }
                Ok(vec![])
            }
        });
        let store = store.into_shared();
        assert_eq!(live(), (0, 0));

        for round in 0..2 {
            let call =
                |fail: i32| Box::pin(host.call_async(&mut store.clone(), &[Value::I32(fail)]));

            // Returns.
            let mut returning = call(0);
            let mut cx = Context::from_waker(Waker::noop());
            assert!(returning.as_mut().poll(&mut cx).is_pending());
            assert_eq!(
                live(),
                (1, 0),
                "round {round}: suspended, the store is free"
            );
            returning.await.unwrap();
            assert_eq!(live(), (0, 0), "round {round}: returned");

            // Fails.
            call(1).await.unwrap_err();
            assert_eq!(live(), (0, 0), "round {round}: failed");

            // Is cancelled: its guest is still to be unwound.
            let mut cancelled = call(0);
            assert!(cancelled.as_mut().poll(&mut cx).is_pending());
            drop(cancelled);
            assert_eq!(live(), (1, 0), "round {round}: cancelled");
            for _ in 0..3 {
                tick().await;
            }
            assert_eq!(live(), (0, 0), "round {round}: unwound");

            // Cannot be suspended.
            store
                .try_with(|mut store| host.call(&mut store, &[Value::I32(0)]).unwrap_err())
                .unwrap();
            assert_eq!(live(), (0, 0), "round {round}: refused");
        }
        let ready = store
            .try_with(|mut store| {
                Function::new_async(&mut store, FunctionType::new([], [Type::I32]), |_| async {
                    Ok(vec![Value::I32(7)])
                })
            })
            .unwrap();
        let mut call = Box::pin(ready.call_async(&mut store.clone(), &[]));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(call.as_mut().poll(&mut cx).is_pending());
        assert_eq!(live(), (1, 1), "returned without waiting");
        assert_eq!(call.await.unwrap().to_vec(), [Value::I32(7)]);
        assert_eq!(live(), (0, 0));
    }

    #[wasm_bindgen(inline_js = r#"
    let compile;
    export function denyCompilation(deny) {
      if (deny) {
        compile = WebAssembly.Module;
        WebAssembly.Module = function () {
          throw new WebAssembly.CompileError("compilation denied");
        };
      } else {
        WebAssembly.Module = compile;
      }
    }
    "#)]
    extern "C" {
        /// Has `new WebAssembly.Module` throw, as in a host that compiled the
        /// guest and refuses to compile anything since, or not.
        #[wasm_bindgen(js_name = denyCompilation)]
        fn deny_compilation(deny: bool);
    }

    /// A host may refuse to compile WebAssembly once the guest is compiled.
    /// The error of a trap, which unwinds a guest, is then made before a call
    /// enters anything, and a call that cannot make it fails there, with its
    /// store free, rather than when its guest has to be unwound, as one whose
    /// host function fails does, past every handler.
    #[cfg(feature = "wat")]
    #[wasm_bindgen_test]
    async fn a_call_that_cannot_make_a_trap_fails_before_it_enters() {
        let mut store = Store::default();
        let ty = FunctionType::new([], []);
        let fail = Function::new_async(&mut store, ty, |_| async {
            tick().await;
            Err(RuntimeError::new("failed"))
        });
        let module = crate::Module::new(
            &store,
            r#"(module (import "h" "f" (func $f)) (func (export "run") (call $f)))"#,
        )
        .unwrap();
        let imports = crate::imports! { "h" => { "f" => fail.clone() } };
        let instance = crate::Instance::new(&mut store, &module, &imports).unwrap();
        let run = instance.exports.get_function("run").unwrap().clone();
        let store = store.into_shared();

        jspi::forget_trap();
        deny_compilation(true);
        let through_guest = run.call_async(&mut store.clone(), &[]).await;
        let direct = fail.call_async(&mut store.clone(), &[]).await;
        deny_compilation(false);
        for result in [through_guest, direct] {
            let error = result.unwrap_err();
            assert!(error.message().contains("cannot be made"), "{error}");
            assert!(error.message().contains("compilation denied"), "{error}");
        }
        assert_eq!(live(), (0, 0));

        // The host compiles again: the guest fails as its host function does.
        let error = run.call_async(&mut store.clone(), &[]).await.unwrap_err();
        assert!(error.message().contains("failed"), "{error}");
        assert!(!error.message().contains("cannot be made"), "{error}");
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
        let store = store.into_shared();
        let form =
            |function: &Function| jspi::promising(&function.as_js().handle.function).unwrap();

        host.call_async(&mut store.clone(), &[Value::I32(1)])
            .await
            .unwrap();
        let first = form(&host);
        again
            .call_async(&mut store.clone(), &[Value::I32(2)])
            .await
            .unwrap();
        assert!(js_sys::Object::is(&first, &form(&host)));
        assert!(js_sys::Object::is(&first, &form(&again)));
    }
}

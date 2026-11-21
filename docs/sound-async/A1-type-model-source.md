# Appendix A1: type model source

The stub-bodied model of the proposed API, its accept and reject harness, and
the review scenarios. See [the type model notes](./04-type-model.md).

The [rebuild command](./README.md#rebuild-the-trials) extracts these files.

### `model/Cargo.toml`

<!-- trial-file: model/Cargo.toml -->
````toml
[package]
name = "async_api_model"
version = "0.0.0"
edition = "2024"
publish = false

[lib]
path = "src/lib.rs"

[workspace]
````

### `model/src/lib.rs`

<!-- trial-file: model/src/lib.rs -->
````rust
//! A type-level model of the proposed async API. Bodies are stubs: this crate
//! exists so the compiler can check what the contract accepts and rejects.
#![forbid(unsafe_code)]
#![allow(dead_code, unused_variables)]

use std::{
    future::Future,
    marker::PhantomData,
    ops::{Deref, DerefMut},
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
};

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    I32(i32),
}
#[derive(Debug)]
pub struct RuntimeError;
#[derive(Debug)]
pub struct StoreBusy;

type CallResult = Result<Vec<Value>, RuntimeError>;

pub struct Store {
    // The real `Store` is already `!Send`.
    not_send: PhantomData<*mut ()>,
}
pub struct StoreRef<'a>(&'a Store);
pub struct StoreMut<'a>(&'a mut Store);

pub trait AsStoreRef {
    fn as_store_ref(&self) -> StoreRef<'_>;
}
pub trait AsStoreMut: AsStoreRef {
    fn as_store_mut(&mut self) -> StoreMut<'_>;
}
// The real crate has these blanket impls, which matter for coherence below.
impl<P> AsStoreRef for P
where
    P: Deref,
    P::Target: AsStoreRef,
{
    fn as_store_ref(&self) -> StoreRef<'_> {
        (**self).as_store_ref()
    }
}
impl<P> AsStoreMut for P
where
    P: DerefMut,
    P::Target: AsStoreMut,
{
    fn as_store_mut(&mut self) -> StoreMut<'_> {
        (**self).as_store_mut()
    }
}
impl AsStoreRef for StoreRef<'_> {
    fn as_store_ref(&self) -> StoreRef<'_> {
        StoreRef(self.0)
    }
}
impl AsStoreRef for Store {
    fn as_store_ref(&self) -> StoreRef<'_> {
        StoreRef(self)
    }
}
impl AsStoreMut for Store {
    fn as_store_mut(&mut self) -> StoreMut<'_> {
        StoreMut(self)
    }
}
impl AsStoreRef for StoreMut<'_> {
    fn as_store_ref(&self) -> StoreRef<'_> {
        StoreRef(self.0)
    }
}
impl AsStoreMut for StoreMut<'_> {
    fn as_store_mut(&mut self) -> StoreMut<'_> {
        StoreMut(self.0)
    }
}

impl Store {
    pub fn new() -> Self {
        Self { not_send: PhantomData }
    }
    pub fn touch(&mut self) {}
    /// Opt into several guest stacks on this store.
    pub fn into_shared(self) -> SharedStore {
        SharedStore(Rc::new(self))
    }
}

pub struct FunctionEnv<T>(PhantomData<fn() -> T>);
impl<T> Clone for FunctionEnv<T> {
    fn clone(&self) -> Self {
        Self(PhantomData)
    }
}
impl<T> FunctionEnv<T> {
    pub fn new(store: &mut impl AsStoreMut, value: T) -> Self
    where
        T: Send + 'static,
    {
        Self(PhantomData)
    }
    /// Takes `AsStoreMut`, so a plain `StoreRef` cannot reach `T`.
    pub fn as_ref<'a>(&self, store: &'a impl AsStoreMut) -> &'a T {
        todo!()
    }
    pub fn as_mut<'a>(&self, store: &'a mut impl AsStoreMut) -> &'a mut T {
        todo!()
    }
}

/// The same capability a synchronous host function receives today.
pub struct FunctionEnvMut<'a, T> {
    store: StoreMut<'a>,
    env: FunctionEnv<T>,
}
impl<T> AsStoreRef for FunctionEnvMut<'_, T> {
    fn as_store_ref(&self) -> StoreRef<'_> {
        self.store.as_store_ref()
    }
}
impl<T> AsStoreMut for FunctionEnvMut<'_, T> {
    fn as_store_mut(&mut self) -> StoreMut<'_> {
        self.store.as_store_mut()
    }
}
impl<T> FunctionEnvMut<'_, T> {
    pub fn data(&self) -> &T {
        todo!()
    }
    pub fn data_mut(&mut self) -> &mut T {
        todo!()
    }
    /// Replaces `data_and_store_mut`: a `StoreRef` cannot reach `T` again.
    pub fn data_and_store(&mut self) -> (&mut T, StoreRef<'_>) {
        todo!()
    }
    /// The store's shared handle, if it is a `SharedStore`. Resolved from a
    /// private back-reference, so `T` stays `Send`.
    pub fn shared(&self) -> Option<SharedStore> {
        None
    }
    /// Lets a `SharedStore` handle use the store while `f` runs.
    pub fn release<R>(&mut self, f: impl FnOnce() -> R) -> R {
        f()
    }
}

/// What an async host function holds across `.await`. It holds no borrow.
pub struct AsyncEnv<T>(PhantomData<*mut T>);
impl<T> AsyncEnv<T> {
    /// Borrows the store for one synchronous slice of a poll.
    pub fn with<R>(&mut self, f: impl FnOnce(FunctionEnvMut<'_, T>) -> R) -> R {
        todo!()
    }
}

/// Implemented for async closures and async functions.
pub trait AsyncHostFunction<T, Args, Rets>: 'static {
    fn invoke<'a>(
        &'a self,
        env: &'a mut AsyncEnv<T>,
        args: Args,
    ) -> Pin<Box<dyn Future<Output = Rets> + 'a>>;
}

impl<T: 'static, F, R: 'static> AsyncHostFunction<T, (), R> for F
where
    F: for<'e> AsyncFn(&'e mut AsyncEnv<T>) -> R + 'static,
{
    fn invoke<'a>(
        &'a self,
        env: &'a mut AsyncEnv<T>,
        _: (),
    ) -> Pin<Box<dyn Future<Output = R> + 'a>> {
        Box::pin(async move { self(env).await })
    }
}

impl<T: 'static, F, A1: 'static, R: 'static> AsyncHostFunction<T, (A1,), R> for F
where
    F: for<'e> AsyncFn(&'e mut AsyncEnv<T>, A1) -> R + 'static,
{
    fn invoke<'a>(
        &'a self,
        env: &'a mut AsyncEnv<T>,
        (a1,): (A1,),
    ) -> Pin<Box<dyn Future<Output = R> + 'a>> {
        Box::pin(async move { self(env, a1).await })
    }
}

#[derive(Clone)]
pub struct Function;
impl Function {
    pub fn new_typed_async<T, Args, Rets>(
        store: &mut impl AsStoreMut,
        env: &FunctionEnv<T>,
        func: impl AsyncHostFunction<T, Args, Rets>,
    ) -> Self {
        Function
    }
    pub fn new_async<T: 'static, F>(store: &mut impl AsStoreMut, env: &FunctionEnv<T>, func: F) -> Self
    where
        F: for<'e> AsyncFn(&'e mut AsyncEnv<T>, &'e [Value]) -> CallResult + 'static,
    {
        Function
    }
    pub fn ty(&self, store: &impl AsStoreRef) -> u32 {
        0
    }
    pub fn call(&self, store: &mut impl AsStoreMut, params: &[Value]) -> CallResult {
        Ok(vec![])
    }
    /// One entry point: `&mut store`, `cx` in an async host function, or
    /// `&mut shared`, whose call is `'static`.
    pub fn call_async<'a, S: AsyncCallStore>(
        &'a self,
        store: &'a mut S,
        params: &'a [Value],
    ) -> S::Call<'a, CallResult> {
        let _ = params;
        store.start_call()
    }
}

pub struct TypedFunction<Args, Rets>(PhantomData<fn(Args) -> Rets>);
impl<Args, Rets: 'static> TypedFunction<Args, Rets> {
    pub fn call(&self, store: &mut impl AsStoreMut, args: Args) -> Result<Rets, RuntimeError> {
        todo!()
    }
    pub fn call_async<'a, S: AsyncCallStore>(
        &'a self,
        store: &'a mut S,
        args: Args,
    ) -> S::Call<'a, Result<Rets, RuntimeError>> {
        let _ = args;
        store.start_call()
    }
}

/// The future of every async call.
pub struct AsyncCall<'a, R>(Pin<Box<dyn Future<Output = R> + 'a>>);
impl<R> Future for AsyncCall<'_, R> {
    type Output = R;
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<R> {
        self.get_mut().0.as_mut().poll(cx)
    }
}

mod sealed {
    pub trait Sealed {}
}

/// What an async call can run on. Sealed; users never name it.
pub trait AsyncCallStore: sealed::Sealed {
    type Call<'a, R: 'static>: Future<Output = R>
    where
        Self: 'a;
    #[doc(hidden)]
    fn start_call<'a, R: 'static>(&'a mut self) -> Self::Call<'a, R>;
}

impl<S: AsStoreMut> sealed::Sealed for S {}
impl<S: AsStoreMut> AsyncCallStore for S {
    type Call<'a, R: 'static>
        = AsyncCall<'a, R>
    where
        Self: 'a;
    fn start_call<'a, R: 'static>(&'a mut self) -> AsyncCall<'a, R> {
        todo!()
    }
}
impl<T> sealed::Sealed for AsyncEnv<T> {}
impl<T> AsyncCallStore for AsyncEnv<T> {
    type Call<'a, R: 'static>
        = AsyncCall<'a, R>
    where
        Self: 'a;
    fn start_call<'a, R: 'static>(&'a mut self) -> AsyncCall<'a, R> {
        todo!()
    }
}
impl sealed::Sealed for SharedStore {}
impl AsyncCallStore for SharedStore {
    // Holds its own handle and copies, so it borrows nothing.
    type Call<'a, R: 'static> = AsyncCall<'static, R>;
    fn start_call<'a, R: 'static>(&'a mut self) -> AsyncCall<'static, R> {
        todo!()
    }
}

pub struct Memory;
impl Memory {
    pub fn read(&self, store: &impl AsStoreRef) -> u8 {
        0
    }
}

/// Opt-in: several guest stacks on one store, one running at a time.
#[derive(Clone)]
pub struct SharedStore(Rc<Store>);
impl SharedStore {
    /// Fails instead of waiting: a free store has no holder to wait for.
    pub fn try_with<R>(&self, f: impl FnOnce(StoreMut<'_>) -> R) -> Result<R, StoreBusy> {
        todo!()
    }
    pub fn into_store(self) -> Result<Store, Self> {
        Rc::try_unwrap(self.0).map_err(Self)
    }
    /// For a handle kept inside the store's own environment.
    pub fn downgrade(&self) -> WeakSharedStore {
        WeakSharedStore(Rc::downgrade(&self.0))
    }
}

#[derive(Clone)]
pub struct WeakSharedStore(std::rc::Weak<Store>);
impl WeakSharedStore {
    pub fn upgrade(&self) -> Option<SharedStore> {
        self.0.upgrade().map(SharedStore)
    }
}
````

### `model/gen-ui.py`

<!-- trial-file: model/gen-ui.py -->
````python
"""Writes one snippet per contract claim and checks how rustc treats it."""
import pathlib
import subprocess
import sys

ROOT = pathlib.Path(sys.argv[1])
UI = ROOT / "ui"
UI.mkdir(exist_ok=True)
RUSTC = ["rustc", "+1.99", "--edition", "2024", "--crate-type", "lib", "--emit=metadata",
         "--extern", f"m={ROOT}/target/debug/libasync_api_model.rlib",
         "-L", f"{ROOT}/target/debug/deps", "--error-format=short", "--out-dir", f"{ROOT}/out"]

PRE = """#![allow(unused)]
use m::*;
use std::future::Future;
struct S { n: u32 }
fn setup() -> (Store, FunctionEnv<S>, Function) {
    let mut store = Store::new();
    let env = FunctionEnv::new(&mut store, S { n: 0 });
    (store, env, Function)
}
fn assert_send<X: Send>(_: X) {}
fn spawn<X: Future + 'static>(_: X) {}
"""

ACCEPT = {
    "typed_async_host_closure": """fn t() { let (mut s, e, _) = setup();
        Function::new_typed_async(&mut s, &e, async |cx: &mut AsyncEnv<S>, x: i32| -> i32 {
            let a = cx.with(|mut env| { env.data_mut().n += 1; env.data().n });
            std::future::ready(()).await;
            cx.with(|env| env.data().n) as i32 + a as i32 + x
        }); }""",
    "named_async_host_function": """async fn host(cx: &mut AsyncEnv<S>, x: u64) -> Result<u64, RuntimeError> {
            std::future::ready(()).await; Ok(x) }
        fn t() { let (mut s, e, _) = setup(); Function::new_typed_async(&mut s, &e, host); }""",
    "dynamic_async_host": """fn t() { let (mut s, e, _) = setup();
        Function::new_async(&mut s, &e, async |cx: &mut AsyncEnv<S>, p: &[Value]| {
            std::future::ready(()).await; Ok(p.to_vec()) }); }""",
    "sequential_calls_then_store_use": """async fn t() { let (mut s, _, f) = setup();
        f.call_async(&mut s, &[]).await.unwrap();
        f.call_async(&mut s, &[]).await.unwrap(); s.touch(); }""",
    "owned_store_future_is_static": """fn t() { let (s, _, f) = setup();
        spawn(async move { let mut s = s; f.call_async(&mut s, &[]).await }); }""",
    "nested_guest_call_from_async_host": """fn t() { let (mut s, e, inner) = setup();
        Function::new_typed_async(&mut s, &e, async move |cx: &mut AsyncEnv<S>| -> i32 {
            inner.call_async(cx, &[]).await.unwrap(); 1 }); }""",
    "shared_store_overlapping_tasks": """fn t() { let (s, _, f) = setup(); let mut s = s.into_shared();
        let a = f.call_async(&mut s, &[]); let b = f.call_async(&mut s, &[]); spawn(a); spawn(b); }""",
    "one_call_async_on_all_three": """async fn t(mut s: Store, cx: &mut AsyncEnv<S>, mut shared: SharedStore, f: Function) {
        f.call_async(&mut s, &[]).await.unwrap();
        f.call_async(cx, &[]).await.unwrap();
        f.call_async(&mut shared, &[]).await.unwrap(); }""",
    "typed_call_async_on_all_three": """async fn t(mut s: Store, cx: &mut AsyncEnv<S>, mut shared: SharedStore, f: TypedFunction<(i32,), i32>) {
        let a = f.call_async(&mut s, (1,)).await.unwrap();
        let b = f.call_async(cx, (a,)).await.unwrap();
        f.call_async(&mut shared, (b,)).await.unwrap(); }""",
    "async_env_usable_after_nested_call": """async fn t(cx: &mut AsyncEnv<S>, f: Function) {
        f.call_async(cx, &[]).await.unwrap(); cx.with(|_| ()); f.call_async(cx, &[]).await.unwrap(); }""",
    "call_on_function_env_mut": """async fn t(mut env: FunctionEnvMut<'_, S>, f: Function) {
        f.call_async(&mut env, &[]).await.unwrap(); env.data_mut().n = 1; }""",
    "shared_call_borrows_nothing": """fn t(mut shared: SharedStore) {
        let call = { let f = Function; let params = vec![Value::I32(1)]; f.call_async(&mut shared, &params) };
        drop(shared); spawn(call); }""",
    "shared_store_sync_access": """fn t() { let (s, _, _) = setup(); let s = s.into_shared();
        let _ = s.try_with(|mut store| { store.as_store_mut(); }); }""",
    "wasix_context_switch_shape": """async fn context_switch(cx: &mut AsyncEnv<S>, target: u64) -> Result<u32, RuntimeError> {
            let wait = cx.with(|mut env| { env.data_mut().n = target as u32; std::future::ready(()) });
            wait.await;
            Ok(cx.with(|env| env.data().n)) }
        fn t() { let (mut s, e, _) = setup(); Function::new_typed_async(&mut s, &e, context_switch); }""",
    "wasix_call_dynamic_shape": """async fn call_dynamic(cx: &mut AsyncEnv<S>, id: u32) -> Result<u32, RuntimeError> {
            let (function, params) = cx.with(|mut env| { env.data_mut().n = id; (Function, vec![Value::I32(1)]) });
            let results = function.call_async(cx, &params).await?;
            Ok(cx.with(|mut env| { env.data_mut().n += results.len() as u32; env.data().n })) }
        fn t() { let (mut s, e, _) = setup(); Function::new_typed_async(&mut s, &e, call_dynamic); }""",
    "sync_host_gets_shared_handle": """fn t(env: FunctionEnvMut<'_, S>, f: Function) {
            if let Some(mut shared) = env.shared() { spawn(f.call_async(&mut shared, &[])); } }""",
    "shared_handle_from_async_host": """async fn h(cx: &mut AsyncEnv<S>) -> i32 {
            let shared = cx.with(|env| env.shared()); drop(shared); 1 }
        fn t() { let (mut s, e, _) = setup(); Function::new_typed_async(&mut s, &e, h); }""",
    "store_ref_reads_function_type": """fn t(mut env: FunctionEnvMut<'_, S>, f: Function) {
            let (data, view) = env.data_and_store(); data.n = f.ty(&view); }""",
    "store_ref_reads_memory_beside_data": """fn t(mut env: FunctionEnvMut<'_, S>, m: Memory) {
        let (data, view) = env.data_and_store(); data.n = m.read(&view) as u32; }""",
    "release_lends_store_to_shared_handle": """fn t(mut env: FunctionEnvMut<'_, S>, shared: SharedStore) {
        env.release(|| { let _ = shared.try_with(|_| ()); }); env.data_mut().n = 1; }""",
}

REJECT = {
    "overlapping_call_async": ("E0499", """fn t() { let (mut s, _, f) = setup();
        let a = f.call_async(&mut s, &[]); let b = f.call_async(&mut s, &[]); drop((a, b)); }"""),
    "store_use_during_call": ("E0499", """fn t() { let (mut s, _, f) = setup();
        let a = f.call_async(&mut s, &[]); s.touch(); drop(a); }"""),
    "store_drop_during_call": ("E0505", """fn t() { let (mut s, _, f) = setup();
        let a = f.call_async(&mut s, &[]); drop(s); drop(a); }"""),
    "borrowed_call_is_not_static": ("E0597", """fn t() { let (mut s, _, f) = setup(); spawn(f.call_async(&mut s, &[])); }"""),
    "call_future_cannot_cross_threads": ("E0277", """fn t() { let (mut s, _, f) = setup(); assert_send(f.call_async(&mut s, &[])); }"""),
    "async_env_cannot_escape_import": ("E0521", """thread_local! { static K: std::cell::Cell<Option<&'static mut AsyncEnv<S>>> = const { std::cell::Cell::new(None) }; }
        fn t() { let (mut s, e, _) = setup();
        Function::new_typed_async(&mut s, &e, async |cx: &mut AsyncEnv<S>| -> i32 { K.set(Some(cx)); 1 }); }"""),
    "store_borrow_cannot_leave_with": ("lifetime", """async fn t(cx: &mut AsyncEnv<S>) { let n = cx.with(|mut env| env.data_mut()); }"""),
    "await_inside_with": ("E0728", """async fn t(cx: &mut AsyncEnv<S>) { cx.with(|env| { std::future::ready(()).await; }); }"""),
    "shared_store_borrow_cannot_leave": ("lifetime", """fn t(s: SharedStore) { let b = s.try_with(|store| store); }"""),
    "store_ref_cannot_produce_store_mut": ("E0599", """fn t(mut env: FunctionEnvMut<'_, S>) {
        let (a, mut view) = env.data_and_store(); let m = view.as_store_mut(); a.n = 1; drop(m); }"""),
    "store_ref_cannot_read_env_again": ("E0277", """fn t(mut env: FunctionEnvMut<'_, S>, h: FunctionEnv<S>) {
        let (a, view) = env.data_and_store(); let b = h.as_ref(&view); a.n = b.n + 1; }"""),
    "store_ref_cannot_write_env_again": ("E0277", """fn t(mut env: FunctionEnvMut<'_, S>, h: FunctionEnv<S>) {
        let (a, mut view) = env.data_and_store(); let b = h.as_mut(&mut view); a.n = 1; b.n = 2; }"""),
    "store_ref_cannot_call_guest": ("E0277", """fn t(mut env: FunctionEnvMut<'_, S>, f: Function) {
        let (a, mut view) = env.data_and_store(); f.call(&mut view, &[]); a.n = 1; }"""),
    "overlapping_nested_guest_calls": ("E0499", """async fn t(cx: &mut AsyncEnv<S>, f: Function) {
        let a = f.call_async(cx, &[]); let b = f.call_async(cx, &[]); drop((a, b)); }"""),
    "recursive_store_access": ("E0501", """fn t(cx: &mut AsyncEnv<S>) { cx.with(|_| { cx.with(|_| ()); }); }"""),
    "async_env_use_during_nested_call": ("E0499", """async fn t(cx: &mut AsyncEnv<S>, f: Function) {
        let a = f.call_async(cx, &[]); cx.with(|_| ()); drop(a); }"""),
    "overlapping_typed_calls": ("E0499", """fn t(mut s: Store, f: TypedFunction<(i32,), i32>) {
        let a = f.call_async(&mut s, (1,)); let b = f.call_async(&mut s, (2,)); drop((a, b)); }"""),
    "typed_nested_call_holds_async_env": ("E0499", """fn t(cx: &mut AsyncEnv<S>, f: TypedFunction<(i32,), i32>) {
        let a = f.call_async(cx, (1,)); cx.with(|_| ()); drop(a); }"""),
    "sync_call_rejects_async_env": ("E0277", """fn t(cx: &mut AsyncEnv<S>, f: Function) { let _ = f.call(cx, &[]); }"""),
    "with_cannot_return_a_call": ("E0515", """fn t(cx: &mut AsyncEnv<S>, f: Function) { let c = cx.with(|mut env| f.call_async(&mut env, &[])); drop(c); }"""),
    "cannot_implement_call_store": ("E0277", """struct Mine; impl AsyncCallStore for Mine {
        type Call<'a, R: 'static> = std::future::Pending<R> where Self: 'a;
        fn start_call<'a, R: 'static>(&'a mut self) -> Self::Call<'a, R> { std::future::pending() } }"""),
    "weak_handle_cannot_live_in_env": ("E0277", """struct W { shared: Option<WeakSharedStore> }
        fn t(mut s: Store) { let _ = FunctionEnv::new(&mut s, W { shared: None }); }"""),
    "released_env_is_unusable": ("E0501", """fn t(mut env: FunctionEnvMut<'_, S>) { let e = &mut env; e.release(|| { e.data_mut(); }); }"""),
}

failures = 0
for name, body in ACCEPT.items():
    path = UI / f"accept_{name}.rs"
    path.write_text(PRE + body)
    run = subprocess.run(RUSTC + [str(path)], capture_output=True, text=True)
    ok = run.returncode == 0
    failures += not ok
    print(f"{'PASS' if ok else 'FAIL'}  accepts  {name}")
    if not ok:
        print(run.stderr[:900])
for name, (code, body) in REJECT.items():
    path = UI / f"reject_{name}.rs"
    path.write_text(PRE + body)
    run = subprocess.run(RUSTC + [str(path)], capture_output=True, text=True)
    errors = [l for l in run.stderr.splitlines() if "error" in l]
    ok = run.returncode != 0 and (code in run.stderr)
    failures += not ok
    print(f"{'PASS' if ok else 'FAIL'}  rejects  {name}  [{code}]")
    if not ok:
        print("   rc", run.returncode, *errors[:4], sep="\n   ")
print(f"\n{len(ACCEPT)} accept, {len(REJECT)} reject, {failures} failures")
sys.exit(1 if failures else 0)
````

### `model/review_signatures.rs`

<!-- trial-file: model/review_signatures.rs -->
````rust
//! Scenarios from the independent review. All of them compile in safe code,
//! which is the point: the signatures alone cannot stop them, so the JavaScript
//! guest guard and the "derive the store once" rule are required.
#![forbid(unsafe_code)]
#![allow(unused)]
use m::*;
use std::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    task::{Context, Waker},
};

pub struct S {
    n: u32,
}

/// On JSPI a microtask can still resume the guest after this returns.
pub fn forget_then_free(mut store: Store, function: Function) {
    let mut call = Box::pin(function.call_async(&mut store, &[]));
    let _ = call.as_mut().poll(&mut Context::from_waker(Waker::noop()));
    std::mem::forget(call);
    drop(store);
}

type Task = Pin<Box<dyn Future<Output = Result<Vec<Value>, RuntimeError>>>>;
thread_local! {
    static SLOT: RefCell<Option<Task>> = const { RefCell::new(None) };
}

/// A host function in a resumed JSPI slice drops the future that owns its store.
pub fn host_drops_its_own_store(mut store: Store, env: FunctionEnv<S>, function: Function) {
    Function::new_typed_async(&mut store, &env, async |cx: &mut AsyncEnv<S>| -> i32 {
        drop(SLOT.with(|slot| slot.borrow_mut().take()));
        cx.with(|env| env.data().n) as i32
    });
    let owned: Task = Box::pin(async move {
        let mut store = store;
        function.call_async(&mut store, &[]).await
    });
    SLOT.with(|slot| *slot.borrow_mut() = Some(owned));
}

/// `AsStoreMut` is public, so a second `as_store_mut` can return another store.
struct Flip {
    store: Store,
    calls: u32,
}
impl AsStoreRef for Flip {
    fn as_store_ref(&self) -> StoreRef<'_> {
        self.store.as_store_ref()
    }
}
impl AsStoreMut for Flip {
    fn as_store_mut(&mut self) -> StoreMut<'_> {
        self.calls += 1;
        if self.calls == 2 {
            self.store = Store::new();
        }
        self.store.as_store_mut()
    }
}
pub fn flip(function: Function) {
    let mut store = Flip { store: Store::new(), calls: 0 };
    let _ = function.call_async(&mut store, &[]);
}
````

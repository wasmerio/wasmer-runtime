# 1. Wasmtime's async contract

Paths are under `crates/wasmtime/src/runtime/` at Wasmtime `fb792bf`, unless
noted.

**Wasmtime's central decision is that the call future borrows the store
exclusively until it finishes or is dropped.** There is no store lock or shared
store handle on the `call_async` path.

## Signatures

```rust
// func.rs:1129
pub async fn call_async(
    &self,
    store: impl AsContextMut<Data: Send>,
    params: &[Val],
    results: &mut [Val],
) -> Result<()>;

// func/typed.rs:137
pub async fn call_async(
    &self,
    store: impl AsContextMut<Data: Send>,
    params: Params,
) -> Result<Results>
where
    Params: Sync,
    Results: Sync;

// func.rs:849
pub fn wrap_async<T, F, P, R>(store: impl AsContextMut<Data = T>, func: F) -> Func
where
    F: for<'a> Fn(Caller<'a, T>, P) -> Box<dyn Future<Output = R> + Send + 'a>
        + Send
        + Sync
        + 'static,
    P: WasmTyList,
    R: WasmRet,
    T: Send + 'static;
```

## Life of a call

1. `on_fiber` (`fiber.rs:907`) allocates a stack and builds `FiberFuture`, which
   holds `&mut StoreOpaque` and the fiber (`fiber.rs:980`).
2. Each poll calls `resume_fiber` (`fiber.rs:738`). It swaps in
   `FiberResumeState` (`fiber.rs:580`): TLS activations, MPK mask, stack limit
   and executor.
3. An async import is a sync host function wrapped by `vmctx_async`
   (`func.rs:2350`). It calls `BlockingContext::block_on` (`fiber.rs:305`),
   which polls the host future **on the fiber stack**.
4. A pending future suspends the fiber with `StoreFiberYield::KeepStore`
   (`fiber.rs:410`).
5. A drop guard restores the thread's state when control returns. Wasmtime then
   asserts no TLS pointer still points into the fiber stack (`fiber.rs:798`).
6. Dropping the future (`fiber.rs:1021`) resumes the fiber with an error so it
   unwinds. Because the future still holds the store, cleanup can use it.

## What is enforced, and how

| Property | Mechanism | Kind |
| --- | --- | --- |
| One running call per store | `&mut` held by `FiberFuture` | Compile time |
| Host future cannot outlive the import | `for<'a>` on `Caller<'a, T>` and the future | Compile time |
| Host data crossing threads is `Send` | `T: Send`, `Send` host futures | Compile time |
| Fiber may cross threads | `unsafe impl Send for RawFiber` (`fiber.rs:572`) | Hand-audited |
| Task `Context` cannot escape | Anonymous lifetimes on `BlockingContext` (`fiber.rs:150`) | Compile time around `unsafe` |
| State follows the stack | One swap point, plus an assertion | Private invariant |
| No sync call from a fiber | `validate_sync_call` (`store.rs:2482`) | Runtime error |
| Wrong store | Store id checks | Runtime panic |
| Concurrent task holds no store across `.await` | `Accessor::with` (`component/concurrent.rs:480`) | Compile time, recursion panics |

## Details that are easy to get wrong

- `Config::async_support` is a deprecated no-op
  (`crates/wasmtime/src/config.rs:434`). A store becomes async-required when it
  gets async host functions or similar configuration.
- Once async is required, sync entry points return an error. That is why
  `Instance::new_async` exists (`instance.rs:200`). Wasmer has no such rule and
  does not need it.
- A sync `Func::call` from a fiber is refused, so a host frame can never sit
  beneath a suspension.
- Call futures are `Send` when the store data is. The `sync_call_from_fiber`
  test stores one as `dyn Future + Send` (`tests/all/async_functions.rs:1183`).
- Cancellation poisons only component stores (`component/store.rs:405`). A core
  Wasm instance stays usable with partial state.
- `Accessor` holds no borrow. `with` finds the store through a TLS slot set
  during a poll (`component/concurrent/tls.rs:47`).
- Concurrent execution exists only for components: `run_concurrent`
  (`component/concurrent.rs:1360`) and `func_wrap_concurrent`
  (`component/linker.rs:806`). Core Wasm has no equivalent.

## What the proposal takes and changes

| Wasmtime | Proposal | Reason |
| --- | --- | --- |
| `call_async` borrows the store | Same | This is the core soundness property |
| State swapped at one point | Same | PR 16 already does this |
| `Caller` and `Accessor` host shapes | Only `AsyncEnv` | One shape serves exclusive and shared calls |
| `&Accessor` | `&mut AsyncEnv<T>` | Recursive `with` and overlapping nested calls become compile errors |
| Host futures polled on the fiber | Polled on the executor stack | A deep future cannot overflow the fixed stack |
| `Send` futures | `!Send` | Wasmer's `Store` is `!Send`, and WASIX runs on one thread |
| `Instance::new_async` | Not added | A start function is a sync entry |
| Concurrency only for components | `SharedStore` | WASIX and NAPI need several core Wasm stacks |

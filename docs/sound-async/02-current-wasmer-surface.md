# 2. Current Wasmer surface

Paths are at PR 16, `aa65437e924`, unless noted.

**Today's `call_async` returns a `'static` future over a shared store handle, so
the compiler cannot prove who holds the store.** Most of the async machinery
exists to prove that at run time.

```rust
// lib/api/src/entities/function/mod.rs:447
pub fn call_async(
    &self,
    store: &impl AsStoreAsync,
    params: Vec<Value>,
) -> impl Future<Output = Result<Box<[Value]>, RuntimeError>> + 'static;
```

## Public async types

| Type | Location | Role |
| --- | --- | --- |
| `StoreAsync` | `lib/api/src/entities/store/async_.rs:12` | Shared `Rc` handle to the store |
| `AsStoreAsync` | `lib/api/src/entities/store/async_.rs:85` | Accepts anything holding that handle |
| `StoreAsyncReadLock` | `lib/api/src/entities/store/async_.rs:121` | `'static` read guard |
| `StoreAsyncWriteLock` | `lib/api/src/entities/store/async_.rs:139` | `'static` write guard |
| `AsyncFunctionEnv` | `lib/api/src/entities/function/async_host.rs:9` | With-env or without-env wrapper |
| `AsyncHostFunction` | `lib/api/src/entities/function/async_host.rs:39` | Trait for async host closures |
| `AsyncFunctionEnvMut` | `lib/api/src/entities/function/env/mod.rs:125` | `'static` environment handle |
| `AsyncFunctionEnvHandle` | `lib/api/src/entities/function/env/mod.rs:129` | Read guard for the environment |
| `AsyncFunctionEnvHandleMut` | `lib/api/src/entities/function/env/mod.rs:133` | Write guard for the environment |
| `Cothread` | `lib/api/src/entities/store/context.rs:573` | Embedder-switched coroutine state |

## Machinery that compensates for the signature

| Component | Size | What it does |
| --- | --- | --- |
| `LocalRwLock` | 956 lines | Async read-write lock with waiter lists |
| `StoreContext` | 1,405 lines | Thread-local store stack, borrow and pause counts, lent pointers, entry serials |
| `lent.rs` | 663 lines | JavaScript store-lend stack |
| `Function::call_async` docs | 66 lines | Rules the types do not carry |

## Native call path

1. `AsyncCallFuture` (`lib/api/src/backend/sys/async_runtime.rs:65`) owns a
   `corosensei` coroutine and a `StoreAsync`.
2. Each resume takes the write lock, installs a `StoreContext` entry and calls
   `switch_to_coroutine` (`async_runtime.rs:269`).
3. `Activation` (`async_runtime.rs:160`) swaps per-call VM state exactly around
   the stack switch.
4. An async import yields its host future to the call future. The future is
   polled on the executor's stack.
5. `block_on_host_future` (`async_runtime.rs:516`) suspends only if
   `StoreContext::top_can_suspend` (`context.rs:272`) holds. Otherwise
   `run_immediate` (`async_runtime.rs:579`) polls once and traps if pending.
6. `cancel` (`async_runtime.rs:445`) unwinds a suspended coroutine. If the
   thread is exiting, it forgets the coroutine instead.

## JavaScript call path

- A guest suspends through JSPI, and the engine owns its stack.
- `Activation` (`lib/api/src/backend/js/jspi.rs:99`) owns an `Rc` store handle
  and the write lock.
- A resumed guest runs from a **microtask with no `poll` on the stack**: see
  `wake` (`jspi.rs:658`) and `check` (`jspi.rs:954`).
- Host functions in that slice get the store from the activation through
  `hold_running` (`jspi.rs:848`).
- `lent.rs` limits nested calls to `MAX_DEPTH = 100` (`lent.rs:58`), so a V8
  stack overflow cannot unwind Rust frames silently.

## What PR 16 already got right

- Call state moves with its stack through `ActivationState`
  (`lib/vm/src/trap/traphandlers.rs:917`).
- Host futures are polled on the executor's stack.
- A guest suspends only where no host function holds its store, so suspended
  stacks hold no host frames.

## Defects found in the current API

Evidence is in the [probes](./03-soundness-probes.md).

| Defect | Severity | Where |
| --- | --- | --- |
| Two live `&mut T` from safe code | Undefined behavior (Miri) | `data_and_store_mut`, `lib/api/src/entities/function/env/mod.rs:85` |
| WASIX holds `&mut WasiEnv` across a guest call | Undefined behavior, found by reading | `lib/wasix/src/syscalls/wasix/call_dynamic.rs:117,152` on `next` |
| Overlapping calls corrupt a guest's shadow stack | Guest memory corruption | `Function::call_async` |
| A call awaited under a held lock never finishes | Hang | `AsStoreAsync::read_lock` |
| A kept host handle blocks `into_store` | Store cannot be recovered | `AsyncFunctionEnvMut` |
| Typed async import always traps on a sync entry | Valid call fails | `lib/api/src/backend/sys/entities/function/mod.rs:309` |

The typed import bug has a confirmed cause. The wrapper awaits `env_mut.write()`
after the host future finishes, and in a sync context `write` is `pending()`
forever (`lib/api/src/backend/sys/entities/function/env.rs:288`). Dynamic async
imports do not take that path and work.

## PR 16 validation baseline

These ran on arm64 macOS at `aa65437e924`. Use them to detect regressions.

| Check | Command | Result |
| --- | --- | --- |
| Format | `cargo fmt --all -- --check` | Pass |
| Native clippy | `cargo clippy -p wasmer -p wasmer-vm --all-targets --features "wasmer/sys,wasmer/cranelift,wasmer/wat,wasmer/singlepass,wasmer/experimental-async,wasmer/experimental-host-interrupt,wasmer/unsafe-cothread" --locked -- -D clippy::all` | Pass |
| JavaScript clippy | `cargo clippy --target wasm32-unknown-unknown -p wasmer --no-default-features --features js-default[,experimental-async] --locked -- -D clippy::all` | Pass for both |
| Native | `cargo test -p wasmer --lib --tests --features sys,cranelift,wat --locked` | 106 passed |
| Native async | Same, plus `experimental-async,experimental-host-interrupt,unsafe-cothread` | 181 passed |
| Singlepass async | `cargo test -p wasmer --features sys,singlepass,wat,experimental-async,experimental-host-interrupt,unsafe-cothread --locked --test async_activation --test cothread --test interrupt_async` | 34 passed |
| VM | `cargo test -p wasmer-vm --features experimental-host-interrupt --locked` | 56 passed, 1 ignored |
| Compilers | `cargo test -p wasmer-workspace --no-default-features --features cranelift,singlepass,wast,middlewares --test compilers --locked` | 1619 passed, 531 ignored |
| JS sync, Node 26 | `js-default,wat` | 75 passed, 4 ignored |
| JS module, Node 26 | `js-default,wat,js-serializable-module --test module` | 9 passed |
| JSPI, Node 26 and 24 | `js-default,experimental-async,wat` | 131 passed, 5 ignored on each |
| JSPI without string codegen | Same, with `--disallow-code-generation-from-strings` | 131 passed, 5 ignored on each |

The JavaScript rows ran from `lib/api` without `wasm-pack`:

```bash
CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUNNER=wasm-bindgen-test-runner \
WASM_BINDGEN_TEST_ONLY_NODE=1 \
NODE_ARGS="--require=./tests/fail_on_hang.cjs" \
cargo test --target wasm32-unknown-unknown --no-default-features --features js-default,experimental-async,wat
```

Node 24 also needs `--experimental-wasm-jspi` in `NODE_ARGS`. The runner must
match `wasm-bindgen` 0.2.130 from `Cargo.lock`.

These were not run locally: Linux, Windows, x86_64, LLVM, V8, Miri on PR 16, and
Bun.

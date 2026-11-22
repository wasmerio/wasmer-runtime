# 7. Consumers and backends

**WASIX and NAPI are the two real users of overlapping guest stacks, so they
define what `SharedStore` must support.** WASIX paths are on `next` at
`fa7ec59be53`. NAPI paths are at the local checkout `795dfe434db`.

## WASIX

WASIX context switching runs each context as a `call_async` future on one
thread.

| Piece | Today | Under the proposal |
| --- | --- | --- |
| Entry | `run_main_context` makes a `StoreAsync` and a `LocalPool` (`lib/wasix/src/state/context_switching.rs:110`) | `Store::into_shared`, same pool |
| Executor | `ThreadLocalExecutor` with an `unsafe impl Send` spawner that checks its thread (`lib/wasix/src/utils/thread_local_executor.rs:26`) | Unchanged |
| Create | Sync import uses `as_store_async` (`lib/wasix/src/syscalls/wasix/context_create.rs:68`) | `FunctionEnvMut::shared`, then `call_async(&mut shared, ..)` |
| Switch | `ctx.write().await`, unblock target, drop lock, await (`lib/wasix/src/syscalls/wasix/context_switch.rs:17`) | `cx.with(..)`, await, `cx.with(..)` |
| Re-entry | Sync `call_dynamic` calls the guest (`lib/wasix/src/syscalls/wasix/call_dynamic.rs:152`) | Async host function using `call_async(cx, ..)` |
| Cancel | Dropping an unblocker makes the context trap with `ContextCanceled` | Unchanged |

Requirements that shaped the design:

- A context future must be `'static`, because `create_context` spawns it.
- A sync import must be able to obtain the shared handle, because
  `context_create` is sync.
- The wait must hold no store borrow, or no other context can run.
- WASIX does not need `Send` futures.

**Migration hazard:** under PR 16, a guest entered from a sync host function
cannot suspend. `call_dynamic` is such a function today, so a `context_switch`
beneath it would trap with `YieldOutsideAsyncContext`. This was found by
reading, not by running. Porting PR 16 to `next` must convert `call_dynamic` to
an async host function at the same time.

## NAPI

NAPI's JavaScript build re-enters the guest from JavaScript while another guest
stack may be suspended. This is the hardest case for the proposal.

- `install_persistent_callback_state` keeps an `AsyncFunctionEnvMut` for the
  environment (`lib/napi/src/guest/callback_js.rs:38`).
- `snapi_host_invoke_wasm_callback` first tries the running host function
  through `with_current_mut`, then `try_write`, and otherwise returns
  `CALLBACK_DEFERRED` (`callback_js.rs:533-560`).
- Deferred callbacks run at the guest's event loop checkpoint.
- It stores a raw `FunctionEnvMut` pointer for callbacks made while a sync
  import is on the stack.

The planned mapping:

| Today | Proposed |
| --- | --- |
| Persistent `AsyncFunctionEnvMut` | `WeakSharedStore` |
| `try_write` | `SharedStore::try_with` |
| Lock unavailable | `StoreBusy`, then `CALLBACK_DEFERRED` |
| Raw pointer to the sync `FunctionEnvMut` | JavaScript runs inside `FunctionEnvMut::release` |

Open points:

- Whether every callback that finds the store busy can be deferred. NAPI does
  this today when `try_write` fails, but a JavaScript caller may need a
  synchronous result.
- The pinned NAPI calls `with_current_mut`, which `next`'s `lib/api` does not
  define. NAPI and `next` are already out of step, so check the NAPI revision
  before porting.

## JavaScript (JSPI)

- The engine owns the guest stack, and any JavaScript can call a host function.
- The borrow is not enough, because guests resume from microtasks. The [guest
  guard](./05-independent-review.md#the-javascript-guest-guard) covers this.
- Cancelling a guest that has not been woken uses an uncatchable trap, and the
  engine frees its stack.
- A guest resumed where Wasmer cannot see it has no lend, so its host functions
  fail and touch nothing.
- JavaScript that Wasmer never saw must not call a guest's export while a guest
  of that store runs. This limit remains documented.
- Keep the nested-call bound in `lent.rs`. Without it, a V8 stack overflow can
  unwind Rust frames without running them.

## V8

- V8's C API cannot switch stacks. `lib/api/src/backend/v8/async_runtime.rs`
  runs host futures synchronously on the same thread, and no V8 stack is
  suspended.
- Level 1 works because `poll` blocks while the import's future is driven. A
  call cannot be cancelled during an import, because `poll` has not returned.
- Level 2 supports lexical nesting only. Stacks cannot resume out of order, so a
  `SharedStore` task or synchronous entry whose import must wait fails with a
  trap instead of blocking.
- `Engine::supports_async` returns false for V8 on `next`, so WASIX already
  disables context switching there.

## Threading

`Store` is `!Send` on the backends checked, so all proposed call futures are
`!Send`. Embedders use `spawn_local` or a per-thread runtime. To support
`tokio::spawn` later, `Store` must become `Send` first, and requiring `Send`
host futures at that point is a breaking change.

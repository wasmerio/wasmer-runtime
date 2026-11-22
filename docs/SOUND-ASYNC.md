# Type-Safe Async API for Wasmer

**Status:** implemented in `lib/api` on the `sound-async` branch, 2026-11-21.
The WASIX and NAPI ports (migration steps 4 and 5) remain for `next`. The trials
and design notes are in [`sound-async/`](./sound-async/README.md).

## Summary

**Make the `&mut Store` borrow the only async lock, and let host code reach the
store only in synchronous slices.** Today's `'static` `call_async` hides the
store from the borrow checker. Wasmer then rebuilds exclusivity at run time with
an async lock, a thread-local context stack and borrow counters.

The proposed contract has three rules:

1. `Function::call_async` borrows the store exclusively until the call finishes
   or is dropped.
2. An async host function receives `&mut AsyncEnv<T>`. It reaches the store only
   through `with`, a synchronous closure, so no store borrow can cross an
   `.await`.
3. Several guest stacks on one store are an explicit opt-in through
   `SharedStore`. Each stack takes the store only for the slice it runs.

Nothing suspends while holding the store, so a native store never has a waiter.
**The async lock and its wake-up machinery are no longer needed.**

|  | Today | Proposed |
| --- | --- | --- |
| Public async types | 10 | 4, of which users write only `AsyncEnv`, plus 3 for opt-in shared stores |
| Overlapping calls on one store | Compile, then interleave | Compile error without `SharedStore` |
| Store borrow across `.await` | Allowed through lock guards | Compile error |
| Store handle outliving an import | Allowed | Compile error |
| Async store lock | 956-line `LocalRwLock` | None |

A stub-bodied type model of this API, built on Rust 1.99, accepts all 20
intended uses and rejects all 23 misuses. The
[implementation status](./sound-async/README.md#implementation-status) records
what was run against the real code.

JavaScript also needs one private runtime guard, because its engine can resume a
guest with no `poll` on the stack. The Backends section defines it.

## The problem

**One signature causes most of the complexity.** The future below is `'static`
and takes a shared handle, so the compiler cannot know who holds the store:

```rust
pub fn call_async(
    &self,
    store: &impl AsStoreAsync,
    params: Vec<Value>,
) -> impl Future<Output = Result<Box<[Value]>, RuntimeError>> + 'static
```

Everything a borrow would prove must then be enforced at run time:

- `LocalRwLock` (956 lines) arbitrates a store that many futures can name.
- `StoreContext` (1,405 lines) keeps a thread-local stack with borrow counts,
  paused borrows and lent pointers.
- The JavaScript `lent.rs` (663 lines) repeats the same accounting for host
  functions the engine can call.
- `Function::call_async` needs about 65 lines of documentation to state rules
  the types do not carry.

These probes use only safe Rust (`#![forbid(unsafe_code)]`) against
[wasmer-runtime#16](https://github.com/wasmerio/wasmer-runtime/pull/16) at
`aa65437e924`:

| Hole | What safe code does | Observed | Impact |
| --- | --- | --- | --- |
| Aliased host state | Calls `data_and_store_mut`, then `FunctionEnv::as_mut` on the returned `StoreMut` | Two live `&mut` at one address; **Miri reports undefined behavior** | Memory unsoundness, also in the sync API |
| Interleaved guest | Runs two overlapping `call_async` on one instance | Both calls find their shadow stack pointer corrupted | Guest memory corruption |
| Self-deadlock | Awaits a call while holding `read_lock` | Still pending after 1,000 polls | Hang |
| Escaped handle | Keeps `AsyncFunctionEnvMut` in a thread-local | `into_store` fails after the call has finished | Store cannot be recovered |

The first hole also has a real instance. WASIX `call_dynamic` on `next` keeps
the `&mut WasiEnv` from `data_and_store_mut` across a guest call, and any host
function that guest reaches gets another `&mut WasiEnv`. This one was found by
reading the code, not by running it.

PR 16 got three things right, and this proposal keeps them:

- Call state moves with its stack through `ActivationState`.
- Host futures are polled on the executor's stack.
- A guest suspends only where no host function holds its store.

## First principles

**A sound async Wasm call must uphold seven invariants, whatever the
implementation.** A Wasm store is mutable state, and a suspended call is a
paused stack that can point into it. Each invariant below follows from those two
facts.

1. **One mutator.** At any instant, at most one control flow can read or write a
   store mutably.
2. **No borrow crosses a suspension.** Once other code can run, no reference
   into the store, its host state or its memory may survive from before.
3. **Suspended stacks hold no host frames.** Only engine and guest frames may
   remain. Host locals on a parked stack would hide borrows and non-`Send` data
   from the compiler.
4. **Call state lives with its stack.** Trap handlers and similar per-call state
   are on the thread only while that stack runs, because stacks resume in any
   order.
5. **Cancel and leak are safe.** Dropping a call ends its stack without running
   guest or host code. Forgetting it leaves nothing that the store or thread
   still depends on.
6. **Thread affinity is in the types.** Anything that can cross a thread proves
   it with `Send`. The store is already `!Send`, so calls on it stay on one
   thread.
7. **Interleaving guests is opt-in.** Two stacks on one instance share globals
   and memory. C and Rust guests keep a shadow stack there, so interleaving is
   only correct for guests built for it.

Invariants 1 and 2 together produce the key simplification. **If no holder ever
suspends with the store, a thread can only find its store busy through
re-entrancy.** The holder is then below it on the same stack, so waiting can
never succeed. A store therefore needs no queue, no waker list and no async
lock. A busy store is an immediate error. JavaScript has one narrow exception,
covered under Backends.

## Wasmtime's contract

Wasmtime makes `call_async` safe with one decision: **the future borrows the
store exclusively until it completes or is dropped.** Most other guarantees
follow from that borrow. This analysis reads [Wasmtime at
`fb792bf`](https://github.com/bytecodealliance/wasmtime/tree/fb792bf7a143801c22c0ba5b16fe2c6085a8598b/crates/wasmtime/src/runtime),
the `main` branch on 2026-11-21.

| Guarantee | Type or mechanism | What it rules out | Enforced by |
| --- | --- | --- | --- |
| One running call per store | `Func::call_async(&self, store: impl AsContextMut<Data: Send>, ..)`; `FiberFuture` holds `&'a mut StoreOpaque` | Overlapping calls, store use during a call, dropping the store mid-call | Borrow checker |
| Host code cannot keep the store | `for<'a> Fn(Caller<'a, T>, P) -> Box<dyn Future<Output = R> + Send + 'a>` | A host future or store handle outliving the import | Borrow checker |
| Thread safety | `T: Send`, `Send` host futures, `block_on<F: Future + Send>` | `!Send` host state crossing threads on a fiber | Trait bounds, plus one audited `unsafe impl Send` |
| Waker cannot escape | `BlockingContext<'_, '_>` handed to a closure | A task `Context` used after its `poll` | Anonymous lifetimes around a small `unsafe` core |
| Call state lives with its stack | `FiberResumeState`, swapped only in `resume_fiber`, restored by a drop guard | Out-of-order resumes seeing another call's TLS, MPK mask or stack limit | Private invariant, plus a runtime assertion |
| Cancellation has the store | `FiberFuture::drop` resumes the fiber with an error, then poisons component stores | Freeing a stack with live frames | The future still owns the store borrow in `Drop` |
| No blocking call on a fiber | `validate_sync_call` | A sync call suspending underneath a host frame | Runtime error |
| Concurrent tasks never hold the store | `Accessor::with(FnOnce(Access<'_, T, D>) -> R)` | A store borrow across `.await` | Synchronous closure and higher-ranked lifetime; misuse panics |

Three parts are worth copying:

- **The store borrow is the lock.** Wasmtime has no store lock, no shared store
  handle and no reference count on the `call_async` path.
- **A borrow never outlives its capability.** `Caller<'a, T>` and
  `Accessor::with` make that a compile error.
- **State moves at one switch point.** Everything a call leaves on the thread is
  exchanged in `resume_fiber`.

Three parts should not be copied as they are:

- **Host futures are polled on the fiber stack.** A deep future can overflow the
  fixed stack, and Wasmtime's own comment on `vmctx_async` names this design as
  something to remove. Wasmer already polls host futures on the executor's
  stack, which is the better design.
- **Two async host shapes.** Core Wasm uses `Caller`, while components add
  `Accessor`. Only the second supports concurrency.
- **Types do not cover everything.** The `Send` argument for fibers is a
  hand-audited `unsafe impl`. Wrong-store use, sync calls on fibers and
  recursive `Accessor::with` are runtime checks. Those checks fail safely, which
  is the standard to hold.

## Proposed type surface

**The API has two levels: exclusive calls for almost every embedder, and an
opt-in shared store for guests that really run several stacks.** Both use the
same host function shape and the same `call_async`, so a host function does not
need to know which level called it.

```text
Call future  [=============== &mut Store for the whole call ================]
Guest stack  [ runs Wasm ]    . suspended, no host frames .     [ runs Wasm ]
Host future               [ with ] . await, no borrow . [ with ]

[ ... ]  has the store for this slice
 . . .   holds no store borrow
```

The call future keeps exclusivity for the whole call. It lends the store to one
synchronous slice at a time, and no slice lasts across a wait.

### Level 1: exclusive calls

```rust
impl Function {
    /// `store` is a store, the `AsyncEnv` of an async host function, or a
    /// `SharedStore`.
    pub fn call_async<'a, S: AsyncCallStore>(
        &'a self,
        store: &'a mut S,
        params: &'a [Value],
    ) -> S::Call<'a, Result<Box<[Value]>, RuntimeError>>;

    pub fn new_typed_async<T, Args, Rets>(
        store: &mut impl AsStoreMut,
        env: &FunctionEnv<T>,
        func: impl AsyncHostFunction<T, Args, Rets>,
    ) -> Self;
}

/// Sealed. `Call<'a, R>` is `AsyncCall<'a, R>`, or `AsyncCall<'static, R>` for
/// a `SharedStore`.
pub trait AsyncCallStore { /* private */ }

/// The future of every async call.
pub struct AsyncCall<'a, R> { /* private */ }

/// Sealed. Implemented for `for<'e> AsyncFn(&'e mut AsyncEnv<T>, Args..) -> Rets`.
pub trait AsyncHostFunction<T, Args, Rets>: 'static { /* private */ }

/// What an async host function holds across `.await`. It contains no borrow.
pub struct AsyncEnv<T> { /* private */ }

impl<T> AsyncEnv<T> {
    /// Borrows the store for one synchronous slice of a poll.
    pub fn with<R>(&mut self, f: impl FnOnce(FunctionEnvMut<'_, T>) -> R) -> R;
}
```

`TypedFunction::call_async` follows the same borrow. The `with` closure receives
the existing `FunctionEnvMut`, so sync and async host functions can share
helpers. A host function looks like this:

```rust
async fn fetch(cx: &mut AsyncEnv<State>, key: i32) -> Result<i32, RuntimeError> {
    let client = cx.with(|env| env.data().client.clone());
    let value = client.get(key).await?;
    cx.with(|mut env| env.data_mut().last = value);
    Ok(value)
}
```

Taking `&mut self` is stricter than Wasmtime's `&Accessor`. Recursive `with` and
overlapping nested guest calls become compile errors instead of runtime panics.

### Level 2: shared stores

```rust
impl Store {
    pub fn into_shared(self) -> SharedStore;
}

/// `Clone + !Send`. One running slice at a time, with no lock guards.
impl SharedStore {
    /// Fails at once if the store is busy: there is nothing to wait for.
    pub fn try_with<R>(&self, f: impl FnOnce(StoreMut<'_>) -> R) -> Result<R, StoreBusy>;

    pub fn into_store(self) -> Result<Store, Self>;
    pub fn downgrade(&self) -> WeakSharedStore;
}

impl<T> FunctionEnvMut<'_, T> {
    /// The store's shared handle, if it has one. It comes from a private
    /// back-reference in the store, because `T` must stay `Send`.
    pub fn shared(&self) -> Option<SharedStore>;

    /// Lets a `SharedStore` handle use the store while `f` runs.
    pub fn release<R>(&mut self, f: impl FnOnce() -> R) -> R;
}
```

Every async call uses the same method on `Function` and `TypedFunction`:

```rust
f.call_async(&mut store, &params).await?;  // exclusive call
f.call_async(cx, &params).await?;          // inside an async host function
f.call_async(&mut shared, &params).await?; // 'static guest task
```

A shared call keeps its own store handle and copies the function and params, so
its future borrows nothing and can be spawned. Its `&mut` gives all three forms
the same shape and does not make the call exclusive. A sync call from an async
host function stays `cx.with(|mut env| f.call(&mut env, &params))`.

WASIX `context_switch` keeps its structure. The lock guard becomes a `with`
slice, and the wait happens with no store borrow:

```rust
async fn context_switch(cx: &mut AsyncEnv<WasiEnv>, target: u64) -> Result<Errno, RuntimeError> {
    let wait = cx.with(|mut env| prepare_switch(&mut env, target))?;
    wait.await?;
    Ok(cx.with(|mut env| finish_switch(&mut env)))
}
```

### Closing the aliasing hole

`data_and_store_mut` becomes `data_and_store`, which returns the existing
`StoreRef`. `FunctionEnv::as_ref` stops accepting a plain `StoreRef`:

```rust
impl<T> FunctionEnvMut<'_, T> {
    pub fn data_and_store(&mut self) -> (&mut T, StoreRef<'_>);
}

impl<T> FunctionEnv<T> {
    /// Takes `AsStoreMut`, so a plain `StoreRef` cannot reach `T`.
    pub fn as_ref<'a>(&self, store: &'a impl AsStoreMut) -> &'a T;
}
```

`StoreRef` can still view memories and read function types, but it cannot reach
a `FunctionEnv` or call a guest while `&mut T` is live. Code that grows a memory
or uses a table or global clones the handle from `T` first, then uses
`FunctionEnvMut`. A trial port on `next` type-checks WASIX and the C API imports
with about 70 changed lines in 8 files. NAPI has not been ported.

## Soundness argument

**On `sys`, the compiler enforces four of the seven invariants outright, and a
small private core upholds the other three.** JavaScript adds one runtime guard.
Misuse of the public API is either a compile error or a safe runtime failure.

| Invariant | Enforced by | Misuse the model rejects | Remaining runtime behavior |
| --- | --- | --- | --- |
| 1. One mutator | `&'a mut` on the call; `&mut self` on `AsyncEnv` | Overlapping calls, store use or drop during a call (E0499, E0505) | `StoreBusy` on a shared store |
| 2. No borrow crosses a suspension | `with` takes a synchronous closure with a higher-ranked lifetime | `.await` inside `with` (E0728); returning a store borrow | None |
| 3. No host frames on suspended stacks | Host futures live in the call future; sync frames cannot suspend | Not expressible in types | `YieldOutsideAsyncContext` trap |
| 4. Call state lives with its stack | One private switch point | Not expressible in types | Internal assertion |
| 5. Cancel and leak are safe | Follows from 3 and 4 | Not expressible in types | Stack is discarded or leaked |
| 6. Thread affinity | `Store: !Send` carries into every future | Sending a call future (E0277) | None |
| 7. Interleaving is opt-in | Only `SharedStore` can overlap calls | Overlapping calls on a plain store (E0499) | Guest correctness is the embedder's under `SharedStore` |

The hard cases follow directly:

- **Dropped call.** The call drops the host future first, while it can still
  lend the store. It then discards a stack that holds only engine and guest
  frames, so no user code runs. The instance stays usable with partial guest
  state, as it does after a trap and in Wasmtime's core API.
- **Forgotten call on `sys`.** The `&mut Store` borrow ends and the stack leaks.
  Nothing refers to that stack, because its state left the thread when it
  suspended.
- **Forgotten call on JavaScript.** The engine can still resume the guest, so
  the end of the borrow proves nothing. The guest guard under Backends keeps
  this case safe.
- **Panicking host future.** The panic reaches whoever polled the call. Dropping
  the call then ends the stack as above.
- **Sync host function calling a guest.** This still works, but that guest
  cannot suspend because a host frame is below it. A pending async import traps.
  A host function that needs to wait must be async and use `call_async(cx, ..)`.
- **Start function.** Wasm has no async start function, so instantiation stays
  synchronous through `Instance::new`. An async import that a start function
  calls works if it is ready and traps if it has to wait.

Six obligations remain in private `unsafe` code and need Miri and direct tests:

1. State is exchanged at exactly one switch point, including when control
   returns by unwinding.
2. `AsyncEnv` holds a store pointer only during a poll by its own call.
3. Discarding a suspended stack never touches the store.
4. A shared store hands out at most one slice at a time, including across
   `release`.
5. A call derives its store from `as_store_mut` exactly once, because
   `AsStoreMut` is a public trait and a later call could return a different
   store.
6. On JavaScript, the guest guard holds whenever a guest can run with no `poll`
   on the stack.

Three checks stay dynamic and fail safely:

- Using a handle with the wrong store panics, as in Wasmtime.
- Polling a task from inside a slice of its own store panics, as `sys` does
  today. An executor whose `wake` polls inline must defer that poll.
- `with` panics if a task is dropped inside another slice and its host future's
  destructor asks for the store, so destructors should not use it.

## Backends

**One public contract works on all three backends, but only `sys` can implement
it entirely in Rust.** JavaScript engines can call host functions outside Rust's
view, and V8's C API cannot switch stacks.

|  | `sys` | JavaScript (JSPI) | V8 |
| --- | --- | --- | --- |
| Guest stack | `corosensei` coroutine owned by the call future | Engine-owned JSPI stack | The calling OS stack, never suspended |
| Level 1 | Full | Full, with the guest guard below. Host futures run only inside `poll` | Yes: `poll` blocks while a pending import's future is driven |
| Level 2 overlap | Full | Full, with the guest guard below | Lexical nesting only; stacks cannot resume out of order, so a task whose import must wait fails with a trap |
| Cancellation | Drop the host future, then discard the stack | Unwind with an uncatchable trap. A guest already woken runs until it leaves the stack, with its host functions refused | Not possible during an import, because `poll` has not returned |
| Runtime check that remains | Wrong-store handles | Guest guard, and a lend or live call for each host function | Thread and store ownership checks |

**JavaScript needs a private guest guard, because the engine resumes a guest
from a microtask with no `poll` on the stack.** The borrow cannot protect that
slice. A forgotten call ends the borrow while its guest can still run and reach
the store. The guard has three rules:

1. A started guest co-owns the store's allocation until it leaves the engine's
   stack, so dropping the `Store` cannot free memory under it.
2. A call that is neither finished nor dropped marks the store as held. If its
   borrow ended anyway because the call was forgotten, every public store entry
   panics until the guest leaves.
3. Dropping a call marks its guest cancelled. If the engine can no longer be
   stopped from resuming it, its host functions fail without touching the store,
   and the store is usable at once. A new guest call waits for that guest to
   leave the stack, or fails if it is synchronous.

This is one flag and one owner per call, not a lock. Any JavaScript can also
call a host function, so each host function still checks for a current lend or
live call. The existing limit stays documented: JavaScript that Wasmer never saw
must not call a guest's export while a guest of that store runs.

## What gets removed

**Nine of today's ten public async types go away, along with the async lock and
most of the async store-context bookkeeping.**

| Today | After this proposal |
| --- | --- |
| `StoreAsync`, `Store::into_async` | `&mut Store`; `SharedStore` for the opt-in case |
| `AsStoreAsync` | `AsStoreMut` |
| `StoreAsyncReadLock`, `StoreAsyncWriteLock` | `AsyncEnv::with`, `SharedStore::try_with` |
| `AsyncFunctionEnvMut`, `AsyncFunctionEnvHandle`, `AsyncFunctionEnvHandleMut` | `AsyncEnv<T>` |
| `AsyncFunctionEnv` | Removed |
| `AsyncHostFunction` | Kept and sealed, implemented through `AsyncFn` |
| `Cothread` and `unsafe-cothread` | Removed |
| `FunctionEnvMut::as_store_async` | `FunctionEnvMut::shared` |
| `data_and_store_mut` | `data_and_store` |
| `LocalRwLock` and its waiter lists | Deleted |
| Async `StoreContext` entries, pause counts, lent pointers and entry serials | Deleted; sync entry tracking stays only where trampolines need it |

PR 16's activation swap and its async test suites stay. The tests become the
acceptance suite for the new API.

## Migration

**Land the aliasing fix first, then move consumers onto the new API before
deleting the old one.** The async API is behind `experimental-async`, so it can
break, but WASIX and NAPI need a path that keeps working at each step. The work
is a PR stacked on PR 16's branch.

1. **Fix aliasing.** Rename `data_and_store_mut` to `data_and_store`, return
   `StoreRef` from it, and make `FunctionEnv::as_ref` take `AsStoreMut`. Port
   the roughly 93 uses in 31 files across `next` and NAPI. Done when no safe
   path yields two `&mut T`, and the alias probe is a compile error.
2. **Add Level 1.** Ship `AsyncEnv`, the borrowing `call_async` and the
   JavaScript guest guard in a temporary `async_v2` module. Done when PR 16's
   native and JSPI async suites pass through it, including Miri, forgotten and
   dropped JSPI calls stay safe, and a ready typed async import works on a sync
   entry.
3. **Add Level 2.** Ship `SharedStore`, its weak handle,
   `FunctionEnvMut::shared` and `release`. Done when overlapping tasks,
   cancellation and forgotten tasks pass under Miri.
4. **Port WASIX.** `run_main_context` uses `into_shared`. `context_switch` and
   `call_dynamic` become `AsyncEnv` host functions, and `context_create` spawns
   `call_async(&mut shared, ..)` with the handle from `FunctionEnvMut::shared`.
   Done when the `context_switching` fixtures pass.
5. **Port NAPI.** The persistent `AsyncFunctionEnvMut` becomes a
   `WeakSharedStore`. `try_write` becomes `try_with`, `StoreBusy` keeps the
   existing `CALLBACK_DEFERRED` path, and JavaScript called from a sync host
   function runs inside `release`. Done when the NAPI suite passes on the
   JavaScript backend.
6. **Delete the old surface.** Remove the types listed above and `LocalRwLock`,
   and promote `async_v2` to the public names.

The largest risk is step 5. NAPI re-enters the guest from JavaScript while
another guest stack is suspended, so it depends most on the exact semantics of
`try_with` and `release`. Prototype that path before committing to step 6.

## Decisions

**All five decisions are settled, and the first two define the public
contract.**

| Decision | Choice | Trade-off |
| --- | --- | --- |
| Should call futures be `Send`? | **No.** Keep them `!Send`, matching `Store` today | Embedders use `spawn_local` or a per-thread runtime. Adding a `Send` bound on host futures later is a breaking change |
| Should host functions be able to hold the store across `.await`? | **No.** Offer only `AsyncEnv` | Slightly more verbose than Wasmtime's `Caller`, but one host shape serves both levels |
| Remove `unsafe-cothread`? | **Yes.** No use exists in `lib/api` outside its own tests, WASIX on `next`, or NAPI | External embedders are unknown. Its stacks hold host frames, which breaks invariant 3 |
| Build on PR 16? | **Yes, as a stacked PR.** PR 16 does not need to merge first | This work must be rebased when PR 16 changes, and it later deletes some of PR 16's JavaScript and store-context code |
| What does Level 2 do on V8? | **Allow lexical nesting**, and fail clearly when a task needs an out-of-order resume | Guests that switch stacks out of order cannot run on V8 |

One question stays open: whether NAPI can accept deferring every callback that
arrives while the store is busy. It does this today when `try_write` fails, so
the proposal keeps that behavior, but step 5 must confirm it.

## Sources

- Wasmtime at `fb792bf`:
  [`func.rs`](https://github.com/bytecodealliance/wasmtime/blob/fb792bf7a143801c22c0ba5b16fe2c6085a8598b/crates/wasmtime/src/runtime/func.rs),
  [`func/typed.rs`](https://github.com/bytecodealliance/wasmtime/blob/fb792bf7a143801c22c0ba5b16fe2c6085a8598b/crates/wasmtime/src/runtime/func/typed.rs),
  [`fiber.rs`](https://github.com/bytecodealliance/wasmtime/blob/fb792bf7a143801c22c0ba5b16fe2c6085a8598b/crates/wasmtime/src/runtime/fiber.rs),
  [`store.rs`](https://github.com/bytecodealliance/wasmtime/blob/fb792bf7a143801c22c0ba5b16fe2c6085a8598b/crates/wasmtime/src/runtime/store.rs),
  [`component/concurrent.rs`](https://github.com/bytecodealliance/wasmtime/blob/fb792bf7a143801c22c0ba5b16fe2c6085a8598b/crates/wasmtime/src/runtime/component/concurrent.rs)
- Wasmer:
  [wasmer-runtime#16](https://github.com/wasmerio/wasmer-runtime/pull/16),
  [wasmerio/wasmer#7183](https://github.com/wasmerio/wasmer/pull/7183), and the
  `next` branch at `fa7ec59be53`

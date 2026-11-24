# Async calls

This is the design of the async API in `lib/api`, behind the
`experimental-async` feature: the contract it offers, why that contract is
sound, how each backend implements it, and what it does not support yet. It
is implemented for `sys`, JavaScript and V8. WASIX and NAPI live on `next` and
are not ported to it yet.

## The contract

**The `&mut Store` borrow is the only async lock, and host code reaches the
store only in synchronous slices.** Three rules follow:

1. `Function::call_async` and `TypedFunction::call_async` borrow the store
   exclusively until the call finishes or is dropped.
2. An async host function receives `&mut AsyncEnv<T>`. It reaches the store
   only through `AsyncEnv::with`, a synchronous closure, so no store borrow
   can cross an `.await`.
3. Several guest stacks on one store are an explicit opt-in through
   `SharedStore`. Each stack takes the store only for the slice it runs.

Nothing suspends while it holds the store, so a store never has a waiter, and
there is no async lock.

```rust
f.call_async(&mut store, &params).await?;  // exclusive call
f.call_async(cx, &params).await?;          // inside an async host function
f.call_async(&mut shared, &params).await?; // 'static guest task

async fn fetch(cx: &mut AsyncEnv<State>, key: i32) -> Result<i32, RuntimeError> {
    let client = cx.with(|env| env.data().client.clone());
    let value = client.get(key).await?;
    cx.with(|mut env| env.data_mut().last = value);
    Ok(value)
}
```

```text
Call future  [=============== &mut Store for the whole call ================]
Guest stack  [ runs Wasm ]    . suspended, no host frames .     [ runs Wasm ]
Host future               [ with ] . await, no borrow . [ with ]

[ ... ]  has the store for this slice
 . . .   holds no store borrow
```

An async host function need not be `async`. `Function::new_typed_async` and
`Function::new_async` also take a function that returns a `Deferred`: the
results of the call at once, or a future for them. It receives the same
`&mut AsyncEnv<T>`, and reaches the store through the same slices. A host
function that usually has its results at once is written this way, because an
`async` function pays for being able to wait on every call (see
[`sys`](#sys)).

```rust
fn read(cx: &mut AsyncEnv<State>, fd: i32) -> Deferred<i32> {
    if let Some(byte) = cx.with(|mut env| env.data_mut().buffered(fd)) {
        return Deferred::ready(byte);             // the call is over
    }
    let mut cx = cx.clone();                      // for the future to keep
    Deferred::pending(async move {                // the guest waits
        let byte = receive(fd).await;
        cx.with(|mut env| env.data_mut().count(fd));
        byte
    })
}

let read = Function::new_typed_async(&mut store, &env, read);
```

`Deferred::pending` takes a `'static` future, which keeps a clone of the
`AsyncEnv`. A clone is a handle and holds no borrow, like the one
`FunctionEnvMut::async_env` gives a sync host function. `with` still decides
at each use whether the store can be had: it panics outside a poll of a call
on its store, and inside another `with`.

### Exclusive calls

```rust
impl Function {
    /// `store` is a store, the `AsyncEnv` of an async host function, or a
    /// `SharedStore`.
    pub fn call_async<'a, S: AsyncCallStore>(
        &'a self,
        store: &'a mut S,
        params: &'a [Value],
    ) -> S::Call<'a, Result<Box<[Value]>, RuntimeError>>;

    /// `Kind` is inferred: `ReturnsFuture` for an `async` function,
    /// `ReturnsDeferred` for one that returns a `Deferred`.
    pub fn new_typed_async<T: Send + 'static, Args, Rets, Kind>(
        store: &mut impl AsStoreMut,
        env: &FunctionEnv<T>,
        func: impl AsyncHostFunction<T, Args, Rets, Kind>,
    ) -> Self;
}

/// `Clone`: a handle, which holds no borrow.
impl<T> AsyncEnv<T> {
    /// Borrows the store for one synchronous slice of a poll.
    pub fn with<R>(&mut self, f: impl FnOnce(FunctionEnvMut<'_, T>) -> R) -> R;
}

/// The results of a call, or a future for them.
pub struct Deferred<R> { /* private */ }

impl<R> Deferred<R> {
    pub fn ready(results: R) -> Self;
    pub fn pending(future: impl Future<Output = R> + 'static) -> Self;
}

impl<T> FunctionEnvMut<'_, T> {
    /// A handle for code that outlives this borrow to keep.
    pub fn async_env(&self) -> AsyncEnv<T>;
}
```

`AsyncCallStore` is sealed. Its `Call<'a, R>` is `AsyncCall<'a, R>`, or
`AsyncCall<'static, R>` for a `SharedStore`. `AsyncHostFunction` is sealed too,
and implemented for `for<'e> AsyncFn(&'e mut AsyncEnv<T>, Args..) -> Rets` and
for `for<'e> Fn(&'e mut AsyncEnv<T>, Args..) -> Deferred<Rets>`. `new_async` is
the dynamic form, and takes the same two kinds of function
(`AsyncDynamicHostFunction`). The `with` closure receives the ordinary
`FunctionEnvMut`, so sync and async host functions can share helpers.

`AsyncEnv` methods take `&mut self`, so a recursive `with` and two overlapping
nested calls through one `AsyncEnv` are compile errors. Through a second
handle, a clone or the one of `FunctionEnvMut::async_env`, they are not: a
recursive `with` panics, and two nested calls that overlap are two guest
stacks on one store, which is only correct for a guest built for it, as on a
`SharedStore`. A sync call from an async host function is
`cx.with(|mut env| f.call(&mut env, &params))`.

### Shared stores

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
    /// The store's shared handle, if it has one.
    pub fn shared(&self) -> Option<SharedStore>;
    /// Lets a `SharedStore` handle use the store while `f` runs.
    pub fn release<R>(&mut self, f: impl FnOnce() -> R) -> R;
}
```

A call on a `SharedStore` keeps its own handle and copies the function and
params, so its future borrows nothing and can be spawned. Its `&mut` gives the
three call forms the same shape. It does not make the call exclusive.
`FunctionEnvMut::shared` reads a private back-reference in the store, because
`T` must stay `Send` and the handle is `Rc`-based.

### Host state and the store

`FunctionEnvMut::data_and_store` returns `(&mut T, StoreRef<'_>)`, and
`FunctionEnv::as_ref` takes `AsStoreMut`. The `StoreRef` can read memories,
globals and tables, but it cannot reach `T` through a `FunctionEnv` or run a
guest while the `&mut T` is live. An operation that needs a mutable store
clones a handle out of `T` first, then uses the `FunctionEnvMut`. This closes a hole that the earlier `data_and_store_mut`
had in the sync API too: safe code could get two `&mut T` at one address.

The misuses the types reject are compile-fail doctests on
`COMPILE_FAIL_CONTRACT` in
[`async_call.rs`](../lib/api/src/entities/function/async_call.rs).

## Invariants

**A sound async Wasm call upholds seven invariants, whatever the
implementation.** A store is mutable state, and a suspended call is a paused
stack that can point into it. Each invariant follows from those two facts.

1. **One mutator.** At any instant, at most one control flow can read or write
   a store mutably.
2. **No borrow crosses a suspension.** Once other code can run, no reference
   into the store, its host state or its memory may survive from before.
3. **Suspended stacks hold no host frames.** Only engine and guest frames may
   remain. Host locals on a parked stack would hide borrows and `!Send` data
   from the compiler.
4. **Call state does not outlive its slice.** Trap handlers and other
   per-call state are on the thread only while that stack runs, because stacks
   resume in any order.
5. **Cancel and leak are safe.** Dropping a call ends its stack without running
   guest or host code. Forgetting it leaves nothing that the store or thread
   still depends on.
6. **Thread affinity is in the types.** Anything that can cross a thread proves
   it with `Send`. `Store` is `!Send`, so calls on it stay on one thread.
7. **Interleaving guests is opt-in.** Two stacks on one instance share globals
   and memory. C and Rust guests keep a shadow stack there, so interleaving is
   only correct for guests built for it.

Invariants 1 and 2 give the main simplification. **If no holder ever suspends
with the store, a thread can only find its store busy through re-entrancy.**
The holder is then below it on the same stack, so waiting could never succeed.
A busy store is an immediate error, and needs no queue, waker list or lock.

## Soundness

**On `sys`, the compiler enforces four of the seven invariants, and a small
private core upholds the other three.** Misuse of the public API is either a
compile error or a safe runtime failure.

| Invariant | Enforced by | Misuse rejected | What is left at run time |
| --- | --- | --- | --- |
| 1. One mutator | `&'a mut` on the call; `&mut self` on `AsyncEnv` | Overlapping calls, store use or drop during a call | `StoreBusy` on a shared store |
| 2. No borrow crosses a suspension | `with` takes a synchronous closure with a higher-ranked lifetime | `.await` inside `with`; returning a store borrow | None |
| 3. No host frames on suspended stacks | Host futures live in the call future; sync frames cannot suspend | Not expressible in types | `YieldOutsideAsyncContext` trap |
| 4. Call state does not outlive its slice | Installed and removed around each resumption | Not expressible in types | Internal assertion |
| 5. Cancel and leak are safe | Follows from 3 and 4 | Not expressible in types | The stack is discarded, or leaked |
| 6. Thread affinity | `Store: !Send` carries into every future | Sending a call future | None |
| 7. Interleaving is opt-in | Only `SharedStore` can overlap calls | Overlapping calls on a plain store | Guest correctness is the embedder's under `SharedStore` |

The hard cases:

- **Dropped call.** The call drops its host future first, while it can still
  lend the store. It then discards a stack that holds only engine and guest
  frames, so no user code runs. The instance stays usable with partial guest
  state, as after a trap.
- **Forgotten call on `sys`.** The borrow ends and the stack leaks. Nothing
  refers to that stack, because its state left the thread when it suspended.
- **Forgotten call on JavaScript.** The engine can still resume the guest, so
  the end of the borrow proves nothing. The guest guard keeps this case safe;
  see [JavaScript](#javascript).
- **Panicking host future.** The panic reaches whoever polled the call.
  Dropping the call then ends the stack as above.
- **Sync host function calling a guest.** This works, but that guest cannot
  suspend, because a host frame below it holds the store. A ready async import
  runs at once, and one that must wait traps. A host function that needs to
  wait must be async and use `call_async(cx, ..)`.
- **Start function.** Wasm has no async start function, so instantiation stays
  synchronous through `Instance::new`, with the same rule as a sync host
  function. A start function that must wait should become an exported init
  function, called with `call_async`.

Obligations of the private `unsafe` code:

1. A suspended stack holds the frames of the guest and of the import that
   suspended it, with nothing a destructor would have to release: it is reset,
   not unwound.
2. `AsyncEnv` holds a store pointer only during a poll by its own call.
3. Discarding a suspended stack never touches the store.
4. A shared store hands out at most one slice at a time, including across
   `release`.
5. A call derives its store from `as_store_mut` exactly once, because
   `AsStoreMut` is a public trait and a later call could return another store.
6. On JavaScript, the guest guard holds whenever a guest can run with no
   `poll` on the stack.

Checks that stay dynamic, and fail safely:

- Using a handle with the wrong store panics.
- Polling a task from inside a slice of its own store panics. An executor
  whose `wake` polls inline must defer that poll.
- `with` panics if a task is dropped inside another slice of its shared store
  and its host future's destructor asks for the store. Destructors should not
  use the store.

## Backends

**One public contract works on all three backends, but only `sys` implements
it entirely in Rust.** JavaScript engines call host functions outside Rust's
view, and V8's C API cannot switch stacks.

|  | `sys` | JavaScript (JSPI) | V8 |
| --- | --- | --- | --- |
| Guest stack | A Wasm stack of the VM, owned by the call future while its guest waits | An engine-owned JSPI stack | The calling OS stack, never suspended |
| Exclusive calls | Full | Full. The engine drives the call, and host futures run on the microtask queue | `poll` blocks while a pending import's future is driven |
| Shared stores | Full | Full | Lexical nesting only. A task whose import must wait fails with a trap |
| Cancellation | Drop the host future, then discard the stack | Drop the host future, and unwind the guest with a trap it cannot catch | Not during an import, because `poll` has not returned |

### `sys`

Each `call_async` runs its guest on a Wasm stack of its own, a
`wasmer_vm::SuspendableCall`
([`async_runtime.rs`](../lib/api/src/backend/sys/async_runtime.rs)). An async
import polls its host future once where it is called, on the stack of whoever
polls the call. If the future is pending, the import suspends the guest, with
only engine and guest frames on its stack, and the call future polls the host
future on the executor's stack, then resumes the guest with what it returned.
A deep host future therefore cannot overflow the guest's Wasm stack, and an
import whose future is ready at once switches no stack. A typed async host
function also keeps the heap block of a future that was ready at once, and
makes its next future there, so such a call does not allocate either.

A typed function that returns `Deferred` has a trampoline of its own. It
calls the function as the trampoline of a sync host function does, with the
store published for the slices the function takes, and returns the results if
the function gave them: no future is made, placed or polled, and the call
costs what a call of a sync host function costs. Only if the function gave a
future does the trampoline ask whether the guest can wait, and poll, as the
one of an `async` function does. The function has returned before its guest
waits. A dynamic function that returns `Deferred` is called through a future
like any other dynamic one.

Nothing a call leaves on the thread outlives the slice that put it there. Each
resumption installs what a synchronous call installs, the store context entry
of the call and the trap handler, and `wasmer_vm::ActivationState`, which holds
the stores the interrupt registry sees as entered, and takes them off when
control comes back, whether the guest waits, returns or unwinds.

A call's Wasm stack comes from the VM's stack cache and goes back to it when
the call is over or dropped. A dropped call drops its host future and resets
its stack, which runs no guest or host code: a suspended stack holds only the
frames of the guest and of the import that suspended it. A call dropped as its
thread exits gives up its interrupt registrations.

`baremetal` does not support `experimental-async`, and the two features fail
to build together. A `baremetal` trap leaves through the embedder's unwinder,
which cannot stop at the coroutine a call's guest runs on.

### JavaScript

A call runs the `WebAssembly.promising` form of a function, and an async host
function is a `WebAssembly.Suspending` import whose promise runs the host
future on the microtask queue
([`jspi.rs`](../lib/api/src/backend/js/jspi.rs)). JavaScript runs one thing at
a time, and a microtask only on an empty stack, so a resumed guest overlaps
nothing else that uses its store. A guest holds nothing, and only host code
takes the store, one synchronous slice at a time.

**The borrow alone is not enough, because the engine resumes a guest from a
microtask with no `poll` on the stack.** A forgotten call ends its borrow while
its guest can still run. The guest guard
([`guard.rs`](../lib/api/src/backend/js/guard.rs)) keeps that safe with one
counter per store:

1. A call that is neither finished nor dropped co-owns the store's allocation,
   so dropping the `Store` cannot free memory under it.
2. It also marks the store as held. If its borrow ended because the call was
   forgotten, every public store entry panics until its guest is done.

A dropped call gives the store up at once. Its guest is woken with a trap no
exception handler catches, so its stack unwinds without running guest code. A
guest that was already resumed runs on until its next import, which fails.

Each host function still checks for a current lend or live call, because any
JavaScript can call it. JavaScript that Wasmer never saw must not call a
guest's export while a guest of that store runs. The engine refuses to suspend
a guest that was entered synchronously, so there an async import fails even if
it is ready.

### V8

A guest stays on the calling OS stack, and `poll` runs the whole call
([`async_call.rs`](../lib/api/src/backend/v8/async_call.rs)). An exclusive call
blocks its thread while an import's future is driven, so another thread, or the
future itself, must wake it. Anything else that would have to suspend fails at
once: a synchronous entry, and a `SharedStore` task, which could be waiting
for a task that cannot run until it returns.

## Interrupts

With `experimental-host-interrupt` on `sys`, an interrupt reaches every call
that has entered the store, also a suspended one: the interrupt wakes it, and
it ends with `HostInterrupt` at its next poll. A forgotten call keeps the flag
it holds. A new call on the store gets a fresh flag, so that a forgotten call
cannot keep the store interrupted, and the store's interrupt entries are
released when it drops.

This has a known gap. A call nested behind a switch of stacks is taken for a
new call, so it can run after its caller was interrupted. That happens when an
async import calls `call_async(cx, ..)` after its future waited, since its own
call's entry of the store is off the thread while the guest waits, or when an
interrupt lands between the poll and the entry of a `call_async` made from a
sync host function. A nested call made in the first poll of an import's future
is refused, as that poll runs with its call on the thread
(`an_async_call_nested_after_an_interrupt_is_interrupted`).
`an_async_call_nested_after_waiting_is_interrupted` in
[`interrupt_async.rs`](../lib/api/tests/interrupt_async.rs) and a unit test in
[`interrupt_registry/unix.rs`](../lib/vm/src/interrupt_registry/unix.rs) record
the gap, and are ignored until it is fixed.

## Limitations and porting notes

- **Futures are `!Send`.** `Store` is `!Send`, so embedders use `spawn_local`
  or a per-thread runtime. Supporting `tokio::spawn` needs a `Send` store
  first, and requiring `Send` host futures then is a breaking change.
- **WASIX.** `run_main_context` should use `into_shared`. `context_switch` and
  `call_dynamic` become async host functions, and `context_create` spawns
  `call_async(&mut shared, ..)` with the handle from `FunctionEnvMut::shared`.
  `call_dynamic` must become async in the same change: a guest under a sync
  host function cannot suspend, so a context switch beneath it would trap.
- **NAPI.** Its persistent async env becomes a `WeakSharedStore`, `try_write`
  becomes `try_with`, `StoreBusy` keeps the `CALLBACK_DEFERRED` path, and
  JavaScript called from a sync host function runs inside `release`. Whether
  every callback that finds the store busy can be deferred is still open.
- **V8** supports shared stores only as lexical nesting, and cannot cancel a
  call during an import.
- **Miri** cannot run coroutine switches. It covers the store context and the
  provenance of slices, and the native test suites cover the switches.

## Decisions

| Decision | Rejected alternative | Why |
| --- | --- | --- |
| `&mut Store` is the only async lock | A shared handle with an async read-write lock | The compiler proves exclusivity, and a store with no suspended holder needs no waiters |
| Async host functions get `&mut AsyncEnv<T>` | Hold `FunctionEnvMut` across `.await`, as Wasmtime's `Caller` does | A held borrow blocks other stacks, and one host shape serves exclusive and shared calls |
| `AsyncEnv` methods take `&mut self` | `&self`, as Wasmtime's `Accessor` does | Recursive `with` and overlapping nested calls become compile errors |
| Call futures stay `!Send` | `Send` futures and host futures | `Store` is `!Send`, and WASIX does not need it |
| One `call_async`, with the difference in a sealed trait | `AsyncEnv::call` and `SharedStore::call` | One way to call, and `cx` reborrows like a store |
| `SharedStore` owns the store behind `Rc` | A scoped `run_concurrent` that borrows the store | A forgotten scope would end the borrow while handles still reach the store |
| `try_with` fails instead of waiting | Async acquisition | On one thread, a busy store means the holder is below you on the stack |
| `data_and_store` returns `StoreRef` | A new view type | No new types, and WASIX compiles after sequencing a few borrows |
| No `Instance::new_async` | Copy Wasmtime | Wasm has no async start function, and sync entry already works |
| Host futures are polled on the executor stack | Poll them on the guest's stack, as Wasmtime does | A deep future cannot overflow the guest's fixed Wasm stack |
| JavaScript gets a private guest guard | Rely on the borrow | JSPI resumes guests with no `poll` on the stack |
| A call derives its store from `as_store_mut` once | Derive it for each slice | `AsStoreMut` is public, and can return another store |
| V8 allows lexical nesting and fails out-of-order resumes | Reject shared stores on V8 | V8 can nest calls but cannot switch stacks |
| A cancelled instance stays usable | Poison it | Matches a trap, and Wasmtime's core API |
| `Cothread` and `unsafe-cothread` are removed | Keep them behind `unsafe` | Their stacks hold host frames, which breaks invariant 3 |

## Background

This API replaced one whose `call_async` returned a `'static` future over a
shared store handle. The compiler could not see who held the store, so
exclusivity was rebuilt at run time with an async read-write lock
(`LocalRwLock`), thread-local borrow counters and lent pointers. Safe code could
still alias host state, interleave two guests on one instance, deadlock by
awaiting a call under a lock guard, and keep an env handle past its call.

Wasmtime makes `call_async` safe the same way: its future borrows the store
until it completes or is dropped, `Caller` and `Accessor::with` keep borrows
inside their capability, and per-call state moves at one switch point. This
design differs in polling host futures on the executor's stack, in installing
per-call state around each resumption instead, in having one host function
shape, and in `&mut self` on `AsyncEnv`. The comparison was made
against [Wasmtime at
`fb792bf`](https://github.com/bytecodealliance/wasmtime/tree/fb792bf7a143801c22c0ba5b16fe2c6085a8598b/crates/wasmtime/src/runtime).

The design notes, soundness probes and type-model trials behind these choices
are in the history of
[wasmerio/wasmer-runtime#18](https://github.com/wasmerio/wasmer-runtime/pull/18).

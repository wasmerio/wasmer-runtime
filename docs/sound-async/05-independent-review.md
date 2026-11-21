# 5. Independent review

**An independent reviewer tried to break the first full draft. It found no break
on `sys`, but it found that the `&mut Store` borrow alone does not protect a
JSPI guest.** That changed the design. Nothing below ran against an
implementation, because none exists. "Confirmed" means a source citation plus a
compile test.

## Findings and outcomes

| # | Severity | Finding | Evidence | Outcome |
| --- | --- | --- | --- | --- |
| 1 | Unsound | On JSPI, a forgotten call ends the borrow while the guest can still run | Confirmed | JavaScript guest guard |
| 2 | Unsound | A woken JSPI guest cannot be stopped by a plain `drop` | Confirmed | Cancelled guests lose store access, and new guest calls wait for them |
| 3 | Blocks WASIX | `WeakSharedStore` cannot live in a `FunctionEnv`, which requires `T: Send` | Confirmed | `FunctionEnvMut::shared` |
| 4 | Unsound unless constrained | `AsStoreMut` is public, so a later `as_store_mut` can return another store | Plausible | Derive the store exactly once |
| 5 | Minor | `StoreView` could not call `Function::ty` | Confirmed | Superseded by `StoreRef` |
| 6 | Factual | The migration count missed crates | Confirmed | Corrected to about 93 uses in 31 files |
| 7 | Factual | Wasmtime's cancellation poisons component stores | Confirmed | Documented; core instances stay usable |

## 1. A forgotten JSPI call

A resumed guest and a first host-future poll run from microtasks with no `poll`
on the stack (`lib/api/src/backend/js/jspi.rs:954`, `658`). Their host functions
get the store from the activation (`jspi.rs:848`). Today that is safe because
the activation owns an `Rc` and the lock (`jspi.rs:109`). With only a borrow, it
would keep a raw pointer.

Two scenarios compile in safe code. Both are in `review_signatures.rs`:

- Poll to pending, `mem::forget(call)`, then `drop(store)`. The queued microtask
  resumes the guest, and its import reaches freed memory.
- Put an owning `async move { let mut s = s; f.call_async(&mut s, ..).await }`
  in a thread-local. A host function in a resumed slice takes and drops it,
  freeing the store under its own `FunctionEnvMut`.

Refusing host functions without a lend does not solve this. Every JSPI resume
lacks a poll, so every import after the first wait would fail.

## 2. A woken JSPI guest

Once its promise is resolved, the engine will run the guest. Today's
`Call::drop` records this: such a guest keeps the store until it leaves the
stack (`jspi.rs:1153`). After `drop` returns under the proposed API, the borrow
is over, so a new `call_async` could interleave with that guest on one instance.

## The JavaScript guest guard

The review proposed moving the store into the activation and leaving a
tombstone. That has a problem: when the guest exits, the store must be written
back to a location that may be gone. The final design keeps the store in place:

1. A started guest co-owns the store's allocation until it leaves the engine's
   stack.
2. A call that is neither finished nor dropped marks the store as held. If the
   borrow ended anyway, because the call was forgotten, every public store entry
   panics until the guest leaves.
3. Dropping a call marks its guest cancelled. If the engine can no longer be
   stopped from resuming it, its host functions fail without touching the store,
   so the store is usable at once. A new guest call waits for that guest to
   leave, or fails if it is synchronous.

Rule 3 matters for ordinary timeouts. `select!` drops a call and then
immediately uses the store, and that must not panic.

This guard has **not been prototyped**. It is the highest-risk part of Level 1.

## 3. Where the shared handle lives

The real `FunctionEnv::new` requires `T: Any + Send + 'static`
(`lib/api/src/entities/function/env/mod.rs:22`). `WeakSharedStore` is
`Rc`-based, so it cannot be stored in `WasiEnv`. The early model omitted the
bound and hid this.

`FunctionEnvMut::shared` resolves the handle from a private back-reference in
the store, which is already `!Send`. Sync imports such as `context_create` can
call it directly.

## 4. A public `AsStoreMut`

A safe implementation can replace and drop its store on the second
`as_store_mut` call. If the call future derived the store per slice or in
`Drop`, a parked guest would resume against a freed store. The future must call
`as_store_mut` exactly once and keep that `StoreMut<'a>`.

## Other issues found during the design

- A store polled from inside its own slice, such as by an executor whose `wake`
  polls inline, must panic or defer. It can never be satisfied by waiting.
- A host future destructor that calls `with` during a shared-store cancellation
  can find the store busy, so destructors should not use the store.
- A dropped call leaves the instance usable with partial guest state, as a trap
  does. This matches Wasmtime's core API.

## What held up

The reviewer checked these by reading and found no break: `sys` forget, `Pin`,
panics during polls and destructors, nested `AsyncEnv` calls, `release`,
`try_with` re-entrancy, `into_store`, and multiple stores. It also confirmed
`Store: !Send`, Wasmtime's signatures and `block_on` bound, the `vmctx_async`
comment, the `Accessor::with` panic, and the Wasmer line counts.

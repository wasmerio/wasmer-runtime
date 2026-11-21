# 9. Implementation playbook

**Start here to continue the work.** The target API is in the
[proposal](../SOUND-ASYNC.md). Every step below should leave the tree building,
with its acceptance check passing.

## Before writing code

1. Read `AGENTS.md`, `docs/CONTRIBUTING.md` and `docs/BUILD.md`.
2. Work on a branch stacked on `sound-async`. Rebase when PR 16 changes.
3. Run the [trials](./README.md#rebuild-the-trials) to confirm your baseline.
4. Keep the [invariants](#invariants-to-check-in-every-review) open during
   review.

`main` has no `Makefile` and no `lib/wasix`. WASIX, NAPI and the `Makefile` are
on `next`, so steps 4 and 5 must happen there or on a branch that has them.

## Steps

| Step | Work | Acceptance |
| --- | --- | --- |
| 1. Fix aliasing | Apply the [`StoreRef` change](./06-storeref-trial.md) and rename to `data_and_store` | Alias probe is a compile error; WASIX, C API and NAPI build and pass |
| 2. Level 1 on `sys` | `AsyncEnv`, `AsyncCallStore`, `AsyncCall`, borrowing `call_async` in `async_v2` | Native async suites and Miri pass; ready typed import works on a sync entry |
| 3. JavaScript guard | [Guest guard](./05-independent-review.md#the-javascript-guest-guard) and JSPI `call_async` | JSPI suites pass on Node 24 and 26; forgotten and dropped calls stay safe |
| 4. Level 2 | `SharedStore`, `WeakSharedStore`, `shared`, `release` | Overlap, cancellation and forgotten tasks pass under Miri |
| 5. Port WASIX | `context_switch`, `context_create`, `call_dynamic`, `run_main_context` | `context_switching` fixtures pass |
| 6. Port NAPI | Handles, `try_with`, deferral and `release` | NAPI suite passes on the JavaScript backend |
| 7. Remove the old API | Types, `LocalRwLock`, async `StoreContext` state | Full lint and test suites pass |

Prototype the JavaScript guard and the NAPI path before investing in the rest.
They are the parts most likely to force a design change.

## Invariants to check in every review

1. **One mutator.** At most one control flow can mutate a store.
2. **No borrow crosses a suspension.** No store, host-state or memory reference
   survives a point where other code can run.
3. **Suspended stacks hold no host frames.** Only engine and guest frames
   remain.
4. **Call state lives with its stack.** It is swapped at one point, including on
   unwind.
5. **Cancel and leak are safe.** A drop runs no guest or host code, and a forget
   leaves nothing the store or thread depends on.
6. **Thread affinity is in the types.** Nothing `!Send` hides inside a `Send`
   value.
7. **Interleaving guests is opt-in.** Only `SharedStore` can overlap calls.

Private `unsafe` obligations:

1. State is exchanged at exactly one switch point.
2. `AsyncEnv` has a store pointer only during a poll by its own call.
3. Discarding a suspended stack never touches the store.
4. A shared store hands out one slice at a time, including across `release`.
5. A call derives its store from `as_store_mut` exactly once.
6. On JavaScript, the guest guard holds whenever a guest can run with no `poll`
   on the stack.

## Tests to add

| Area | Test |
| --- | --- |
| Exclusive calls | Compile-fail tests from the [type model](./04-type-model.md) |
| Sync entry | Ready typed and dynamic async imports work; a pending import traps |
| Start function | Ready import works; waiting import traps; instance state stays valid |
| Cancellation | Drop before start, during a wait, and after a wake |
| Leaks | `mem::forget` a suspended call, then use and drop the store |
| Nesting | Nested `call_async(cx, ..)` suspends and resumes; recursive use fails to compile |
| Shared store | Overlapping tasks, out-of-order resume, `StoreBusy`, inline-polling waker |
| JavaScript | Forgotten call, drop of a woken guest, drop then immediate store use, new call during unwind |
| Store identity | An `AsStoreMut` that changes store on its second call |
| Guest state | The P3 shadow stack guest on `SharedStore` documents the embedder's responsibility |

## Pitfalls

- `call_dynamic` must become async when PR 16's suspension rule reaches `next`,
  or a context switch beneath it traps.
- Do not hold `env` from `data_and_store` across a guest call. The new type
  makes this a compile error, so fix the code instead of working around it.
- Do not poll a task from inside `with`. It panics by design.
- Do not use the store in a host future's destructor.
- A JSPI `drop` is not synchronous cancellation when the guest was already
  woken.
- Keep `Instance::new` synchronous. A start function that must wait should
  become an exported init function called with `call_async`.
- Do not let a `SharedStore` strong handle live in the store itself, because
  that creates a reference cycle.

## Environment notes

- `wasm-pack` was not installed. Use `wasm-bindgen-test-runner` directly, as
  shown in [the
  baseline](./02-current-wasmer-surface.md#pr-16-validation-baseline).
- `wasm-bindgen-cli` must match `Cargo.lock`. At these revisions that is
  0.2.130.
- A Miri build of `wasmer` needs a backend feature. `sys,headless` works.
- `git submodule update --init lib/napi` can stall even with `--reference`.
  Extract the pinned commit with `git -C <napi clone> archive <sha> | tar -x -C
  lib/napi`.
- Selecting a single crate on `next` still needs `lib/napi` present, because
  workspace metadata reads it.
- The repository's `typos` CI check scans these notes. Run it before pushing.

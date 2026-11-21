# 3. Soundness probes

**These probes test today's API with safe Rust, under `#![forbid(unsafe_code)]`,
against PR 16 at `aa65437e924`.** Their sources are in the [probe
appendix](./A2-probe-source.md), and `run-trials.sh` reruns them.

| Probe | Question | Result |
| --- | --- | --- |
| P1 | Can a `!Send` host closure cross threads inside a `Store`? | No: the hypothesis was wrong |
| P2 | Can safe code alias host state? | Yes, and Miri reports undefined behavior |
| P3 | Do overlapping calls corrupt a guest? | Yes |
| P4 | Can a task deadlock on its own store? | Yes |
| P5 | Can a host handle outlive its call? | Yes, and the store cannot be recovered |
| P6 | Which async types are `Send`? | None |
| P7 | How do start functions treat async imports? | Dynamic works; typed always traps |

## P1: `!Send` closure across threads (disproved)

The first suspicion was that `new_typed_async` accepts a non-`Send` closure, so
moving the `Store` to another thread would race on an `Rc`. The probe did not
compile. `Store` is `!Send`, because it contains raw VM pointers and `dyn Any`.

**Learning:** thread affinity is already part of Wasmer's contract. This is why
the proposal can keep `!Send` futures and avoid Wasmtime's hand-audited `unsafe
impl Send` for fibers.

## P2: aliased host state

`data_and_store_mut` returns `(&mut T, StoreMut<'_>)`. The `StoreMut` can call
`FunctionEnv::as_mut` and reach the same `T`.

```text
both &mut live, same address = true, value = 2
```

Miri, using a `sys` and `headless` build:

```text
error: Undefined Behavior: trying to retag from <2976> for SharedReadOnly permission at alloc1495[0x0], but that tag does not exist in the borrow stack for this location
```

**Consequence:** this is independent of async and should be fixed first. See the
[`StoreRef` trial](./06-storeref-trial.md).

## P3: interleaved guest

The guest keeps a shadow stack pointer in a global, as C and Rust guests do. Two
overlapping `call_async` futures run on one instance. Each moves the pointer,
suspends, then checks it.

```text
shadow stack intact: first call = false, second call = false
```

**Consequence:** interleaving guest stacks is not just a host concern. It must
be an explicit opt-in, which is why `SharedStore` exists.

## P4: self-deadlock

A task holds `store.read_lock().await`, then awaits `call_async` on the same
store.

```text
after 1000 polls the call is still waiting for a lock its own task holds
```

**Consequence:** lock guards that can cross `.await` make hangs expressible.
`with` takes a synchronous closure, so the equivalent code does not compile.

## P5: escaped handle

A host function stores its `AsyncFunctionEnvMut` in a thread-local.

```text
call finished; store can be taken back = false
```

**Consequence:** the host capability must be borrowed for the import. The
proposal passes `&mut AsyncEnv<T>` under a higher-ranked lifetime.

## P6: `Send` across the current API

The compile probe requires `Send` for each type. `Store`, `StoreAsync`,
`AsyncFunctionEnvMut` and the `call_async` future all fail with E0277. An async
host function holding an `Rc` across `.await` compiles.

WASIX does not need `Send` futures. It runs contexts on a single-thread
`LocalPool`, and `create_context` and `spawn_local` require only `'static`.

Wasmtime's are `Send` when the store data is. See [Wasmtime's
contract](./01-wasmtime-contract.md).

## P7: start functions and async imports

Wasm has no async start function, but a start function can call an async import.

```text
typed ready, direct Function::call = Err("RuntimeError: async imported function yielded when not called via `Function::call_async`")
typed ready import: Instance::new failed: RuntimeError: async imported function yielded when not called via `Function::call_async`
dynamic ready import: instantiated, start ran = I32(1)
pending import: Instance::new failed: RuntimeError: async imported function yielded when not called via `Function::call_async`
```

**Consequences:**

- `Instance::new_async` is unnecessary. A start function is a sync entry: a
  ready import works and a waiting import traps.
- The typed import failure is a real bug in PR 16, even for a future that is
  immediately ready. The acceptance criteria for Level 1 cover it.

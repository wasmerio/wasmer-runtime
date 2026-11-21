# 6. `StoreRef` trial

**`data_and_store_mut` can return the existing `StoreRef`, with no new type or
trait, if `FunctionEnv::as_ref` stops accepting a plain `StoreRef`.** A trial
port on `next` at `fa7ec59be53` type-checks WASIX and the C API imports. Its
tests were not run, and NAPI was not ported.

## The question

The first draft added `StoreView` and an `AsStoreObjects` trait. The question
was whether they were necessary or only convenient. They were a convenience:
they allowed mutating tables, globals and memories while holding `&mut T`.

## The change

```rust
impl<T> FunctionEnvMut<'_, T> {
    // The trial keeps the old name. The proposal renames it to `data_and_store`.
    pub fn data_and_store_mut(&mut self) -> (&mut T, StoreRef<'_>);
}

impl<T> FunctionEnv<T> {
    // Was `&'a impl AsStoreRef`. A plain `StoreRef` must not reach `T`.
    pub fn as_ref<'a>(&self, store: &'a impl AsStoreMut) -> &'a T;
}
```

The `as_ref` change is required. Without it, `handle.as_ref(&store_ref)` reads
the `T` that is live as `&mut T`.

## Results

| Check | Result |
| --- | --- |
| First type-check of WASIX | 8 errors across its 17 uses |
| WASIX after the port | Type-checks |
| C API imports after the port | Type-checks |
| Change size | About 70 lines in 8 files, plus the API change |
| Re-reading `T` through the `StoreRef` | E0277 on the real crate |
| Re-writing `T` through the `StoreRef` | E0277 on the real crate |
| Calling a guest through the `StoreRef` | E0277 on the real crate |

## Sites that needed a change

| Site | Why it needed a mutable store | Fix |
| --- | --- | --- |
| `lib/wasix/src/syscalls/wasix/call_dynamic.rs:123` | Table lookup | Clone the table handle first |
| `lib/wasix/src/syscalls/wasix/call_dynamic.rs:152` | **Guest call while `&mut WasiEnv` is live** | Call through `ctx`, then take `data_and_store` again |
| `lib/wasix/src/syscalls/wasix/context_create.rs:87` | Table lookup | Pass `ctx` to the lookup |
| `lib/wasix/src/syscalls/wasix/reflect_signature.rs:62` | Table lookup | Look up before splitting |
| `lib/wasix/src/syscalls/mod.rs:1227` | Reads the stack pointer global | `get_memory_stack` takes `ctx` |
| `lib/wasix/src/syscalls/mod.rs:1502` | Writes the stack pointer global | `set_memory_stack` takes `ctx` |
| `lib/wasix/src/journal/effector/memory_and_snapshot.rs:213` | Grows memory | Clone the memory handle first |
| `lib/wasix/src/state/func_env.rs:111` | Helper bound | Take `AsStoreMut` |
| `lib/c-api-imports/src/lib.rs:672,689,2146` | Discarded `T`, then used the store | Use `env` directly |

The fix pattern already exists in WASIX: `get_memory_stack_pointer` clones the
global handle from `ctx.data()` and then uses `ctx`. Handles are cheap, so
sequencing the two borrows costs almost nothing.

## NAPI

NAPI was read, not compiled. It has 23 uses in 4 files. Its six uses that need a
mutable store all discard `T` with `let (_, mut store_ref)`, and then call a
function or refill a heap, so they can use `env` directly.

## Migration size

`next` has 70 code uses in 27 files: 40 in `lib/api`, 17 in `lib/wasix`, 9 in
`lib/c-api-imports`, and 4 in examples and tests. NAPI adds 23 in 4 files, for
about 93 uses in 31 files. Many in `lib/api` are the backend forwarding methods.

## Rerun

`run-storeref-trial.sh` applies `storeref/port.py` in a throwaway worktree,
type-checks both crates, and checks that the three aliasing forms fail. Its
source is in the [port appendix](./A3-storeref-port-source.md).

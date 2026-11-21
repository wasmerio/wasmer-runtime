# 4. Type model

**The type model is a small crate with the proposed public signatures and stub
bodies, so `rustc` can check what the contract accepts and rejects.** It proves
the shape of the API. It does not prove any implementation. Its source is in the
[model appendix](./A1-type-model-source.md).

The harness compiles one snippet per claim. An accepted snippet must compile. A
rejected snippet must fail with the listed error code, so it cannot pass for an
unrelated reason.

## How the design changed

| Iteration | Accept / reject | Change | What forced it |
| --- | --- | --- | --- |
| 1 | 11 / 12 | Borrowing `call_async`, `AsyncEnv`, `SharedStore`, `StoreView` | First draft |
| 2 | 14 / 15 | `AsyncEnv` methods take `&mut self`; WASIX shapes added | `join(cx.call(..), cx.call(..))` would have overlapped nested guests |
| 3 | 14 / 16 | `StoreView` stopped implementing `AsStoreMut` | `view.as_store_mut()` returned a full `StoreMut`, reopening the alias |
| 4 | 16 / 17 | `FunctionEnv::new` requires `T: Send`; `FunctionEnvMut::shared`; `Function::ty` on views | [Independent review](./05-independent-review.md) |
| 5 | 15 / 18 | `StoreRef` replaces `StoreView`; `Instance::new_async` removed | Design review |
| 6 | 20 / 23 | One `call_async` through a sealed `AsyncCallStore` | Design review |

## The single `call_async`

The three call forms differ only in what the future borrows:

```rust
pub trait AsyncCallStore: sealed::Sealed {
    type Call<'a, R: 'static>: Future<Output = R>
    where
        Self: 'a;
}

impl<S: AsStoreMut> AsyncCallStore for S { /* Call = AsyncCall<'a, R> */ }
impl<T> AsyncCallStore for AsyncEnv<T> { /* Call = AsyncCall<'a, R> */ }
impl AsyncCallStore for SharedStore { /* Call = AsyncCall<'static, R> */ }
```

Three details matter:

- The parameter is `store: &'a mut S`, not a by-value generic. A by-value
  generic would move `cx` into the call and force `&mut *cx` at every call site.
- The shared case returns `AsyncCall<'static, R>`, so `&mut shared` is released
  immediately and the future can be spawned.
- Coherence depends on `AsyncEnv` and `SharedStore` not implementing `Deref`,
  because the real crate has a blanket `AsStoreMut` impl for `DerefMut` types.
  The model includes that blanket impl to keep this check honest.

## Results

The current model accepts all 20 intended uses and rejects all 23 misuses.

| Accepted use |
| --- |
| `typed_async_host_closure` |
| `named_async_host_function` |
| `dynamic_async_host` |
| `sequential_calls_then_store_use` |
| `owned_store_future_is_static` |
| `nested_guest_call_from_async_host` |
| `shared_store_overlapping_tasks` |
| `one_call_async_on_all_three` |
| `typed_call_async_on_all_three` |
| `async_env_usable_after_nested_call` |
| `call_on_function_env_mut` |
| `shared_call_borrows_nothing` |
| `shared_store_sync_access` |
| `wasix_context_switch_shape` |
| `wasix_call_dynamic_shape` |
| `sync_host_gets_shared_handle` |
| `shared_handle_from_async_host` |
| `store_ref_reads_function_type` |
| `store_ref_reads_memory_beside_data` |
| `release_lends_store_to_shared_handle` |

| Rejected misuse | Required error |
| --- | --- |
| `overlapping_call_async` | `E0499` |
| `store_use_during_call` | `E0499` |
| `store_drop_during_call` | `E0505` |
| `borrowed_call_is_not_static` | `E0597` |
| `call_future_cannot_cross_threads` | `E0277` |
| `async_env_cannot_escape_import` | `E0521` |
| `store_borrow_cannot_leave_with` | lifetime error |
| `await_inside_with` | `E0728` |
| `shared_store_borrow_cannot_leave` | lifetime error |
| `store_ref_cannot_produce_store_mut` | `E0599` |
| `store_ref_cannot_read_env_again` | `E0277` |
| `store_ref_cannot_write_env_again` | `E0277` |
| `store_ref_cannot_call_guest` | `E0277` |
| `overlapping_nested_guest_calls` | `E0499` |
| `recursive_store_access` | `E0501` |
| `async_env_use_during_nested_call` | `E0499` |
| `overlapping_typed_calls` | `E0499` |
| `typed_nested_call_holds_async_env` | `E0499` |
| `sync_call_rejects_async_env` | `E0277` |
| `with_cannot_return_a_call` | `E0515` |
| `cannot_implement_call_store` | `E0277` |
| `weak_handle_cannot_live_in_env` | `E0277` |
| `released_env_is_unusable` | `E0501` |

## What the model cannot show

- Invariants 3, 4 and 5 are private implementation properties.
- The JavaScript guard is a runtime mechanism. `review_signatures.rs` shows the
  signatures alone allow the scenarios it must defend against.
- Signature ergonomics are checked for closures and named functions, but not at
  the scale of a real port.
- Wasmtime's `AccessorTask` comment records that `AsyncFn` cannot put a `Send`
  bound on its future. This was not retested on Rust 1.99. If `Send` host
  futures are ever required, recheck this before relying on `AsyncFn`.

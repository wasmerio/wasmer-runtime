# 8. Decision log

**These decisions are settled unless new evidence contradicts them.** Each entry
records the alternative, so it does not need to be rediscovered. "Owner" means
the project owner decided or confirmed it during the design review.

| # | Decision | Rejected alternative | Why | Source |
| --- | --- | --- | --- | --- |
| 1 | `&mut Store` is the only async lock | Shared handle with `LocalRwLock` | The compiler proves exclusivity, and a store with no suspended holder needs no waiters | Analysis, owner |
| 2 | Async host functions get `&mut AsyncEnv<T>` | Hold `FunctionEnvMut` across `.await`, as Wasmtime's `Caller` does | A held borrow blocks other stacks, and one host shape serves both levels | Owner |
| 3 | `AsyncEnv` methods take `&mut self` | `&self`, as Wasmtime's `Accessor` does | Recursive `with` and overlapping nested calls become compile errors | Type model |
| 4 | Call futures stay `!Send` | `Send` futures and host futures | `Store` is `!Send`, and WASIX does not need it | Owner |
| 5 | Remove `Cothread` and `unsafe-cothread` | Keep it behind `unsafe` | Its stacks hold host frames, breaking invariant 3. No in-repo user exists | Owner |
| 6 | Stack the work on PR 16 without merging it | Merge PR 16 first | Reuses its activation swap and tests while it is still under review | Owner |
| 7 | V8 allows lexical nesting and fails out-of-order resumes | Reject Level 2 entirely | V8 can nest calls but cannot switch stacks | Owner |
| 8 | `data_and_store` returns `StoreRef` | New `StoreView` and `AsStoreObjects` | No new types. WASIX compiles after sequencing a few borrows | Owner, trial |
| 9 | No `Instance::new_async` | Copy Wasmtime | Wasm has no async start function, and Wasmer does not refuse sync entry | Owner, probe P7 |
| 10 | One `call_async` on `Function` and `TypedFunction` | `AsyncEnv::call` and `SharedStore::call` | One way to call, with the differences carried by a sealed trait | Owner, type model |
| 11 | `store: &'a mut S` in `call_async` | By-value generic store | `cx` reborrows automatically, and all three call forms look alike | Type model |
| 12 | `SharedStore` owns the store behind `Rc` | Scoped `run_concurrent` with a borrowed store | A forgotten scope would end the borrow while handles still reach the store | Analysis |
| 13 | `try_with` fails instead of waiting | Async lock acquisition | On one thread, a busy store means the holder is below you on the stack | Analysis |
| 14 | `FunctionEnvMut::shared` uses a private back-reference | Keep a weak handle in `T` | `FunctionEnv` requires `T: Send`, and the handle is `Rc`-based | Review |
| 15 | JavaScript gets a private guest guard | Rely on the borrow | JSPI resumes guests with no `poll` on the stack | Review |
| 16 | A call derives its store from `as_store_mut` once | Derive it per slice | `AsStoreMut` is public and can return a different store | Review |
| 17 | Host futures are polled on the executor stack | Poll on the fiber, as Wasmtime does | A deep future cannot overflow the fixed coroutine stack | Analysis |
| 18 | A cancelled instance stays usable | Poison it | Matches a trap and Wasmtime's core API | Analysis |
| 19 | Fix aliasing first | Fold it into the async work | It is a sync API bug and is independently valuable | Analysis |
| 20 | Migrate through a temporary `async_v2` module | Replace the API in one change | WASIX and NAPI need a working path at each step | Analysis |

## Hypotheses that were wrong

| Hypothesis | What happened |
| --- | --- |
| A `!Send` async closure can cross threads inside a `Store` | `Store` is `!Send`, so the probe did not compile |
| `&mut Store` is enough on every backend | JSPI can resume a guest with no `poll` on the stack |
| A weak store handle can live in the environment | `FunctionEnv` requires `T: Send` |
| `StoreView` is required to fix aliasing | `StoreRef` works once `FunctionEnv::as_ref` is narrowed |
| A start function needs async instantiation | It is an ordinary sync entry |
| Today's `Instance::new` handles any ready async import | A ready typed import traps, because its wrapper awaits a lock that never resolves |

# Sound async: working notes

These notes keep what was learned while designing the [async API
proposal](../SOUND-ASYNC.md), so the implementation can continue without the
original design session. The proposal says what to build. These notes record
why, what was tried, what failed, and how to rerun every trial.

## Current state

- The proposal is in
  [wasmerio/wasmer-runtime#18](https://github.com/wasmerio/wasmer-runtime/pull/18),
  stacked on
  [wasmerio/wasmer-runtime#16](https://github.com/wasmerio/wasmer-runtime/pull/16).
- **The proposed API is implemented in `lib/api`** on `sound-async`. See
  [implementation status](#implementation-status).
- All design decisions are settled. See the [decision
  log](./08-decision-log.md).
- The JavaScript guest guard and the NAPI port are the least proven parts.
  Prototype them early.
- A trial `StoreRef` port type-checks WASIX and the C API imports on `next`. Its
  tests were not run.

## Implementation status

The old async surface is gone from `lib/api`, including `LocalRwLock` and
`Cothread`. Nothing in this branch needed the temporary `async_v2` module, so
the API was replaced directly.

| Area | State |
| --- | --- |
| `sys` Level 1 and Level 2 | Done |
| JavaScript and the guest guard | Done, in `lib/api/src/backend/js/guard.rs` |
| V8 | Done. `poll` blocks during an import, so another thread must wake it |
| WASIX and NAPI ports | **Not done.** They live on `next` |

Differences from the proposal's signatures:

- `new_async` and `new_typed_async` require `T: Send + 'static`, which every
  `FunctionEnv<T>` already needs to exist.
- A guest whose call was dropped now fails at its next host function, even if
  it holds no wait. That follows guard rule 3, and one PR 16 test was changed
  to expect it.

Tests that depended on the removed lock were deleted or rewritten. Coroutine
switches cannot run under Miri, so Miri covers the store context and slice
provenance, not the switch itself. The probes in `A2` target the PR 16 API and
no longer build against this branch.

## Reading order

| Document | Read it when |
| --- | --- |
| [Implementation playbook](./09-implementation-playbook.md) | You are about to continue the work |
| [Decision log](./08-decision-log.md) | You want to change a design choice |
| [Wasmtime's contract](./01-wasmtime-contract.md) | You need the reference design and where it differs |
| [Current Wasmer surface](./02-current-wasmer-surface.md) | You need the code being replaced and PR 16's test baseline |
| [Soundness probes](./03-soundness-probes.md) | You need proof of a hole in today's API |
| [Type model](./04-type-model.md) | You change a public signature |
| [Independent review](./05-independent-review.md) | You work on cancellation, leaks or JSPI |
| [`StoreRef` trial](./06-storeref-trial.md) | You fix the `data_and_store_mut` aliasing |
| [Consumers and backends](./07-consumers-and-backends.md) | You port WASIX or NAPI, or touch JavaScript or V8 |
| [Model source](./A1-type-model-source.md), [probe source](./A2-probe-source.md), [port source](./A3-storeref-port-source.md) | You rebuild or extend a trial |

## Pinned revisions

Line numbers in these notes refer to these revisions.

| Name | Revision |
| --- | --- |
| PR 16 head | `aa65437e9242d502ff04c8392d82503db187071c` |
| Proposal commit | `174ba7cb36a7c34329444554b4b82521dc694e1b` |
| `next` | `fa7ec59be53260497ceb111d586967c6dbd37aef` |
| NAPI pinned by `next` | `82b328f50ddaac78de388b67b4707f21864b9245` |
| NAPI checkout that was read | `795dfe434db56ebfbfe9605eb9f588b3f390f192` |
| Upstream [wasmerio/wasmer#7183](https://github.com/wasmerio/wasmer/pull/7183) head | `af20121df66c6d5a39834bffb6f0e2616d60ff9d` |
| Wasmtime | `fb792bf7a143801c22c0ba5b16fe2c6085a8598b` |

The trials ran on arm64 macOS with Rust 1.99.1, nightly 1.100.0 (`0dfb098f3`),
Node 26.0.0 and 24.18.0, and `wasm-bindgen` 0.2.130.

## Rebuild the trials

The appendices contain every trial source. Run this from the repository root on
`sound-async`:

```bash
python3 - docs/sound-async target/sound-async-trials <<'PY'
import pathlib, re, sys
docs, out = map(pathlib.Path, sys.argv[1:])
pattern = re.compile(r"<!-- trial-file: (\S+) -->\n````\w*\n(.*?)\n````", re.S)
for doc in sorted(docs.glob("*.md")):
    for name, body in pattern.findall(doc.read_text()):
        path = out / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body + "\n")
PY
```

```bash
bash target/sound-async-trials/run-trials.sh
```

The second command rebuilds the type model, the probes and the Miri check. It
expects the current API from PR 16, so the probes must be retired as the old API
is removed.

The `StoreRef` trial needs the `next` commit and its NAPI submodule. `NAPI_REPO`
is optional, and avoids a slow submodule download:

```bash
NAPI_REPO=/path/to/local/napi bash target/sound-async-trials/run-storeref-trial.sh
```

On 2026-11-21, files extracted from these notes reproduced every trial result
with no failures.

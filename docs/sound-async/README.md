# Sound async: working notes

These notes keep what was learned while designing the [async API
proposal](../SOUND-ASYNC.md), so the implementation can continue without the
original design session. The proposal says what to build. These notes record
why, what was tried, what failed, and how to rerun every trial.

## Current state

The `sound-async-codex` branch implements the proposal on PR 18's head
`4fab3dcb52dfc9c9e52e6744e0b2051fda30195c`.

- The public API now uses borrowing `Function::call_async` and
  `TypedFunction::call_async`, `AsyncEnv::with`, and explicit `SharedStore`
  handles. The implementation lives in `lib/api/src/async_runtime.rs`;
  `async_v2` was only a migration stage.
- `FunctionEnvMut::data_and_store` returns a `StoreRef`, and accessing host
  state through `FunctionEnv` requires mutable store access. Compile-fail
  doctests cover the former aliasing hole and async borrow restrictions.
- Native calls retain PR 16's activation switching and executor-stack host
  polling. JavaScript calls own their live engine allocation and guard public
  store access after a forgotten call. V8 polls host futures synchronously and
  supports lexical nesting, without interleaving independent guest stacks.
- The old async API, `LocalRwLock`, and `unsafe-cothread` feature are removed,
  together with the obsolete pause counts, entry serials, and async lent-entry
  shortcut in `StoreContext`. Nested synchronous calls install their own
  context; only an unborrowed root async entry can suspend.
- WASIX, asyncify, and NAPI are ported on a companion
  `sound-async-codex-consumers` branch based on the pinned `next` revision below.
  NAPI's nested repository has a branch with the same name. The consumer port
  and runtime baseline synchronization are exported separately for review.
  WASIX context-switching and dynamic-call fixtures, asyncify unwind handlers,
  and NAPI JavaScript callbacks pass against the new API.

The numbered documents and source appendices below are the original design
record. Their descriptions of the old API and trial results refer to the pinned
revisions, not to the implementation branch.

## Implementation validation

Run the native API (including compile-fail doctests), compiler fixtures, and lint
checks from the repository root:

```bash
cargo test --locked -p wasmer --features experimental-async,experimental-host-interrupt
cargo test --locked -p wasmer-workspace --features cranelift --test compilers
cargo clippy --locked -p wasmer --features experimental-async,experimental-host-interrupt --all-targets -- -D clippy::all
cargo fmt --all -- --check
```

Miri covers capability lifetimes, shared-store release/re-entry, store-pointer
provenance, and overlapping dynamic host-import contexts without executing JIT
code. `-Zmiri-ignore-leaks` permits tests that intentionally forget a call:

```bash
MIRIFLAGS='-Zmiri-disable-isolation -Zmiri-ignore-leaks' cargo +nightly miri test --locked -p wasmer --no-default-features --features sys,headless,experimental-async --lib async_runtime:: -- --skip backend::
MIRIFLAGS='-Zmiri-disable-isolation -Zmiri-ignore-leaks' cargo +nightly miri test --locked -p wasmer --no-default-features --features sys,headless,experimental-async --test shared_store
```

JavaScript tests require Node 24 or 26 and `wasm-bindgen-test-runner` matching
`Cargo.lock`. Run with `WASM_BINDGEN_TEST_ONLY_NODE=1` and the runner configured
for `wasm32-unknown-unknown`, selecting `js-default,experimental-async` with
`--no-default-features`. `sound_async_js` tests the proposal's cancellation,
leak, nesting, ready-import, shared-release, and JavaScript exception recovery. The existing
`jspi_async_js` and `jspi_calls_js` suites use the new public API too.

V8's full API suite uses `--no-default-features --features
v8-default,experimental-async`. `sound_async_v8` exercises ready and waiting
imports, nested calls, start functions, and panic handling.

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

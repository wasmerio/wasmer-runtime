# Working in this repo as an agent

This repository contains the embeddable Wasmer runtime, its Rust crates, and
the C API. The CLI, WASI/WASIX, and package tooling belong in
[`wasmerio/wasmer`](https://github.com/wasmerio/wasmer).

Before your first edit, read these guides:

| Read | For |
| --- | --- |
| [CONTRIBUTING.md](./docs/CONTRIBUTING.md) | PR expectations, lint, and code style |
| [BUILD.md](./docs/BUILD.md) | Toolchain setup and backend features |
| [ARCHITECTURE.md](./docs/ARCHITECTURE.md) | Workspace layout and submodules |
| [TEST.md](./docs/TEST.md) | Compiler tests, WAST fixtures, and CI |
| [SECURITY.md](./docs/SECURITY.md) | Private vulnerability reporting |

For platform-specific work, see [RISCV.md](./docs/RISCV.md) for RISC-V support.

Trust the workspace manifests, test harness, and CI configuration over external
docs. Use Cargo and select the package and backend features relevant to the change.

## Before you write code

Find a sibling test, fixture, or backend implementation and match its location,
naming, and structure. If you cannot find one, say so before inventing a layout.

Before implementing a new mechanism, state the design in one or two sentences
and get agreement. Prefer the smallest change that fits the existing code.

## Attribution

An agent MUST disclose that it is the one that committed its changes, and
MUST NOT feign being a user. The human user is fully responsible for all
contributions.

## Debugging the runtime

- Run a small Rust embedding [example](./examples/README.md), or add a regression
  to the relevant crate or [`tests/compilers`](./tests/compilers).
- For a failing WAST test, find the fixture under `tests/wast/` and rerun the
  backend-specific test with `--nocapture`; see [TEST.md](./docs/TEST.md#running-a-single-wast-test).
  The runner prints `Running wast` with the fixture path.
- The compiler tests use `test-log`. Set `RUST_LOG` (for example,
  `RUST_LOG=warn,wasmer_compiler_cranelift=debug`) and use `--nocapture` to see
  the output immediately.
- Inspect a module with `wasm-tools print file.wasm`; limit large output with
  `head` or `grep`. Convert text to Wasm with `wasm-tools parse file.wat -o file.wasm`.
  Use `wasm-opt` or `creduce` to reduce large fixtures.

## Personal execution preferences

The gitignored `AGENTS.override.md` is the user's personal addendum. When
it exists, it overrides this file's defaults.

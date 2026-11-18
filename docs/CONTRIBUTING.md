# Contributing to Wasmer Runtime

## Issues and feature requests

Use the [runtime issue tracker](https://github.com/wasmerio/wasmer-runtime/issues)
for the Rust API, compilers, VM, and other runtime crates. Include the backend,
Cargo features, host platform, and a minimal example if possible. For the CLI,
WASI/WASIX, C API, and packaging, use the
[original Wasmer issue tracker](https://github.com/wasmerio/wasmer/issues).

> [!WARNING]
> Do not report or fix a security vulnerability in a public issue or pull
> request. Use the private process in [SECURITY.md](./SECURITY.md).

## Code style and local checks

Use short, single-responsibility functions. Prefer comments that explain why
rather than restating the code.

```sh
cargo fmt --all -- --check
cargo clippy --locked -p wasmer -- -D clippy::all
```

Adjust the package and features to cover what you changed, and run the relevant
[runtime tests](./TEST.md). See the lint job in
[`test.yaml`](../.github/workflows/test.yaml) for the full CI commands.

## Pull requests

- Discuss large changes in a GitHub issue first.
- Use conventional-commit-style titles, for example `fix(Singlepass): ...`,
  `feat(runtime): ...`, `feat!: ...`, `chore: ...`, or `deps: ...`.
- Explain why the change is needed and say what you tested.
- Do not bump crate versions or submodule pointers in unrelated PRs.

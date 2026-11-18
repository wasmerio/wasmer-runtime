# Testing the runtime

Run these commands from the repository root after following the
[build setup](./BUILD.md#getting-started). The compiler integration suite
runs the [WebAssembly spec tests](https://github.com/WebAssembly/testsuite)
and Wasmer-specific tests against the backends enabled with Cargo features.

## Local checks

```sh
cargo test --locked -p wasmer
cargo test --locked -p wasmer-workspace --features cranelift --test compilers
cargo test --locked -p wasmer-workspace --features cranelift --examples
```

These commands exercise a native Cranelift configuration. To test another
native compiler, select `singlepass` or `llvm` instead where the target and
[backend prerequisites](./BUILD.md#selecting-a-backend) allow it. The examples
command only selects examples whose required features are enabled.

See [`test.yaml`](../.github/workflows/test.yaml) for the full CI matrix,
including artifact features, `cargo nextest`, doctests, and V8 on Windows.
macOS and musl jobs run on pull requests only when the `macos` or `musl` label
is present, respectively.

## Running a single WAST test

Spec fixtures live in `tests/wast/spec`; runtime regressions go in
[`tests/wast/wasmer`](../tests/wast/wasmer). `build.rs` generates backend-specific
tests from these fixtures. To run the basic Cranelift variant of `fac.wast`:

```sh
cargo test --locked -p wasmer-workspace --features cranelift --test compilers \
  wast::wasmer::fac::cranelift::cranelift -- --exact --nocapture
```

Use `-- --list` instead of the filter and test flags to find generated names.
For diagnostics, see [Debugging the runtime](../AGENTS.md#debugging-the-runtime).

## Skipping tests

If a failure is due to a known backend or platform limitation, use the
narrowest match in [`tests/ignores.txt`](../tests/ignores.txt) and include a
reason. A rule can combine selectors with `+`, for example
`cranelift+riscv64 spec::simd`. A bare test pattern applies to all backends
and platforms. Use `-- --ignored` to run ignored tests explicitly.

For other test workflows, see [benchmarks](../benches/README.md) and
[fuzzing](../fuzz/README.md).

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

## C API

The C API tests link against the release build of `libwasmer`, so build it first
and rebuild it after each change. They crash without `WASMER_CAPI_CONFIG`, so
set it to the backend you built.

```sh
cargo build --locked --release -p wasmer-c-api
WASMER_CAPI_CONFIG=cranelift cargo test --locked --release -p wasmer-c-api
cargo test --locked --release -p wasmer-c-api-imports
```

The C integration tests and examples use a packaged copy of the library. On
Linux or macOS:

```sh
mkdir -p package/include package/lib
cp lib/c-api/wasmer.h lib/c-api/wasmer_wasm.h lib/c-api/tests/wasm-c-api/include/wasm.h package/include/
find target/release -maxdepth 1 \( -name libwasmer.so -o -name libwasmer.dylib \) -exec cp {} package/lib/ \;
export WASMER_CAPI_CONFIG=cranelift WASMER_DIR="$PWD/package"
ROOT_DIR="$PWD/lib/c-api/tests" cargo test --locked --release -p wasmer-c-api-test-runner
ROOT_DIR="$PWD/lib/c-api/examples" cargo test --locked --release -p wasmer-capi-examples-runner
```

CI runs these steps through the
[`test_capi` action](../.github/actions/test_capi/action.yml).

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

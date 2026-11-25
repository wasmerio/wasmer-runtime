# Building Wasmer Runtime from source

This workspace builds Rust libraries and the C API with Cargo. It does not build
the Wasmer CLI or WASI/WASIX; see
[the original Wasmer repository](https://github.com/wasmerio/wasmer) for those
components.

## Getting started

Install [Rustup](https://rustup.rs/) and a native linker for your platform
(such as the MSVC build tools on Windows). Rustup selects the version pinned in
[`rust-toolchain.toml`](../rust-toolchain.toml) from this checkout.

```sh
git clone --recurse-submodules https://github.com/wasmerio/wasmer-runtime.git
cd wasmer-runtime
```

In an existing clone, run `git submodule update --init --recursive` to fetch
the test fixtures.

Build the `wasmer` API with its default native backend (Cranelift), or use
`cargo check` for faster iteration:

```sh
cargo build --locked -p wasmer
cargo check --locked -p wasmer
```

## Selecting a backend

Use Cargo features to select the backend explicitly. Backend availability
depends on the target. The root `wasmer-workspace` package has its own feature
list for examples and compiler tests; the library options are in
[`lib/api/Cargo.toml`](../lib/api/Cargo.toml).

### Singlepass compiler

```sh
cargo build --locked -p wasmer --no-default-features --features singlepass,wat
```

See the [Singlepass README](../lib/compiler-singlepass/README.md) for target
support.

### LLVM compiler

The LLVM backend requires LLVM 23.1. See the
[LLVM backend requirements](../lib/compiler-llvm/README.md#requirements). If
Cargo cannot find a compatible installation, set `LLVM_SYS_231_PREFIX` to its
prefix and ensure `llvm-config` is available there.

```sh
cargo build --locked -p wasmer --no-default-features --features llvm,wat
```

### V8

On supported native targets, the build script downloads the V8 archive and
uses `bindgen` to generate bindings, so it needs libclang as well as a native
toolchain. CI uses this backend for Windows tests. The archives cover x86_64
Linux with glibc, aarch64 macOS, x86_64 Windows, and aarch64 Android.

```sh
cargo build --locked -p wasmer --no-default-features --features v8-default
```

To build without network access, set `WASMER_V8_ARTIFACT_DIR` to the absolute
path of an unpacked V8 archive: a directory that has `lib/libv8.a`
(`lib/v8.lib` on Windows). The build script then downloads nothing and writes
nothing to that directory.

### JavaScript host backend

Build the Rust API for a JavaScript host such as a browser or Node.js. This is
the `js` backend of the Rust API, not the separately maintained sandbox SDK.

```sh
rustup target add wasm32-unknown-unknown
cargo build --locked -p wasmer --target wasm32-unknown-unknown --no-default-features --features js-default
```

See [the runtime examples](../examples/README.md) and [testing guide](./TEST.md)
for commands that exercise the runtime.

## C API

Build `libwasmer` in `target/release` and generate `lib/c-api/wasmer.h`:

```sh
cargo build --locked --release -p wasmer-c-api
```

The default features enable Cranelift and the middlewares. To select Singlepass
or V8 instead:

```sh
cargo build --locked --release -p wasmer-c-api --no-default-features --features wat,compiler,middlewares,singlepass
cargo build --locked --release -p wasmer-c-api --no-default-features --features wat,v8-default
```

See [TEST.md](./TEST.md#c-api) to run the C API tests.

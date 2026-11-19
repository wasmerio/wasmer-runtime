<div align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="./assets/logo-white.svg">
    <img width="300" src="./assets/logo.svg" alt="Wasmer">
  </picture>
</div>

# Wasmer Runtime

Wasmer Runtime is the embeddable [WebAssembly](https://webassembly.org/) engine
for Rust. It compiles, instantiates, and executes Wasm modules. This repository
contains the `wasmer` Rust API, the virtual machine, the Cranelift, Singlepass,
and LLVM compilers, the V8 and JavaScript backends, and runtime crates such as
`wasmer-cache` and `wasmer-middlewares`.

The Wasmer CLI, WASI/WASIX support, C API, and packaging tools live in
[`wasmerio/wasmer`](https://github.com/wasmerio/wasmer). For higher-level sandbox
SDKs, see [`wasmerio/wasmer-sdk`](https://github.com/wasmerio/wasmer-sdk).

## Try the runtime

From a clone of this repository with [submodules initialized](./docs/BUILD.md#getting-started),
run the Rust embedding example:

```sh
cargo run --locked --example hello-world --features cranelift
```

To use the runtime in your own Rust project, add the `wasmer` crate with
`cargo add wasmer`. Its default native backend uses Cranelift. See the
[crate README](./lib/api/README.md) for a minimal embedding example and the
[Rust API reference](https://docs.rs/wasmer/) for more details.

## Develop and contribute

- [Documentation index](./docs/README.md)
- [Build the runtime from source](./docs/BUILD.md)
- [Repository architecture](./docs/ARCHITECTURE.md)
- [Runtime examples](./examples/README.md)
- [Run tests](./docs/TEST.md)
- [Contributing guide](./docs/CONTRIBUTING.md)
- [Report a runtime issue](https://github.com/wasmerio/wasmer-runtime/issues) or
  [disclose a security issue privately](./docs/SECURITY.md)

See the [root license](./LICENSE) and the separate licenses for the
[LLVM](./lib/compiler-llvm/LICENSE) and [Singlepass](./lib/compiler-singlepass/LICENSE)
backends.

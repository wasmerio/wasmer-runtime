# Wasmer Runtime documentation

Start with the [repository README](../README.md). To embed WebAssembly in Rust,
see the [`wasmer` crate README](../lib/api/README.md), the
[Rust API reference](https://docs.rs/wasmer/), and the
[runnable examples](../examples/README.md). To embed it from C, see the
[C API README](../lib/c-api/README.md).

## Develop the runtime

- [Architecture](./ARCHITECTURE.md)
- [Async calls](./ASYNC.md)
- [Building from source](./BUILD.md)
- [Testing](./TEST.md)
- [Contributing](./CONTRIBUTING.md)
- [Agent guidance and debugging](../AGENTS.md)
- [RISC-V support](./RISCV.md)
- [Security policy](./SECURITY.md)
- [Third-party attributions](./ATTRIBUTIONS.md)

## Other Wasmer components

The CLI, WASI/WASIX, and packaging documentation are in the
[original Wasmer repository](https://github.com/wasmerio/wasmer) and the
[public Wasmer docs](https://docs.wasmer.io/). See the original repository for
[distro packaging](https://github.com/wasmerio/wasmer/blob/main/docs/PACKAGING.md)
and [product release procedures](https://github.com/wasmerio/wasmer/blob/main/scripts/RELEASE.md).
This runtime repository does not ship the CLI or its installers. For the tooling
that publishes Rust crates from this workspace, see [`scripts/publish.py`](../scripts/publish.py).

The original repository also hosts the [historical changelog](https://github.com/wasmerio/wasmer/blob/main/CHANGELOG.md)
and the [1.0](https://github.com/wasmerio/wasmer/blob/main/docs/migration_to_1.0.0.md)
and [3.0](https://github.com/wasmerio/wasmer/blob/main/docs/migration_to_3.0.0.md)
migration guides.

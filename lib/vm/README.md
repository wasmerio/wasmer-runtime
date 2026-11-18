# `wasmer-vm`

This crate implements Wasmer's low-level native WebAssembly VM and ABI. It
provides instances, memories, tables, trap handling, signature registration,
`VMContext`, and runtime libcalls.

Most users should embed the runtime through the higher-level
[`wasmer` crate](../api/README.md) rather than depending on `wasmer-vm` directly.

## Acknowledgments

This project borrowed some of its VM structure and trapping code from
[`wasmtime-runtime`](https://crates.io/crates/wasmtime-runtime). See
[attributions](../../docs/ATTRIBUTIONS.md) for licenses and other notices.

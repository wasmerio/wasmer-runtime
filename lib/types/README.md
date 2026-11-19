# `wasmer-types`

This crate provides the shared WebAssembly types used across Wasmer Runtime:

- Value and external types such as `Type`, `ExternType`, and `FunctionType`.
- Memory units such as `Pages` and `Bytes`.
- Module metadata, indexes, and table and data initializers.
- Feature flags for WebAssembly proposals.

The [`wasmer` crate](../api/README.md) exposes the higher-level embedding API.

## Acknowledgments

This project borrowed some of the entity code from
[`cranelift-entity`](https://crates.io/crates/cranelift-entity) and adapted it
for serialization. See [attributions](../../docs/ATTRIBUTIONS.md) for licenses
and other notices.

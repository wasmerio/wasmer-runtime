# `wasmer-compiler-cranelift`

[![crates.io](https://img.shields.io/crates/v/wasmer-compiler-cranelift.svg)](https://crates.io/crates/wasmer-compiler-cranelift)

This crate implements the default native Wasmer compiler using Cranelift.
Cranelift balances compilation time and the performance of generated code.
If execution speed matters more than compilation time, compare it with the
[LLVM backend](../compiler-llvm/README.md) on your workload.

## Usage

```rust
use wasmer::Store;
use wasmer_compiler_cranelift::Cranelift;

let mut store = Store::new(Cranelift::default());
```

See the [runnable Cranelift example](../../examples/compiler_cranelift.rs).

## Acknowledgments

This project borrowed some of the function lowering from
[`cranelift-wasm`](https://crates.io/crates/cranelift-wasm). See
[attributions](../../docs/ATTRIBUTIONS.md) for licenses and other notices.

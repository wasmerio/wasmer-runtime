# `wasmer-compiler-singlepass`

[![BUSL-1.1 License](https://img.shields.io/badge/license-BUSL--1.1-blue?style=flat-square)](./LICENSE)
[![crates.io](https://img.shields.io/crates/v/wasmer-compiler-singlepass.svg)](https://crates.io/crates/wasmer-compiler-singlepass)

This crate implements Wasmer's Singlepass compiler. It prioritizes fast and
predictable compilation over optimizing the generated code. Compare it with
[Cranelift](../compiler-cranelift/README.md) and [LLVM](../compiler-llvm/README.md)
when execution speed matters more than compilation time.

Singlepass supports x86_64 and aarch64. Its [RISC-V support](../../docs/RISCV.md)
is experimental.

## Usage

```rust
use wasmer::Store;
use wasmer_compiler_singlepass::Singlepass;

let mut store = Store::new(Singlepass::default());
```

See the [runnable Singlepass example](../../examples/compiler_singlepass.rs).

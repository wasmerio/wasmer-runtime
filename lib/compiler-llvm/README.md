# `wasmer-compiler-llvm`

[![BUSL-1.1 License](https://img.shields.io/badge/license-BUSL--1.1-blue?style=flat-square)](./LICENSE)
[![crates.io](https://img.shields.io/crates/v/wasmer-compiler-llvm.svg)](https://crates.io/crates/wasmer-compiler-llvm)

This crate implements a Wasmer compiler using [LLVM](https://llvm.org/). It
prioritizes optimizing generated code over compilation speed. Benchmark on
your workload when selecting a compiler.

## Usage

```rust
use wasmer::Store;
use wasmer_compiler_llvm::LLVM;

let mut store = Store::new(LLVM::default());
```

See the [runnable LLVM example](../../examples/compiler_llvm.rs).

## Requirements

Building this backend requires **LLVM 23.1**. Install it with your platform's
package manager if available, or use a compatible [LLVM build](https://releases.llvm.org/download.html).
CI uses the [official LLVM 23.1.5 binaries](https://github.com/llvm/llvm-project/releases/tag/llvmorg-23.1.5)
on glibc-based Linux and Apple Silicon macOS.

If needed, point `LLVM_SYS_231_PREFIX` at the LLVM installation prefix.
See the [runtime build guide](../../docs/BUILD.md#llvm-compiler) for the Cargo
command that enables this backend.

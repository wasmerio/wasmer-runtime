# `wasmer-compiler`

This crate provides the native engine and shared infrastructure for Wasmer's
[Cranelift](../compiler-cranelift/README.md),
[Singlepass](../compiler-singlepass/README.md), and
[LLVM](../compiler-llvm/README.md) compilers. It translates Wasm modules into
shared metadata and delegates compilation of function bodies to each backend.

## Implementing a compiler

A backend implements `CompilerConfig` to configure and create an instance
of `Compiler`, which compiles a translated Wasm module to native code.
See [the trait definitions](./src/compiler.rs) and the existing native backends.

## Acknowledgments

This project borrowed some of its structure from
[`cranelift-wasm`](https://crates.io/crates/cranelift-wasm), adapted to be
independent of any compiler IR. See [attributions](../../docs/ATTRIBUTIONS.md)
for licenses and other notices.

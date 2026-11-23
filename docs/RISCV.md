# RISC-V support in Wasmer Runtime

Cranelift and LLVM support `riscv64`. The
[Singlepass backend](../lib/compiler-singlepass/README.md) has experimental
`riscv64` support.

CI runs the Cranelift tests for `riscv64gc-unknown-linux-gnu`, plus a few
Singlepass tests selected by name; see
[`test.yaml`](../.github/workflows/test.yaml) for the runner and feature flags.
Known limitations, including skipped Cranelift SIMD tests and LLVM ABI-related
failures, are tracked in [`tests/ignores.txt`](../tests/ignores.txt).

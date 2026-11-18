# Wasmer-specific WAST tests

These fixtures cover runtime regressions beyond the standard WebAssembly
spec tests. See the [testing guide](../../../docs/TEST.md#running-a-single-wast-test)
for how to run a specific fixture.

## NaN canonicalization: `nan-canonicalization.wast`

This is an extra set of tests that assure that operations with NaNs
are deterministic regardless of the environment/chipset where it executes in.

## Call Indirect Spilled Stack: `call-indirect-spilled-stack.wast`

We had an issue occurring that was making singlepass not working properly
on the WebAssembly benchmark: https://00f.net/2019/10/22/updated-webassembly-benchmark/.

This is a test case to ensure it doesn't reproduce again in the future.

## Multiple Traps: `multiple-traps.wast`

This is a test assuring functions that trap can be called multiple times.

## Fac: `fac.wast`

This is a simple factorial program.

## Check that struct-return on the stack doesn't overflow: `stack-overflow-sret.wast`

Stack space for a structure returning function call should be allocated once up
front, not once in each call.

## Divide by Zero: `divide.wast`

This is a simple test to check that a divide by zero is correctly trapped

## Atomic Load: `atomic_load.wast`

An out-of-bounds atomic load must trap.

## Nested Unreachable Blocks: `nested-unreachable-blocks.wast`

This was broken at one point in our LLVM backend. Otherwise, it's nothing specific.

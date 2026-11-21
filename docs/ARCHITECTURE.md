# Runtime repository architecture

The [`members` list in Cargo.toml](../Cargo.toml) is the source of truth for
the workspace. The root package, `wasmer-workspace`, provides examples,
benchmarks, and compiler integration tests.

| Path | Purpose |
| --- | --- |
| [`lib/api`](../lib/api) | Public `wasmer` Rust API: modules, instances, stores, and imports and exports. Includes the native (`sys`), V8, and JavaScript host backends. |
| [`lib/c-api`](../lib/c-api), [`lib/c-api-imports`](../lib/c-api-imports) | C API (`libwasmer` and `wasmer.h`) without WASI, and host imports for guests that use the Wasm C API. |
| [`lib/compiler`](../lib/compiler) | Native engine, shared compiler traits, Wasm translation, and compilation artifacts. |
| [`lib/compiler-cranelift`](../lib/compiler-cranelift), [`lib/compiler-singlepass`](../lib/compiler-singlepass), [`lib/compiler-llvm`](../lib/compiler-llvm) | Native compiler implementations. |
| [`lib/vm`](../lib/vm) | Low-level native VM: instances, memory, tables, traps, and the Wasm ABI. |
| [`lib/types`](../lib/types) | Shared WebAssembly types and metadata. |
| [`lib/cache`](../lib/cache), [`lib/middlewares`](../lib/middlewares), [`lib/derive`](../lib/derive) | Compiled module caching, compiler middleware, and procedural macros. |
| [`tests/compilers`](../tests/compilers), [`tests/wast`](../tests/wast), [`tests/lib`](../tests/lib) | Compiler integration tests, WAST suites, and test support crates. |
| [`examples`](../examples), [`benches`](../benches), [`fuzz`](../fuzz) | Embedding examples, benchmarks, and fuzz targets. |

The CLI, WASI/WASIX (including its C bindings), and package tooling are
maintained in [`wasmerio/wasmer`](https://github.com/wasmerio/wasmer), not in
this workspace.

## Submodules

- `tests/wast/spec`: the WebAssembly spec testsuite.
- `wasmer-test-files`: shared runtime test fixtures.

See [BUILD.md](./BUILD.md#getting-started) for initialization instructions.

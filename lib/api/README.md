# `wasmer`

[![crates.io](https://img.shields.io/crates/v/wasmer.svg)](https://crates.io/crates/wasmer)

The `wasmer` crate is the public Rust API of
[Wasmer Runtime](https://github.com/wasmerio/wasmer-runtime), an embeddable
[WebAssembly](https://webassembly.org/) engine.

## Usage

Add `wasmer` with `cargo add wasmer`. The default native configuration uses
Cranelift and accepts both binary WebAssembly and its text format (WAT):

```rust
use wasmer::{imports, Instance, Module, Store};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut store = Store::default();
    let module = Module::new(
        &store,
        r#"(module
            (func (export "add_one") (param i32) (result i32)
                local.get 0
                i32.const 1
                i32.add))"#,
    )?;
    let instance = Instance::new(&mut store, &module, &imports! {})?;
    let add_one = instance.exports.get_typed_function::<i32, i32>(&store, "add_one")?;
    assert_eq!(add_one.call(&mut store, 42)?, 43);
    Ok(())
}
```

## Backends and compilation

- **Native (`sys`)**: choose the Cranelift, Singlepass, or LLVM compiler with
  Cargo features. You can enable more than one and select the compiler when
  creating an engine.
- **V8**: on supported native targets, use `v8-default` with default features
  disabled to select V8 instead of the native compiler stack.
- **JavaScript host (`js`)**: build a Rust program for
  `wasm32-unknown-unknown` with `js-default` and default features disabled.
  Execution uses the host's WebAssembly engine.
- **Ahead-of-time compilation**: compile a module for a target and serialize
  it for later use. A headless engine can load compatible compiled artifacts
  without a compiler. Only deserialize artifacts you trust.

See the [Rust API reference](https://docs.rs/wasmer/), the
[build guide](../../docs/BUILD.md#selecting-a-backend), and the
[runnable examples](../../examples/README.md).

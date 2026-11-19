# `wasmer-cache`

The `wasmer-cache` crate stores compiled WebAssembly modules so that subsequent
runs can reuse them instead of recompiling. The `Cache` trait represents a
module cache, and `FileSystemCache` stores artifacts on disk. Only load cached
artifacts from trusted sources.

## Usage

```rust
use wasmer::Module;
use wasmer_cache::{Cache, FileSystemCache, Hash};

fn store_module(module: &Module, wasm_bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    let mut cache = FileSystemCache::new("some/directory/goes/here")?;
    let hash = Hash::generate(wasm_bytes);
    cache.store(hash, module)?;
    Ok(())
}
```

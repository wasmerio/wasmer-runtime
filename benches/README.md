# Runtime benchmarks

These Criterion benchmarks cover runtime operations such as compilation,
instantiation, and function calls. After following the
[build setup](../docs/BUILD.md), run a benchmark from the repository root:

```sh
cargo bench --locked -p wasmer-workspace --features cranelift --bench compile
```

See the `[[bench]]` entries in [`Cargo.toml`](../Cargo.toml) for available
benchmarks and required features.

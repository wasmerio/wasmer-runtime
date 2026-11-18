# Wasmer Fuzz Testing

[Fuzz testing](https://en.wikipedia.org/wiki/Fuzzing) is:

> An automated testing technique that involves providing invalid,
> unexpected, or random data as inputs to a program.

We use fuzz testing to automatically discover bugs in the Wasmer runtime.

This `fuzz/` directory contains the configuration and the fuzz tests
for Wasmer. To generate and to run the fuzz tests, we use the
[`cargo-fuzz`] library.

## Installation

You may need to install the [`cargo-fuzz`] library to get the `cargo
fuzz` subcommand. Use

```sh
$ cargo install cargo-fuzz
```

`cargo-fuzz` is documented in the [Rust Fuzz
Book](https://rust-fuzz.github.io/book/cargo-fuzz.html).

## Running a fuzzer

Use a nightly Rust toolchain with `cargo-fuzz`. For example, from the
repository root:

```sh
rustup toolchain install nightly
cargo +nightly fuzz run --features=cranelift,sanitizer cranelift
```

See [`Cargo.toml`](./Cargo.toml) for the target names and required features,
and [`fuzz_targets`](./fuzz_targets) for their implementations.

You should see output that looks something like this:

```
#1408022        NEW    cov: 115073 ft: 503843 corp: 4659/1807Kb lim: 4096 exec/s: 889 rss: 857Mb L: 2588/4096 MS: 1 ChangeASCIIInt-
#1408273        NEW    cov: 115073 ft: 503844 corp: 4660/1808Kb lim: 4096 exec/s: 888 rss: 857Mb L: 1197/4096 MS: 1 ShuffleBytes-
#1408534        NEW    cov: 115073 ft: 503866 corp: 4661/1809Kb lim: 4096 exec/s: 886 rss: 857Mb L: 977/4096 MS: 1 ShuffleBytes-
#1408540        NEW    cov: 115073 ft: 503869 corp: 4662/1811Kb lim: 4096 exec/s: 886 rss: 857Mb L: 2067/4096 MS: 1 ChangeBit-
#1408831        NEW    cov: 115073 ft: 503945 corp: 4663/1811Kb lim: 4096 exec/s: 885 rss: 857Mb L: 460/4096 MS: 1 CMP- DE: "\x16\x00\x00\x00\x00\x00\x00\x00"-
#1408977        NEW    cov: 115073 ft: 503946 corp: 4664/1813Kb lim: 4096 exec/s: 885 rss: 857Mb L: 1972/4096 MS: 1 ShuffleBytes-
#1408999        NEW    cov: 115073 ft: 503949 corp: 4665/1814Kb lim: 4096 exec/s: 884 rss: 857Mb L: 964/4096 MS: 2 ChangeBit-ShuffleBytes-
#1409040        NEW    cov: 115073 ft: 503950 corp: 4666/1814Kb lim: 4096 exec/s: 884 rss: 857Mb L: 90/4096 MS: 1 ChangeBit-
#1409042        NEW    cov: 115073 ft: 503951 corp: 4667/1814Kb lim: 4096 exec/s: 884 rss: 857Mb L: 174/4096 MS: 2 ChangeByte-ChangeASCIIInt-
```

It will continue to generate random inputs forever, until it finds a
bug or is terminated. The testcases for bugs it finds go into
`fuzz/artifacts/cranelift` and you can rerun the fuzzer on a
single input by running
`cargo +nightly fuzz run --features=cranelift,sanitizer cranelift /path/to/testcase`.

## The corpus

Each fuzzer has an individual corpus under `fuzz/corpus/test_name`,
created on first run if not already present. The fuzzers use
`wasm-smith` which means that the testcase files are random number
seeds input to the Wasm generator, not `.wasm` files themselves. In
order to debug a testcase, you may find that you need to convert it
into a `.wasm` file. Using the standalone `wasm-smith` tool doesn't
work for this purpose because we use a custom configuration to our
`wasm_smith::Module`. Instead, our fuzzers use an environment variable
`DUMP_TESTCASE=path`. For example:

```sh
$ DUMP_TESTCASE=/tmp/crash.wasm cargo +nightly fuzz run --features=singlepass,sanitizer singlepass /path/to/testcase
```

[`cargo-fuzz`]: https://github.com/rust-fuzz/cargo-fuzz

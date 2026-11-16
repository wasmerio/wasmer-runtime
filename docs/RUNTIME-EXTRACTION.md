# Runtime extraction

`main` is exactly upstream bb6cb1a3c5dbdfc2609ecd897dbf28f7df84e5cb with full
history. `next` replays the engine changes from wasmer-next
50933d71d975c5499b6d9d205136545644850c04 since
4a7f16c1154251fdc7b2de43b213c1043ad65d76 onto that upstream main.

129 non-merge commits were replayed in topological order, restricted to engine
and V8 producer paths. Original authors and messages are retained, with
Replayed-from and Replayed-by: Codex (AI agent) trailers. The exact mapping is
in runtime-replay-map.json. Codex performed the replay and committed the
integration changes; the human owner remains responsible for contributions.

Both engine-touching source merges (a8fe55b54 and a9a39c9cb) were audited
against the final source tree. No source-only engine changes are missing.
Per-file differences from wasmer-next are enumerated in runtime-engine-diff.json.
Files changed only upstream retain upstream changes. The producer workflow's
repository/ref guards intentionally target this repository and next.

## Scope and conflicts

Engine crates, WAST/compiler tests, ignores and fuzzing were replayed. V8
provider/header/patch handling lives inside lib/api; producer scripts, tests and
workflow under .github were replayed too. Existing upstream Makefile engine and
V8 targets remain. The added runtime-engine workflow tests next. Non-engine
workspace crates remain upstream's versions.

lib/sys-utils depends on wasmer, wasmer-vm and wasmer-types, rather than supplying
them. No engine manifest depends on it; upstream already removed it. No extra
runtime helper crate was needed. Existing engine macros and compiler-test/wast
support crates are included. The fork's Nix V8 helper consumes NAPI's provider,
not API's wee8 provider, and was not imported. Upstream NAPI stays unchanged.

- Cranelift ELF: retain upstream Mach-O compact unwind and local relocation
  adjustments, adding fork dynamic trampoline DWARF unwind separately.
- JS deserialization: retain upstream IntoBytes and fork cached error propagation.
- Interrupts: combine fork parked-stack installation IDs and exact guard removal
  with upstream atomic interruption flags and local queries without a global lock.
- Native/V8 functions: retain fork pins, origins, async custody and retirement
  checks. Adapt upstream CallWrapper around these paths. Typed wrappers convert
  arguments once to Values. Retain upstream hidden-memory/tail-call ABI flags.
- Retain fork OnCalledAction and StoreMut::on_called compatibility alongside
  CallWrapper, including callback loops on JS.
- Resolver/tables: combine upstream tail-call/fixed-funcref fields with fork
  function pins and definition-storage ownership.
- V8 build: retain fork private provider, headers, authoritative explicit
  override and atomic archive handling; retain upstream normalized cache path
  and static dependency link order.

## Consumption

Use one Git source identity for all direct engine dependencies, for example:

```toml
[workspace.dependencies]
wasmer = { git = "https://github.com/wasmerio/wasmer-runtime", branch = "next", default-features = false }
wasmer-types = { git = "https://github.com/wasmerio/wasmer-runtime", branch = "next" }
wasmer-vm = { git = "https://github.com/wasmerio/wasmer-runtime", branch = "next" }
wasmer-compiler = { git = "https://github.com/wasmerio/wasmer-runtime", branch = "next" }
```

Also migrate direct dependencies on wasmer-compiler-cranelift,
wasmer-compiler-singlepass, wasmer-compiler-llvm, wasmer-derive and
wasmer-middlewares to that URL/branch. Versions remain 7.5.0. Test-only packages
are macro-wasmer-engine-test, compiler-test-derive and wasmer-wast.

Upstream requires Rust 1.99, wasmparser/encoder 0.263, Cranelift 0.136.5 and LLVM
23 via inkwell 0.11. Consumers exposing these types must align dependency versions;
the fork's Rust 1.96/LLVM 22 setup is insufficient here.

Features include cranelift, singlepass, llvm, v8, v8-default, experimental-async
and experimental-host-interrupt. For V8-only use --no-default-features with
v8-default,experimental-async; sys is the default.

The engine needs no root patch entries. If retained registry dependencies pull
Wasmer packages, add consumer [patch.crates-io] entries for every involved engine
package listed above, all using the same Git URL/branch. Never mix local,
registry and Git engine type identities.

Private HTTPS fetches need gh auth login, gh auth setup-git and Cargo config:

```toml
[net]
git-fetch-with-cli = true
```

CI needs Contents-read access to wasmer-runtime; a token scoped only to
wasmer-next cannot fetch this repository. SSH URLs need an authorized SSH identity.
Keep one URL spelling throughout dependencies and patches.

## V8 provider

Linux x86_64 GNU uses private wasmer-next release asset 726693826. Archive SHA256:
56d466127af223f0fc4ca453d6326bcd8db8eeba50482326def81c9674949a41.
libv8.a SHA256:
8624c72e07e7c4c08f12fa7274ff57ddc3f11ddc3b3a74c45bc246a088d3496c.

Automatic download requires GH_TOKEN or GITHUB_TOKEN with Contents-read access
to wasmer-next, separately from credentials fetching wasmer-runtime. The new
runtime-engine workflow uses RUNTIME_V8_READ_TOKEN. No asset was republished.

Alternatively WASMER_V8_ARTIFACT_DIR must be an absolute directory containing
lib/libv8.a (Windows: lib/v8.lib). It is authoritative and has no fallback;
callers must verify matching provider/ABI. Keep headers, owner shim and canonical
patches in lib/api/third-party/wee8 paired with the provider; SOURCE.md records
provenance. Other supported platforms use wee8-custom-builds 11.9.7. Building
requires libclang, a C++ compiler and archive/symbol tools. The producer workflow
builds an uncertified candidate; running it does not certify/adopt a provider.

Initialize upstream submodules with git submodule update --init --recursive.
Workspace metadata requires upstream NAPI even when selecting only engine crates.
The WAST spec submodule supplies spec fixtures.

## Validation collected on next

No validation ran on main. Further validation stopped at the owner's request.
All commands below ran in the fresh runtime checkout.

| Command | Result |
| --- | --- |
| `cargo check -p wasmer --features cranelift,singlepass,experimental-async` | Passed |
| `cargo check -p wasmer --features cranelift,singlepass,experimental-async,experimental-host-interrupt` | Passed |
| `LLVM_SYS_231_PREFIX=/nix/store/5lkpccyv590xcyff02y7vbbb3v49i7yx-llvm-23.1.3-dev cargo check -p wasmer --features llvm` | Passed |
| `cargo check -p wasmer-cli --features cranelift,singlepass` | Passed, including upstream WASIX/journal/package-sdk dependencies |
| `cargo test -p wasmer --features cranelift` | Passed, including integration tests and doctests |
| `cargo test -p wasmer --features cranelift,singlepass,experimental-async` | Initial unit run: 165 passed, tiny-stack test failed due to larger cached stack reuse |
| `cargo test -p wasmer --features cranelift,singlepass,experimental-async -- --skip backend::sys::tunables::tests::check_small_stack` | Passed, including integration tests and doctests |
| `target/debug/deps/wasmer-d53a108cc93ee466 --exact backend::sys::tunables::tests::check_small_stack --nocapture` | Passed alone; CI likewise runs this test in a fresh process |
| `cargo test -p wasmer-workspace --no-default-features --features cranelift,singlepass,wast,middlewares --test compilers` | 2776 passed, 1014 existing ignores, zero failures |
| `cargo test -p wasmer-vm --features experimental-host-interrupt interrupt_registry::` | 3 passed |
| `python3 -m unittest discover -s .github -p 'test_capi_v8_*.py'` | 20 passed |
| `cargo fmt --all -- --check` | Passed |
| `git diff --check` | Passed |
| `make lint` | Stopped at missing yamlfmt; subsequent lint gates were not run |

V8 commands used GH_TOKEN loaded privately from gh auth token, plus these Nix
build environment settings (credentials were not printed or committed):

```sh
export LIBCLANG_PATH=/nix/store/5c0rqk890xw9wz74msjsl00m2x42q7ag-clang-23.1.3-lib/lib
export BINDGEN_EXTRA_CLANG_ARGS='-isystem /nix/store/2f69qaci676cq5kr8svry3gbf6i6iqzi-glibc-2.44-57-dev/include'
cargo check -p wasmer --no-default-features --features v8-default,experimental-async
cargo test -p wasmer --no-default-features --features v8-default,experimental-async --test async_v8 --test call_wrapper -- --test-threads=1
```

Both passed: 10 async tests and 5 call-wrapper tests. Earlier attempts identified
missing libclang and system include paths, resolved by the settings above.
The parallel call-wrapper run crashed with SIGSEGV; the same five tests pass
serially. This concurrency limitation remains; runtime CI runs V8 tests serially.
No general V8 thread-safety claim is made by this extraction.

The workspace-wide check exposes a remaining incompatibility in unchanged
upstream NAPI, pinned at 82b328f50ddaac78de388b67b4707f21864b9245:

```sh
LLVM_SYS_231_PREFIX=/nix/store/5lkpccyv590xcyff02y7vbbb3v49i7yx-llvm-23.1.3-dev \
LIBCLANG_PATH=/nix/store/5c0rqk890xw9wz74msjsl00m2x42q7ag-clang-23.1.3-lib/lib \
cargo check --workspace --features wasmer-bin-fuzz/cranelift,wasmer-bin-fuzz/singlepass
```

It fails with four errors in lib/napi/src/budget.rs: create_vm_memory at line
873 and create_vm_table at 893 take NonNull definition pointers, but Tunables
now requires VMDefinitionLocation. Their forwarding calls at 881 and 896 also
have mismatched argument types (E0053 and E0308). The NAPI submodule was not
modified or pushed. Its Tunables adapter needs to accept and forward the new
ownership-bearing locations. CLI compilation passes without enabling NAPI.
Other workspace packages reached checking, but the workspace as a whole is not
claimed to pass.

A plain workspace check first stops at the equivalence fuzzer's explicit
compiler-feature requirement. Enabling only cranelift then hits its denied unused
cranelift variable: equivalence needs a pair of compilers for its comparison
branches. Enabling both as shown gets through fuzz checking to the NAPI errors.

Local raw validation logs and the full engine diff are retained under
.runtime-validation/ (excluded locally from Git). No pull requests were opened.
Only wasmer-runtime main and next were pushed; wasmer-next and upstream were
not modified or pushed.

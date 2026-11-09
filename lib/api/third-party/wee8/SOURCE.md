# wee8 public headers and private Store ownership adapter

## Explicit artifact input

`WASMER_V8_ARTIFACT_DIR=/absolute/path/to/artifact` selects an already unpacked,
host-supplied artifact containing `lib/libv8.a` (`lib/v8.lib` on Windows).
Configured relative paths, missing directories and missing libraries fail the
build; they never fall back to the original cache or a network download. The
selection is tracked with Cargo's `rerun-if-env-changed`, and the library input
is tracked with `rerun-if-changed`.

The raw input is not modified: symbol-prefixed copies and the Store ownership
adapter are generated under the build's `OUT_DIR`. Verify the artifact's source,
SHA256 and paired headers before selecting it. This explicit input is also the
offline/prepopulated route; it does not certify an arbitrary supplied library.
When unset, the existing provider and cache selection remain unchanged.

The `wasm.h` and complete `wasm.hh` come from V8
`b0a55a7dad7f536cce1f9aaddba89894c8533946` (13.6.233.17), with the original
eight patches from producer `844d`, the authoritative library-TLS patch9, and
`canonical-references.patch` below. The C header is unchanged; the C++ header
includes the canonical reference-kind metadata. The original artifact release
label `11.9.7` alone does not identify this source tuple.

The header-affecting original patches, applied in their original order, are:

| patch | SHA-256 |
| --- | --- |
| 0001-Add-ability-to-define-sharedness-of-memory.patch | fcca746140f65570036b34a6b7b64753f40b576734cb1a330957d7339b05cec4 |
| 0002-Add-support-for-tags-eh.patch | 35f07f131f9e1caab36e8438cb68236f63c8a4fabdb37904bff501ac00c7efc5 |
| 0005-Add-v128-support-to-wasm-c-api.patch | 1ef4fbe65020669d914c0a61afccd1fafe43703955f6f7b7dcfa727abbaf8d00 |
| 0007-shared-memory.patch | f09a3c97261ecd9257f441b68a4c6768b51d48fd20cfac55c00a5298d4a6d0ba |

The other four original patches and TLS9 do not modify these headers. Reproduce
the exact V8 revision with the original eight patches, TLS9, then the canonical
reference patch; copy the complete `third_party/wasm-api/wasm.{h,hh}` unchanged.

| tracked file | SHA-256 |
| --- | --- |
| wasm.h | 97f49de5418f02c55633dbcdc96b72fb404659c9275e9a47a3b4d6bdce384b96 |
| wasm.hh | 67eae9b38696c2620e797e3c96c30441ea5dfdfa62957d1f635040c010429729 |
| LICENSE | c6596eb7be8581c18be736c846fb9173b69eccf6ef94c5135893ec56bd92ba08 |

`LICENSE` is the exact Apache 2.0 `third_party/wasm-api/LICENSE`, rather than the
separate V8 project BSD license. The adapter `owned-store.cc` is authored by
OpenAI Codex. It uses the public `wasm::own<wasm::Store>` destroyer, which calls
the pinned `Store::destroy`, rather than assuming C facade `delete x` destroys
its opaque implementation. Only actual BackendStore ownership is passed here.
Its raw-pointer correspondence follows the pinned `release_store` conversion.

The adapter is compiled only with the V8 feature, against these exact paired
headers, with `LIBWASM_STATIC` and the existing selected target C++ toolchain.
Its archive precedes `v8prefixed`; the existing C++ standard-library selection
is unchanged. It does not modify the provider, GN flags, Engine destruction,
public C ABI, or header declarations. Actual C++ finalizer timing is a runtime
validation gate, not established by a wrapper return or weak-origin expiry.

## Private candidate canonical-reference patch

`canonical-references.patch` is an OpenAI Codex authored source patch for the
private V8 provider candidate. Apply it only to V8
`b0a55a7dad7f536cce1f9aaddba89894c8533946`, after the original eight patches
from producer `844d01dc10edaa0461715f484e06b004f1fd023e` and the authoritative
library-TLS patch9. Its SHA-256 is
`cf248bf0964a508002860577ddb2b29bc955e389c89649868e2895abf297dfbc`.
It uses V8's existing canonical callable-reference conversions and rooting;
it changes only `src/wasm/c-api.cc` and `third_party/wasm-api/wasm.hh`.

The candidate C++ header SHA-256 is
`67eae9b38696c2620e797e3c96c30441ea5dfdfa62957d1f635040c010429729`;
the public C header remains
`97f49de5418f02c55633dbcdc96b72fb404659c9275e9a47a3b4d6bdce384b96`.
The tracked C++ header is the exact `candidate-wasm.hh` retained by private Next
Actions run `42537667351`, candidate commit
`dd13e1dd0d34db284f6c8eee0afd878ac2d60144`. The corresponding library archive
SHA-256 is `efcfa8cd7d417e3ad043d6d759acd988860df0996f48876efa1449677cf0379b`;
its raw `lib/libv8.a` SHA-256 is
`d27d644a21409e41081cc4c66565534112c31ef18a201cdd13c55fd73389a95d`.
The isolated validation tuple selects this archive explicitly. Default provider
selection remains unchanged until the actual shared C API and reference gates
pass; successful packaging alone does not establish consumer acceptance.

## Private candidate C-entry layout and Global scope correction

`canonical-call-layout.patch` follows the original eight patches, TLS9 and
`canonical-references.patch`, as producer patch0011. It is authored by OpenAI
Codex; its SHA-256 is
`6ff5755d0d7d6de03d329b9d49a7d05f3a313bf192ae36abf6a647b37aa9f0bc`.

It changes only both `BuildCWasmEntry` parameter/result offsets from
`value_kind_size()` to `value_kind_full_size()` in
`src/compiler/wasm-compiler.cc`, and adds the lexical `Global::set` HandleScope
in `src/wasm/c-api.cc`. With the unchanged pointer-compressed configuration,
the existing packer and full-pointer loads/stores use eight-byte references;
four-byte offset increments overlap those slots. Numeric and v128 widths,
C/C++ headers, reference ownership, GN, tools and other producer policies are
unchanged. No Rust pointer compensation is used.

The exact source hashes after patch0011 are:

| file | SHA-256 |
| --- | --- |
| src/compiler/wasm-compiler.cc | eec8744ceceb6f98509844494d3d40a8e6db3b3f128301154386eac3e15977a5 |
| src/wasm/c-api.cc | 533a9bcf0c66075485a4b612f94466e13b4b9de9edc8974bbb6d08799db9b894 |

The prior run425376 packaging evidence remains valid for its original tuple;
its first reference matrix passed one test and failed four, so it is not an
accepted default provider. A new source build and actual affected reference
gates are required before adoption.

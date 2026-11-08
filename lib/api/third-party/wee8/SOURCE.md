# wee8 public headers and private Store ownership adapter

The existing `wasm.h` and added complete `wasm.hh` are the paired public headers
from V8 `b0a55a7dad7f536cce1f9aaddba89894c8533946` (13.6.233.17), as built by the
original wee8 producer revision `844d`. The artifact release label is `11.9.7`.
The archive's C header is byte-identical to this tracked `wasm.h`.

The header-affecting original patches, applied in their original order, are:

| patch | SHA-256 |
| --- | --- |
| 0001-Add-ability-to-define-sharedness-of-memory.patch | fcca746140f65570036b34a6b7b64753f40b576734cb1a330957d7339b05cec4 |
| 0002-Add-support-for-tags-eh.patch | 35f07f131f9e1caab36e8438cb68236f63c8a4fabdb37904bff501ac00c7efc5 |
| 0005-Add-v128-support-to-wasm-c-api.patch | 1ef4fbe65020669d914c0a61afccd1fafe43703955f6f7b7dcfa727abbaf8d00 |
| 0007-shared-memory.patch | f09a3c97261ecd9257f441b68a4c6768b51d48fd20cfac55c00a5298d4a6d0ba |

The other four original patches do not modify these headers. Reproduction is a
checkout of the exact V8 revision followed by those original header patches;
copy the resulting complete `third_party/wasm-api/wasm.{h,hh}` unchanged.

| tracked file | SHA-256 |
| --- | --- |
| wasm.h | 97f49de5418f02c55633dbcdc96b72fb404659c9275e9a47a3b4d6bdce384b96 |
| wasm.hh | d2dfa5e09297be15ffbbfed29c71769bbbd5a9cd57c3c95344dffd1d97bb2ed3 |
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

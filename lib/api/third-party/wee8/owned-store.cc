// Authored by OpenAI Codex. The paired header supplies the pinned destroyer.
#include "wasm.hh"

// Only the actual ownership returned by wasm_store_new reaches this boundary.
// The C facade's delete does not invoke Store::destroy on the pinned archive.
extern "C" void wasmer_v8_owned_store_delete(void* actual_store) noexcept {
  wasm::own<wasm::Store> store(reinterpret_cast<wasm::Store*>(actual_store));
}

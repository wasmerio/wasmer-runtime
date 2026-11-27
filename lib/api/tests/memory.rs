use macro_wasmer_engine_test::engine_test;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
#[cfg(feature = "js")]
use wasm_bindgen::JsCast;
#[cfg(feature = "js")]
use wasm_bindgen_test::*;
use wasmer::{Instance, Memory, MemoryLocation, MemoryType, Module, Pages, Store, imports};
#[cfg(feature = "js")]
use wasmer::{
    MemoryAccessError, Value,
    js::{AsJs, JsSharedMemoryExt},
};

fn memory_type(minimum: u32, maximum: u32, shared: bool) -> MemoryType {
    MemoryType::new(
        Pages::try_from_pages(minimum).unwrap(),
        Some(Pages::try_from_pages(maximum).unwrap()),
        shared,
    )
}

#[test]
#[allow(unused_attributes)]
#[cfg_attr(
    feature = "v8",
    ignore = "v8 does not currently support shared memory through wasm_c_api"
)]
fn test_shared_memory_atomics_notify_send() {
    let mut store = Store::default();
    let wat = r#"(module
(import "host" "memory" (memory 10 65536 shared))
)"#;
    let module = Module::new(&store, wat)
        .map_err(|e| format!("{e:?}"))
        .unwrap();

    let mem = Memory::new(&mut store, memory_type(10, 65536, true)).unwrap();

    let imports = imports! {
        "host" => {
            "memory" => mem.clone(),
        },
    };

    let _inst = Instance::new(&mut store, &module, &imports).unwrap();

    let mem = if let Some(m) = mem.as_shared(&store) {
        m
    } else {
        #[cfg(feature = "sys")]
        panic!("Memory is not shared");
        #[cfg(not(feature = "sys"))]
        return;
    };

    // Test basic notify.
    let mem2 = mem.clone();
    std::thread::spawn(move || {
        loop {
            if mem2.notify(MemoryLocation::new_32(10), 1).unwrap() > 0 {
                break;
            }
        }
    });

    mem.wait(MemoryLocation::new_32(10), None).unwrap();

    // Test wake_all

    let done = Arc::new(AtomicBool::new(false));

    std::thread::spawn({
        let mem = mem.clone();
        let done = done.clone();
        move || {
            while !done.load(Ordering::SeqCst) {
                mem.wake_all_atomic_waiters().ok();
            }
        }
    });

    mem.wait(MemoryLocation::new_32(10), None).unwrap();
    done.store(true, Ordering::SeqCst);
}

#[cfg(feature = "sys")]
#[test]
#[cfg_attr(
    feature = "v8-default",
    ignore = "shared memory atomics are not supported by the default v8 backend"
)]
fn test_shared_memory_disable_atomics() {
    use wasmer::AtomicsError;

    let mut store = Store::default();
    let mem = Memory::new(&mut store, memory_type(10, 65536, true)).unwrap();

    let mem = mem.as_shared(&store).unwrap();
    mem.disable_atomics().unwrap();

    let err = mem.wait(MemoryLocation::new_32(1), None).unwrap_err();
    assert_eq!(err, AtomicsError::AtomicsDisabled);
}

#[cfg(feature = "sys")]
#[test]
fn test_shared_memory_copy_is_independent() {
    let mut store = Store::default();
    let memory = Memory::new(&mut store, memory_type(1, 2, true)).unwrap();

    memory.view(&store).write(0, b"before").unwrap();

    let mut copy_store = Store::default();
    let copied = memory.copy(&store).unwrap().attach(&mut copy_store);

    memory.view(&store).write(0, b"after!").unwrap();

    let mut buf = [0; 6];
    copied.view(&copy_store).read(0, &mut buf).unwrap();
    assert_eq!(&buf, b"before");
}

#[engine_test]
fn test_non_shared_memory_copy_is_rejected() -> Result<(), String> {
    let mut store = Store::default();
    let memory = Memory::new(&mut store, memory_type(1, 2, false)).unwrap();
    assert!(matches!(
        memory.copy(&store),
        Err(wasmer::MemoryError::MemoryNotShared)
    ));
    assert!(memory.as_shared(&store).is_none());
    let mut other_store = Store::default();
    assert!(matches!(
        memory.copy_to_store(&store, &mut other_store),
        Err(wasmer::MemoryError::MemoryNotShared)
    ));
    Ok(())
}

/// See https://github.com/wasmerio/wasmer/issues/5444
#[test]
#[cfg(feature = "sys")]
fn test_wasm_slice_issue_5444() {
    let mut store = Store::default();
    let wat = r#"(module
(import "host" "memory" (memory 10 65536))
)"#;
    let module = Module::new(&store, wat)
        .map_err(|e| format!("{e:?}"))
        .unwrap();

    let mem = Memory::new(&mut store, memory_type(10, 65536, false)).unwrap();

    let imports = imports! {
        "host" => {
            "memory" => mem.clone(),
        },
    };

    let _inst = Instance::new(&mut store, &module, &imports).unwrap();

    let view = mem.view(&store);
    let slice = wasmer::WasmSlice::<u64>::new(&view, 1, 10).unwrap();
    let access = slice.access();

    assert!(matches!(
        access.err(),
        Some(wasmer::MemoryAccessError::UnalignedPointerRead)
    ))
}

#[test]
fn test_wasm_slice_new_rejects_out_of_bounds_ranges() {
    let mut store = Store::default();
    let memory = Memory::new(&mut store, memory_type(1, 1, false)).unwrap();
    let view = memory.view(&store);

    let slice = wasmer::WasmSlice::<u64>::new(&view, 0, 8193);
    assert!(matches!(
        slice.err(),
        Some(wasmer::MemoryAccessError::HeapOutOfBounds)
    ));
}

#[test]
fn test_wasm_memory_size() {
    let mut store = Store::default();

    // Test once with not-shared memory...
    {
        let memory = Memory::new(&mut store, memory_type(10, 65536, false)).unwrap();
        assert_eq!(memory.size(&store).pages(), 10);
        memory.grow(&mut store, wasmer::Pages::ONE).unwrap();
        assert_eq!(memory.size(&store).pages(), 11);
    }

    // ... and once with shared memory, since JS has different representations for it
    {
        let memory = Memory::new(&mut store, memory_type(10, 65536, true)).unwrap();
        assert_eq!(memory.size(&store).pages(), 10);
        memory.grow(&mut store, wasmer::Pages::ONE).unwrap();
        assert_eq!(memory.size(&store).pages(), 11);
    }
}

#[test]
fn test_wasm_memory_grow_at_least_rounds_up() {
    let mut store = Store::default();
    let memory = Memory::new(&mut store, memory_type(1, 2, false)).unwrap();
    let page_size = wasmer::WASM_PAGE_SIZE as u64;

    memory.grow_at_least(&mut store, page_size + 1).unwrap();
    assert_eq!(memory.size(&store).pages(), 2);
}

#[test]
fn test_wasm_memory_grow_at_least_rejects_unrepresentable_size() {
    let mut store = Store::default();
    let memory = Memory::new(&mut store, memory_type(1, 2, false)).unwrap();

    assert!(memory.grow_at_least(&mut store, u64::MAX).is_err());
    assert_eq!(memory.size(&store), wasmer::Pages::ONE);
}

/// The 32-bit word of `memory` at `offset`.
#[cfg(feature = "js")]
fn word_at(memory: &Memory, store: &Store, offset: u64) -> u32 {
    let mut bytes = [0; 4];
    memory.view(store).read(offset, &mut bytes).unwrap();
    u32::from_le_bytes(bytes)
}

#[cfg(feature = "js")]
#[wasm_bindgen_test]
fn js_compare_exchange_replaces_only_an_expected_word() {
    for shared in [false, true] {
        let mut store = Store::default();
        let memory = Memory::new(&mut store, memory_type(1, 2, shared)).unwrap();
        let view = memory.as_js().view(&store);

        // The word is the expected one: it is replaced.
        assert_eq!(view.compare_exchange_u32(8, 0, u32::MAX).unwrap(), 0);
        assert_eq!(word_at(&memory, &store, 8), u32::MAX);
        // It is another one: it stays, and comes back.
        assert_eq!(view.compare_exchange_u32(8, 1, 2).unwrap(), u32::MAX);
        assert_eq!(word_at(&memory, &store, 8), u32::MAX);
        // A word with its high bit set compares as the same bits.
        assert_eq!(view.compare_exchange_u32(8, u32::MAX, 7).unwrap(), u32::MAX);
        assert_eq!(word_at(&memory, &store, 8), 7);

        // The words around it are not touched.
        assert_eq!(word_at(&memory, &store, 4), 0);
        assert_eq!(word_at(&memory, &store, 12), 0);
        // The last word of the memory is in bounds.
        assert_eq!(view.compare_exchange_u32(65532, 0, 1).unwrap(), 0);
        assert_eq!(word_at(&memory, &store, 65532), 1);
    }
}

#[cfg(feature = "js")]
#[wasm_bindgen_test]
fn js_compare_exchange_refuses_unaligned_and_out_of_bounds_offsets() {
    for shared in [false, true] {
        let mut store = Store::default();
        let memory = Memory::new(&mut store, memory_type(1, 2, shared)).unwrap();
        let view = memory.as_js().view(&store);

        for offset in [1, 2, 3, 65534] {
            assert!(matches!(
                view.compare_exchange_u32(offset, 0, 1),
                Err(MemoryAccessError::UnalignedPointerRead)
            ));
        }
        for offset in [65536, 1 << 20] {
            assert!(matches!(
                view.compare_exchange_u32(offset, 0, 1),
                Err(MemoryAccessError::HeapOutOfBounds)
            ));
        }
        // The word would end past what a JavaScript buffer can address.
        for offset in [u64::from(u32::MAX) - 3, 1 << 32, u64::MAX - 3] {
            assert!(matches!(
                view.compare_exchange_u32(offset, 0, 1),
                Err(MemoryAccessError::Overflow)
            ));
        }
        // A refused exchange writes nothing.
        assert_eq!(view.copy_range_to_vec(0..8).unwrap(), [0; 8]);
    }
}

#[cfg(feature = "js")]
#[wasm_bindgen_test]
fn js_compare_exchange_needs_a_new_view_after_growth() {
    for shared in [false, true] {
        let mut store = Store::default();
        let memory = Memory::new(&mut store, memory_type(1, 2, shared)).unwrap();
        let old_view = memory.as_js().view(&store);
        // A view borrows its store, so the memory grows as a guest on another
        // worker grows it: through its JavaScript object.
        let js_memory: js_sys::WebAssembly::Memory = memory.as_jsvalue(&store).unchecked_into();
        js_memory.grow(1);

        // The old view does not reach the new page.
        assert!(matches!(
            old_view.compare_exchange_u32(65536, 0, 42),
            Err(MemoryAccessError::HeapOutOfBounds)
        ));
        if shared {
            // The buffer of a shared memory stays, with its old length.
            assert_eq!(old_view.compare_exchange_u32(0, 0, 3).unwrap(), 0);
            assert_eq!(word_at(&memory, &store, 0), 3);
        } else {
            // Growing detached the buffer: the old view has nothing left.
            assert!(matches!(
                old_view.compare_exchange_u32(0, 0, 3),
                Err(MemoryAccessError::HeapOutOfBounds)
            ));
            assert_eq!(word_at(&memory, &store, 0), 0);
        }

        let view = memory.as_js().view(&store);
        assert_eq!(view.compare_exchange_u32(65536, 0, 42).unwrap(), 0);
        assert_eq!(word_at(&memory, &store, 65536), 42);
    }
}

/// The host's exchange and a guest's `i32.atomic.rmw.cmpxchg` act on the same
/// word, so each sees what the other did.
#[cfg(feature = "js")]
#[wasm_bindgen_test]
fn js_compare_exchange_is_the_atomic_exchange_of_a_guest() {
    let mut store = Store::default();
    let wat = r#"(module
(import "host" "memory" (memory 1 2 shared))
(func (export "cas") (param i32 i32 i32) (result i32)
  (i32.atomic.rmw.cmpxchg (local.get 0) (local.get 1) (local.get 2)))
)"#;
    let module = Module::new(&store, wat).unwrap();
    let memory = Memory::new(&mut store, memory_type(1, 2, true)).unwrap();
    let imports = imports! { "host" => { "memory" => memory.clone() } };
    let instance = Instance::new(&mut store, &module, &imports).unwrap();
    let guest_cas = instance.exports.get_function("cas").unwrap();
    let guest = |store: &mut Store, expected: i32, new: i32| {
        let args = [Value::I32(16), Value::I32(expected), Value::I32(new)];
        guest_cas.call(store, &args).unwrap()[0].unwrap_i32()
    };

    let host = |store: &Store, expected, new| {
        let view = memory.as_js().view(store);
        view.compare_exchange_u32(16, expected, new).unwrap()
    };
    assert_eq!(host(&store, 0, 5), 0);
    assert_eq!(guest(&mut store, 5, 9), 5);
    // The guest replaced the word the host expects.
    assert_eq!(host(&store, 5, 1), 9);
    assert_eq!(host(&store, 9, 2), 9);
    // And the host the one the guest expects.
    assert_eq!(guest(&mut store, 9, 3), 2);
    assert_eq!(word_at(&memory, &store, 16), 2);
}

#[cfg(feature = "js")]
#[wasm_bindgen_test]
fn js_shared_memory_gives_its_local_js_memory() {
    let mut store = Store::default();
    let memory = Memory::new(&mut store, memory_type(1, 2, true)).unwrap();
    let shared = memory.as_shared(&store).unwrap();

    // It is the object of the memory, not a copy of it.
    let js_memory = shared.local_js_memory().unwrap();
    assert!(js_sys::Object::is(&js_memory, &memory.as_jsvalue(&store)));
    assert!(js_sys::Object::is(
        &js_memory,
        &shared.clone().local_js_memory().unwrap()
    ));

    // A write through Wasmer is seen through the object, and the other way.
    memory.view(&store).write(0, b"wasmer").unwrap();
    let bytes = js_sys::Uint8Array::new(&js_memory.buffer());
    assert_eq!(bytes.subarray(0, 6).to_vec(), b"wasmer");
    bytes.set_index(0, b'W');
    let mut buffer = [0; 6];
    memory.view(&store).read(0, &mut buffer).unwrap();
    assert_eq!(&buffer, b"Wasmer");

    // So is a growth, and the memory attached to another store.
    memory.grow(&mut store, Pages::ONE).unwrap();
    let bytes = js_sys::Uint8Array::new(&js_memory.buffer());
    assert_eq!(bytes.length(), 2 * 65536);
    let mut other_store = Store::default();
    let attached = shared.clone().attach(&mut other_store);
    bytes.set_index(65536, 7);
    assert_eq!(attached.view(&other_store).read_u8(65536).unwrap(), 7);
    assert!(js_sys::Object::is(
        &js_memory,
        &attached.as_jsvalue(&other_store)
    ));
}

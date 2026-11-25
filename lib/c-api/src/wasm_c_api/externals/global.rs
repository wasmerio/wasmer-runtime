use crate::error::update_last_error;

use super::super::store::wasm_store_t;
use super::super::types::wasm_globaltype_t;
use super::super::value::wasm_val_t;
use super::wasm_extern_t;
use std::convert::TryInto;
use wasmer_api::{Extern, Global, Mutability, StoreMut, Value};

#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Clone)]
pub struct wasm_global_t {
    pub(crate) extern_: wasm_extern_t,
}

impl wasm_global_t {
    pub(crate) fn try_from(e: &wasm_extern_t) -> Option<&wasm_global_t> {
        match &e.inner {
            Extern::Global(_) => Some(unsafe { &*(e as *const _ as *const _) }),
            _ => None,
        }
    }
}

/// Creates a global holding `val`, unless `val` is a reference of another
/// store: `Global::new` panics on one instead of returning an error.
fn new_global(
    store: &mut StoreMut<'_>,
    val: Value,
    mutability: Mutability,
) -> Result<Global, &'static str> {
    if !val.is_from_store(store) {
        return Err("cross-`Store` values are not supported");
    }
    Ok(if mutability.is_mutable() {
        Global::new_mut(store, val)
    } else {
        Global::new(store, val)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_global_new(
    store: Option<&mut wasm_store_t>,
    global_type: Option<&wasm_globaltype_t>,
    val: Option<&wasm_val_t>,
) -> Option<Box<wasm_global_t>> {
    let global_type = global_type?;
    let store = store?;
    let val = val?;

    let mutability = global_type.inner().global_type.mutability;
    let wasm_val = val.try_into().ok()?;
    let created = c_try!(store.inner.with_mut(|mut store_mut| new_global(
        &mut store_mut,
        wasm_val,
        mutability
    )));
    let global = c_try!(created);
    Some(Box::new(wasm_global_t {
        extern_: wasm_extern_t::new(store.inner.clone(), global.into()),
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_global_delete(_global: Option<Box<wasm_global_t>>) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_global_copy(global: &wasm_global_t) -> Box<wasm_global_t> {
    // do shallow copy
    Box::new(global.clone())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_global_get(
    global: Option<&mut wasm_global_t>,
    // own
    out: Option<&mut wasm_val_t>,
) {
    let Some(global) = global else {
        update_last_error("global pointer is null");
        return;
    };
    let Some(out) = out else {
        update_last_error("out pointer is null");
        return;
    };
    let wasm_global = global.extern_.global();
    let store = &global.extern_.store;
    let value = store.with_mut(|mut store_mut| wasm_global.get(&mut store_mut));
    let value = c_try!(value; otherwise ());
    let new_val = c_try!(wasm_val_t::from_value(&value, store); otherwise ());
    // `out` is an owned out-parameter; write without dropping stale contents.
    unsafe { std::ptr::write(out, new_val) };
}

/// Note: This function returns nothing by design but it can raise an
/// error if setting a new value fails.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_global_set(
    global: Option<&mut wasm_global_t>,
    val: Option<&wasm_val_t>,
) {
    let Some(global) = global else {
        update_last_error("global pointer is null");
        return;
    };
    let Some(val) = val else {
        update_last_error("val pointer is null");
        return;
    };
    let value: Value = c_try!(val.try_into(); otherwise ());
    let wasm_global = global.extern_.global();
    let store = &global.extern_.store;
    let set = store.with_mut(|mut store_mut| wasm_global.set(&mut store_mut, value));
    let set = c_try!(set; otherwise ());
    c_try!(set; otherwise ());
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_global_same(
    wasm_global1: &wasm_global_t,
    wasm_global2: &wasm_global_t,
) -> bool {
    wasm_global1.extern_.global() == wasm_global2.extern_.global()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_global_type(
    global: Option<&wasm_global_t>,
) -> Option<Box<wasm_globaltype_t>> {
    let global = global?;
    let wasm_global = global.extern_.global();
    let ty = c_try!(global.extern_.store.with(|store| wasm_global.ty(&store)));
    Some(Box::new(wasm_globaltype_t::new(ty)))
}

#[cfg(test)]
mod tests {
    use inline_c::assert_c;

    // Externref creation is only implemented on the `sys` backend.
    #[cfg(feature = "sys")]
    #[allow(
        unexpected_cfgs,
        reason = "tools like cargo-llvm-coverage pass --cfg coverage"
    )]
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[test]
    fn test_global_refuses_a_reference_of_another_store() {
        (assert_c! {
            #include "tests/wasmer.h"

            // Asserts that the last call registered an error, and clears it.
            static void assert_refused() {
                int length = wasmer_last_error_length();
                assert(length > 0);

                char* message = malloc(length);
                wasmer_last_error_message(message, length);
                free(message);
            }

            int main() {
                wasm_engine_t* engine = wasm_engine_new();
                wasm_store_t* store = wasm_store_new(engine);
                wasm_store_t* other_store = wasm_store_new(engine);

                wasm_foreign_t* foreign = wasm_foreign_new(store);
                wasm_foreign_t* other_foreign = wasm_foreign_new(other_store);
                wasm_val_t value = WASM_REF_VAL(wasm_foreign_as_ref(foreign));
                wasm_val_t other_value = WASM_REF_VAL(wasm_foreign_as_ref(other_foreign));

                wasm_globaltype_t* global_type =
                    wasm_globaltype_new(wasm_valtype_new_externref(), WASM_VAR);

                // A global cannot start with a reference of another store.
                assert(wasm_global_new(store, global_type, &other_value) == NULL);
                assert_refused();

                wasm_global_t* global = wasm_global_new(store, global_type, &value);
                assert(global);
                assert(wasmer_last_error_length() == 0);

                // Nor can it be set to one. It keeps its value.
                wasm_global_set(global, &other_value);
                assert_refused();

                wasm_val_t current;
                wasm_global_get(global, &current);
                assert(current.kind == WASM_EXTERNREF);
                assert(wasm_ref_same(current.of.ref, value.of.ref));

                wasm_val_delete(&current);
                wasm_global_delete(global);
                wasm_globaltype_delete(global_type);
                wasm_foreign_delete(other_foreign);
                wasm_foreign_delete(foreign);
                wasm_store_delete(other_store);
                wasm_store_delete(store);
                wasm_engine_delete(engine);

                return 0;
            }
        })
        .success();
    }

    #[allow(
        unexpected_cfgs,
        reason = "tools like cargo-llvm-coverage pass --cfg coverage"
    )]
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[test]
    fn test_set_host_global_immutable() {
        (assert_c! {
            #include "tests/wasmer.h"

            int main() {
                wasm_engine_t* engine = wasm_engine_new();
                wasm_store_t* store = wasm_store_new(engine);

                wasm_val_t forty_two = WASM_F32_VAL(42);
                wasm_val_t forty_three = WASM_F32_VAL(43);

                wasm_valtype_t* valtype = wasm_valtype_new_i32();
                wasm_globaltype_t* global_type = wasm_globaltype_new(valtype, WASM_CONST);
                wasm_global_t* global = wasm_global_new(store, global_type, &forty_two);

                wasm_globaltype_delete(global_type);

                wasm_global_set(global, &forty_three);

                assert(wasmer_last_error_length() > 0);

                wasm_global_delete(global);
                wasm_store_delete(store);
                wasm_engine_delete(engine);

                return 0;
            }
        })
        .success();
    }

    #[allow(
        unexpected_cfgs,
        reason = "tools like cargo-llvm-coverage pass --cfg coverage"
    )]
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[test]
    fn test_set_guest_global_immutable() {
        (assert_c! {
            #include "tests/wasmer.h"

            int main() {
                wasm_engine_t* engine = wasm_engine_new();
                wasm_store_t* store = wasm_store_new(engine);

                wasm_byte_vec_t wat;
                wasmer_byte_vec_new_from_string(&wat, "(module (global $global (export \"global\") f32 (f32.const 1)))");
                wasm_byte_vec_t wasm_bytes;
                wat2wasm(&wat, &wasm_bytes);
                wasm_module_t* module = wasm_module_new(store, &wasm_bytes);
                wasm_extern_vec_t import_object = WASM_EMPTY_VEC;
                wasm_instance_t* instance = wasm_instance_new(store, module, &import_object, NULL);

                wasm_extern_vec_t exports;
                wasm_instance_exports(instance, &exports);
                wasm_global_t* global = wasm_extern_as_global(exports.data[0]);

                wasm_val_t forty_two = WASM_F32_VAL(42);
                wasm_global_set(global, &forty_two);

                printf("%d", wasmer_last_error_length());
                assert(wasmer_last_error_length() > 0);

                wasm_instance_delete(instance);
                wasm_byte_vec_delete(&wasm_bytes);
                wasm_byte_vec_delete(&wat);
                wasm_extern_vec_delete(&exports);
                wasm_store_delete(store);
                wasm_engine_delete(engine);

                return 0;
            }
        })
        .success();
    }
}

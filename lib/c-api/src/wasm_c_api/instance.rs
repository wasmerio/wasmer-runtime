use super::externals::{wasm_extern_t, wasm_extern_vec_t};
use super::module::wasm_module_t;
use super::store::{StoreRef, wasm_store_t};
use super::trap::wasm_trap_t;
use wasmer_api::{Extern, Instance, InstantiationError, Module, StoreMut};

/// Opaque type representing a WebAssembly instance.
#[allow(non_camel_case_types)]
pub struct wasm_instance_t {
    pub store: StoreRef,
    pub inner: Instance,
}

/// Lets the origin of each import bind the memory the instance will import,
/// before its start function can run.
#[cfg(feature = "extensions")]
fn prebind_memory(
    store: &mut StoreMut<'_>,
    imports: &[Option<Box<wasm_extern_t>>],
    externs: &[Extern],
) {
    let Some(memory) = externs.iter().find_map(|value| match value {
        Extern::Memory(memory) => Some(memory),
        _ => None,
    }) else {
        return;
    };
    for import in imports.iter().flatten() {
        if let Some(origin) = &import.origin {
            origin.prebind_memory(store, memory);
        }
    }
}

/// Instantiates `module` with `externs`, the values of `imports`.
///
/// The error is boxed because it is large.
fn instantiate(
    store: &mut StoreMut<'_>,
    module: &Module,
    #[allow(unused_variables)] imports: &[Option<Box<wasm_extern_t>>],
    externs: &[Extern],
) -> Result<Instance, Box<InstantiationError>> {
    // The Rust API refuses an import of another store below, but that is
    // after the import origins would have been handed its memory.
    #[cfg(feature = "extensions")]
    {
        if externs.iter().any(|value| !value.is_from_store(store)) {
            return Err(Box::new(InstantiationError::DifferentStores));
        }
        prebind_memory(store, imports, externs);
    }
    Instance::new_by_index(store, module, externs).map_err(Box::new)
}

/// Creates a new instance from a WebAssembly module and a
/// set of imports.
///
/// ## Errors
///
/// The function can fail in 2 ways:
///
/// 1. Link errors that happen when plugging the imports into the
///    instance,
/// 2. Runtime errors that happen when running the module `start`
///    function.
///
/// The failure is stored in the `trap` argument; the program doesn't
/// panic.
///
/// # Notes
///
/// The `store` argument is ignored. The store from the given module
/// will be used.
///
/// # Example
///
/// See the module's documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_instance_new(
    store: Option<&mut wasm_store_t>,
    module: Option<&wasm_module_t>,
    imports: Option<&wasm_extern_vec_t>,
    trap: Option<&mut *mut wasm_trap_t>,
) -> Option<Box<wasm_instance_t>> {
    let store = store?;
    let module = module?;
    let imports = imports?;

    let wasm_module = &module.inner;
    let module_imports = wasm_module.imports();
    let module_import_count = module_imports.len();
    let imports = &imports.as_slice()[..module_import_count.min(imports.as_slice().len())];
    let externs = imports
        .iter()
        .map(|imp| Extern::from(imp.as_ref().unwrap().as_ref().clone()))
        .collect::<Vec<Extern>>();

    let created = c_try!(store.inner.with_mut(|mut store_mut| instantiate(
        &mut store_mut,
        wasm_module,
        imports,
        &externs
    )));
    let instance = match created.map_err(|error| *error) {
        Ok(instance) => instance,

        Err(InstantiationError::Link(link_error)) => {
            crate::error::update_last_error(link_error);

            return None;
        }

        Err(InstantiationError::Start(runtime_error)) => {
            if let Some(trap) = trap {
                let this_trap: Box<wasm_trap_t> = Box::new(runtime_error.into());
                *trap = Box::into_raw(this_trap);
            }

            return None;
        }

        Err(e @ InstantiationError::CpuFeature(_)) => {
            crate::error::update_last_error(e);

            return None;
        }

        Err(e @ InstantiationError::DifferentStores) => {
            crate::error::update_last_error(e);

            return None;
        }

        Err(e @ InstantiationError::DifferentArchOS) => {
            crate::error::update_last_error(e);

            return None;
        }
    };

    Some(Box::new(wasm_instance_t {
        store: store.inner.clone(),
        inner: instance,
    }))
}

/// Deletes an instance.
///
/// # Example
///
/// See [`wasm_instance_new`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_instance_delete(_instance: Option<Box<wasm_instance_t>>) {}

/// Gets the exports of the instance.
///
/// # Example
///
/// ```rust
/// # use inline_c::assert_c;
/// # fn main() {
/// #    (assert_c! {
/// # #include "tests/wasmer.h"
/// #
/// int main() {
///     // Create the engine and the store.
///     wasm_engine_t* engine = wasm_engine_new();
///     wasm_store_t* store = wasm_store_new(engine);
///
///     // Create a WebAssembly module from a WAT definition.
///     wasm_byte_vec_t wat;
///     wasmer_byte_vec_new_from_string(
///         &wat,
///         "(module\n"
///         "  (func (export \"function\") (param i32 i64))\n"
///         "  (global (export \"global\") i32 (i32.const 7))\n"
///         "  (table (export \"table\") 0 funcref)\n"
///         "  (memory (export \"memory\") 1))"
///     );
///     wasm_byte_vec_t wasm;
///     wat2wasm(&wat, &wasm);
///
///     // Create the module.
///     wasm_module_t* module = wasm_module_new(store, &wasm);
///
///     // Instantiate the module.
///     wasm_extern_vec_t imports = WASM_EMPTY_VEC;
///     wasm_trap_t* trap = NULL;
///
///     wasm_instance_t* instance = wasm_instance_new(store, module, &imports, &trap);
///     assert(instance);
///
///     // Read the exports.
///     wasm_extern_vec_t exports;
///     wasm_instance_exports(instance, &exports);
///
///     // We have 4 of them.
///     assert(exports.size == 4);
///
///     // The first one is a function. Use `wasm_extern_as_func`
///     // to go further.
///     assert(wasm_extern_kind(exports.data[0]) == WASM_EXTERN_FUNC);
///
///     // The second one is a global. Use `wasm_extern_as_global` to
///     // go further.
///     assert(wasm_extern_kind(exports.data[1]) == WASM_EXTERN_GLOBAL);
///
///     // The third one is a table. Use `wasm_extern_as_table` to
///     // go further.
///     assert(wasm_extern_kind(exports.data[2]) == WASM_EXTERN_TABLE);
///
///     // The fourth one is a memory. Use `wasm_extern_as_memory` to
///     // go further.
///     assert(wasm_extern_kind(exports.data[3]) == WASM_EXTERN_MEMORY);
///
///     // Free everything.
///     wasm_extern_vec_delete(&exports);
///     wasm_instance_delete(instance);
///     wasm_module_delete(module);
///     wasm_byte_vec_delete(&wasm);
///     wasm_byte_vec_delete(&wat);
///     wasm_store_delete(store);
///     wasm_engine_delete(engine);
///
///     return 0;
/// }
/// #    })
/// #    .success();
/// # }
/// ```
///
/// To go further:
///
/// * [`wasm_extern_as_func`][super::externals::wasm_extern_as_func],
/// * [`wasm_extern_as_global`][super::externals::wasm_extern_as_global],
/// * [`wasm_extern_as_table`][super::externals::wasm_extern_as_table],
/// * [`wasm_extern_as_memory`][super::externals::wasm_extern_as_memory].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_instance_exports(
    instance: &wasm_instance_t,
    // own
    out: &mut wasm_extern_vec_t,
) {
    let original_instance = instance;
    let instance = &instance.inner;
    let extern_vec: Vec<Option<Box<wasm_extern_t>>> = instance
        .exports
        .iter()
        .map(|(_name, r#extern)| {
            Some(Box::new(wasm_extern_t::new(
                original_instance.store.clone(),
                r#extern.clone(),
            )))
        })
        .collect();
    out.set_buffer(extern_vec);
}

#[cfg(test)]
mod tests {
    use inline_c::assert_c;

    #[cfg(feature = "extensions")]
    #[test]
    fn import_origin_binds_its_memory_before_the_start_function() {
        use super::{wasm_instance_delete, wasm_instance_new};
        use crate::wasm_c_api::{
            engine::{wasm_engine_delete, wasm_engine_new},
            externals::{ExternOrigin, wasm_extern_t, wasm_extern_vec_t},
            module::wasm_module_t,
            store::{wasm_store_delete, wasm_store_new},
        };
        use std::{
            rc::Rc,
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            },
        };
        use wasmer_api::{Extern, Function, Memory, MemoryType, Module, StoreMut};

        struct Origin {
            memory: Memory,
            bound: Arc<AtomicBool>,
        }
        impl ExternOrigin for Origin {
            fn prebind_memory(&self, _: &mut StoreMut<'_>, memory: &Memory) {
                assert!(*memory == self.memory);
                self.bound.store(true, Ordering::SeqCst);
            }
        }

        let engine = wasm_engine_new();
        let mut store = unsafe { wasm_store_new(Some(&engine)) }.unwrap();
        let wasm = wat::parse_str(
            r#"(module
                (import "env" "start" (func $start))
                (import "env" "memory" (memory 1))
                (func $run (call $start))
                (start $run))"#,
        )
        .unwrap();
        let bound = Arc::new(AtomicBool::new(false));
        let bound_at_start = Arc::new(AtomicBool::new(false));
        let (seen, record) = (bound.clone(), bound_at_start.clone());
        let (module, function, memory) = store
            .inner
            .with_mut(|mut raw| {
                let module = Module::new(&raw, wasm).unwrap();
                let function = Function::new_typed(&mut raw, move || {
                    record.store(seen.load(Ordering::SeqCst), Ordering::SeqCst);
                });
                let memory = Memory::new(&mut raw, MemoryType::new(1, None, false)).unwrap();
                (module, function, memory)
            })
            .unwrap();
        let module = wasm_module_t { inner: module };
        let mut import = wasm_extern_t::new(store.inner.clone(), Extern::Function(function));
        import.origin = Some(Rc::new(Origin {
            memory: memory.clone(),
            bound: bound.clone(),
        }));
        let imports: wasm_extern_vec_t = vec![
            Some(Box::new(import)),
            Some(Box::new(wasm_extern_t::new(
                store.inner.clone(),
                Extern::Memory(memory),
            ))),
        ]
        .into();

        let instance =
            unsafe { wasm_instance_new(Some(&mut store), Some(&module), Some(&imports), None) };

        assert!(instance.is_some());
        assert!(bound_at_start.load(Ordering::SeqCst));
        unsafe {
            wasm_instance_delete(instance);
            drop(imports);
            drop(module);
            wasm_store_delete(Some(store));
            wasm_engine_delete(Some(engine));
        }
    }

    #[cfg(feature = "extensions")]
    #[test]
    fn import_origin_is_not_given_a_memory_of_another_store() {
        use super::wasm_instance_new;
        use crate::error::take_last_error;
        use crate::wasm_c_api::{
            engine::wasm_engine_new,
            externals::{ExternOrigin, wasm_extern_t, wasm_extern_vec_t},
            module::wasm_module_t,
            store::wasm_store_new,
        };
        use std::{
            rc::Rc,
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            },
        };
        use wasmer_api::{
            Extern, Function, InstantiationError, Memory, MemoryType, Module, StoreMut,
        };

        struct Origin(Arc<AtomicBool>);
        impl ExternOrigin for Origin {
            fn prebind_memory(&self, _: &mut StoreMut<'_>, _: &Memory) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let engine = wasm_engine_new();
        let mut store = unsafe { wasm_store_new(Some(&engine)) }.unwrap();
        let wasm = wat::parse_str(
            r#"(module
                (import "env" "function" (func))
                (import "env" "memory" (memory 1)))"#,
        )
        .unwrap();
        let (module, function) = store
            .inner
            .with_mut(|mut raw| {
                let module = Module::new(&raw, wasm).unwrap();
                (module, Function::new_typed(&mut raw, || {}))
            })
            .unwrap();
        let module = wasm_module_t { inner: module };
        let other_store = unsafe { wasm_store_new(Some(&engine)) }.unwrap();
        let other_memory = other_store
            .inner
            .with_mut(|mut raw| Memory::new(&mut raw, MemoryType::new(1, None, false)))
            .unwrap()
            .unwrap();

        let given = Arc::new(AtomicBool::new(false));
        let mut import = wasm_extern_t::new(store.inner.clone(), Extern::Function(function));
        import.origin = Some(Rc::new(Origin(given.clone())));
        let imports: wasm_extern_vec_t = vec![
            Some(Box::new(import)),
            Some(Box::new(wasm_extern_t::new(
                other_store.inner.clone(),
                Extern::Memory(other_memory),
            ))),
        ]
        .into();

        let instance =
            unsafe { wasm_instance_new(Some(&mut store), Some(&module), Some(&imports), None) };

        assert!(instance.is_none());
        assert_eq!(
            take_last_error(),
            Some(InstantiationError::DifferentStores.to_string())
        );
        assert!(!given.load(Ordering::SeqCst));
    }

    #[allow(
        unexpected_cfgs,
        reason = "tools like cargo-llvm-coverage pass --cfg coverage"
    )]
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[test]
    fn test_instance_new() {
        (assert_c! {
            #include "tests/wasmer.h"

            // The `sum` host function implementation.
            wasm_trap_t* sum_callback(
                const wasm_val_vec_t* arguments,
                wasm_val_vec_t* results
            ) {
                wasm_val_t sum = {
                    .kind = WASM_I32,
                    .of = { arguments->data[0].of.i32 + arguments->data[1].of.i32 },
                };
                results->data[0] = sum;

                return NULL;
            }

            int main() {
                // Create the engine and the store.
                wasm_engine_t* engine = wasm_engine_new();
                wasm_store_t* store = wasm_store_new(engine);

                // Create a WebAssembly module from a WAT definition.
                wasm_byte_vec_t wat;
                wasmer_byte_vec_new_from_string(
                    &wat,
                    "(module\n"
                    "  (import \"math\" \"sum\" (func $sum (param i32 i32) (result i32)))\n"
                    "  (func (export \"add_one\") (param i32) (result i32)\n"
                    "    local.get 0\n"
                    "    i32.const 1\n"
                    "    call $sum))"
                );
                wasm_byte_vec_t wasm;
                wat2wasm(&wat, &wasm);

                // Create the module.
                wasm_module_t* module = wasm_module_new(store, &wasm);

                assert(module);

                // Prepare the imports.
                wasm_functype_t* sum_type = wasm_functype_new_2_1(
                    wasm_valtype_new_i32(),
                    wasm_valtype_new_i32(),
                    wasm_valtype_new_i32()
                );
                wasm_func_t* sum_function = wasm_func_new(store, sum_type, sum_callback);
                wasm_extern_t* externs[] = { wasm_func_as_extern(sum_function) };
                wasm_extern_vec_t imports = WASM_ARRAY_VEC(externs);

                // Instantiate the module.
                wasm_trap_t* trap = NULL;
                wasm_instance_t* instance = wasm_instance_new(store, module, &imports, &trap);

                assert(instance);

                // Run the exported function.
                wasm_extern_vec_t exports;
                wasm_instance_exports(instance, &exports);

                assert(exports.size == 1);

                const wasm_func_t* run_function = wasm_extern_as_func(exports.data[0]);

                assert(run_function);

                wasm_val_t arguments[1] = { WASM_I32_VAL(1) };
                wasm_val_t results[1] = { WASM_INIT_VAL };

                wasm_val_vec_t arguments_as_array = WASM_ARRAY_VEC(arguments);
                wasm_val_vec_t results_as_array = WASM_ARRAY_VEC(results);

                trap = wasm_func_call(run_function, &arguments_as_array, &results_as_array);

                assert(trap == NULL);
                assert(results[0].of.i32 == 2);

                // Free everything.
                wasm_extern_vec_delete(&exports);
                wasm_instance_delete(instance);
                wasm_func_delete(sum_function);
                wasm_functype_delete(sum_type);
                wasm_module_delete(module);
                wasm_byte_vec_delete(&wasm);
                wasm_byte_vec_delete(&wat);
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
    fn test_instance_refuses_an_import_of_another_store() {
        (assert_c! {
            #include "tests/wasmer.h"

            static int host_calls = 0;

            wasm_trap_t* host_callback(
                const wasm_val_vec_t* arguments,
                wasm_val_vec_t* results
            ) {
                (void) arguments;
                (void) results;
                host_calls++;

                return NULL;
            }

            int main() {
                wasm_engine_t* engine = wasm_engine_new();
                wasm_store_t* store = wasm_store_new(engine);
                wasm_store_t* other_store = wasm_store_new(engine);

                wasm_byte_vec_t wat;
                wasmer_byte_vec_new_from_string(
                    &wat,
                    "(module\n"
                    "  (import \"env\" \"host\" (func $host))\n"
                    "  (func $run (call $host))\n"
                    "  (start $run))"
                );
                wasm_byte_vec_t wasm;
                wat2wasm(&wat, &wasm);

                wasm_module_t* module = wasm_module_new(store, &wasm);
                assert(module);

                // The import is a function of the other store.
                wasm_functype_t* host_type = wasm_functype_new_0_0();
                wasm_func_t* other_host = wasm_func_new(other_store, host_type, host_callback);
                wasm_extern_t* externs[] = { wasm_func_as_extern(other_host) };
                wasm_extern_vec_t imports = WASM_ARRAY_VEC(externs);

                wasm_trap_t* trap = NULL;
                wasm_instance_t* instance = wasm_instance_new(store, module, &imports, &trap);

                // It is a link error, and the start function did not run.
                assert(instance == NULL);
                assert(trap == NULL);
                assert(wasmer_last_error_length() > 0);
                assert(host_calls == 0);

                wasm_func_delete(other_host);
                wasm_functype_delete(host_type);
                wasm_module_delete(module);
                wasm_byte_vec_delete(&wasm);
                wasm_byte_vec_delete(&wat);
                wasm_store_delete(other_store);
                wasm_store_delete(store);
                wasm_engine_delete(engine);

                return 0;
            }
        })
        .success();
    }
}

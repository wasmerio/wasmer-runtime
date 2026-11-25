mod function;
mod global;
mod memory;
mod table;

use super::store::{StoreBusy, StoreRef};
// use super::types::{wasm_externkind_enum, wasm_externkind_t};
pub use function::*;
pub use global::*;
pub use memory::*;
pub use table::*;
use wasmer_api::{Extern, ExternType, Function, Global, Memory, Table};

/// The extension that created an import, such as a WASI environment.
#[cfg(feature = "extensions")]
pub trait ExternOrigin {
    /// Binds the memory the instance will import before it is created.
    fn prebind_memory(&self, store: &mut wasmer_api::StoreMut<'_>, memory: &Memory);
}

#[allow(non_camel_case_types)]
#[derive(Clone)]
pub struct wasm_extern_t {
    pub inner: Extern,
    pub store: StoreRef,
    #[cfg(feature = "extensions")]
    pub origin: Option<std::rc::Rc<dyn ExternOrigin>>,
}

impl wasm_extern_t {
    pub fn new(store: StoreRef, inner: Extern) -> Self {
        Self {
            inner,
            store,
            #[cfg(feature = "extensions")]
            origin: None,
        }
    }

    pub fn global(&self) -> Global {
        match &self.inner {
            Extern::Global(g) => g.clone(),
            _ => unsafe { std::hint::unreachable_unchecked() },
        }
    }

    pub fn function(&self) -> Function {
        match &self.inner {
            Extern::Function(f) => f.clone(),
            _ => unsafe { std::hint::unreachable_unchecked() },
        }
    }

    pub fn table(&self) -> Table {
        match &self.inner {
            Extern::Table(t) => t.clone(),
            _ => unsafe { std::hint::unreachable_unchecked() },
        }
    }

    pub fn memory(&self) -> Memory {
        match &self.inner {
            Extern::Memory(m) => m.clone(),
            _ => unsafe { std::hint::unreachable_unchecked() },
        }
    }
}

// #[unsafe(no_mangle)]
// pub extern "C" fn wasm_extern_kind(e: &wasm_extern_t) -> wasm_externkind_t {
//     (match e.inner {
//         Extern::Function(_) => wasm_externkind_enum::WASM_EXTERN_FUNC,
//         Extern::Table(_) => wasm_externkind_enum::WASM_EXTERN_TABLE,
//         Extern::Global(_) => wasm_externkind_enum::WASM_EXTERN_GLOBAL,
//         Extern::Memory(_) => wasm_externkind_enum::WASM_EXTERN_MEMORY,
//     }) as wasm_externkind_t
// }

impl wasm_extern_t {
    pub(crate) fn ty(&self) -> Result<ExternType, StoreBusy> {
        self.store.with(|store| self.inner.ty(&store))
    }
}

impl From<wasm_extern_t> for Extern {
    fn from(other: wasm_extern_t) -> Self {
        other.inner
    }
}

wasm_declare_boxed_vec!(extern);

/// Copy a `wasm_extern_t`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_extern_copy(r#extern: &wasm_extern_t) -> Box<wasm_extern_t> {
    Box::new(r#extern.clone())
}

/// Delete an extern.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_extern_delete(_extern: Option<Box<wasm_extern_t>>) {}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_func_as_extern(func: Option<&wasm_func_t>) -> Option<&wasm_extern_t> {
    Some(&func?.extern_)
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_global_as_extern(global: Option<&wasm_global_t>) -> Option<&wasm_extern_t> {
    Some(&global?.extern_)
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_memory_as_extern(memory: Option<&wasm_memory_t>) -> Option<&wasm_extern_t> {
    Some(&memory?.extern_)
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_table_as_extern(table: Option<&wasm_table_t>) -> Option<&wasm_extern_t> {
    Some(&table?.extern_)
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_extern_as_func(r#extern: Option<&wasm_extern_t>) -> Option<&wasm_func_t> {
    wasm_func_t::try_from(r#extern?)
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_extern_as_global(
    r#extern: Option<&wasm_extern_t>,
) -> Option<&wasm_global_t> {
    wasm_global_t::try_from(r#extern?)
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_extern_as_memory(
    r#extern: Option<&wasm_extern_t>,
) -> Option<&wasm_memory_t> {
    wasm_memory_t::try_from(r#extern?)
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_extern_as_table(r#extern: Option<&wasm_extern_t>) -> Option<&wasm_table_t> {
    wasm_table_t::try_from(r#extern?)
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
    fn test_externs_refuse_a_reference_of_another_store() {
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

                wasm_foreign_t* other_foreign = wasm_foreign_new(other_store);
                wasm_ref_t* other_ref = wasm_foreign_as_ref(other_foreign);

                // A table takes no element of another store: not as its
                // initial value, not when set, and not when grown.
                wasm_limits_t limits = { 1, 4 };
                wasm_tabletype_t* table_type =
                    wasm_tabletype_new(wasm_valtype_new_externref(), &limits);

                assert(wasm_table_new(store, table_type, other_ref) == NULL);
                assert_refused();

                wasm_table_t* table = wasm_table_new(store, table_type, NULL);
                assert(table);

                assert(!wasm_table_set(table, 0, other_ref));
                assert_refused();
                assert(!wasm_table_grow(table, 1, other_ref));
                assert_refused();
                assert(wasm_table_size(table) == 1);
                assert(wasm_table_get(table, 0) == NULL);

                // A function takes no argument of another store.
                wasm_functype_t* take_type = wasm_functype_new_1_0(wasm_valtype_new_externref());
                wasm_func_t* take = wasm_func_new(store, take_type, host_callback);
                assert(take);

                wasm_val_t arguments[1] = { WASM_REF_VAL(other_ref) };
                wasm_val_vec_t arguments_as_array = WASM_ARRAY_VEC(arguments);
                wasm_val_vec_t results_as_array = WASM_EMPTY_VEC;

                wasm_trap_t* trap = wasm_func_call(take, &arguments_as_array, &results_as_array);
                assert(trap != NULL);
                assert(host_calls == 0);

                wasm_trap_delete(trap);
                wasm_func_delete(take);
                wasm_functype_delete(take_type);
                wasm_table_delete(table);
                wasm_tabletype_delete(table_type);
                wasm_foreign_delete(other_foreign);
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
    fn test_extern_copy() {
        (assert_c! {
            #include "tests/wasmer.h"

            int main() {
                wasm_engine_t* engine = wasm_engine_new();
                wasm_store_t* store = wasm_store_new(engine);

                wasm_byte_vec_t wat;
                wasmer_byte_vec_new_from_string(
                    &wat,
                    "(module\n"
                    "  (func (export \"function\")))"
                );
                wasm_byte_vec_t wasm;
                wat2wasm(&wat, &wasm);

                wasm_module_t* module = wasm_module_new(store, &wasm);
                assert(module);

                wasm_extern_vec_t imports = WASM_EMPTY_VEC;
                wasm_trap_t* trap = NULL;

                wasm_instance_t* instance = wasm_instance_new(store, module, &imports, &trap);
                assert(instance);

                wasm_extern_vec_t exports;
                wasm_instance_exports(instance, &exports);

                assert(exports.size == 1);

                wasm_extern_t* function = exports.data[0];
                assert(wasm_extern_kind(function) == WASM_EXTERN_FUNC);

                wasm_extern_t* function_copy = wasm_extern_copy(function);
                assert(wasm_extern_kind(function_copy) == WASM_EXTERN_FUNC);

                wasm_extern_delete(function_copy);
                wasm_extern_vec_delete(&exports);
                wasm_instance_delete(instance);
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
}

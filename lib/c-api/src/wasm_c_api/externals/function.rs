use super::super::store::{StoreRef, WeakStoreRef, wasm_store_t};
use super::super::trap::wasm_trap_t;
use super::super::types::{wasm_functype_t, wasm_ref_t, wasm_valkind_enum};
use super::super::value::{wasm_val_inner, wasm_val_t, wasm_val_vec_t};
use super::wasm_extern_t;
use crate::error::update_last_error;
use crate::wasm_c_api::function_env::FunctionCEnv;
use libc::c_void;
use std::mem::MaybeUninit;
use std::sync::{Arc, Mutex};
use wasmer_api::{
    AsStoreMut, Extern, Function, FunctionEnv, FunctionEnvMut, FunctionType, RuntimeError,
    StoreMut, Value,
};

#[derive(Clone)]
#[allow(non_camel_case_types)]
#[repr(C)]
pub struct wasm_func_t {
    pub extern_: wasm_extern_t,
}

impl wasm_func_t {
    pub fn try_from(e: &wasm_extern_t) -> Option<&wasm_func_t> {
        match &e.inner {
            Extern::Function(_) => Some(unsafe { &*(e as *const _ as *const _) }),
            _ => None,
        }
    }
}

#[allow(non_camel_case_types)]
pub type wasm_func_callback_t = unsafe extern "C" fn(
    args: &wasm_val_vec_t,
    results: &mut wasm_val_vec_t,
) -> Option<Box<wasm_trap_t>>;

#[allow(non_camel_case_types)]
pub type wasm_func_callback_with_env_t = unsafe extern "C" fn(
    env: *mut c_void,
    args: &wasm_val_vec_t,
    results: &mut wasm_val_vec_t,
) -> Option<Box<wasm_trap_t>>;

#[allow(non_camel_case_types)]
pub type wasm_env_finalizer_t = unsafe extern "C" fn(*mut c_void);

/// Convert host-callback argument [`Value`]s into C `wasm_val_t`s, boxing
/// reference values into `store`.
fn callback_args_to_wasm(args: &[Value], store: &StoreRef) -> Result<wasm_val_vec_t, RuntimeError> {
    let vals = args
        .iter()
        .map(|v| wasm_val_t::from_value(v, store))
        .collect::<Result<Vec<wasm_val_t>, &'static str>>()
        .map_err(RuntimeError::new)?;
    Ok(vals.into())
}

/// Convert the C `wasm_val_t`s produced by a host callback of `store` back
/// into [`Value`]s.
fn callback_results_to_values(
    results: Vec<wasm_val_t>,
    store: &StoreRef,
) -> Result<Vec<Value>, RuntimeError> {
    let values = results
        .into_iter()
        .map(Value::try_from)
        .collect::<Result<Vec<Value>, &'static str>>()
        .map_err(RuntimeError::new)?;
    // The Rust API hands a host function's results to the guest unchecked, so
    // a reference of another store has to be refused here.
    let own = store
        .with(|store| values.iter().all(|value| value.is_from_store(&store)))
        .map_err(|busy| RuntimeError::new(busy.to_string()))?;
    if !own {
        return Err(RuntimeError::new("cross-`Store` values are not supported"));
    }
    Ok(values)
}

/// Runs the C callback of a host function.
///
/// `store_mut` is the borrow the guest's caller gave the host function. It is
/// lent to the store for the whole callback, so that C API calls the callback
/// makes on its own store borrow from it instead of from the store's cell,
/// which that caller still holds.
fn call_host_callback(
    store: &WeakStoreRef,
    store_mut: &mut StoreMut<'_>,
    args: &[Value],
    num_rets: usize,
    callback: impl FnOnce(&wasm_val_vec_t, &mut wasm_val_vec_t) -> Option<Box<wasm_trap_t>>,
) -> Result<Vec<Value>, RuntimeError> {
    let store = store
        .upgrade()
        .ok_or_else(|| RuntimeError::new("store was dropped"))?;
    store.lend(store_mut, || {
        let processed_args = callback_args_to_wasm(args, &store)?;

        let mut results: wasm_val_vec_t = vec![
            wasm_val_t {
                kind: wasm_valkind_enum::WASM_I64 as _,
                of: wasm_val_inner { int64_t: 0 },
            };
            num_rets
        ]
        .into();

        if let Some(trap) = callback(&processed_args, &mut results) {
            return Err(trap.inner);
        }

        callback_results_to_values(results.take(), &store)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_func_new(
    store: Option<&mut wasm_store_t>,
    function_type: Option<&wasm_functype_t>,
    callback: Option<wasm_func_callback_t>,
) -> Option<Box<wasm_func_t>> {
    let function_type = function_type?;
    let callback = callback?;
    let store = store?;
    // Capture a weak store handle (not a strong `StoreRef`) to avoid a
    // store → function → store cycle; upgrade it per call to box ref args.
    let store_weak = store.inner.downgrade();

    let func_sig = &function_type.inner().function_type;
    let num_rets = func_sig.results().len();
    let inner_callback = move |mut env: FunctionEnvMut<'_, FunctionCEnv>,
                               args: &[Value]|
          -> Result<Vec<Value>, RuntimeError> {
        call_host_callback(
            &store_weak,
            &mut env.as_store_mut(),
            args,
            num_rets,
            |args, results| unsafe { callback(args, results) },
        )
    };
    let function = c_try!(store.inner.with_mut(|mut store_mut| {
        let env = FunctionEnv::new(&mut store_mut, FunctionCEnv::default());
        Function::new_with_env(&mut store_mut, &env, func_sig, inner_callback)
    }));
    Some(Box::new(wasm_func_t {
        extern_: wasm_extern_t::new(store.inner.clone(), function.into()),
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_func_new_with_env(
    store: Option<&mut wasm_store_t>,
    function_type: Option<&wasm_functype_t>,
    callback: Option<wasm_func_callback_with_env_t>,
    env: *mut c_void,
    env_finalizer: Option<wasm_env_finalizer_t>,
) -> Option<Box<wasm_func_t>> {
    let function_type = function_type?;
    let callback = callback?;
    let store = store?;
    let store_weak = store.inner.downgrade();

    let func_sig = &function_type.inner().function_type;
    let num_rets = func_sig.results().len();

    #[derive(Clone)]
    #[repr(C)]
    struct WrapperEnv {
        env: FunctionCEnv,
        env_finalizer: Arc<Mutex<Option<wasm_env_finalizer_t>>>,
    }

    // Only relevant when using multiple threads in the C API;
    // Synchronization will be done via the C API / on the C side.
    unsafe impl Send for WrapperEnv {}
    unsafe impl Sync for WrapperEnv {}

    impl Drop for WrapperEnv {
        fn drop(&mut self) {
            if let Ok(mut guard) = self.env_finalizer.lock()
                && Arc::strong_count(&self.env_finalizer) == 1
                && let Some(env_finalizer) = guard.take()
            {
                unsafe { (env_finalizer)(self.env.as_ptr()) };
            }
        }
    }
    let inner_callback = move |mut env: FunctionEnvMut<'_, WrapperEnv>,
                               args: &[Value]|
          -> Result<Vec<Value>, RuntimeError> {
        let c_env = env.data().env.as_ptr();
        call_host_callback(
            &store_weak,
            &mut env.as_store_mut(),
            args,
            num_rets,
            |args, results| unsafe { callback(c_env, args, results) },
        )
    };
    let wrapper_env = WrapperEnv {
        env: FunctionCEnv::new(c_try!(
            std::ptr::NonNull::new(env),
            "Function environment cannot be a null pointer."
        )),
        env_finalizer: Arc::new(Mutex::new(env_finalizer)),
    };
    let function = c_try!(store.inner.with_mut(|mut store_mut| {
        let env = FunctionEnv::new(&mut store_mut, wrapper_env);
        Function::new_with_env(&mut store_mut, &env, func_sig, inner_callback)
    }));
    Some(Box::new(wasm_func_t {
        extern_: wasm_extern_t::new(store.inner.clone(), function.into()),
    }))
}

#[unsafe(no_mangle)]
pub extern "C" fn wasm_func_copy(func: &wasm_func_t) -> Box<wasm_func_t> {
    Box::new(func.clone())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_func_delete(_func: Option<Box<wasm_func_t>>) {}

// A `funcref` view of a function, and back. Per `wasm.h` these are non-owning
// views; since our `wasm_ref_t` is a distinct allocation the returned ref is
// typically never freed and leaks. It holds only a weak store handle, so it
// does not pin the store.
//
// NOTE: storing the resulting funcref into a table or global works only for
// *static* functions. Dynamic host functions (created via `wasm_func_new`) have
// no funcref representation in the sys VM and will abort on `table.set` — a
// separate VM limitation, not something this shim can work around.

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_func_as_ref(
    func: Option<&mut wasm_func_t>,
) -> Option<Box<wasm_ref_t>> {
    let func = func?;
    wasm_ref_t::new(
        func.extern_.store.clone(),
        Value::FuncRef(Some(func.extern_.function())),
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_func_as_ref_const(
    func: Option<&wasm_func_t>,
) -> Option<Box<wasm_ref_t>> {
    let func = func?;
    wasm_ref_t::new(
        func.extern_.store.clone(),
        Value::FuncRef(Some(func.extern_.function())),
    )
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_ref_as_func(
    ref_: Option<&mut wasm_ref_t>,
) -> Option<Box<wasm_func_t>> {
    let ref_ = ref_?;
    let func = match &ref_.inner {
        Value::FuncRef(Some(f)) => f.clone(),
        _ => return None,
    };
    let store = ref_.store.upgrade()?;
    Some(Box::new(wasm_func_t {
        extern_: wasm_extern_t::new(store, func.into()),
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_ref_as_func_const(
    ref_: Option<&wasm_ref_t>,
) -> Option<Box<wasm_func_t>> {
    let ref_ = ref_?;
    let func = match &ref_.inner {
        Value::FuncRef(Some(f)) => f.clone(),
        _ => return None,
    };
    let store = ref_.store.upgrade()?;
    Some(Box::new(wasm_func_t {
        extern_: wasm_extern_t::new(store, func.into()),
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_func_call(
    func: Option<&mut wasm_func_t>,
    args: Option<&wasm_val_vec_t>,
    results: &mut wasm_val_vec_t,
) -> Option<Box<wasm_trap_t>> {
    let func = func?;
    let args = args?;
    // Taken out of `func` first: a host function that the call reaches may
    // delete it.
    let store = func.extern_.store.clone();
    let function = func.extern_.function();
    // Convert by reference (not `.cloned()`): a shallow clone of a ref-carrying
    // `wasm_val_t` would double-free the boxed `wasm_ref_t`.
    let params = c_try!(
        args.as_slice()
            .iter()
            .map(Value::try_from)
            .collect::<Result<Vec<Value>, _>>()
    );

    // The results are converted once the store is released: boxing a
    // reference borrows it again.
    let call = || {
        store
            .with_mut(|mut store_mut| function.call(&mut store_mut, &params))
            .unwrap_or_else(|busy| Err(RuntimeError::new(busy.to_string())))
    };
    #[cfg(not(feature = "extensions-async"))]
    let outcome = call();
    // An extension of the store may run the call itself.
    #[cfg(feature = "extensions-async")]
    let outcome = store.call_hooked(&function, &params, call);

    match outcome {
        Ok(wasm_results) => {
            for (slot, val) in results
                .as_uninit_slice()
                .iter_mut()
                .zip(wasm_results.iter())
            {
                let converted = c_try!(wasm_val_t::from_value(val, &store));
                *slot = MaybeUninit::new(converted);
            }

            None
        }
        Err(e) => Some(Box::new(e.into())),
    }
}

/// The type of `func`. `None`, with the error registered, if its store cannot
/// be borrowed.
fn func_type(func: &wasm_func_t) -> Option<FunctionType> {
    let function = func.extern_.function();
    Some(c_try!(func.extern_.store.with(|store| function.ty(&store))))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_func_param_arity(func: Option<&wasm_func_t>) -> usize {
    let Some(func) = func else {
        update_last_error("func pointer is null");
        return 0;
    };
    func_type(func).map_or(0, |ty| ty.params().len())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_func_result_arity(func: Option<&wasm_func_t>) -> usize {
    let Some(func) = func else {
        update_last_error("func pointer is null");
        return 0;
    };
    func_type(func).map_or(0, |ty| ty.results().len())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_func_type(
    func: Option<&wasm_func_t>,
) -> Option<Box<wasm_functype_t>> {
    Some(Box::new(wasm_functype_t::new(func_type(func?)?)))
}

#[cfg(test)]
mod tests {
    use inline_c::assert_c;

    #[allow(
        unexpected_cfgs,
        reason = "tools like cargo-llvm-coverage pass --cfg coverage"
    )]
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[test]
    fn test_host_function_reenters_its_store() {
        (assert_c! {
            #include "tests/wasmer.h"

            static wasm_memory_t* memory;
            static wasm_global_t* counter;
            static wasm_func_t* twice;
            static wasm_func_t* run;
            static int host_calls = 0;

            // Calls a function from an `i32` to an `i32`.
            static int32_t call(wasm_func_t* function, int32_t argument) {
                wasm_val_t arguments[1] = { WASM_I32_VAL(argument) };
                wasm_val_t results[1] = { WASM_INIT_VAL };
                wasm_val_vec_t arguments_as_array = WASM_ARRAY_VEC(arguments);
                wasm_val_vec_t results_as_array = WASM_ARRAY_VEC(results);

                wasm_trap_t* trap = wasm_func_call(function, &arguments_as_array, &results_as_array);

                assert(trap == NULL);
                assert(results[0].kind == WASM_I32);

                return results[0].of.i32;
            }

            // The `host` import. `run` calls it while `wasm_func_call` is
            // using the store, and everything it does uses that same store.
            wasm_trap_t* host_callback(
                const wasm_val_vec_t* arguments,
                wasm_val_vec_t* results
            ) {
                int32_t depth = arguments->data[0].of.i32;
                host_calls++;

                // The memory has what `run` stored before it called.
                assert(wasm_memory_size(memory) == 1);
                assert(wasm_memory_data_size(memory) == 65536);
                byte_t* data = wasm_memory_data(memory);
                assert(data != NULL);
                assert(data[0] == depth);
                data[1]++;

                wasm_val_t count;
                wasm_global_get(counter, &count);
                assert(count.kind == WASM_I32);
                wasm_val_t next = WASM_I32_VAL(count.of.i32 + 1);
                wasm_global_set(counter, &next);
                wasm_global_get(counter, &count);
                assert(count.of.i32 == next.of.i32);

                // Enter the guest again: back through this import while
                // there is depth left, and into a plain function after that.
                int32_t result;
                if (depth > 0) {
                    result = call(run, depth - 1) + 1;
                    // The innermost call grew the memory, and its `run` was
                    // the last one to store its depth.
                    assert(wasm_memory_size(memory) == 2);
                    assert(wasm_memory_data(memory)[0] == 0);
                } else {
                    result = call(twice, 21);
                    assert(wasm_memory_grow(memory, 1));
                    assert(wasm_memory_size(memory) == 2);
                }

                assert(wasmer_last_error_length() == 0);

                wasm_val_t value = WASM_I32_VAL(result);
                results->data[0] = value;

                return NULL;
            }

            int main() {
                wasm_engine_t* engine = wasm_engine_new();
                wasm_store_t* store = wasm_store_new(engine);

                wasm_byte_vec_t wat;
                wasmer_byte_vec_new_from_string(
                    &wat,
                    "(module\n"
                    "  (import \"env\" \"host\" (func $host (param i32) (result i32)))\n"
                    "  (memory $memory 1)\n"
                    "  (global $counter (mut i32) (i32.const 7))\n"
                    "  (func $twice (param i32) (result i32)\n"
                    "    (i32.mul (local.get 0) (i32.const 2)))\n"
                    "  (func $run (param i32) (result i32)\n"
                    "    (i32.store8 (i32.const 0) (local.get 0))\n"
                    "    (call $host (local.get 0)))\n"
                    "  (export \"memory\" (memory $memory))\n"
                    "  (export \"counter\" (global $counter))\n"
                    "  (export \"twice\" (func $twice))\n"
                    "  (export \"run\" (func $run)))"
                );
                wasm_byte_vec_t wasm;
                wat2wasm(&wat, &wasm);

                wasm_module_t* module = wasm_module_new(store, &wasm);
                assert(module);

                wasm_functype_t* host_type = wasm_functype_new_1_1(
                    wasm_valtype_new_i32(),
                    wasm_valtype_new_i32()
                );
                wasm_func_t* host_function = wasm_func_new(store, host_type, host_callback);
                wasm_extern_t* externs[] = { wasm_func_as_extern(host_function) };
                wasm_extern_vec_t imports = WASM_ARRAY_VEC(externs);

                wasm_trap_t* trap = NULL;
                wasm_instance_t* instance = wasm_instance_new(store, module, &imports, &trap);
                assert(instance);

                wasm_extern_vec_t exports;
                wasm_instance_exports(instance, &exports);
                assert(exports.size == 4);

                memory = wasm_extern_as_memory(exports.data[0]);
                counter = wasm_extern_as_global(exports.data[1]);
                twice = wasm_extern_as_func(exports.data[2]);
                run = wasm_extern_as_func(exports.data[3]);
                assert(memory && counter && twice && run);

                // The import runs at depths 2, 1 and 0. The innermost one
                // returns `twice(21)` and each of the others adds one.
                assert(call(run, 2) == 44);
                assert(host_calls == 3);

                // What the callbacks did to the store is there afterwards.
                wasm_val_t count;
                wasm_global_get(counter, &count);
                assert(count.of.i32 == 10);
                assert(wasm_memory_size(memory) == 2);
                assert(wasm_memory_data(memory)[1] == 3);
                assert(wasmer_last_error_length() == 0);

                wasm_extern_vec_delete(&exports);
                wasm_instance_delete(instance);
                wasm_func_delete(host_function);
                wasm_functype_delete(host_type);
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

    // Externref creation is only implemented on the `sys` backend.
    #[cfg(feature = "sys")]
    #[allow(
        unexpected_cfgs,
        reason = "tools like cargo-llvm-coverage pass --cfg coverage"
    )]
    #[cfg_attr(coverage_nightly, coverage(off))]
    #[test]
    fn test_host_function_returning_a_reference_of_another_store_traps() {
        (assert_c! {
            #include "tests/wasmer.h"

            static wasm_ref_t* reference;

            // Returns a reference that is not of the store the function is in.
            wasm_trap_t* leak_callback(
                const wasm_val_vec_t* arguments,
                wasm_val_vec_t* results
            ) {
                (void) arguments;

                wasm_val_t value = WASM_REF_VAL(wasm_ref_copy(reference));
                results->data[0] = value;

                return NULL;
            }

            int main() {
                wasm_engine_t* engine = wasm_engine_new();
                wasm_store_t* store = wasm_store_new(engine);
                wasm_store_t* other_store = wasm_store_new(engine);

                wasm_foreign_t* foreign = wasm_foreign_new(other_store);
                reference = wasm_foreign_as_ref(foreign);
                assert(reference);

                wasm_functype_t* leak_type = wasm_functype_new_0_1(wasm_valtype_new_externref());
                wasm_func_t* leak = wasm_func_new(store, leak_type, leak_callback);
                assert(leak);

                wasm_val_t results[1] = { WASM_INIT_VAL };
                wasm_val_vec_t arguments_as_array = WASM_EMPTY_VEC;
                wasm_val_vec_t results_as_array = WASM_ARRAY_VEC(results);

                wasm_trap_t* trap = wasm_func_call(leak, &arguments_as_array, &results_as_array);

                // The call fails rather than hand out the reference as if it
                // were of `store`.
                assert(trap != NULL);
                assert(results[0].of.ref == NULL);

                wasm_message_t message;
                wasm_trap_message(trap, &message);
                assert(strstr(message.data, "values are not supported") != NULL);

                wasm_byte_vec_delete(&message);
                wasm_trap_delete(trap);
                wasm_func_delete(leak);
                wasm_functype_delete(leak_type);
                wasm_foreign_delete(foreign);
                wasm_store_delete(other_store);
                wasm_store_delete(store);
                wasm_engine_delete(engine);

                return 0;
            }
        })
        .success();
    }
}

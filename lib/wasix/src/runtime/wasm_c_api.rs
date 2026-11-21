//! Integration of the `wasm_c_api_v0` guest imports with the instantiation hooks.

use anyhow::{Context, Result};
use wasmer::{Imports, Instance, Memory, Module, StoreMut};
use wasmer_c_api_imports::{WasmCapiInstantiationState, WasmCapiRuntimeHooks};

use super::{InstantiationHook, InstantiationState};

/// Lets WASIX embedders register the hooks directly, e.g. through
/// `PluggableRuntime::with_instantiation_hook`.
impl InstantiationHook for WasmCapiRuntimeHooks {
    fn additional_imports(
        &self,
        module: &Module,
        store: &mut StoreMut,
    ) -> Result<(Imports, InstantiationState)> {
        let (imports, state) = WasmCapiRuntimeHooks::additional_imports(self, module, store)?;
        Ok((imports, InstantiationState::new(state)))
    }

    fn prepare_imports(
        &self,
        module: &Module,
        store: &mut StoreMut,
        imports: &mut Imports,
    ) -> Result<InstantiationState> {
        let state = WasmCapiRuntimeHooks::add_imports(self, module, store, imports)?;
        Ok(InstantiationState::new(state))
    }

    fn configure_new_instance(
        &self,
        module: &Module,
        store: &mut StoreMut,
        instance: &Instance,
        imported_memory: Option<&Memory>,
        state: InstantiationState,
    ) -> Result<()> {
        let state = state
            .take::<WasmCapiInstantiationState>()
            .context("invalid Wasm C API instance setup state")?;
        WasmCapiRuntimeHooks::configure_instance(
            self,
            module,
            store,
            instance,
            imported_memory,
            state,
        )
    }
}

#[cfg(all(test, feature = "sys"))]
mod tests {
    use super::*;
    use crate::{
        PluggableRuntime, WasiError,
        runners::wasi::{RuntimeOrEngine, WasiRunner},
        runtime::task_manager::tokio::TokioTaskManager,
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use wasmer::{AsStoreMut, MemoryType, Pages, Store};
    use wasmer_types::ModuleHash;

    #[test]
    fn prepare_imports_reuses_wasix_memory_before_start() {
        let mut store = Store::default();
        let module = Module::new(
            &store,
            r#"(module
                (import "env" "memory" (memory 1 1))
                (import "wasm_c_api_v0" "wasm_byte_vec_new"
                    (func $wasm_byte_vec_new (param i32 i32 i32)))
                (data (i32.const 16) "\ff\ff\ff\ff\ff\ff\ff\ff")
                (func $initialize
                    (call $wasm_byte_vec_new
                        (i32.const 16)
                        (i32.const 0)
                        (i32.const 0)))
                (start $initialize)
            )"#,
        )
        .expect("module compiles");
        let memory = Memory::new(&mut store, MemoryType::new(Pages(1), Some(Pages(1)), false))
            .expect("WASIX memory can be created");
        let mut imports = Imports::new();
        imports.define("env", "memory", memory.clone());

        let hooks = WasmCapiRuntimeHooks::new();
        let _state = InstantiationHook::prepare_imports(
            &hooks,
            &module,
            &mut store.as_store_mut(),
            &mut imports,
        )
        .expect("C API imports can be prepared");
        Instance::new(&mut store, &module, &imports).expect("start function succeeds");

        let mut header = [0xff; 8];
        memory
            .view(&store)
            .read(16, &mut header)
            .expect("vector header can be read");
        assert_eq!(header, [0; 8]);
    }

    #[test]
    fn wasm_c_api_imports_run_from_wasix_guest() {
        let wasm = wasmer::wat2wasm(
            br#"(module
                (import "wasi_snapshot_preview1" "proc_exit" (func $proc_exit (param i32)))
                (import "wasm_c_api_v0" "wasm_engine_new" (func $wasm_engine_new (result i32)))
                (import "wasm_c_api_v0" "wasm_store_new" (func $wasm_store_new (param i32) (result i32)))
                (import "wasm_c_api_v0" "wasm_module_validate" (func $wasm_module_validate (param i32 i32) (result i32)))
                (import "wasm_c_api_v0" "wasm_module_new" (func $wasm_module_new (param i32 i32) (result i32)))

                (memory (export "memory") 1)
                ;; wasm_byte_vec_t { size: 8, data: 32 }
                (data (i32.const 16) "\08\00\00\00\20\00\00\00")
                ;; Empty wasm module: \0asm + version 1.
                (data (i32.const 32) "\00asm\01\00\00\00")

                (func (export "_start")
                    (local $engine i32)
                    (local $store i32)
                    (local $module i32)

                    (local.set $engine (call $wasm_engine_new))
                    (if (i32.eqz (local.get $engine))
                        (then (call $proc_exit (i32.const 10))))

                    (local.set $store (call $wasm_store_new (local.get $engine)))
                    (if (i32.eqz (local.get $store))
                        (then (call $proc_exit (i32.const 11))))

                    (if (i32.eqz (call $wasm_module_validate (local.get $store) (i32.const 16)))
                        (then (call $proc_exit (i32.const 12))))

                    (local.set $module (call $wasm_module_new (local.get $store) (i32.const 16)))
                    (if (i32.eqz (local.get $module))
                        (then (call $proc_exit (i32.const 13))))
                )
            )"#,
        )
        .expect("guest wat parses");
        let store = Store::default();
        let module = Module::new(&store, &wasm).expect("guest module compiles");

        let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("tokio runtime starts");
        let _guard = tokio_runtime.enter();
        let mut runtime = PluggableRuntime::new(Arc::new(TokioTaskManager::new(
            tokio_runtime.handle().clone(),
        )));
        runtime.set_engine(store.engine().clone());

        let resolve_calls = Arc::new(AtomicUsize::new(0));
        let resolve_engine = store.engine().clone();
        let hooks = WasmCapiRuntimeHooks::new().with_resolve_module_sync({
            let resolve_calls = resolve_calls.clone();
            move |bytes| {
                resolve_calls.fetch_add(1, Ordering::SeqCst);
                Ok(Module::new(&resolve_engine, bytes)?)
            }
        });
        runtime.with_instantiation_hook(hooks);

        let result = WasiRunner::new().run_wasm(
            RuntimeOrEngine::Runtime(Arc::new(runtime)),
            "wasm-c-api-smoke",
            module,
            ModuleHash::new(&wasm),
        );

        match result {
            Ok(()) => {}
            Err(err) => {
                if let Some(WasiError::Exit(code)) = err.downcast_ref::<WasiError>() {
                    panic!("guest exited with status {code}");
                }
                panic!("guest failed: {err:?}");
            }
        }
        assert_eq!(resolve_calls.load(Ordering::SeqCst), 1);
    }
}

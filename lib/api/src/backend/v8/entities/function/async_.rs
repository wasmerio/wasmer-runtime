//! Async host adapters for V8's synchronous C API. The guest stack stays live
//! on this thread; only Rust Store guards are released while imports run.

use std::{
    any::Any,
    ffi::c_void,
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    rc::Rc,
    sync::Arc,
};

use crate::{
    AsStoreAsync, AsStoreMut, AsStoreRef, AsyncFunctionEnvMut, BackendAsyncFunctionEnvMut,
    FunctionEnv, FunctionType, RuntimeError, StoreAsyncWriteLock, StoreMut, Value, WasmTypeList,
    WithEnv, WithoutEnv,
    backend::v8::{
        async_runtime::AsyncBinding,
        bindings::*,
        entities::function::env,
        error::Trap,
        utils::convert::{IntoCApiType, IntoCApiValue, IntoWasmerValue},
    },
    entities::function::async_host::{AsyncFunctionEnv, AsyncHostFunction},
};

use super::Function;

type ImportCapture<T> = fn(&T) -> Option<Arc<dyn Any + Send + Sync>>;

struct AsyncCallback<T, F> {
    binding: Rc<AsyncBinding>,
    raw_store: *mut wasm_store_t,
    environment: env::FunctionEnv<T>,
    signature: FunctionType,
    factory: F,
    capture: Option<ImportCapture<T>>,
}

impl Function {
    pub(crate) fn new_async<FT, F, Fut>(store: &mut impl AsStoreMut, ty: FT, factory: F) -> Self
    where
        FT: Into<FunctionType>,
        F: Fn(&[Value]) -> Fut + 'static,
        Fut: Future<Output = Result<Vec<Value>, RuntimeError>> + 'static,
    {
        let environment = FunctionEnv::new(store, ());
        Self::new_with_env_async(store, &environment, ty, move |_, args| factory(args))
    }

    pub(crate) fn new_with_env_async<FT, F, Fut, T: 'static>(
        store: &mut impl AsStoreMut,
        environment: &FunctionEnv<T>,
        ty: FT,
        factory: F,
    ) -> Self
    where
        FT: Into<FunctionType>,
        F: Fn(AsyncFunctionEnvMut<T>, &[Value]) -> Fut + 'static,
        Fut: Future<Output = Result<Vec<Value>, RuntimeError>> + 'static,
    {
        Self::new_with_env_async_captured(store, environment, ty.into(), factory, None)
    }

    fn new_with_env_async_captured<F, Fut, T: 'static>(
        store: &mut impl AsStoreMut,
        environment: &FunctionEnv<T>,
        signature: FunctionType,
        factory: F,
        capture: Option<ImportCapture<T>>,
    ) -> Self
    where
        F: Fn(AsyncFunctionEnvMut<T>, &[Value]) -> Fut + 'static,
        Fut: Future<Output = Result<Vec<Value>, RuntimeError>> + 'static,
    {
        super::super::check_isolate(store);
        let mut view = store.as_store_mut();
        let raw_store = view.inner.store.as_v8().inner;
        let binding = view.inner.store.as_v8().async_binding.clone();
        let callback = Box::new(AsyncCallback {
            binding: binding.clone(),
            raw_store,
            environment: environment.as_v8().clone(),
            signature: signature.clone(),
            factory,
            capture,
        });
        // The Store owns the SAME stable Box before any V8 publication. Its
        // Rust arena catches capture Drop; wee8 receives no Rust finalizer.
        let pointer = view
            .objects_mut()
            .as_v8_mut()
            .host_callbacks
            .insert(callback, binding);
        let function_type = unsafe {
            let parameters = signature
                .params()
                .iter()
                .map(|ty| wasm_valtype_new((*ty).into_ct()))
                .collect::<Vec<_>>();
            let results = signature
                .results()
                .iter()
                .map(|ty| wasm_valtype_new((*ty).into_ct()))
                .collect::<Vec<_>>();
            let mut parameters_vec = Default::default();
            let mut results_vec = Default::default();
            wasm_valtype_vec_new(&mut parameters_vec, parameters.len(), parameters.as_ptr());
            wasm_valtype_vec_new(&mut results_vec, results.len(), results.as_ptr());
            wasm_functype_new(&mut parameters_vec, &mut results_vec)
        };
        let handle = unsafe {
            wasm_func_new_with_env(
                raw_store,
                function_type,
                Some(invoke::<T, F, Fut>),
                pointer.cast(),
                None,
            )
        };
        unsafe { wasm_functype_delete(function_type) };
        assert!(!handle.is_null(), "failed to create V8 async host function");
        Self { handle }
    }

    pub(crate) fn new_typed_async<F, Args, Rets>(store: &mut impl AsStoreMut, factory: F) -> Self
    where
        F: AsyncHostFunction<(), Args, Rets, WithoutEnv> + 'static,
        Args: WasmTypeList + 'static,
        Rets: WasmTypeList + 'static,
    {
        let environment = FunctionEnv::new(store, ());
        let signature = FunctionType::new(Args::wasm_types(), Rets::wasm_types());
        let factory = Rc::new(factory);
        let arguments = signature.clone();
        let results = signature.clone();
        Self::new_with_env_async(
            store,
            &environment,
            signature,
            move |environment, values| {
                let values = values.to_vec();
                let factory = factory.clone();
                let arguments = arguments.clone();
                let results = results.clone();
                async move {
                    let args = {
                        let mut guard = environment.write().await;
                        typed_arguments::<Args>(&mut guard.as_store_mut(), &arguments, &values)?
                    };
                    let output = factory.call_async(AsyncFunctionEnv::new(), args).await?;
                    let mut guard = environment.write().await;
                    typed_results::<Rets>(&mut guard.as_store_mut(), &results, output)
                }
            },
        )
    }

    pub(crate) fn new_typed_with_env_async<T: 'static, F, Args, Rets>(
        store: &mut impl AsStoreMut,
        environment: &FunctionEnv<T>,
        factory: F,
    ) -> Self
    where
        F: AsyncHostFunction<T, Args, Rets, WithEnv> + 'static,
        Args: WasmTypeList + 'static,
        Rets: WasmTypeList + 'static,
    {
        Self::new_typed_with_env_async_captured(store, environment, factory, None)
    }

    pub(crate) fn new_typed_with_env_async_captured<T: 'static, F, Args, Rets>(
        store: &mut impl AsStoreMut,
        environment: &FunctionEnv<T>,
        factory: F,
        capture: Option<ImportCapture<T>>,
    ) -> Self
    where
        F: AsyncHostFunction<T, Args, Rets, WithEnv> + 'static,
        Args: WasmTypeList + 'static,
        Rets: WasmTypeList + 'static,
    {
        let signature = FunctionType::new(Args::wasm_types(), Rets::wasm_types());
        let arguments = signature.clone();
        let results = signature.clone();
        let factory = Rc::new(factory);
        Self::new_with_env_async_captured(
            store,
            environment,
            signature,
            move |mut environment, values| {
                let values = values.to_vec();
                let arguments = arguments.clone();
                let results = results.clone();
                let factory = factory.clone();
                async move {
                    let args = {
                        let mut guard = environment.write().await;
                        typed_arguments::<Args>(&mut guard.as_store_mut(), &arguments, &values)?
                    };
                    let result_environment = environment.as_mut();
                    let output = factory
                        .call_async(AsyncFunctionEnv::with_env(environment), args)
                        .await?;
                    let mut guard = result_environment.write().await;
                    typed_results::<Rets>(&mut guard.as_store_mut(), &results, output)
                }
            },
            capture,
        )
    }
}

unsafe extern "C" fn invoke<T: 'static, F, Fut>(
    pointer: *mut c_void,
    arguments: *const wasm_val_vec_t,
    results: *mut wasm_val_vec_t,
) -> *mut wasm_trap_t
where
    F: Fn(AsyncFunctionEnvMut<T>, &[Value]) -> Fut + 'static,
    Fut: Future<Output = Result<Vec<Value>, RuntimeError>> + 'static,
{
    // This Box is owned by the actual Store's Rust callback arena. The outer
    // guard-free call retains that actual Store through every callback.
    let callback = unsafe { &*pointer.cast::<AsyncCallback<T, F>>() };
    let binding = &callback.binding;
    let unwinding = std::thread::panicking();
    let outcome = catch_unwind(AssertUnwindSafe(|| -> Result<(), RuntimeError> {
        binding.check_async_entry()?;
        if !binding.has_active_call() {
            return Err(RuntimeError::new(
                "V8 async imports require Function::call_async",
            ));
        }
        let store = binding.store()?;
        let (environment, values) = {
            let guard = StoreAsyncWriteLock::try_acquire(&store)
                .ok_or_else(|| RuntimeError::new("V8 import Store is still borrowed"))?;
            let current = &guard.as_store_ref().inner.store.as_v8().async_binding;
            if !Rc::ptr_eq(current, binding) {
                return Err(RuntimeError::new(
                    "V8 import belongs to another Store backing",
                ));
            }
            let snapshot = callback
                .capture
                .and_then(|capture| capture(callback.environment.as_ref(&guard)));
            let environment =
                AsyncFunctionEnvMut(BackendAsyncFunctionEnvMut::V8(env::AsyncFunctionEnvMut {
                    store: store.store(),
                    func_env: callback.environment.clone(),
                    import_snapshot: snapshot,
                }));
            let arguments = unsafe { &*arguments };
            let values = unsafe { std::slice::from_raw_parts(arguments.data, arguments.size) }
                .iter()
                .map(|value| (*value).into_wv())
                .collect::<Vec<_>>();
            (environment, values)
        }; // no actual Store/context guard survives the factory or its future
        let future = (callback.factory)(environment, &values);
        let output = binding.run_host_future(future)?;
        if output.len() != callback.signature.results().len()
            || output
                .iter()
                .zip(callback.signature.results())
                .any(|(value, ty)| value.ty() != *ty)
        {
            return Err(RuntimeError::new(
                "V8 async host function returned wrong results",
            ));
        }
        let _guard = StoreAsyncWriteLock::try_acquire(&store)
            .ok_or_else(|| RuntimeError::new("V8 host result Store is still borrowed"))?;
        let results = unsafe { &mut *results };
        if results.size != output.len() {
            return Err(RuntimeError::new("V8 host result arity changed"));
        }
        let destination = unsafe { std::slice::from_raw_parts_mut(results.data, results.size) };
        for (slot, value) in destination.iter_mut().zip(output) {
            *slot = value.into_cv();
        }
        Ok(())
    }));
    let error = match outcome {
        Ok(Ok(())) => return std::ptr::null_mut(),
        Ok(Err(error)) => error,
        Err(panic) => {
            binding.record(panic, unwinding);
            RuntimeError::new("V8 host callback panicked")
        }
    };
    // Never resume a Rust panic through wee8's C callback. The outer actual
    // call receives its trap, disposes its fields, then claims the same value.
    unsafe { Trap::user(Box::new(error)).into_wasm_trap_raw(callback.raw_store) }
}

fn typed_arguments<Args: WasmTypeList>(
    store: &mut StoreMut<'_>,
    signature: &FunctionType,
    values: &[Value],
) -> Result<Args, RuntimeError> {
    if values.len() != signature.params().len() {
        return Err(RuntimeError::new(
            "typed host function received wrong parameters",
        ));
    }
    let mut raw = Args::empty_array();
    for (slot, value) in raw.as_mut().iter_mut().zip(values) {
        *slot = value.as_raw(store);
    }
    Ok(unsafe { Args::from_array(store, raw) })
}

fn typed_results<Rets: WasmTypeList>(
    store: &mut StoreMut<'_>,
    signature: &FunctionType,
    values: Rets,
) -> Result<Vec<Value>, RuntimeError> {
    let mut raw = unsafe { values.into_array(store) };
    Ok(raw
        .as_mut()
        .iter()
        .zip(signature.results())
        .map(|(value, ty)| unsafe { Value::from_raw(store, *ty, *value) })
        .collect())
}

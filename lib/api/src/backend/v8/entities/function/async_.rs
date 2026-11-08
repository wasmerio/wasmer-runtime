//! Async host adapters for V8's synchronous C API. The guest stack stays live
//! on this thread; only Rust Store guards are released while imports run.

use std::{
    any::Any,
    ffi::c_void,
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
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
        origin::{Handle, Origin},
        references::{OwnedValue, Values as CallValues},
        utils::convert::{IntoCApiType, IntoCApiValue, IntoWasmerValue},
    },
    entities::function::async_host::{AsyncFunctionEnv, AsyncHostFunction},
};

use super::Function;

type ImportCapture<T> = fn(&T) -> Option<Arc<dyn Any + Send + Sync>>;

struct AsyncCallback<T, F> {
    binding: Rc<AsyncBinding>,
    raw_store: *mut wasm_store_t,
    origin: Origin,
    environment: env::FunctionEnv<T>,
    signature: FunctionType,
    factory: F,
    capture: Option<ImportCapture<T>>,
}

impl Function {
    /// V8 preserves the synchronous importing-thread contract. Construction is
    /// inert; after the checked first poll, a pending host import blocks this
    /// OS stack while its selected existing owner drives that exact future.
    pub(crate) fn call_async(
        &self,
        asynchronous: &impl AsStoreAsync,
        parameters: Vec<Value>,
    ) -> Pin<Box<dyn Future<Output = Result<Box<[Value]>, RuntimeError>>>> {
        // Clone/Drop are opaque pointer + inert atomic metadata only, including
        // expiration and foreign-thread metadata destruction. No wee8 query.
        let function = self.clone();
        let store = asynchronous.store();
        Box::pin(async move {
            let mut locked = Some(StoreAsyncWriteLock::try_acquire(&store).ok_or_else(|| {
                RuntimeError::new("V8 call Store is busy; release it before polling")
            })?);
            function
                .handle
                .origin
                .check_store(locked.as_ref().unwrap())?;
            if parameters
                .iter()
                .any(|value| !value.is_from_store(locked.as_ref().unwrap()))
            {
                return Err(RuntimeError::new(
                    "V8 call parameter belongs to another Store",
                ));
            }
            let binding = locked
                .as_ref()
                .unwrap()
                .as_store_ref()
                .inner
                .store
                .as_v8()
                .async_binding
                .clone();
            binding.check_async_entry()?;
            let owner = binding.selected_executor()?;
            let frame = binding.enter()?;
            let mut arguments = None;
            let mut results = None;
            let entry_unwinding = std::thread::panicking();
            let outcome = catch_unwind(AssertUnwindSafe(
                || -> Result<Box<[Value]>, RuntimeError> {
                    let signature = function.ty(locked.as_ref().unwrap());
                    if signature.params().len() != parameters.len()
                        || signature
                            .params()
                            .iter()
                            .zip(&parameters)
                            .any(|(ty, value)| *ty != value.ty())
                    {
                        return Err(RuntimeError::new(
                            "V8 call parameters do not match signature",
                        ));
                    }
                    arguments = Some(CallValues::from_values(&parameters));
                    results = Some(CallValues::empty(signature.results().len()));
                    // Actual backing remains strongly owned, but NO StoreMut or
                    // LocalRwLock guard remains across wee8 or any host factory.
                    drop(locked.take());
                    loop {
                        results.as_mut().unwrap().clear();
                        if frame.has_panic() {
                            return Err(RuntimeError::new("V8 call cleanup panicked"));
                        }
                        let trap = unsafe {
                            wasm_func_call(
                                function.handle.raw(),
                                &arguments.as_ref().unwrap().0,
                                &mut results.as_mut().unwrap().0,
                            )
                        };
                        // A host callback already caught the original panic.
                        // Never run on_called or replay the guest under it.
                        if frame.has_panic() {
                            discard_trap(&frame, trap);
                            return Err(RuntimeError::new("V8 call panicked"));
                        }
                        let mut guard =
                            StoreAsyncWriteLock::try_acquire(&store).ok_or_else(|| {
                                RuntimeError::new("V8 call returned with a borrowed Store")
                            })?;
                        function.handle.origin.check_store(&guard)?;
                        let callback = guard.as_store_mut().inner.on_called.take();
                        let action = callback.map(|callback| callback(guard.as_store_mut()));
                        drop(guard);
                        if frame.has_panic() {
                            discard_trap(&frame, trap);
                            guarded_dispose(&frame, || drop(action));
                            return Err(RuntimeError::new("V8 call panicked"));
                        }
                        match action {
                            Some(Ok(wasmer_types::OnCalledAction::InvokeAgain)) => {
                                discard_trap(&frame, trap);
                                if frame.has_panic() {
                                    return Err(RuntimeError::new("V8 call panicked"));
                                }
                                continue;
                            }
                            Some(Ok(wasmer_types::OnCalledAction::Trap(error)))
                            | Some(Err(error)) => {
                                discard_trap(&frame, trap);
                                return Err(RuntimeError::user(error));
                            }
                            Some(Ok(wasmer_types::OnCalledAction::Finish)) | None => {}
                        }
                        if !trap.is_null() {
                            return Err(Trap::from(trap).into());
                        }
                        let guard = StoreAsyncWriteLock::try_acquire(&store).ok_or_else(|| {
                            RuntimeError::new("V8 result Store is still borrowed")
                        })?;
                        function.handle.origin.check_store(&guard)?;
                        let values = results
                            .as_ref()
                            .unwrap()
                            .as_slice()
                            .iter()
                            .map(|value| (*value).into_wv(&guard))
                            .collect::<Vec<_>>()
                            .into_boxed_slice();
                        return Ok(values);
                    }
                },
            ));
            let mut outcome = match outcome {
                Ok(value) => Some(value),
                Err(panic) => {
                    frame.record(panic, entry_unwinding);
                    Some(Err(RuntimeError::new("V8 call panicked")))
                }
            };
            // Each callback-bearing resource stays outside the preceding catch.
            // Capture original failure before its subsequent destructor can run.
            guarded_dispose(&frame, || drop(locked.take()));
            guarded_dispose(&frame, || drop(arguments.take()));
            guarded_dispose(&frame, || drop(results.take()));
            guarded_dispose(&frame, || drop(parameters));
            guarded_dispose(&frame, || drop(owner));
            if frame.has_panic() {
                let unwinding = std::thread::panicking();
                if let Err(panic) = catch_unwind(AssertUnwindSafe(|| drop(outcome.take()))) {
                    frame.record(panic, unwinding);
                }
            }
            let panic = frame.take_panic();
            drop(frame); // callback-free restore; B cannot take A's carrier
            if let Some(panic) = panic {
                // Actual Store capture unwinds under this already-owned first
                // value; its arena protects/discards secondary capture panics.
                resume_unwind(panic);
            }
            // No active CallFrame: the arena transports its own first value at
            // its actual Rust Drop boundary if this is the last Store owner.
            drop(store);
            outcome.take().unwrap()
        })
    }

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
            origin: Origin::of(&view).expect("actual V8 constructor Store"),
            environment: environment.as_v8().clone(),
            signature: signature.clone(),
            factory,
            capture,
        });
        // The Store owns the SAME stable Box before any V8 publication. Its
        // Rust arena catches capture Drop; wee8 receives no Rust finalizer.
        let pointer = view
            .inner
            .store
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
        Self {
            handle: Handle::new(
                view.inner.store.as_v8().reference_roots.function(handle),
                Origin::of(&view).expect("actual V8 constructor Store"),
            ),
        }
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
            let values = crate::backend::v8::references::borrowed_values(arguments)
                .iter()
                .map(|value| (*value).into_wv(&guard))
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
        if output.iter().any(|value| !value.is_from_store(&_guard)) {
            return Err(RuntimeError::new("V8 host result belongs to another Store"));
        }
        let destination = crate::backend::v8::references::borrowed_values_mut(results);
        for (slot, value) in destination.iter_mut().zip(output) {
            OwnedValue::copy(&value.into_cv()).replace(slot);
        }
        Ok(())
    }));
    let error = match outcome {
        Ok(Ok(())) if !binding.current_has_panic() => return std::ptr::null_mut(),
        Ok(Ok(())) => RuntimeError::new("V8 host cleanup panicked"),
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

fn guarded_dispose(frame: &crate::backend::v8::async_runtime::CallGuard, cleanup: impl FnOnce()) {
    let unwinding = std::thread::panicking();
    if let Err(panic) = catch_unwind(AssertUnwindSafe(cleanup)) {
        frame.record(panic, unwinding);
    }
}

// A discarded trap can own an encoded Rust error as well as the C wrapper.
// Catch the decode before later destruction and retain the same first carrier.
fn discard_trap(frame: &crate::backend::v8::async_runtime::CallGuard, raw: *mut wasm_trap_t) {
    if raw.is_null() {
        return;
    }
    let unwinding = std::thread::panicking();
    let decoded = match catch_unwind(AssertUnwindSafe(|| Trap::from(raw))) {
        Ok(trap) => Some(trap),
        Err(panic) => {
            frame.record(panic, unwinding);
            None
        }
    };
    guarded_dispose(frame, || unsafe { wasm_trap_delete(raw) });
    guarded_dispose(frame, || drop(decoded));
}

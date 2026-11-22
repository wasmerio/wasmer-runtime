//! Data types, functions and traits for `sys` runtime's `Function` implementation

pub(crate) mod env;
pub(crate) mod typed;

#[cfg(feature = "experimental-async")]
use crate::sys::async_runtime::{AsyncRuntimeError, HostFuture, block_on_host_future};

use crate::{
    BackendFunction, CallWrapper, FunctionEnv, FunctionEnvMut, FunctionType, HostFunction,
    RuntimeError, StoreContext, StoreInner, Value, WithEnv, WithoutEnv,
    backend::sys::{engine::NativeEngineExt, vm::VMFunctionCallback},
    entities::function::call_wrapper::call_wrapped,
    entities::store::{AsStoreMut, AsStoreRef, StoreMut},
    utils::{FromToNativeWasmType, IntoResult, NativeWasmTypeInto, WasmTypeList},
    vm::{VMExtern, VMExternFunction},
};
use std::panic::{self, AssertUnwindSafe};
use std::{
    cell::UnsafeCell, cmp::max, error::Error, ffi::c_void, future::Future, marker::PhantomData,
};
use wasmer_types::{NativeWasmType, RawValue, StoreId};
#[cfg(feature = "experimental-host-interrupt")]
use wasmer_vm::interrupt_registry;
use wasmer_vm::{
    MaybeInstanceOwned, StoreHandle, Trap, TrapCode, VMCallerCheckedAnyfunc, VMContext,
    VMDynamicFunctionContext, VMExceptionRef, VMFuncRef, VMFunction, VMFunctionBody,
    VMFunctionContext, VMFunctionKind, VMTrampoline, on_host_stack, raise_lib_trap,
    raise_user_trap, resume_panic, wasmer_call_trampoline,
};

#[cfg_attr(feature = "artifact-size", derive(loupe::MemoryUsage))]
#[derive(Debug, Clone, PartialEq, Eq)]
/// A WebAssembly `function` instance, in the `sys` runtime.
pub struct Function {
    pub(crate) handle: StoreHandle<VMFunction>,
}

impl From<StoreHandle<VMFunction>> for Function {
    fn from(handle: StoreHandle<VMFunction>) -> Self {
        Self { handle }
    }
}

#[derive(Clone, Copy)]
enum CallEntry {
    Synchronous,
    #[cfg(feature = "experimental-async")]
    AsyncRoot,
}

impl Function {
    pub(crate) fn new_with_env<FT, F, T: Send + 'static>(
        store: &mut impl AsStoreMut,
        env: &FunctionEnv<T>,
        ty: FT,
        func: F,
    ) -> Self
    where
        FT: Into<FunctionType>,
        F: Fn(FunctionEnvMut<T>, &[Value]) -> Result<Vec<Value>, RuntimeError>
            + 'static
            + Send
            + Sync,
    {
        let function_type = ty.into();
        let func_ty = function_type.clone();
        let func_env = env.clone().into_sys();
        let store_id = store.objects_mut().id();
        let wrapper = move |values_vec: *mut RawValue| -> HostCallOutcome {
            unsafe {
                let mut store_wrapper = unsafe { StoreContext::get_current(store_id) };
                let mut store_mut = store_wrapper.as_mut();
                let mut args = Vec::with_capacity(func_ty.params().len());

                for (i, ty) in func_ty.params().iter().enumerate() {
                    args.push(Value::from_raw(
                        &mut store_mut,
                        *ty,
                        values_vec.add(i).read_unaligned(),
                    ));
                }
                let env = env::FunctionEnvMut {
                    store_mut,
                    func_env: func_env.clone(),
                }
                .into();
                HostCallOutcome::Ready(func(env, &args))
            }
        };
        let mut host_data = Box::new(VMDynamicFunctionContext {
            address: std::ptr::null(),
            ctx: DynamicFunction {
                func: wrapper,
                store_id,
                func_ty: function_type.clone(),
            },
        });
        host_data.address = host_data.ctx.func_body_ptr();

        // We don't yet have the address with the Wasm ABI signature.
        // The engine linker will replace the address with one pointing to a
        // generated dynamic trampoline.
        let func_ptr = std::ptr::null() as VMFunctionCallback;
        let type_signature_hash = store
            .as_store_ref()
            .engine()
            .as_sys()
            .register_signature(&function_type);
        let vmctx = VMFunctionContext {
            host_env: host_data.as_ref() as *const _ as *mut c_void,
        };
        let call_trampoline = host_data.ctx.call_trampoline_address();
        let anyfunc = VMCallerCheckedAnyfunc {
            include_m0_param: false,
            func_ptr,
            type_signature_hash,
            vmctx,
            call_trampoline,
        };

        let vm_function = VMFunction {
            anyfunc: MaybeInstanceOwned::Host(Box::new(UnsafeCell::new(anyfunc))),
            kind: VMFunctionKind::Dynamic,
            signature: function_type,
            host_data,
        };
        Self {
            handle: StoreHandle::new(store.objects_mut().as_sys_mut(), vm_function),
        }
    }

    /// Borrowing async API's dynamic host entry. The factory is invoked only
    /// when its future is polled, on the executor's stack and in the call's
    /// access scope (or the immediate scope of a synchronous entry).
    #[cfg(feature = "experimental-async")]
    pub(crate) fn new_async_boxed<FT, F>(store: &mut impl AsStoreMut, ty: FT, func: F) -> Self
    where
        FT: Into<FunctionType>,
        F: Fn(Vec<Value>) -> HostFuture + 'static,
    {
        let function_type = ty.into();
        let func_ty = function_type.clone();
        let store_id = store.objects_mut().id();
        let func = std::rc::Rc::new(func);
        let wrapper = move |values_vec: *mut RawValue| -> HostCallOutcome {
            let args = unsafe {
                let mut store_wrapper = StoreContext::get_current(store_id);
                let mut store = store_wrapper.as_mut();
                let mut args = Vec::with_capacity(func_ty.params().len());
                for (i, ty) in func_ty.params().iter().enumerate() {
                    args.push(Value::from_raw(
                        &mut store,
                        *ty,
                        values_vec.add(i).read_unaligned(),
                    ));
                }
                args
            };
            let func = func.clone();
            HostCallOutcome::Future(Box::pin(async move { func(args).await }))
        };
        let mut host_data = Box::new(VMDynamicFunctionContext {
            address: std::ptr::null(),
            ctx: DynamicFunction {
                func: wrapper,
                store_id,
                func_ty: function_type.clone(),
            },
        });
        host_data.address = host_data.ctx.func_body_ptr();

        let func_ptr = std::ptr::null() as VMFunctionCallback;
        let type_signature_hash = store
            .as_store_ref()
            .engine()
            .as_sys()
            .register_signature(&function_type);
        let vmctx = VMFunctionContext {
            host_env: host_data.as_ref() as *const _ as *mut c_void,
        };
        let call_trampoline = host_data.ctx.call_trampoline_address();
        let anyfunc = VMCallerCheckedAnyfunc {
            include_m0_param: false,
            func_ptr,
            type_signature_hash,
            vmctx,
            call_trampoline,
        };

        let vm_function = VMFunction {
            anyfunc: MaybeInstanceOwned::Host(Box::new(UnsafeCell::new(anyfunc))),
            kind: VMFunctionKind::Dynamic,
            signature: function_type,
            host_data,
        };
        Self {
            handle: StoreHandle::new(store.objects_mut().as_sys_mut(), vm_function),
        }
    }

    /// Creates a new host `Function` from a native function.
    pub(crate) fn new_typed<F, Args, Rets>(store: &mut impl AsStoreMut, func: F) -> Self
    where
        F: HostFunction<(), Args, Rets, WithoutEnv> + 'static + Send + Sync,
        Args: WasmTypeList,
        Rets: WasmTypeList,
    {
        let env = FunctionEnv::new(store, ());
        let func_ptr = func.function_callback_sys().unwrap_sys();
        let host_data = Box::new(StaticFunction {
            store_id: store.objects_mut().id(),
            env,
            func,
        });
        let function_type = FunctionType::new(Args::wasm_types(), Rets::wasm_types());

        let type_signature_hash = store
            .as_store_ref()
            .engine()
            .as_sys()
            .register_signature(&function_type);
        let vmctx = VMFunctionContext {
            host_env: host_data.as_ref() as *const _ as *mut c_void,
        };
        let call_trampoline =
            <F as HostFunction<(), Args, Rets, WithoutEnv>>::call_trampoline_address().unwrap_sys();
        let anyfunc = VMCallerCheckedAnyfunc {
            include_m0_param: false,
            func_ptr,
            type_signature_hash,
            vmctx,
            call_trampoline,
        };

        let vm_function = VMFunction {
            anyfunc: MaybeInstanceOwned::Host(Box::new(UnsafeCell::new(anyfunc))),
            kind: VMFunctionKind::Static,
            signature: function_type,
            host_data,
        };
        Self {
            handle: StoreHandle::new(store.objects_mut().as_sys_mut(), vm_function),
        }
    }

    pub(crate) fn new_typed_with_env<T: Send + 'static, F, Args, Rets>(
        store: &mut impl AsStoreMut,
        env: &FunctionEnv<T>,
        func: F,
    ) -> Self
    where
        F: HostFunction<T, Args, Rets, WithEnv> + 'static + Send + Sync,
        Args: WasmTypeList,
        Rets: WasmTypeList,
    {
        let func_ptr = func.function_callback_sys().unwrap_sys();
        let host_data = Box::new(StaticFunction {
            store_id: store.objects_mut().id(),
            env: env.as_sys().clone().into(),
            func,
        });
        let function_type = FunctionType::new(Args::wasm_types(), Rets::wasm_types());

        let type_signature_hash = store
            .as_store_ref()
            .engine()
            .as_sys()
            .register_signature(&function_type);
        let vmctx = VMFunctionContext {
            host_env: host_data.as_ref() as *const _ as *mut c_void,
        };
        let call_trampoline =
            <F as HostFunction<T, Args, Rets, WithEnv>>::call_trampoline_address().unwrap_sys();
        let anyfunc = VMCallerCheckedAnyfunc {
            include_m0_param: false,
            func_ptr,
            type_signature_hash,
            vmctx,
            call_trampoline,
        };

        let vm_function = VMFunction {
            anyfunc: MaybeInstanceOwned::Host(Box::new(UnsafeCell::new(anyfunc))),
            kind: VMFunctionKind::Static,
            signature: function_type,
            host_data,
        };
        Self {
            handle: StoreHandle::new(store.objects_mut().as_sys_mut(), vm_function),
        }
    }

    pub(crate) fn ty(&self, store: &impl AsStoreRef) -> FunctionType {
        self.handle
            .get(store.as_store_ref().objects().as_sys())
            .signature
            .clone()
    }

    fn call_wasm(
        &self,
        store: &mut impl AsStoreMut,
        wrapper: Option<&mut dyn CallWrapper>,
        trampoline: VMTrampoline,
        params: &[Value],
        results: &mut [Value],
        entry: CallEntry,
    ) -> Result<(), RuntimeError> {
        let format_types_for_error_message = |items: &[Value]| {
            items
                .iter()
                .map(|param| param.ty().to_string())
                .collect::<Vec<String>>()
                .join(", ")
        };
        // TODO: Avoid cloning the signature here, it's expensive.
        let signature = self.ty(store);
        if signature.params().len() != params.len() {
            return Err(RuntimeError::new(format!(
                "Parameters of type [{}] did not match signature {}",
                format_types_for_error_message(params),
                signature
            )));
        }
        if signature.results().len() != results.len() {
            return Err(RuntimeError::new(format!(
                "Results of type [{}] did not match signature {}",
                format_types_for_error_message(results),
                signature,
            )));
        }

        let mut values_vec = vec![RawValue { i32: 0 }; max(params.len(), results.len())];

        // Store the argument values into `values_vec`.
        let param_tys = signature.params().iter();
        for ((arg, slot), ty) in params.iter().zip(&mut values_vec).zip(param_tys) {
            if arg.ty() != *ty {
                let param_types = format_types_for_error_message(params);
                return Err(RuntimeError::new(format!(
                    "Parameters of type [{param_types}] did not match signature {signature}",
                )));
            }
            if !arg.is_from_store(store) {
                return Err(RuntimeError::new("cross-`Store` values are not supported"));
            }
            *slot = arg.as_raw(store);
        }

        // Invoke the call
        self.call_wasm_raw(store, wrapper, trampoline, values_vec, results, entry)?;
        Ok(())
    }

    fn call_wasm_raw(
        &self,
        store: &mut impl AsStoreMut,
        wrapper: Option<&mut dyn CallWrapper>,
        trampoline: VMTrampoline,
        mut params: Vec<RawValue>,
        results: &mut [Value],
        entry: CallEntry,
    ) -> Result<(), RuntimeError> {
        // Call the trampoline.
        let result = {
            #[cfg(feature = "experimental-host-interrupt")]
            let store_id = store.objects_mut().id();

            #[cfg(feature = "experimental-host-interrupt")]
            let interrupt_guard = match interrupt_registry::install(store_id) {
                Ok(x) => x,
                Err(interrupt_registry::InstallError::AlreadyInterrupted) => {
                    return Err(Trap::lib(TrapCode::HostInterrupt).into());
                }
            };

            // The trampoline writes the results over the arguments, so a
            // wrapper that enters again gets them restored first.
            let original_params = wrapper.is_some().then(|| params.clone());

            let r = call_wrapped(store, wrapper, |store| {
                if let Some(original_params) = &original_params {
                    params.copy_from_slice(original_params);
                }
                // Synchronous entries always install their own context, including
                // calls nested beneath an async guest. The root coroutine instead
                // uses the raw context installed separately around each resume.
                let _store_install_guard = match entry {
                    CallEntry::Synchronous => {
                        Some(unsafe { StoreContext::install(store.as_store_mut().inner as *mut _) })
                    }
                    #[cfg(feature = "experimental-async")]
                    CallEntry::AsyncRoot => None,
                };
                // Copy all call metadata before entering Wasm: a suspended root
                // must retain no Rust reference into the store between resumes.
                let (config, signal_handler, vmctx, func_ptr) = {
                    let storeref = store.as_store_ref();
                    let vm_function = self.handle.get(storeref.objects().as_sys());
                    let anyfunc = unsafe { vm_function.anyfunc.as_ptr().as_ref() };
                    let config = wasmer_vm::VMConfig {
                        wasm_stack_size: storeref.engine().tunables().vmconfig().wasm_stack_size,
                    };
                    (
                        config,
                        storeref.signal_handler(),
                        anyfunc.vmctx,
                        anyfunc.func_ptr,
                    )
                };
                unsafe {
                    wasmer_call_trampoline(
                        signal_handler,
                        &config,
                        vmctx,
                        trampoline,
                        func_ptr,
                        params.as_mut_ptr() as *mut u8,
                    )
                }
                .map_err(RuntimeError::from)
            });

            #[cfg(feature = "experimental-host-interrupt")]
            drop(interrupt_guard);

            r
        };

        result?;

        // Load the return values out of `values_vec`.
        let signature = self.ty(store);
        for (index, &value_type) in signature.results().iter().enumerate() {
            unsafe {
                results[index] = Value::from_raw(store, value_type, params[index]);
            }
        }

        Ok(())
    }

    pub(crate) fn result_arity(&self, store: &impl AsStoreRef) -> usize {
        self.ty(store).results().len()
    }

    pub(crate) fn call(
        &self,
        store: &mut impl AsStoreMut,
        params: &[Value],
    ) -> Result<Box<[Value]>, RuntimeError> {
        self.call_wrapped(store, None, params)
    }

    pub(crate) fn call_wrapped(
        &self,
        store: &mut impl AsStoreMut,
        wrapper: Option<&mut dyn CallWrapper>,
        params: &[Value],
    ) -> Result<Box<[Value]>, RuntimeError> {
        self.call_with_entry(store, wrapper, params, CallEntry::Synchronous)
    }

    /// Enters the root coroutine without installing a synchronous store context.
    ///
    /// # Safety
    /// The caller must install the current async store pointer around each
    /// coroutine resume. `store` may only lend references for individual accesses;
    /// it must not itself hold a store reference across this call.
    #[cfg(feature = "experimental-async")]
    pub(crate) unsafe fn call_uninstalled(
        &self,
        store: &mut impl AsStoreMut,
        params: &[Value],
    ) -> Result<Box<[Value]>, RuntimeError> {
        self.call_with_entry(store, None, params, CallEntry::AsyncRoot)
    }

    fn call_with_entry(
        &self,
        store: &mut impl AsStoreMut,
        wrapper: Option<&mut dyn CallWrapper>,
        params: &[Value],
        entry: CallEntry,
    ) -> Result<Box<[Value]>, RuntimeError> {
        let trampoline = unsafe {
            self.handle
                .get(store.objects_mut().as_sys())
                .anyfunc
                .as_ptr()
                .as_ref()
                .call_trampoline
        };
        let mut results = vec![Value::null(); self.result_arity(store)];
        self.call_wasm(store, wrapper, trampoline, params, &mut results, entry)?;
        Ok(results.into_boxed_slice())
    }

    #[doc(hidden)]
    #[allow(missing_docs)]
    pub(crate) fn call_raw(
        &self,
        store: &mut impl AsStoreMut,
        params: Vec<RawValue>,
    ) -> Result<Box<[Value]>, RuntimeError> {
        let trampoline = unsafe {
            self.handle
                .get(store.objects_mut().as_sys())
                .anyfunc
                .as_ptr()
                .as_ref()
                .call_trampoline
        };
        let mut results = vec![Value::null(); self.result_arity(store)];
        self.call_wasm_raw(
            store,
            None,
            trampoline,
            params,
            &mut results,
            CallEntry::Synchronous,
        )?;
        Ok(results.into_boxed_slice())
    }

    pub(crate) fn vm_funcref(&self, store: &impl AsStoreRef) -> VMFuncRef {
        let vm_function = self.handle.get(store.as_store_ref().objects().as_sys());
        if vm_function.kind == VMFunctionKind::Dynamic {
            panic!("dynamic functions cannot be used in tables or as funcrefs");
        }
        VMFuncRef(vm_function.anyfunc.as_ptr())
    }

    pub(crate) unsafe fn from_vm_funcref(store: &mut impl AsStoreMut, funcref: VMFuncRef) -> Self {
        let signature = {
            let anyfunc = unsafe { funcref.0.as_ref() };
            store
                .as_store_mut()
                .engine()
                .as_sys()
                .lookup_signature(anyfunc.type_signature_hash)
                .expect("Signature not found in store")
        };
        let vm_function = VMFunction {
            anyfunc: MaybeInstanceOwned::Instance(funcref.0),
            signature,
            // All functions in tables are already Static (as dynamic functions
            // are converted to use the trampolines with static signatures).
            kind: wasmer_vm::VMFunctionKind::Static,
            host_data: Box::new(()),
        };
        Self {
            handle: StoreHandle::new(store.objects_mut().as_sys_mut(), vm_function),
        }
    }

    pub(crate) fn from_vm_extern(store: &mut impl AsStoreMut, vm_extern: VMExternFunction) -> Self {
        Self {
            handle: unsafe {
                StoreHandle::from_internal(store.objects_mut().id(), vm_extern.unwrap_sys())
            },
        }
    }

    /// Checks whether this `Function` can be used with the given store.
    pub(crate) fn is_from_store(&self, store: &impl AsStoreRef) -> bool {
        self.handle.store_id() == store.as_store_ref().objects().id()
    }

    pub(crate) fn to_vm_extern(&self) -> VMExtern {
        VMExtern::Sys(wasmer_vm::VMExtern::Function(self.handle.internal_handle()))
    }
}

// We want to keep as much logic as possible on the host stack,
// since the WASM stack may be out of memory. In that scenario,
// throwing exceptions won't work since libunwind requires
// considerable stack space to do its magic, but everything else
// should work.
enum InvocationResult<T, E> {
    Success(T),
    Exception(crate::Exception),
    Trap(Box<E>),
    YieldOutsideAsyncContext,
}

fn to_invocation_result<T, E>(result: Result<T, E>) -> InvocationResult<T, E>
where
    E: Error + 'static,
{
    match result {
        Ok(value) => InvocationResult::Success(value),
        Err(trap) => {
            let dyn_err_ref = &trap as &dyn Error;
            if let Some(runtime_error) = dyn_err_ref.downcast_ref::<RuntimeError>()
                && let Some(exception) = runtime_error.to_exception()
            {
                return InvocationResult::Exception(exception);
            }
            InvocationResult::Trap(Box::new(trap))
        }
    }
}

fn write_dynamic_results(
    store_id: StoreId,
    func_ty: &FunctionType,
    returns: Vec<Value>,
    values_vec: *mut RawValue,
) -> Result<(), RuntimeError> {
    let mut store_wrapper = unsafe { StoreContext::get_current(store_id) };
    let mut store = store_wrapper.as_mut();
    let return_types = returns.iter().map(|ret| ret.ty());
    if return_types.ne(func_ty.results().iter().copied()) {
        return Err(RuntimeError::new(format!(
            "Dynamic function returned wrong signature. Expected {:?} but got {:?}",
            func_ty.results(),
            returns.iter().map(|ret| ret.ty())
        )));
    }
    for (i, ret) in returns.iter().enumerate() {
        unsafe {
            values_vec.add(i).write_unaligned(ret.as_raw(&store));
        }
    }
    Ok(())
}

fn finalize_dynamic_call(
    store_id: StoreId,
    func_ty: &FunctionType,
    values_vec: *mut RawValue,
    result: Result<Vec<Value>, RuntimeError>,
) -> Result<(), RuntimeError> {
    match result {
        Ok(values) => write_dynamic_results(store_id, func_ty, values, values_vec),
        Err(err) => Err(err),
    }
}

/// The result of constructing a dynamic host invocation.
pub(crate) enum HostCallOutcome {
    Ready(Result<Vec<Value>, RuntimeError>),
    #[cfg(feature = "experimental-async")]
    Future(HostFuture),
}

/// Host state for a dynamic function.
pub(crate) struct DynamicFunction<F> {
    func: F,
    store_id: StoreId,
    /// The function's type, which its results are checked against.
    func_ty: FunctionType,
}

impl<F> DynamicFunction<F>
where
    F: Fn(*mut RawValue) -> HostCallOutcome + 'static,
{
    // This function wraps our func, to make it compatible with the
    // reverse trampoline signature
    unsafe extern "C-unwind" fn func_wrapper(
        // Multiple calls can be suspended in the same import. Its host context
        // is immutable, so reentry must not create overlapping mutable borrows.
        this: &VMDynamicFunctionContext<Self>,
        values_vec: *mut RawValue,
    ) {
        let result = on_host_stack(|| {
            panic::catch_unwind(AssertUnwindSafe(|| match (this.ctx.func)(values_vec) {
                HostCallOutcome::Ready(result) => to_invocation_result(finalize_dynamic_call(
                    this.ctx.store_id,
                    &this.ctx.func_ty,
                    values_vec,
                    result,
                )),
                #[cfg(feature = "experimental-async")]
                HostCallOutcome::Future(future) => {
                    let awaited = block_on_host_future(this.ctx.store_id, future);
                    let result = match awaited {
                        Ok(value) => Ok(value),
                        Err(AsyncRuntimeError::RuntimeError(e)) => Err(e),
                        Err(AsyncRuntimeError::YieldOutsideAsyncContext) => {
                            return InvocationResult::YieldOutsideAsyncContext;
                        }
                    };
                    to_invocation_result(finalize_dynamic_call(
                        this.ctx.store_id,
                        &this.ctx.func_ty,
                        values_vec,
                        result,
                    ))
                }
            }))
        });

        // IMPORTANT: DO NOT ALLOCATE ON THE STACK,
        // AS WE ARE IN THE WASM STACK, NOT ON THE HOST ONE.
        // See: https://github.com/wasmerio/wasmer/pull/5700
        match result {
            Ok(InvocationResult::Success(())) => unsafe {
                // Note: can't acquire a proper ref-counted context ref here, since we can switch
                // away from the WASM stack at any time.
                // Safety: The pointer is only used for the duration of the call to
                // `get_current_transient`.
                let mut store_wrapper = StoreContext::get_current_transient(this.ctx.store_id);
                let mut store = store_wrapper.as_mut().unwrap();
                #[cfg(feature = "experimental-host-interrupt")]
                if interrupt_registry::is_interrupted(store.objects.id()) {
                    raise_lib_trap(Trap::lib(TrapCode::HostInterrupt))
                }
            },
            Ok(InvocationResult::Exception(exception)) => unsafe {
                // Note: can't acquire a proper ref-counted context ref here, since we can switch
                // away from the WASM stack at any time.
                // Safety: The pointer is only used for the duration of the call to `throw` and
                // `is_interrupted`.
                let mut store_wrapper = StoreContext::get_current_transient(this.ctx.store_id);
                let mut store = store_wrapper.as_mut().unwrap();
                #[cfg(feature = "experimental-host-interrupt")]
                if interrupt_registry::is_interrupted(store.objects.id()) {
                    raise_lib_trap(Trap::lib(TrapCode::HostInterrupt))
                }
                wasmer_vm::libcalls::throw(
                    store.objects.as_sys(),
                    exception.vm_exceptionref().unwrap_sys_ref().to_u32_exnref(),
                )
            },
            Ok(InvocationResult::Trap(trap)) => unsafe { raise_user_trap(trap) },
            Ok(InvocationResult::YieldOutsideAsyncContext) => unsafe {
                raise_lib_trap(Trap::lib(TrapCode::YieldOutsideAsyncContext))
            },
            Err(panic) => unsafe { resume_panic(panic) },
        }
    }

    fn func_body_ptr(&self) -> VMFunctionCallback {
        Self::func_wrapper as VMFunctionCallback
    }

    fn call_trampoline_address(&self) -> VMTrampoline {
        Self::call_trampoline
    }

    unsafe extern "C" fn call_trampoline(
        vmctx: *mut VMContext,
        _body: VMFunctionCallback,
        args: *mut RawValue,
    ) {
        // The VMFunctionCallback is null here: it is only filled in later
        // by the engine linker.
        unsafe {
            let dynamic_function = &*(vmctx as *const VMDynamicFunctionContext<Self>);
            Self::func_wrapper(dynamic_function, args);
        }
    }
}

/// Represents a low-level Wasm static host function. See
/// [`crate::Function::new_typed`] and
/// [`crate::Function::new_typed_with_env`] to learn more.
pub(crate) struct StaticFunction<F, T> {
    pub(crate) store_id: StoreId,
    pub(crate) env: FunctionEnv<T>,
    pub(crate) func: F,
}

impl crate::Function {
    /// Consume [`self`] into [`crate::backend::sys::function::Function`].
    pub fn into_sys(self) -> crate::backend::sys::function::Function {
        match self.0 {
            BackendFunction::Sys(s) => s,
            _ => panic!("Not a `sys` function!"),
        }
    }

    /// Convert a reference to [`self`] into a reference to [`crate::backend::sys::function::Function`].
    pub fn as_sys(&self) -> &crate::backend::sys::function::Function {
        match self.0 {
            BackendFunction::Sys(ref s) => s,
            _ => panic!("Not a `sys` function!"),
        }
    }

    /// Convert a mutable reference to [`self`] into a mutable reference [`crate::backend::sys::function::Function`].
    pub fn as_sys_mut(&mut self) -> &mut crate::backend::sys::function::Function {
        match self.0 {
            BackendFunction::Sys(ref mut s) => s,
            _ => panic!("Not a `sys` function!"),
        }
    }
}

macro_rules! impl_host_function {
    ([$c_struct_representation:ident] $c_struct_name:ident, $( $x:ident ),* ) => {
        paste::paste! {
        #[allow(non_snake_case)]
        pub(crate) fn [<gen_fn_callback_ $c_struct_name:lower _no_env>]
            <$( $x: FromToNativeWasmType, )* Rets: WasmTypeList, RetsAsResult: IntoResult<Rets>, Func: Fn($( $x , )*) -> RetsAsResult + 'static>
            (this: &Func) -> crate::backend::sys::vm::VMFunctionCallback {
            /// This is a function that wraps the real host
            /// function. Its address will be used inside the
            /// runtime.
            unsafe extern "C-unwind" fn func_wrapper<$( $x, )* Rets, RetsAsResult, Func>( env: &StaticFunction<Func, ()>, $( $x: <$x::Native as NativeWasmType>::Abi, )* ) -> Rets::CStruct
            where
                $( $x: FromToNativeWasmType, )*
                Rets: WasmTypeList,
                RetsAsResult: IntoResult<Rets>,
                Func: Fn($( $x , )*) -> RetsAsResult + 'static,
            {
                let result = on_host_stack(|| {
                    panic::catch_unwind(AssertUnwindSafe(|| {
                        let mut store_wrapper = unsafe { StoreContext::get_current(env.store_id) };
                        let mut store = store_wrapper.as_mut();
                        $(
                            let $x = unsafe {
                                FromToNativeWasmType::from_native(NativeWasmTypeInto::from_abi(&mut store, $x))
                            };
                        )*
                        to_invocation_result((env.func)($($x),* ).into_result())
                    }))
                });

                // IMPORTANT: DO NOT ALLOCATE ON THE STACK,
                // AS WE ARE IN THE WASM STACK, NOT ON THE HOST ONE.
                // See: https://github.com/wasmerio/wasmer/pull/5700
                match result {
                    Ok(InvocationResult::Success(result)) => unsafe {
                        // Note: can't acquire a proper ref-counted context ref here, since we can switch
                        // away from the WASM stack at any time.
                        // Safety: The pointer is only used for the duration of the call to
                        // `into_c_struct` and `get_current_transient`.
                        let mut store_wrapper = StoreContext::get_current_transient(env.store_id);
                        let mut store = store_wrapper.as_mut().unwrap();
                        #[cfg(feature = "experimental-host-interrupt")]
                        if interrupt_registry::is_interrupted(store.objects.id()) {
                            raise_lib_trap(Trap::lib(TrapCode::HostInterrupt))
                        }
                        return result.into_c_struct(store);
                    },
                    Ok(InvocationResult::Exception(exception)) => unsafe {
                        // Note: can't acquire a proper ref-counted context ref here, since we can switch
                        // away from the WASM stack at any time.
                        // Safety: The pointer is only used for the duration of the call to `throw` and
                        // `is_interrupted`.
                        let mut store_wrapper = StoreContext::get_current_transient(env.store_id);
                        let mut store = store_wrapper.as_mut().unwrap();
                        #[cfg(feature = "experimental-host-interrupt")]
                        if interrupt_registry::is_interrupted(store.objects.id()) {
                            raise_lib_trap(Trap::lib(TrapCode::HostInterrupt))
                        }
                        wasmer_vm::libcalls::throw(
                            store.objects.as_sys(),
                            exception.vm_exceptionref().unwrap_sys_ref().to_u32_exnref(),
                        )
                    }
                    Ok(InvocationResult::Trap(trap)) => unsafe { raise_user_trap(trap) },
                    Ok(InvocationResult::YieldOutsideAsyncContext) => unsafe {
                        raise_lib_trap(Trap::lib(TrapCode::YieldOutsideAsyncContext))
                    },
                    Err(panic) => unsafe { resume_panic(panic) },
                }
            }

            func_wrapper::< $( $x, )* Rets, RetsAsResult, Func > as _

        }

        #[allow(non_snake_case)]
        pub(crate) fn [<gen_call_trampoline_address_ $c_struct_name:lower _no_env>]
            <$( $x: FromToNativeWasmType, )* Rets: WasmTypeList>
            () -> crate::backend::sys::vm::VMTrampoline {

            unsafe extern "C" fn call_trampoline<$( $x: FromToNativeWasmType, )* Rets: WasmTypeList>
            (
                vmctx: *mut crate::backend::sys::vm::VMContext,
                body: crate::backend::sys::vm::VMFunctionCallback,
                args: *mut RawValue,
            ) {
                let mut _n = 0;

                unsafe {
                    let body: unsafe extern "C" fn(vmctx: *mut crate::backend::sys::vm::VMContext, $( $x: <$x::Native as NativeWasmType>::Abi, )*) -> Rets::CStruct = std::mem::transmute(body);
                    $(
                        let $x = *args.add(_n).cast();
                        _n += 1;
                    )*
                    let results = body(vmctx, $( $x ),*);
                    Rets::write_c_struct_to_ptr(results, args);
                }
            }

            call_trampoline::<$( $x, )* Rets> as _

        }

        #[allow(non_snake_case)]
        pub(crate) fn [<gen_fn_callback_ $c_struct_name:lower>]
            <$( $x: FromToNativeWasmType, )* Rets: WasmTypeList, RetsAsResult: IntoResult<Rets>, T: Send + 'static,  Func: Fn(FunctionEnvMut<T>, $( $x , )*) -> RetsAsResult + 'static>
            (this: &Func) -> crate::backend::sys::vm::VMFunctionCallback {
            /// This is a function that wraps the real host
            /// function. Its address will be used inside the
            /// runtime.
            unsafe extern "C-unwind" fn func_wrapper<T: Send + 'static, $( $x, )* Rets, RetsAsResult, Func>( env: &StaticFunction<Func, T>, $( $x: <$x::Native as NativeWasmType>::Abi, )* ) -> Rets::CStruct
                where
                $( $x: FromToNativeWasmType, )*
                Rets: WasmTypeList,
                RetsAsResult: IntoResult<Rets>,
                Func: Fn(FunctionEnvMut<T>, $( $x , )*) -> RetsAsResult + 'static,
            {
                let result = wasmer_vm::on_host_stack(|| {
                    panic::catch_unwind(AssertUnwindSafe(|| {
                        let mut store_wrapper = unsafe { StoreContext::get_current(env.store_id) };
                        let mut store = store_wrapper.as_mut();
                        $(
                            let $x = unsafe {
                                FromToNativeWasmType::from_native(NativeWasmTypeInto::from_abi(&mut store, $x))
                            };
                        )*
                        let f_env = crate::backend::sys::function::env::FunctionEnvMut {
                            store_mut: store,
                            func_env: env.env.as_sys().clone(),
                        }.into();
                        to_invocation_result((env.func)(f_env, $($x),* ).into_result())
                    }))
                });

                // IMPORTANT: DO NOT ALLOCATE ON THE STACK,
                // AS WE ARE IN THE WASM STACK, NOT ON THE HOST ONE.
                // See: https://github.com/wasmerio/wasmer/pull/5700
                match result {
                    Ok(InvocationResult::Success(result)) => unsafe {
                        // Note: can't acquire a proper ref-counted context ref here, since we can switch
                        // away from the WASM stack at any time.
                        // Safety: The pointer is only used for the duration of the call to
                        // `into_c_struct` and `get_current_transient`.
                        let mut store_wrapper = StoreContext::get_current_transient(env.store_id);
                        let mut store = store_wrapper.as_mut().unwrap();
                        #[cfg(feature = "experimental-host-interrupt")]
                        if interrupt_registry::is_interrupted(store.objects.id()) {
                            raise_lib_trap(Trap::lib(TrapCode::HostInterrupt))
                        }
                        return result.into_c_struct(store);
                    },
                    Ok(InvocationResult::Exception(exception)) => unsafe {
                        // Note: can't acquire a proper ref-counted context ref here, since we can switch
                        // away from the WASM stack at any time.
                        // Safety: The pointer is only used for the duration of the call to `throw` and
                        // `is_interrupted`.
                        let mut store_wrapper = StoreContext::get_current_transient(env.store_id);
                        let mut store = store_wrapper.as_mut().unwrap();
                        #[cfg(feature = "experimental-host-interrupt")]
                        if interrupt_registry::is_interrupted(store.objects.id()) {
                            raise_lib_trap(Trap::lib(TrapCode::HostInterrupt))
                        }
                        wasmer_vm::libcalls::throw(
                            store.objects.as_sys(),
                            exception.vm_exceptionref().unwrap_sys_ref().to_u32_exnref(),
                        )
                    }
                    Ok(InvocationResult::Trap(trap)) => unsafe { raise_user_trap(trap) },
                    Ok(InvocationResult::YieldOutsideAsyncContext) => unsafe {
                        raise_lib_trap(Trap::lib(TrapCode::YieldOutsideAsyncContext))
                    },
                    Err(panic) => unsafe { resume_panic(panic) },
                }
            }
            func_wrapper::< T, $( $x, )* Rets, RetsAsResult, Func > as _
        }

        #[allow(non_snake_case)]
        pub(crate) fn [<gen_call_trampoline_address_ $c_struct_name:lower>]
            <$( $x: FromToNativeWasmType, )* Rets: WasmTypeList>
            () -> crate::backend::sys::vm::VMTrampoline {

            unsafe extern "C" fn call_trampoline<$( $x: FromToNativeWasmType, )* Rets: WasmTypeList>(
                  vmctx: *mut crate::backend::sys::vm::VMContext,
                  body: crate::backend::sys::vm::VMFunctionCallback,
                  args: *mut RawValue,
            ) {
                unsafe {
                    let body: unsafe extern "C" fn(vmctx: *mut crate::backend::sys::vm::VMContext, $( $x: <$x::Native as NativeWasmType>::Abi, )*) -> Rets::CStruct = std::mem::transmute(body);
                    let mut _n = 0;
                    $(
                    let $x = *args.add(_n).cast();
                    _n += 1;
                    )*

                    let results = body(vmctx, $( $x ),*);

                    Rets::write_c_struct_to_ptr(results, args);
                }
            }

            call_trampoline::<$( $x, )* Rets> as _
        }
    }};
}

// Here we go! Let's generate all the C struct, `WasmTypeList`
// implementations and `HostFunction` implementations.
impl_host_function!([C] S0,);
impl_host_function!([transparent] S1, A1);
impl_host_function!([C] S2, A1, A2);
impl_host_function!([C] S3, A1, A2, A3);
impl_host_function!([C] S4, A1, A2, A3, A4);
impl_host_function!([C] S5, A1, A2, A3, A4, A5);
impl_host_function!([C] S6, A1, A2, A3, A4, A5, A6);
impl_host_function!([C] S7, A1, A2, A3, A4, A5, A6, A7);
impl_host_function!([C] S8, A1, A2, A3, A4, A5, A6, A7, A8);
impl_host_function!([C] S9, A1, A2, A3, A4, A5, A6, A7, A8, A9);
impl_host_function!([C] S10, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10);
impl_host_function!([C] S11, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11);
impl_host_function!([C] S12, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12);
impl_host_function!([C] S13, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13);
impl_host_function!([C] S14, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14);
impl_host_function!([C] S15, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15);
impl_host_function!([C] S16, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16);
impl_host_function!([C] S17, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17);
impl_host_function!([C] S18, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18);
impl_host_function!([C] S19, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19);
impl_host_function!([C] S20, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20);
impl_host_function!([C] S21, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21);
impl_host_function!([C] S22, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22);
impl_host_function!([C] S23, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22, A23);
impl_host_function!([C] S24, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22, A23, A24);
impl_host_function!([C] S25, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22, A23, A24, A25);
impl_host_function!([C] S26, A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20, A21, A22, A23, A24, A25, A26);

#[cfg(test)]
mod reentrant_host_context {
    use super::*;
    use std::{cell::Cell, rc::Rc};

    /// Reentry uses the same host context as overlapping suspended calls. Run
    /// the trampoline directly so Miri can check its aliases without JIT code.
    #[test]
    fn a_dynamic_import_can_reenter_its_own_host_context() {
        type Host = DynamicFunction<Box<dyn Fn(*mut RawValue) -> HostCallOutcome>>;
        let mut store = crate::Store::new(crate::backend::sys::entities::engine::default_engine());
        let store_id = store.as_store_ref().objects().id();
        let context_ptr = Rc::new(Cell::new(std::ptr::null_mut::<
            VMDynamicFunctionContext<Host>,
        >()));
        let calls = Rc::new(Cell::new(0));
        let mut context = Box::new(VMDynamicFunctionContext {
            address: std::ptr::null(),
            ctx: Host {
                store_id,
                func_ty: FunctionType::new([], []),
                func: Box::new({
                    let context_ptr = context_ptr.clone();
                    let calls = calls.clone();
                    move |values| {
                        let previous = calls.replace(calls.get() + 1);
                        if previous == 0 {
                            unsafe {
                                Host::call_trampoline(
                                    context_ptr.get().cast(),
                                    std::ptr::null(),
                                    values,
                                );
                            }
                        }
                        HostCallOutcome::Ready(Ok(vec![]))
                    }
                }),
            },
        });
        context_ptr.set(&mut *context);
        let _installed = unsafe { StoreContext::install(store.as_store_mut().inner as *mut _) };
        unsafe {
            Host::call_trampoline(
                context_ptr.get().cast(),
                std::ptr::null(),
                std::ptr::null_mut(),
            );
        }
        assert_eq!(calls.get(), 2);
    }
}

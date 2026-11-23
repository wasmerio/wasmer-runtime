//! Data types, functions and traits for `sys` runtime's `Function` implementation

pub(crate) mod env;
pub(crate) mod typed;

#[cfg(feature = "experimental-async")]
use crate::{
    AsyncEnv, AsyncHostFunction, WithSlice,
    sys::async_runtime::{HostWait, take_host_result, wait_for_host_future},
};
use crate::{
    BackendFunction, CallWrapper, FunctionEnv, FunctionEnvMut, FunctionType, HostFunction,
    RuntimeError, StoreContext, StoreInner, StorePtrWrapper, Value, WithEnv, WithoutEnv,
    backend::sys::{engine::NativeEngineExt, vm::VMFunctionCallback},
    entities::function::call_wrapper::call_wrapped,
    entities::store::{AsStoreMut, AsStoreRef, StoreMut},
    utils::{FromToNativeWasmType, IntoResult, NativeWasmTypeInto, WasmTypeList},
    vm::{VMExtern, VMExternFunction},
};
use smallvec::{SmallVec, smallvec};
use std::panic::{self, AssertUnwindSafe};
use std::{
    any::Any,
    cell::UnsafeCell,
    cmp::max,
    error::Error,
    ffi::c_void,
    marker::PhantomData,
    ptr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
#[cfg(feature = "experimental-async")]
use std::{pin::Pin, rc::Rc};
use wasmer_types::{NativeWasmType, RawValue, StoreId, Type};
#[cfg(feature = "experimental-host-interrupt")]
use wasmer_vm::interrupt_registry;
use wasmer_vm::{
    MaybeInstanceOwned, StoreHandle, Trap, TrapCode, VMCallerCheckedAnyfunc, VMContext,
    VMDynamicFunctionContext, VMExceptionRef, VMFuncRef, VMFunction, VMFunctionBody,
    VMFunctionContext, VMFunctionKind, VMTrampoline, on_host_stack, raise_lib_trap,
    raise_user_trap, resume_panic, wasmer_call_trampoline,
};

/// The values of a synchronous call (its arguments and results, raw or not,
/// and the types of its results), inline for as many as most signatures
/// have, so that a call does not allocate for them.
type InlineValues<T> = SmallVec<[T; 8]>;

/// The results of an async import as its trampoline takes them, inline for
/// the usual few, so that a call does not allocate for them.
#[cfg(feature = "experimental-async")]
pub(crate) type RawResults = SmallVec<[RawValue; 4]>;

/// The arguments of a call as its trampoline takes them, which it overwrites
/// with the results. As many as the guest keeps on its own stack are inline
/// (see [`wasmer_vm::CallValues`]), so that a call does not allocate for them.
#[cfg(feature = "experimental-async")]
pub(crate) type RawValues = SmallVec<[RawValue; wasmer_vm::INLINE_CALL_VALUES]>;

/// The future of one call of an async import.
#[cfg(feature = "experimental-async")]
pub(crate) type RawHostFuture = Pin<Box<dyn Future<Output = Result<RawResults, RuntimeError>>>>;

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
                let mut args = InlineValues::<Value>::new();
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
                let result = func(env, &args);
                HostCallOutcome::Ready(result, store_wrapper)
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

    /// Creates an async host function from its body, which takes the
    /// arguments of a call and returns its future. The trampoline only reads
    /// the arguments and builds the future: nothing of the host function runs
    /// until the call polls it, on the executor's stack.
    #[cfg(feature = "experimental-async")]
    pub(crate) fn new_async<FT, F, Fut, T: Send + 'static>(
        store: &mut impl AsStoreMut,
        env: &FunctionEnv<T>,
        ty: FT,
        func: F,
    ) -> Self
    where
        FT: Into<FunctionType>,
        F: Fn(AsyncEnv<T>, Vec<Value>) -> Fut + 'static,
        Fut: Future<Output = Result<Vec<Value>, RuntimeError>> + 'static,
    {
        let function_type: FunctionType = ty.into();
        let func_ty = Rc::new(function_type.clone());
        let func_env = env.clone();
        let store_id = store.objects_mut().id();
        let wrapper = move |values_vec: *mut RawValue| -> HostCallOutcome {
            // Safety: this is the trampoline of an import, which borrows the
            // active entry. The borrow goes to the caller with the future,
            // for its first poll.
            let mut store = unsafe { StoreContext::get_current(store_id) };
            let mut store_mut = store.as_mut();
            let params = func_ty.params().iter().enumerate();
            // Safety: the trampoline gives `values_vec` one value per parameter.
            let args = params
                .map(|(i, ty)| unsafe {
                    Value::from_raw(&mut store_mut, *ty, values_vec.add(i).read_unaligned())
                })
                .collect();
            let future = func(AsyncEnv::new(func_env.clone(), store_id), args);
            let func_ty = Rc::clone(&func_ty);
            let future = Box::pin(async move {
                let values = future.await?;
                check_dynamic_results(&func_ty, &values)?;
                let slice = WithSlice::enter(store_id);
                // Safety: the slice is the only holder of the store.
                let store = unsafe { StoreMut::from_raw(slice.ptr()) };
                Ok(values.iter().map(|value| value.as_raw(&store)).collect())
            });
            HostCallOutcome::Future { future, store }
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
            on_native_stack: AtomicBool::new(false),
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
            host_data: HostFunctionData::boxed(host_data, |state| &state.on_native_stack, true),
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
            on_native_stack: AtomicBool::new(false),
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
            host_data: HostFunctionData::boxed(host_data, |state| &state.on_native_stack, true),
        };
        Self {
            handle: StoreHandle::new(store.objects_mut().as_sys_mut(), vm_function),
        }
    }

    /// Creates an async host function from a typed async function. Like
    /// [`Self::new_typed_with_env`], it is a static function: the guest calls
    /// its trampoline with the arguments of the function's own signature.
    #[cfg(feature = "experimental-async")]
    pub(crate) fn new_typed_async<T, F, Args, Rets>(
        store: &mut impl AsStoreMut,
        env: &FunctionEnv<T>,
        func: F,
    ) -> Self
    where
        T: Send + 'static,
        F: AsyncHostFunction<T, Args, Rets>,
        Args: WasmTypeList + 'static,
        Rets: WasmTypeList + 'static,
    {
        let func_ptr = <F as AsyncHostTrampolines<T, Args, Rets>>::function_callback(&func);
        // Its future may suspend the coroutine the call came from, which it
        // must do from the native stack: it always runs there.
        let host_data = Box::new(StaticFunction {
            store_id: store.objects_mut().id(),
            env: env.clone(),
            func: Rc::new(func),
            on_native_stack: AtomicBool::new(true),
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
            <F as AsyncHostTrampolines<T, Args, Rets>>::call_trampoline_address();
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
            host_data: HostFunctionData::boxed(host_data, |state| &state.on_native_stack, false),
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
        params: &[Value],
    ) -> Result<Box<[Value]>, RuntimeError> {
        let format_types_for_error_message = |items: &[Value]| {
            items
                .iter()
                .map(|param| param.ty().to_string())
                .collect::<Vec<String>>()
                .join(", ")
        };
        // The signature is borrowed from the function, not cloned.
        let store_ref = store.as_store_ref();
        let vm_function = self.handle.get(store_ref.objects().as_sys());
        let signature = &vm_function.signature;
        let trampoline = unsafe { vm_function.anyfunc.as_ptr().as_ref().call_trampoline };
        if signature.params().len() != params.len() {
            return Err(RuntimeError::new(format!(
                "Parameters of type [{}] did not match signature {}",
                format_types_for_error_message(params),
                signature
            )));
        }

        let mut values_vec: InlineValues<RawValue> =
            smallvec![RawValue { i32: 0 }; max(params.len(), signature.results().len())];

        // Store the argument values into `values_vec`.
        let param_tys = signature.params().iter();
        for ((arg, slot), ty) in params.iter().zip(&mut values_vec).zip(param_tys) {
            if arg.ty() != *ty {
                let param_types = format_types_for_error_message(params);
                return Err(RuntimeError::new(format!(
                    "Parameters of type [{param_types}] did not match signature {signature}",
                )));
            }
            if !arg.is_from_store(&store_ref) {
                return Err(RuntimeError::new("cross-`Store` values are not supported"));
            }
            *slot = arg.as_raw(&store_ref);
        }

        // The result types are copied out, and the borrow of the function
        // ends here: its signature lives in the store's function list, which
        // the call (or reading a funcref result) may grow and move.
        let result_types: InlineValues<Type> = signature.results().iter().copied().collect();

        // Invoke the call
        self.call_wasm_raw(store, wrapper, trampoline, &mut values_vec)?;
        Ok(unsafe { results_from_raw(store, &result_types, &values_vec) })
    }

    /// Calls the function with its arguments in `params`, which must also have
    /// room for its results: the call leaves them at the start of `params`.
    fn call_wasm_raw(
        &self,
        store: &mut impl AsStoreMut,
        wrapper: Option<&mut dyn CallWrapper>,
        trampoline: VMTrampoline,
        params: &mut [RawValue],
    ) -> Result<(), RuntimeError> {
        // Call the trampoline.
        let result = {
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
            let original_params = wrapper
                .is_some()
                .then(|| InlineValues::<RawValue>::from_slice(params));

            let r = call_wrapped(store, wrapper, |store| {
                if let Some(original_params) = &original_params {
                    params.copy_from_slice(original_params);
                }
                // Safety: the store context is uninstalled before this entry
                // returns, and the store mut is valid for its duration. It is
                // installed only while Wasm runs: the wrapper holds the store
                // between entries, so it must not be lendable then.
                let store_entry =
                    unsafe { StoreContext::new(store.as_store_mut().inner as *mut _) };
                let _store_install_guard = unsafe { store_entry.install() };
                let storeref = store.as_store_ref();
                let vm_function = self.handle.get(storeref.objects().as_sys());
                let config = storeref.engine().tunables().vmconfig();
                let signal_handler = storeref.signal_handler();
                unsafe {
                    wasmer_call_trampoline(
                        signal_handler,
                        config,
                        vm_function.anyfunc.as_ptr().as_ref().vmctx,
                        trampoline,
                        vm_function.anyfunc.as_ptr().as_ref().func_ptr,
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
        Ok(())
    }

    /// What `call_async` needs to enter the function on a stack of its own.
    #[cfg(feature = "experimental-async")]
    pub(crate) fn raw_entry(&self, store: &impl AsStoreRef) -> RawEntry {
        let function = self.handle.get(store.as_store_ref().objects().as_sys());
        let anyfunc = unsafe { function.anyfunc.as_ptr().as_ref() };
        RawEntry {
            vmctx: anyfunc.vmctx,
            trampoline: anyfunc.call_trampoline,
            callee: anyfunc.func_ptr,
        }
    }

    /// `params` as the trampoline of the function takes them, with room for
    /// its results, and the types of those.
    #[cfg(feature = "experimental-async")]
    pub(crate) fn raw_params(
        &self,
        store: &mut impl AsStoreMut,
        params: &[Value],
    ) -> Result<(RawValues, Vec<Type>), RuntimeError> {
        let store_ref = store.as_store_ref();
        let signature = &self.handle.get(store_ref.objects().as_sys()).signature;
        let types = |values: &[Value]| {
            let types = values.iter().map(|value| value.ty().to_string());
            types.collect::<Vec<String>>().join(", ")
        };
        if signature.params().len() != params.len()
            || params.iter().zip(signature.params()).any(|(param, ty)| param.ty() != *ty)
        {
            return Err(RuntimeError::new(format!(
                "Parameters of type [{}] did not match signature {signature}",
                types(params),
            )));
        }
        if params.iter().any(|param| !param.is_from_store(&store_ref)) {
            return Err(RuntimeError::new("cross-`Store` values are not supported"));
        }
        let mut values = RawValues::from_elem(
            RawValue { i32: 0 },
            max(params.len(), signature.results().len()),
        );
        for (param, slot) in params.iter().zip(&mut values) {
            *slot = param.as_raw(&store_ref);
        }
        Ok((values, signature.results().to_vec()))
    }

    pub(crate) fn result_arity(&self, store: &impl AsStoreRef) -> usize {
        let store = store.as_store_ref();
        self.handle.get(store.objects().as_sys()).signature.results().len()
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
        self.call_wasm(store, wrapper, params)
    }

    #[doc(hidden)]
    #[allow(missing_docs)]
    pub(crate) fn call_raw(
        &self,
        store: &mut impl AsStoreMut,
        mut params: Vec<RawValue>,
    ) -> Result<Box<[Value]>, RuntimeError> {
        let (trampoline, result_types) = {
            let vm_function = self.handle.get(store.objects_mut().as_sys());
            let trampoline = unsafe { vm_function.anyfunc.as_ptr().as_ref().call_trampoline };
            let result_types = InlineValues::<Type>::from_slice(vm_function.signature.results());
            (trampoline, result_types)
        };
        self.call_wasm_raw(store, None, trampoline, &mut params)?;
        Ok(unsafe { results_from_raw(store, &result_types, &params) })
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

    /// See [`crate::Function::set_calls_on_native_stack`].
    pub(crate) fn set_calls_on_native_stack(&self, store: &mut impl AsStoreMut, always: bool) {
        let objects = store.objects_mut().as_sys_mut();
        if let Some(data) = self.handle.get(objects).host_data.downcast_ref::<HostFunctionData>()
            && data.in_place
        {
            data.on_native_stack().store(always, Ordering::Relaxed);
        }
    }

    /// See [`crate::Function::calls_on_native_stack`].
    pub(crate) fn calls_on_native_stack(&self, store: &impl AsStoreRef) -> bool {
        let store = store.as_store_ref();
        let function = self.handle.get(store.objects().as_sys());
        match function.host_data.downcast_ref::<HostFunctionData>() {
            Some(data) => data.on_native_stack().load(Ordering::Relaxed),
            // A dynamic host function always runs on the native stack.
            None => function.kind == VMFunctionKind::Dynamic,
        }
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

/// Reads the results a call left at the start of `raw`.
///
/// # Safety
/// `raw` must start with values of `types`, as a call of a function with
/// these result types leaves them.
unsafe fn results_from_raw(
    store: &mut impl AsStoreMut,
    types: &[Type],
    raw: &[RawValue],
) -> Box<[Value]> {
    let mut values = Vec::with_capacity(types.len());
    for (&ty, &raw) in types.iter().zip(&raw[..types.len()]) {
        values.push(unsafe { Value::from_raw(store, ty, raw) });
    }
    values.into_boxed_slice()
}

/// Checks the results of a dynamic host function against its signature.
fn check_dynamic_results(func_ty: &FunctionType, returns: &[Value]) -> Result<(), RuntimeError> {
    let return_types = returns.iter().map(|ret| ret.ty());
    if return_types.ne(func_ty.results().iter().copied()) {
        return Err(RuntimeError::new(format!(
            "Dynamic function returned wrong signature. Expected {:?} but got {:?}",
            func_ty.results(),
            returns.iter().map(|ret| ret.ty())
        )));
    }
    Ok(())
}

/// Checks the results of a dynamic host function and writes them back for its
/// trampoline, with the borrow of the store the host function ran with.
fn write_dynamic_results(
    store_wrapper: &mut StorePtrWrapper,
    func_ty: &FunctionType,
    returns: &[Value],
    values_vec: *mut RawValue,
) -> Result<(), RuntimeError> {
    let store = store_wrapper.as_mut();
    check_dynamic_results(func_ty, returns)?;
    for (i, ret) in returns.iter().enumerate() {
        unsafe {
            values_vec.add(i).write_unaligned(ret.as_raw(&store));
        }
    }
    Ok(())
}

/// Gives the trampoline the results of an async import.
///
/// # Safety
/// `values_vec` must have room for the results of the import.
#[cfg(feature = "experimental-async")]
unsafe fn write_raw_results(results: &[RawValue], values_vec: *mut RawValue) {
    for (i, raw) in results.iter().enumerate() {
        unsafe { values_vec.add(i).write_unaligned(*raw) };
    }
}

/// What the trampoline of a typed async host function does with the
/// arguments the guest called it with, which `args` makes typed. It polls
/// the future of the call once and, if the future is not ready, suspends the
/// guest until it is.
///
/// A future that is ready at once gives its results typed, as a typed sync
/// host function does. Only one the guest waits for is made to give them
/// raw, for the call to keep.
///
/// # Safety
/// Called on the Wasm stack, by the trampoline of `env`'s function.
#[cfg(feature = "experimental-async")]
pub(crate) unsafe fn call_typed_async<T, F, Args, Rets>(
    env: &StaticFunction<Rc<F>, T>,
    args: impl FnOnce(&mut StoreMut<'_>) -> Args,
) -> Rets::CStruct
where
    T: 'static,
    F: AsyncHostFunction<T, Args, Rets>,
    Args: WasmTypeList + 'static,
    Rets: WasmTypeList + 'static,
{
    let store_id = env.store_id;
    // Asked here, on the Wasm stack: whether this is the stack of a
    // `call_async`, which an async import may suspend.
    let suspendable = wasmer_vm::wasm_call_is_suspendable();
    // `None` if the guest is to wait for the host future of the import. The
    // results come with the pointer of the store's active entry, which the
    // host stack has in hand and the Wasm stack converts them with.
    let mut result = on_host_stack(|| {
        panic::catch_unwind(AssertUnwindSafe(|| {
            // Safety: this is the trampoline of an import, which borrows the
            // active entry. The first poll of the future keeps the borrow.
            let mut store = unsafe { StoreContext::get_current(store_id) };
            let store_ptr = store.as_ptr();
            let args = args(&mut store.as_mut());
            let mut cx = AsyncEnv::new(env.env.clone(), store_id);
            let func = Rc::clone(&env.func);
            let future = Box::pin(async move { func.invoke(&mut cx, args).await });
            let into_pending = |future: Pin<Box<_>>| -> RawHostFuture {
                Box::pin(async move {
                    let rets: Rets = future.await?;
                    let slice = WithSlice::enter(store_id);
                    // Safety: the slice is the only holder of the store, and
                    // the trampoline reads the raw values back as `Rets`.
                    let mut array = unsafe {
                        let mut store = StoreMut::from_raw(slice.ptr());
                        rets.into_array(&mut store)
                    };
                    Ok(RawResults::from_slice(array.as_mut()))
                })
            };
            match wait_for_host_future(store, future, suspendable, into_pending) {
                HostWait::Ready(result) => {
                    Some(to_invocation_result(result.map(|rets| (rets, store_ptr))))
                }
                HostWait::Suspend => None,
                HostWait::CannotSuspend => Some(InvocationResult::YieldOutsideAsyncContext),
            }
        }))
    });
    if matches!(result, Ok(None)) {
        // The guest waits here, with nothing on this frame to release: its
        // call polls the host future, and resumes it with what the future
        // returned, or drops it.
        unsafe { wasmer_vm::suspend_wasm_call() };
        result = on_host_stack(|| {
            panic::catch_unwind(AssertUnwindSafe(|| {
                Some(to_invocation_result(take_host_result().map(|results| {
                    // Safety: the trampoline of an import again, on the entry
                    // that the call installed to resume the guest.
                    let mut store = unsafe { StoreContext::get_current(store_id) };
                    let rets = unsafe { Rets::from_slice(&mut store.as_mut(), &results) };
                    let rets = rets.expect("the future of a call returns one value per result");
                    (rets, store.as_ptr())
                })))
            }))
        });
    }
    let result = result.map(|result| result.expect("a guest that waited was resumed"));

    // IMPORTANT: DO NOT ALLOCATE ON THE STACK,
    // AS WE ARE IN THE WASM STACK, NOT ON THE HOST ONE.
    // See: https://github.com/wasmerio/wasmer/pull/5700
    match result {
        Ok(InvocationResult::Success((rets, store))) => unsafe {
            #[cfg(feature = "experimental-host-interrupt")]
            if interrupt_registry::is_interrupted(store_id) {
                raise_lib_trap(Trap::lib(TrapCode::HostInterrupt))
            }
            // Safety: the pointer of the active entry, which nothing has
            // replaced since the host stack took it. It is used only for the
            // duration of the call to `into_c_struct`.
            rets.into_c_struct(&mut *store)
        },
        Ok(InvocationResult::Exception(exception)) => unsafe {
            // Note: can't acquire a proper ref-counted context ref here, since we can switch
            // away from the WASM stack at any time.
            // Safety: The pointer is only used for the duration of the call to `throw` and
            // `is_interrupted`.
            let mut store_wrapper = StoreContext::get_current_transient(store_id);
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

/// See [`Function::raw_entry`].
#[cfg(feature = "experimental-async")]
pub(crate) struct RawEntry {
    pub(crate) vmctx: wasmer_vm::VMFunctionContext,
    pub(crate) trampoline: VMTrampoline,
    pub(crate) callee: *const wasmer_vm::VMFunctionBody,
}

pub(crate) enum HostCallOutcome {
    /// The results of the host function, and the borrow of the store it ran
    /// with, which writing the results back reuses.
    Ready(Result<Vec<Value>, RuntimeError>, StorePtrWrapper),
    /// The future of an async import, and the borrow of the store its
    /// arguments were read with, which its first poll reuses.
    #[cfg(feature = "experimental-async")]
    Future {
        future: RawHostFuture,
        store: StorePtrWrapper,
    },
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
        this: &mut VMDynamicFunctionContext<Self>,
        values_vec: *mut RawValue,
    ) {
        // Asked here, on the Wasm stack: whether this is the stack of a
        // `call_async`, which an async import may suspend.
        #[cfg(feature = "experimental-async")]
        let suspendable = wasmer_vm::wasm_call_is_suspendable();
        // `None` if the guest is to wait for the host future of the import.
        #[allow(unused_mut)]
        let mut result = on_host_stack(|| {
            panic::catch_unwind(AssertUnwindSafe(|| match (this.ctx.func)(values_vec) {
                HostCallOutcome::Ready(result, mut store) => {
                    Some(to_invocation_result(result.and_then(|values| {
                        write_dynamic_results(&mut store, &this.ctx.func_ty, &values, values_vec)
                    })))
                }
                #[cfg(feature = "experimental-async")]
                HostCallOutcome::Future { future, store } => {
                    match wait_for_host_future(store, future, suspendable, |future| future) {
                        HostWait::Ready(result) => Some(to_invocation_result(result.map(
                            |results: RawResults| unsafe { write_raw_results(&results, values_vec) },
                        ))),
                        HostWait::Suspend => None,
                        HostWait::CannotSuspend => Some(InvocationResult::YieldOutsideAsyncContext),
                    }
                }
            }))
        });
        #[cfg(feature = "experimental-async")]
        if matches!(result, Ok(None)) {
            // The guest waits here, with nothing on this frame to release: its
            // call polls the host future, and resumes it with what the future
            // returned, or drops it.
            unsafe { wasmer_vm::suspend_wasm_call() };
            result = on_host_stack(|| {
                panic::catch_unwind(AssertUnwindSafe(|| {
                    Some(to_invocation_result(take_host_result().map(
                        |results: RawResults| unsafe { write_raw_results(&results, values_vec) },
                    )))
                }))
            });
        }
        let result = result.map(|result| result.expect("a guest that waited was resumed"));

        // IMPORTANT: DO NOT ALLOCATE ON THE STACK,
        // AS WE ARE IN THE WASM STACK, NOT ON THE HOST ONE.
        // See: https://github.com/wasmerio/wasmer/pull/5700
        match result {
            // The results are in `values_vec` already, so the store is not
            // looked up again.
            Ok(InvocationResult::Success(())) => {
                #[cfg(feature = "experimental-host-interrupt")]
                if interrupt_registry::is_interrupted(this.ctx.store_id) {
                    unsafe { raise_lib_trap(Trap::lib(TrapCode::HostInterrupt)) }
                }
            }
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
            let dynamic_function = &mut *(vmctx as *mut VMDynamicFunctionContext<Self>);
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
    /// Whether every call runs on the native stack, which
    /// [`crate::Function::set_calls_on_native_stack`] sets (see
    /// [`HostFunctionData`]). Only the thread that holds the store writes it.
    pub(crate) on_native_stack: AtomicBool,
}

/// What the `VMFunction` of a typed host function owns: the function's state,
/// and where in it the setting is that
/// [`crate::Function::set_calls_on_native_stack`] writes.
struct HostFunctionData {
    /// Where the setting is in `_state`, in bytes from its start. An offset
    /// rather than a pointer: moving the box asserts that it is the only way
    /// to its contents, which would invalidate a pointer taken before.
    on_native_stack_offset: usize,
    /// Whether a call may run on the Wasm stack at all: false for
    /// asynchronous functions, whose setting says they always run on the
    /// native stack, and keeps saying so.
    in_place: bool,
    _state: Box<dyn Any>,
}

impl HostFunctionData {
    fn boxed<S: 'static>(
        state: Box<S>,
        on_native_stack: fn(&S) -> &AtomicBool,
        in_place: bool,
    ) -> Box<dyn Any> {
        let setting = ptr::from_ref(on_native_stack(&state)).addr();
        let on_native_stack_offset = setting - ptr::from_ref::<S>(&state).addr();
        Box::new(Self {
            on_native_stack_offset,
            in_place,
            _state: state,
        })
    }

    fn on_native_stack(&self) -> &AtomicBool {
        let state = ptr::from_ref::<dyn Any>(&*self._state).cast::<u8>();
        // Safety: `boxed` measured where the setting is in the state, which is
        // still of the type it was measured in, and the reference is derived
        // from a borrow of the state that it does not outlive.
        unsafe { &*state.add(self.on_native_stack_offset).cast::<AtomicBool>() }
    }
}

/// The trampolines of a typed async host function, which make it a static
/// function of this backend. Every [`AsyncHostFunction`] has them.
#[cfg(feature = "experimental-async")]
pub(crate) trait AsyncHostTrampolines<T, Args, Rets> {
    /// The trampoline a guest calls the function through.
    fn function_callback(&self) -> VMFunctionCallback;

    /// The trampoline that calls the one above with an array of values.
    fn call_trampoline_address() -> VMTrampoline;
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
            // With `IN_PLACE`, a call runs on the Wasm stack it came from when
            // enough of it is left, and otherwise goes to the other instance,
            // which switches to the native stack. That instance stays out of
            // line, and with it the registers the switch makes every caller
            // save.
            #[inline(never)]
            unsafe extern "C-unwind" fn func_wrapper<const IN_PLACE: bool, $( $x, )* Rets, RetsAsResult, Func>( env: &StaticFunction<Func, ()>, $( $x: <$x::Native as NativeWasmType>::Abi, )* ) -> Rets::CStruct
            where
                $( $x: FromToNativeWasmType, )*
                Rets: WasmTypeList,
                RetsAsResult: IntoResult<Rets>,
                Func: Fn($( $x , )*) -> RetsAsResult + 'static,
            {
                let call = || {
                    panic::catch_unwind(AssertUnwindSafe(|| {
                        let mut store_wrapper = unsafe { StoreContext::get_current(env.store_id) };
                        let store_ptr = store_wrapper.as_ptr();
                        let mut store = store_wrapper.as_mut();
                        $(
                            let $x = unsafe {
                                FromToNativeWasmType::from_native(NativeWasmTypeInto::from_abi(&mut store, $x))
                            };
                        )*
                        let result = (env.func)($($x),* ).into_result();
                        to_invocation_result(result.map(|rets| (rets, store_ptr)))
                    }))
                };
                let result = if IN_PLACE {
                    let in_place = if env.on_native_stack.load(Ordering::Relaxed) {
                        None
                    } else {
                        wasmer_vm::host_call_in_place()
                    };
                    let Some(in_place) = in_place else {
                        return unsafe { func_wrapper::<false, $( $x, )* Rets, RetsAsResult, Func>(env, $( $x ),*) };
                    };
                    let result = call();
                    drop(in_place);
                    result
                } else {
                    on_host_stack(call)
                };

                // IMPORTANT: DO NOT ALLOCATE ON THE STACK,
                // AS WE ARE IN THE WASM STACK, NOT ON THE HOST ONE.
                // See: https://github.com/wasmerio/wasmer/pull/5700
                match result {
                    Ok(InvocationResult::Success((result, store))) => unsafe {
                        #[cfg(feature = "experimental-host-interrupt")]
                        if interrupt_registry::is_interrupted(env.store_id) {
                            raise_lib_trap(Trap::lib(TrapCode::HostInterrupt))
                        }
                        // Safety: the pointer the host function borrowed the
                        // store through, which is the active entry's: nothing
                        // has replaced it since. It is used only for the
                        // duration of the call to `into_c_struct`.
                        return result.into_c_struct(&mut *store);
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

            func_wrapper::< true, $( $x, )* Rets, RetsAsResult, Func > as _

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
            // See the function without an environment.
            #[inline(never)]
            unsafe extern "C-unwind" fn func_wrapper<const IN_PLACE: bool, T: Send + 'static, $( $x, )* Rets, RetsAsResult, Func>( env: &StaticFunction<Func, T>, $( $x: <$x::Native as NativeWasmType>::Abi, )* ) -> Rets::CStruct
                where
                $( $x: FromToNativeWasmType, )*
                Rets: WasmTypeList,
                RetsAsResult: IntoResult<Rets>,
                Func: Fn(FunctionEnvMut<T>, $( $x , )*) -> RetsAsResult + 'static,
            {
                let call = || {
                    panic::catch_unwind(AssertUnwindSafe(|| {
                        let mut store_wrapper = unsafe { StoreContext::get_current(env.store_id) };
                        let store_ptr = store_wrapper.as_ptr();
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
                        let result = (env.func)(f_env, $($x),* ).into_result();
                        to_invocation_result(result.map(|rets| (rets, store_ptr)))
                    }))
                };
                let result = if IN_PLACE {
                    let in_place = if env.on_native_stack.load(Ordering::Relaxed) {
                        None
                    } else {
                        wasmer_vm::host_call_in_place()
                    };
                    let Some(in_place) = in_place else {
                        return unsafe { func_wrapper::<false, T, $( $x, )* Rets, RetsAsResult, Func>(env, $( $x ),*) };
                    };
                    let result = call();
                    drop(in_place);
                    result
                } else {
                    on_host_stack(call)
                };

                // IMPORTANT: DO NOT ALLOCATE ON THE STACK,
                // AS WE ARE IN THE WASM STACK, NOT ON THE HOST ONE.
                // See: https://github.com/wasmerio/wasmer/pull/5700
                match result {
                    Ok(InvocationResult::Success((result, store))) => unsafe {
                        #[cfg(feature = "experimental-host-interrupt")]
                        if interrupt_registry::is_interrupted(env.store_id) {
                            raise_lib_trap(Trap::lib(TrapCode::HostInterrupt))
                        }
                        // Safety: the pointer the host function borrowed the
                        // store through, which is the active entry's: nothing
                        // has replaced it since. It is used only for the
                        // duration of the call to `into_c_struct`.
                        return result.into_c_struct(&mut *store);
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
            func_wrapper::< true, T, $( $x, )* Rets, RetsAsResult, Func > as _
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

        #[cfg(feature = "experimental-async")]
        #[allow(non_snake_case, unused_parens)]
        impl<T, $( $x, )* Rets, RetsAsResult, Func> AsyncHostTrampolines<T, ( $( $x ),* ), Rets> for Func
        where
            T: 'static,
            Func: for<'e> AsyncFn(&'e mut AsyncEnv<T>, $( $x ),*) -> RetsAsResult + 'static,
            RetsAsResult: IntoResult<Rets> + 'static,
            Rets: WasmTypeList + 'static,
            ( $( $x ),* ): WasmTypeList + 'static,
            $( $x: FromToNativeWasmType + 'static, )*
        {
            fn function_callback(&self) -> VMFunctionCallback {
                #[allow(unused_variables, clippy::unused_unit)]
                unsafe extern "C-unwind" fn func_wrapper<T, $( $x, )* Rets, Func>( env: &StaticFunction<Rc<Func>, T>, $( $x: <$x::Native as NativeWasmType>::Abi, )* ) -> Rets::CStruct
                where
                    T: 'static,
                    $( $x: FromToNativeWasmType + 'static, )*
                    Rets: WasmTypeList + 'static,
                    ( $( $x ),* ): WasmTypeList + 'static,
                    Func: AsyncHostFunction<T, ( $( $x ),* ), Rets>,
                {
                    unsafe {
                        call_typed_async(env, |store: &mut StoreMut<'_>| {
                            ( $( FromToNativeWasmType::from_native(NativeWasmTypeInto::from_abi(&mut *store, $x)) ),* )
                        })
                    }
                }
                func_wrapper::< T, $( $x, )* Rets, Self > as _
            }

            /// The guest calls the trampoline as it calls the one of a
            /// function with an environment, so the two share this one.
            fn call_trampoline_address() -> VMTrampoline {
                [<gen_call_trampoline_address_ $c_struct_name:lower>]::<$( $x, )* Rets>()
            }
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

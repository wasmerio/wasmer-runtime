//! Data types, functions and traits for `v8` runtime's `Function` implementation.
#![allow(non_snake_case)]
use std::{
    ffi::c_void,
    panic::{self, AssertUnwindSafe},
};

use crate::{
    AsStoreMut, AsStoreRef, BackendFunction, BackendFunctionEnvMut, BackendTrap,
    FromToNativeWasmType, FunctionEnv, FunctionEnvMut, IntoResult, NativeWasmType,
    NativeWasmTypeInto, RuntimeError, StoreMut, Value, WasmTypeList, WithEnv, WithoutEnv,
    v8::{
        bindings::*,
        utils::convert::{IntoCApiType, IntoCApiValue, IntoWasmerType, IntoWasmerValue},
        vm::{VMFuncRef, VMFunction, VMFunctionCallback, VMFunctionEnvironment},
    },
    vm::{VMExtern, VMExternFunction},
};

use crate::backend::v8::{
    origin::{Handle, Origin},
    references::{OwnedValue, Values},
};

use super::{super::error::Trap, check_isolate, store::StoreHandle};
use itertools::Itertools;
use wasmer_types::{FunctionType, RawValue};

pub(crate) mod env;
pub(crate) mod typed;

pub use typed::*;

type CCallback = unsafe extern "C" fn(
    *mut c_void,
    *const wasm_val_vec_t,
    *mut wasm_val_vec_t,
) -> *mut wasm_trap_t;

#[derive(Clone, PartialEq, Eq)]
/// A WebAssembly `function` in `v8`.
pub struct Function {
    pub(crate) handle: VMFunction,
}

unsafe impl Send for Function {}
unsafe impl Sync for Function {}

impl From<VMFunction> for Function {
    fn from(handle: VMFunction) -> Self {
        Self { handle }
    }
}

pub(crate) struct FunctionCallbackEnv<'a, F> {
    pub(crate) store: *mut crate::StoreInner,
    pub(crate) marker: std::marker::PhantomData<&'a mut crate::StoreInner>,
    #[cfg(feature = "experimental-async")]
    pub(crate) binding: std::rc::Weak<crate::backend::v8::async_runtime::AsyncBinding>,
    pub(crate) raw_store: *mut wasm_store_t,
    pub(crate) func: F,
    pub(crate) env_handle: Option<StoreHandle<VMFunctionEnvironment>>,
    pub(crate) env_identity: Option<std::sync::Weak<()>>,
    pub(crate) origin: Origin,
    pub(crate) signature: FunctionType,
}

impl<'a, F> std::fmt::Debug for FunctionCallbackEnv<'a, F> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FunctionCallbackEnv")
            .field("env_is_some", &self.env_handle.is_some())
            .finish()
    }
}

impl Function {
    /// To `VMExtern`.
    pub fn to_vm_extern(&self) -> VMExtern {
        let extern_ = unsafe { wasm_func_as_extern(self.handle.raw()) };
        assert!(
            !extern_.is_null(),
            "Returned null Function extern from wasm-c-api"
        );
        VMExtern::V8(Handle::new(extern_, self.handle.origin.clone()))
    }

    #[allow(clippy::cast_ptr_alignment)]
    pub fn new_with_env<FT, F, T: Send + 'static>(
        store: &mut impl AsStoreMut,
        env: &FunctionEnv<T>,
        ty: FT,
        func: F,
    ) -> Self
    where
        FT: Into<FunctionType>,
        F: Fn(FunctionEnvMut<'_, T>, &[Value]) -> Result<Vec<Value>, RuntimeError>
            + 'static
            + Send
            + Sync,
    {
        check_isolate(store);

        let fn_ty: FunctionType = ty.into();
        let params = fn_ty.params();

        let mut param_types = params
            .iter()
            .map(|param| {
                let kind = (*param).into_ct();
                unsafe { wasm_valtype_new(kind) }
            })
            .collect::<Vec<_>>();

        let mut wasm_param_types = unsafe {
            let mut vec = Default::default();
            wasm_valtype_vec_new(&mut vec, param_types.len(), param_types.as_ptr());
            vec
        };

        let results = fn_ty.results();
        let mut result_types = results
            .iter()
            .map(|param| {
                let kind = (*param).into_ct();
                unsafe { wasm_valtype_new(kind) }
            })
            .collect::<Vec<_>>();

        let mut wasm_result_types = unsafe {
            let mut vec = Default::default();
            wasm_valtype_vec_new(&mut vec, result_types.len(), result_types.as_ptr());
            vec
        };

        let wasm_functype = unsafe {
            wasm_functype_new(
                &mut wasm_param_types as *mut _,
                &mut wasm_result_types as *mut _,
            )
        };

        let mut store = store.as_store_mut();
        let inner = store.inner.store.as_v8().inner;
        let origin = store.inner.store.as_v8().origin.capture();

        let callback: CCallback = make_fn_callback(&func, param_types.len());

        let mut callback_env: *mut FunctionCallbackEnv<'_, F> =
            Box::leak(Box::new(FunctionCallbackEnv {
                store: store.as_raw(),
                marker: std::marker::PhantomData,
                #[cfg(feature = "experimental-async")]
                binding: std::rc::Rc::downgrade(&store.inner.store.as_v8().async_binding),
                raw_store: inner,
                func,
                env_handle: Some(env.as_v8().handle.clone()),
                env_identity: Some(env.as_v8().identity.clone()),
                origin: origin.clone(),
                signature: fn_ty.clone(),
            }));

        let wasm_function = unsafe {
            wasm_func_new_with_env(
                inner,
                wasm_functype,
                Some(callback),
                callback_env as *mut _ as _,
                None,
            )
        };

        if wasm_function.is_null() {
            panic!("failed when creating new typed function");
        }

        Self {
            handle: Handle::new(
                store
                    .inner
                    .store
                    .as_v8()
                    .reference_roots
                    .function(wasm_function),
                origin,
            ),
        }
    }

    /// Creates a new host `Function` from a native function.
    pub fn new_typed<F, Args, Rets>(store: &mut impl AsStoreMut, func: F) -> Self
    where
        F: crate::HostFunction<(), Args, Rets, WithoutEnv> + 'static + Send + Sync,
        Args: WasmTypeList,
        Rets: WasmTypeList,
    {
        check_isolate(store);
        let mut param_types = Args::wasm_types()
            .iter()
            .map(|param| {
                let kind = (*param).into_ct();
                unsafe { wasm_valtype_new(kind) }
            })
            .collect::<Vec<_>>();

        let mut wasm_param_types = unsafe {
            let mut vec = Default::default();
            wasm_valtype_vec_new(&mut vec, param_types.len(), param_types.as_ptr());
            vec
        };

        let mut result_types = Rets::wasm_types()
            .iter()
            .map(|param| {
                let kind = (*param).into_ct();
                unsafe { wasm_valtype_new(kind) }
            })
            .collect::<Vec<_>>();

        let mut wasm_result_types = unsafe {
            let mut vec = Default::default();
            wasm_valtype_vec_new(&mut vec, result_types.len(), result_types.as_ptr());
            vec
        };

        let wasm_functype =
            unsafe { wasm_functype_new(&mut wasm_param_types, &mut wasm_result_types) };

        let mut store = store.as_store_mut();
        let inner = store.inner.store.as_v8().inner;
        let origin = store.inner.store.as_v8().origin.capture();

        let callback: CCallback = unsafe {
            std::mem::transmute(func.function_callback(crate::BackendKind::V8).unwrap_v_8())
        };

        let mut callback_env: *mut FunctionCallbackEnv<'_, F> =
            Box::into_raw(Box::new(FunctionCallbackEnv {
                store: store.as_raw(),
                marker: std::marker::PhantomData,
                #[cfg(feature = "experimental-async")]
                binding: std::rc::Rc::downgrade(&store.inner.store.as_v8().async_binding),
                raw_store: inner,
                func,
                env_handle: None,
                env_identity: None,
                origin: origin.clone(),
                signature: FunctionType::new(
                    Args::wasm_types().to_vec(),
                    Rets::wasm_types().to_vec(),
                ),
            }));

        let wasm_function = unsafe {
            wasm_func_new_with_env(
                inner,
                wasm_functype,
                Some(callback),
                callback_env as _,
                None,
            )
        };

        if wasm_function.is_null() {
            panic!("failed when creating new typed function");
        }

        Self {
            handle: Handle::new(
                store
                    .inner
                    .store
                    .as_v8()
                    .reference_roots
                    .function(wasm_function),
                origin,
            ),
        }
    }

    pub fn new_typed_with_env<T, F, Args, Rets>(
        store: &mut impl AsStoreMut,
        env: &FunctionEnv<T>,
        func: F,
    ) -> Self
    where
        F: crate::HostFunction<T, Args, Rets, WithEnv>,
        Args: WasmTypeList,
        Rets: WasmTypeList,
        T: Send + 'static,
    {
        check_isolate(store);
        let mut param_types = Args::wasm_types()
            .iter()
            .map(|param| {
                let kind = (*param).into_ct();
                unsafe { wasm_valtype_new(kind) }
            })
            .collect::<Vec<_>>();

        let mut wasm_param_types = unsafe {
            let mut vec = wasm_valtype_vec_t::default();
            wasm_valtype_vec_new(&mut vec, param_types.len(), param_types.as_ptr());
            vec
        };

        let mut result_types = Rets::wasm_types()
            .iter()
            .map(|param| {
                let kind = (*param).into_ct();
                unsafe { wasm_valtype_new(kind) }
            })
            .collect::<Vec<_>>();

        let mut wasm_result_types = unsafe {
            let mut vec: wasm_valtype_vec_t = Default::default();
            wasm_valtype_vec_new(&mut vec, result_types.len(), result_types.as_ptr());
            vec
        };

        let wasm_functype = unsafe {
            wasm_functype_new(
                &mut wasm_param_types as *mut _,
                &mut wasm_result_types as *mut _,
            )
        };

        let mut store = store.as_store_mut();
        let inner = store.inner.store.as_v8().inner;
        let origin = store.inner.store.as_v8().origin.capture();

        let callback: CCallback = unsafe {
            std::mem::transmute(func.function_callback(crate::BackendKind::V8).unwrap_v_8())
        };

        let mut callback_env: *mut FunctionCallbackEnv<'_, F> =
            Box::into_raw(Box::new(FunctionCallbackEnv {
                store: store.as_raw(),
                marker: std::marker::PhantomData,
                #[cfg(feature = "experimental-async")]
                binding: std::rc::Rc::downgrade(&store.inner.store.as_v8().async_binding),
                raw_store: inner,
                func,
                env_handle: Some(env.as_v8().handle.clone()),
                env_identity: Some(env.as_v8().identity.clone()),
                origin: origin.clone(),
                signature: FunctionType::new(
                    Args::wasm_types().to_vec(),
                    Rets::wasm_types().to_vec(),
                ),
            }));

        let wasm_function = unsafe {
            wasm_func_new_with_env(
                inner,
                wasm_functype,
                Some(callback),
                callback_env as _,
                None,
            )
        };

        if wasm_function.is_null() {
            panic!("failed when creating new typed function");
        }

        Self {
            handle: Handle::new(
                store
                    .inner
                    .store
                    .as_v8()
                    .reference_roots
                    .function(wasm_function),
                origin,
            ),
        }
    }

    pub fn ty(&self, _store: &impl AsStoreRef) -> FunctionType {
        check_isolate(_store);
        self.handle
            .origin
            .check_store(_store)
            .expect("V8 function belongs to another Store");
        let type_ = unsafe { wasm_func_type(self.handle.raw()) };
        let params: *const wasm_valtype_vec_t = unsafe { wasm_functype_params(type_) };
        let returns: *const wasm_valtype_vec_t = unsafe { wasm_functype_results(type_) };

        let params: Vec<wasmer_types::Type> = unsafe {
            let mut res = vec![];
            for i in 0..(*params).size {
                res.push((*(*params).data.wrapping_add(i)).into_wt());
            }
            res
        };

        let returns: Vec<wasmer_types::Type> = unsafe {
            let mut res = vec![];
            for i in 0..(*returns).size {
                res.push((*(*returns).data.wrapping_add(i)).into_wt());
            }
            res
        };

        FunctionType::new(params, returns)
    }

    pub fn call_raw(
        &self,
        _store: &mut impl AsStoreMut,
        _params: Vec<RawValue>,
    ) -> Result<Box<[Value]>, RuntimeError> {
        // There is no optimal call_raw in JSC, so we just
        // simply rely the call
        // self.call(store, params)
        unimplemented!();
    }

    pub fn call(
        &self,
        store: &mut impl AsStoreMut,
        params: &[Value],
    ) -> Result<Box<[Value]>, RuntimeError> {
        self.handle.origin.check_store(store)?;
        check_isolate(store);
        if params.iter().any(|value| !value.is_from_store(store)) {
            return Err(RuntimeError::new(
                "V8 function parameter belongs to another Store",
            ));
        }
        // unimplemented!();
        let signature = self.ty(store);
        if signature.params().len() != params.len()
            || signature
                .params()
                .iter()
                .zip(params)
                .any(|(ty, param)| *ty != param.ty())
        {
            let param_types = params
                .iter()
                .map(|param| param.ty().to_string())
                .collect_vec()
                .join(", ");

            return Err(RuntimeError::new(format!(
                "Parameters of type [{}] did not match signature {}",
                param_types, signature
            )));
        }

        // let wasm_func_param_arity(self.handle)

        let mut results = Values::empty(signature.results().len());

        let mut trap;

        loop {
            results.clear();
            let arguments = Values::from_values(params, signature.params())?;
            let function = self.handle.raw();
            let raw_results = &mut results.0;
            let raw_arguments = arguments.into_call_arguments();
            trap = unsafe { wasm_func_call(function, &raw_arguments, raw_results) };
            let store_mut = store.as_store_mut();
            if let Some(callback) = store_mut.inner.on_called.take() {
                match callback(store_mut) {
                    Ok(wasmer_types::OnCalledAction::InvokeAgain) => {
                        continue;
                    }
                    Ok(wasmer_types::OnCalledAction::Finish) => {
                        break;
                    }
                    Ok(wasmer_types::OnCalledAction::Trap(trap)) => {
                        return Err(RuntimeError::user(trap));
                    }
                    Err(trap) => return Err(RuntimeError::user(trap)),
                }
            }
            break;
        }

        if !trap.is_null() {
            return Err(Into::<Trap>::into(trap).into());
        }

        crate::backend::v8::utils::convert::validate_raw_values(
            results.as_slice(),
            signature.results(),
        )?;
        let values = results
            .as_slice()
            .iter()
            .zip(signature.results())
            .map(|(value, ty)| (*value).into_wv(*ty, store))
            .collect::<Result<Vec<_>, RuntimeError>>()?
            .into_boxed_slice();
        Ok(values)
    }

    pub(crate) fn from_vm_extern(store: &mut impl AsStoreMut, internal: VMExternFunction) -> Self {
        let handle = internal.unwrap_v_8();
        handle
            .origin
            .check_store(store)
            .expect("foreign V8 function export");
        Self { handle }
    }

    pub(crate) fn vm_funcref(&self, _store: &impl AsStoreRef) -> VMFuncRef {
        unimplemented!();
    }

    pub(crate) unsafe fn from_vm_funcref(
        _store: &mut impl AsStoreMut,
        _funcref: VMFuncRef,
    ) -> Self {
        unimplemented!();
    }

    /// Checks whether this `Function` can be used with the given context.
    pub fn is_from_store(&self, store: &impl AsStoreRef) -> bool {
        self.handle.origin.check_store(store).is_ok()
    }
}

fn make_fn_callback<F, T: Send + 'static>(func: &F, args: usize) -> CCallback
where
    F: Fn(FunctionEnvMut<'_, T>, &[Value]) -> Result<Vec<Value>, RuntimeError>
        + 'static
        + Send
        + Sync,
{
    unsafe extern "C" fn fn_callback<F, T: Send + 'static>(
        env: *mut c_void,
        args: *const wasm_val_vec_t,
        rets: *mut wasm_val_vec_t,
    ) -> *mut wasm_trap_t
    where
        F: Fn(FunctionEnvMut<'_, T>, &[Value]) -> Result<Vec<Value>, RuntimeError>
            + 'static
            + Send
            + Sync,
    {
        let pointer = env.cast::<FunctionCallbackEnv<'_, F>>();
        unsafe {
            invoke_sync_callback(pointer, args, rets, |store, values| {
                let callback = &*pointer;
                let environment = env::FunctionEnv::from_handle(
                    callback.env_handle.as_ref().unwrap().clone(),
                    callback.env_identity.as_ref().unwrap().clone(),
                )
                .into_mut(store);
                (callback.func)(environment.into(), values)
            })
        }
    }

    fn_callback::<F, T>
}

impl std::fmt::Debug for Function {
    fn fmt(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.debug_struct("Function").finish()
    }
}

impl crate::Function {
    /// Consume [`self`] into [`crate::backend::v8::function::Function`].
    pub fn into_v8(self) -> crate::backend::v8::function::Function {
        match self.0 {
            BackendFunction::V8(s) => s,
            _ => panic!("Not a `v8` function!"),
        }
    }

    /// Convert a reference to [`self`] into a reference to [`crate::backend::v8::function::Function`].
    pub fn as_v8(&self) -> &crate::backend::v8::function::Function {
        match self.0 {
            BackendFunction::V8(ref s) => s,
            _ => panic!("Not a `v8` function!"),
        }
    }

    /// Convert a mutable reference to [`self`] into a mutable reference [`crate::backend::v8::function::Function`].
    pub fn as_v8_mut(&mut self) -> &mut crate::backend::v8::function::Function {
        match self.0 {
            BackendFunction::V8(ref mut s) => s,
            _ => panic!("Not a `v8` function!"),
        }
    }
}

macro_rules! impl_host_function {
    ([$c_struct_representation:ident] $c_struct_name:ident, $( $x:ident ),* ) => {
        paste::paste! {
        #[allow(non_snake_case)]
        pub(crate) fn [<gen_fn_callback_ $c_struct_name:lower _no_env>]
            <$( $x: FromToNativeWasmType, )* Rets: WasmTypeList, RetsAsResult: IntoResult<Rets>, Func: Fn($( $x , )*) -> RetsAsResult + 'static>
            (_this: &Func) -> crate::backend::v8::vm::VMFunctionCallback {
            unsafe extern "C" fn callback<$( $x, )* Rets, RetsAsResult, Func>(
                pointer: *mut c_void, args: *const wasm_val_vec_t, results: *mut wasm_val_vec_t,
            ) -> *mut wasm_trap_t
            where $( $x: FromToNativeWasmType, )* Rets: WasmTypeList,
                RetsAsResult: IntoResult<Rets>, Func: Fn($( $x , )*) -> RetsAsResult + 'static,
            {
                let pointer = pointer.cast::<FunctionCallbackEnv<'_, Func>>();
                unsafe { invoke_sync_callback(pointer, args, results, |store, values| {
                    let mut values = values.iter();
                    $(let value = values.next().ok_or_else(|| RuntimeError::new("missing V8 typed argument"))?;
                      let raw = value.as_raw(store);
                      let $x = $x::from_native($x::Native::from_raw(store, raw));)*
                    let output = ((*pointer).func)($( $x, )*).into_result()
                        .map_err(|error| RuntimeError::from_dyn(Box::new(error)))?;
                    sync_typed_results::<Rets>(store, output)
                }) }
            }
            callback::<$( $x, )* Rets, RetsAsResult, Func> as _
        }
        #[allow(non_snake_case)]
        pub(crate) fn [<gen_fn_callback_ $c_struct_name:lower>]
            <$( $x: FromToNativeWasmType, )* Rets: WasmTypeList, RetsAsResult: IntoResult<Rets>, T: Send + 'static,
             Func: Fn(FunctionEnvMut<T>, $( $x , )*) -> RetsAsResult + 'static>
            (_this: &Func) -> crate::backend::v8::vm::VMFunctionCallback {
            unsafe extern "C" fn callback<$( $x, )* Rets, RetsAsResult, Func, T>(
                pointer: *mut c_void, args: *const wasm_val_vec_t, results: *mut wasm_val_vec_t,
            ) -> *mut wasm_trap_t
            where $( $x: FromToNativeWasmType, )* Rets: WasmTypeList, RetsAsResult: IntoResult<Rets>,
                T: Send + 'static, Func: Fn(FunctionEnvMut<'_, T>, $( $x , )*) -> RetsAsResult + 'static,
            {
                let pointer = pointer.cast::<FunctionCallbackEnv<'_, Func>>();
                unsafe { invoke_sync_callback(pointer, args, results, |store, values| {
                    let mut values = values.iter();
                    $(let value = values.next().ok_or_else(|| RuntimeError::new("missing V8 typed argument"))?;
                      let raw = value.as_raw(store);
                      let $x = $x::from_native($x::Native::from_raw(store, raw));)*
                    let callback = &*pointer;
                    let environment = env::FunctionEnv::from_handle(
                        callback.env_handle.as_ref().unwrap().clone(),
                        callback.env_identity.as_ref().unwrap().clone(),
                    ).into_mut(store);
                    let output = (callback.func)(environment.into(), $( $x, )*).into_result()
                        .map_err(|error| RuntimeError::from_dyn(Box::new(error)))?;
                    sync_typed_results::<Rets>(store, output)
                }) }
            }
            callback::<$( $x, )* Rets, RetsAsResult, Func, T> as _
        }
        }
    };
}

fn sync_typed_results<Rets: WasmTypeList>(
    store: &mut StoreMut<'_>,
    output: Rets,
) -> Result<Vec<Value>, RuntimeError> {
    let types = Rets::wasm_types();
    let mut values = unsafe { output.into_array(store) };
    Ok(values
        .as_mut()
        .iter()
        .zip(types)
        .map(|(value, ty)| unsafe { Value::from_raw(store, *ty, *value) })
        .collect())
}

struct SyncCallbackScope<'a> {
    legacy: Option<StoreMut<'a>>,
    #[cfg(feature = "experimental-async")]
    actual: Option<crate::StoreAsyncWriteLock>,
    #[cfg(feature = "experimental-async")]
    binding: Option<std::rc::Rc<crate::backend::v8::async_runtime::AsyncBinding>>,
    #[cfg(feature = "experimental-async")]
    borrow: Option<crate::backend::v8::async_runtime::SyncBorrow>,
}
impl<'a> SyncCallbackScope<'a> {
    unsafe fn enter<F>(callback: &FunctionCallbackEnv<'_, F>) -> Result<Self, RuntimeError> {
        callback.origin.check_alive()?;
        #[cfg(feature = "experimental-async")]
        if let Some(binding) = callback.binding.upgrade()
            && binding.has_active_call()
        {
            let store = binding.store()?;
            let actual = crate::StoreAsyncWriteLock::try_acquire(&store)
                .ok_or_else(|| RuntimeError::new("V8 synchronous callback Store is busy"))?;
            callback.origin.check_store(&actual)?;
            let borrow = binding.sync_borrow();
            return Ok(Self {
                legacy: None,
                actual: Some(actual),
                binding: Some(binding),
                borrow,
            });
        }
        let legacy = unsafe { StoreMut::from_raw(callback.store) };
        callback.origin.check_store(&legacy)?;
        Ok(Self {
            legacy: Some(legacy),
            #[cfg(feature = "experimental-async")]
            actual: None,
            #[cfg(feature = "experimental-async")]
            binding: None,
            #[cfg(feature = "experimental-async")]
            borrow: None,
        })
    }
    fn store(&mut self) -> StoreMut<'_> {
        #[cfg(feature = "experimental-async")]
        if let Some(actual) = &mut self.actual {
            return actual.as_store_mut();
        }
        self.legacy.as_mut().unwrap().as_store_mut()
    }
}
impl Drop for SyncCallbackScope<'_> {
    fn drop(&mut self) {
        #[cfg(feature = "experimental-async")]
        {
            // No derived user Store borrow survives. Unlock retirement may
            // reenter only after that real access has ended.
            drop(self.borrow.take());
            let unwinding = std::thread::panicking();
            if let Err(panic) = panic::catch_unwind(AssertUnwindSafe(|| drop(self.actual.take()))) {
                if let Some(binding) = &self.binding {
                    binding.record(panic, unwinding);
                } else {
                    crate::backend::v8::async_runtime::discard_panic(panic);
                }
            }
        }
    }
}

unsafe fn invoke_sync_callback<F>(
    pointer: *mut FunctionCallbackEnv<'_, F>,
    arguments: *const wasm_val_vec_t,
    results: *mut wasm_val_vec_t,
    callback: impl FnOnce(&mut StoreMut<'_>, &[Value]) -> Result<Vec<Value>, RuntimeError>,
) -> *mut wasm_trap_t {
    let descriptor = unsafe { &*pointer };
    let mut scope = match unsafe { SyncCallbackScope::enter(descriptor) } {
        Ok(scope) => scope,
        Err(error) => {
            return unsafe { Trap::user(Box::new(error)).into_wasm_trap_raw(descriptor.raw_store) };
        }
    };
    let unwinding = std::thread::panicking();
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| -> Result<(), RuntimeError> {
        let mut store = scope.store();
        let arguments = unsafe { &*arguments };
        let raw = crate::backend::v8::references::borrowed_values(arguments);
        crate::backend::v8::utils::convert::validate_raw_values(
            raw,
            descriptor.signature.params(),
        )?;
        let values = raw
            .iter()
            .zip(descriptor.signature.params())
            .map(|(value, ty)| (*value).into_wv(*ty, &store))
            .collect::<Result<Vec<_>, RuntimeError>>()?;
        let output = callback(&mut store, &values)?;
        let results = unsafe { &mut *results };
        if output.len() != results.size
            || output.len() != descriptor.signature.results().len()
            || output
                .iter()
                .zip(descriptor.signature.results())
                .any(|(value, ty)| value.ty() != *ty)
            || output.iter().any(|value| !value.is_from_store(&store))
        {
            return Err(RuntimeError::new("invalid V8 synchronous host results"));
        }
        crate::backend::v8::utils::convert::validate_values(
            &output,
            descriptor.signature.results(),
        )?;
        let results = crate::backend::v8::references::borrowed_values_mut(results);
        for ((slot, value), ty) in results
            .iter_mut()
            .zip(output)
            .zip(descriptor.signature.results())
        {
            OwnedValue::copy(&value.into_cv(*ty)?).replace(slot);
        }
        Ok(())
    }));
    let result = match outcome {
        Ok(value) => value,
        Err(panic) => {
            #[cfg(feature = "experimental-async")]
            if let Some(binding) = &scope.binding {
                binding.record(panic, unwinding);
            } else {
                unimplemented!("host function panicked");
            }
            #[cfg(not(feature = "experimental-async"))]
            unimplemented!("host function panicked");
            Err(RuntimeError::new("V8 synchronous host callback panicked"))
        }
    };
    #[cfg(feature = "experimental-async")]
    let binding = scope.binding.clone();
    drop(scope); // all real guard/destructor callbacks settle before C return
    #[cfg(feature = "experimental-async")]
    let result = if binding
        .as_ref()
        .is_some_and(|binding| binding.current_has_panic())
    {
        Err(RuntimeError::new("V8 synchronous host cleanup panicked"))
    } else {
        result
    };
    match result {
        Ok(()) => std::ptr::null_mut(),
        Err(error) => unsafe {
            Trap::user(Box::new(error)).into_wasm_trap_raw(descriptor.raw_store)
        },
    }
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

#[cfg(feature = "experimental-async")]
mod async_;

//! Native Functions.
//!
//! This module creates the helper `TypedFunction` that let us call WebAssembly
//! functions with the native ABI, that is:
//!
//! ```ignore
//! let add_one = instance.exports.get_function("function_name")?;
//! let add_one_native: TypedFunction<i32, i32> = add_one.typed().unwrap();
//! ```
use crate::{
    AsStoreMut, CallWrapper, FromToNativeWasmType, NativeWasmType, NativeWasmTypeInto,
    RuntimeError, TypedFunction, Value, WasmTypeList,
    entities::function::call_wrapper::call_wrapped,
    js::{
        function::numeric,
        lent,
        utils::convert::{AsJs, js_value_to_wasmer},
    },
};
use js_sys::Array;
use std::iter::FromIterator;
use wasm_bindgen::JsValue;
use wasmer_types::{RawValue, Type};

/// The raw arguments of a call as the numbers JavaScript knows them by.
/// `None` if there are too many for the numeric lane, or if one is of a type
/// whose values are not numbers there.
fn raw_numbers(params: &[(Type, RawValue)]) -> Option<numeric::Numbers> {
    let mut numbers = [0.0; numeric::MAX_ARITY];
    if params.len() > numbers.len() {
        return None;
    }
    for (number, (ty, raw)) in numbers.iter_mut().zip(params) {
        *number = match ty {
            Type::I32 => unsafe { raw.i32 as f64 },
            Type::F32 => unsafe { raw.f32 as f64 },
            Type::F64 => unsafe { raw.f64 },
            _ => return None,
        };
    }
    Some((numbers, params.len()))
}

/// The type of the one result of a function on the numeric lane, if it has
/// one. `None` for results that keep a function off the lane.
fn numeric_result(results: &[Type]) -> Option<Option<Type>> {
    match results {
        [] => Some(None),
        [ty @ (Type::I32 | Type::F32 | Type::F64)] => Some(Some(*ty)),
        _ => None,
    }
}

/// The number the engine returned for a result of type `ty`, as a raw value.
fn number_to_raw(ty: Type, number: f64) -> RawValue {
    match ty {
        Type::I32 => RawValue { i32: number as i32 },
        Type::F32 => RawValue { f32: number as f32 },
        _ => RawValue { f64: number },
    }
}

macro_rules! impl_native_traits {
    (  $( $x:ident ),* ) => {
        #[allow(unused_parens, non_snake_case)]
        impl<$( $x , )* Rets> TypedFunction<( $( $x ),* ), Rets>
        where
            $( $x: FromToNativeWasmType, )*
            Rets: WasmTypeList,
        {
            /// Call the typed func and return results.
            #[allow(clippy::too_many_arguments)]
            pub(crate) fn call_js(&self, store: &mut impl AsStoreMut, $( $x: $x, )* ) -> Result<Rets, RuntimeError> where
            $( $x: FromToNativeWasmType,  )*
            {
                self.call_wrapped_js(store, None, $( $x, )*)
            }

            /// Call the typed func through `wrapper` and return results.
            #[allow(clippy::too_many_arguments)]
            pub(crate) fn call_wrapped_js(&self, mut store: &mut impl AsStoreMut, wrapper: Option<&mut dyn CallWrapper>, $( $x: $x, )* ) -> Result<Rets, RuntimeError> where
            $( $x: FromToNativeWasmType,  )*
            {
                #[allow(unused_unsafe)]
                let params_list: [(Type, RawValue); _] = unsafe {
                    [ $( (<$x::Native as NativeWasmType>::WASM_TYPE, $x.to_native().into_raw(store) ) ),* ]
                };
                // Nothing but numbers: they are passed as they are, without
                // an array of arguments in between.
                if let (Some(numbers), Some(result_ty)) =
                    (raw_numbers(&params_list), numeric_result(Rets::wasm_types()))
                {
                    // Called with the receiver of the call below, and its
                    // result read the same way.
                    let function = &self.func.as_js().handle.function;
                    let mut result = 0.0;
                    call_wrapped(store, wrapper, |store| {
                        let _lent = lent::lend(&mut store.as_store_mut())?;
                        let _entered = super::enter_sync();
                        let receiver = &JsValue::UNDEFINED;
                        result = numeric::call(function, receiver, &numbers, result_ty.is_some())?;
                        Ok(())
                    })?;
                    let mut rets_list_array = Rets::empty_array();
                    if let Some(ty) = result_ty {
                        rets_list_array.as_mut()[0] = number_to_raw(ty, result);
                    }
                    return Ok(unsafe { Rets::from_array(store, rets_list_array) });
                }
                let mut results = JsValue::UNDEFINED;
                call_wrapped(store, wrapper, |store| {
                    let args_array = unsafe {
                        Array::from_iter(
                            params_list
                                .iter()
                                .map(|&(b, a)| Value::from_raw(store, b, a).as_jsvalue(store)),
                        )
                    };
                    let _lent = lent::lend(&mut store.as_store_mut())?;
                    let _entered = super::enter_sync();
                    results = self
                        .func
                        .as_js()
                        .handle
                        .function
                        .apply(&JsValue::UNDEFINED, &args_array)?;
                    Ok(())
                })?;
                let mut rets_list_array = Rets::empty_array();
                let mut_rets = rets_list_array.as_mut() as *mut [RawValue] as *mut RawValue;
                match Rets::size() {
                    0 => {},
                    1 => unsafe {
                        let ty = Rets::wasm_types()[0];
                        let val = js_value_to_wasmer(&mut store, &ty, &results);
                        *mut_rets = val.as_raw(&mut store);
                    }
                    _n => {
                        let results: Array = results.into();
                        for (i, ret_type) in Rets::wasm_types().iter().enumerate() {
                            let ret = results.get(i as u32);
                            unsafe {
                                let val = js_value_to_wasmer(&mut store, &ret_type, &ret);
                                let slot = mut_rets.add(i);
                                *slot = val.as_raw(&mut store);
                            }
                        }
                    }
                }
                Ok(unsafe { Rets::from_array(store, rets_list_array) })
            }

            /// See [`TypedFunction::call_raw`].
            pub(crate) unsafe fn call_raw_js(&self, _store: &mut impl AsStoreMut, _params_list: Vec<RawValue> ) -> Result<Rets, RuntimeError> {
                Err(RuntimeError::new("raw calls are not supported by the js backend"))
            }
        }
    };
}

impl_native_traits!();
impl_native_traits!(A1);
impl_native_traits!(A1, A2);
impl_native_traits!(A1, A2, A3);
impl_native_traits!(A1, A2, A3, A4);
impl_native_traits!(A1, A2, A3, A4, A5);
impl_native_traits!(A1, A2, A3, A4, A5, A6);
impl_native_traits!(A1, A2, A3, A4, A5, A6, A7);
impl_native_traits!(A1, A2, A3, A4, A5, A6, A7, A8);
impl_native_traits!(A1, A2, A3, A4, A5, A6, A7, A8, A9);
impl_native_traits!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10);
impl_native_traits!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11);
impl_native_traits!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12);
impl_native_traits!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13);
impl_native_traits!(A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14);
impl_native_traits!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15
);
impl_native_traits!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16
);
impl_native_traits!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17
);
impl_native_traits!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18
);
impl_native_traits!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19
);
impl_native_traits!(
    A1, A2, A3, A4, A5, A6, A7, A8, A9, A10, A11, A12, A13, A14, A15, A16, A17, A18, A19, A20
);

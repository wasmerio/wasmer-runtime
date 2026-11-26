//! Native Functions.
//!
//! This module creates the helper [`TypedFunction`] that let us call WebAssembly
//! functions with the native ABI, that is:
//!
//! ```ignore
//! let add_one = instance.exports.get_function("function_name")?;
//! let add_one_native: TypedFunction<i32, i32> = add_one.native().unwrap();
//! ```
#[cfg(feature = "experimental-async")]
use crate::{AsyncCallStore, StoreMut};
use crate::{
    AsStoreMut, BackendStore, CallWrapper, FromToNativeWasmType, Function, NativeWasmTypeInto,
    RuntimeError, WasmTypeList, store::AsStoreRef,
};
use std::marker::PhantomData;
use wasmer_types::RawValue;

/// A WebAssembly function that can be called natively
/// (using the Native ABI).
#[derive(Clone, Debug)]
pub struct TypedFunction<Args, Rets> {
    pub(crate) func: Function,
    _phantom: PhantomData<fn(Args) -> Rets>,
}

unsafe impl<Args, Rets> Send for TypedFunction<Args, Rets> {}
unsafe impl<Args, Rets> Sync for TypedFunction<Args, Rets> {}

impl<Args, Rets> TypedFunction<Args, Rets>
where
    Args: WasmTypeList,
    Rets: WasmTypeList,
{
    #[allow(dead_code)]
    pub(crate) fn new(_store: &impl AsStoreRef, func: Function) -> Self {
        Self {
            func,
            _phantom: PhantomData,
        }
    }

    pub(crate) fn into_function(self) -> Function {
        self.func
    }
}

macro_rules! impl_native_traits {
    (  $( $x:ident ),* ) => {
        paste::paste!{
        #[allow(unused_parens, non_snake_case)]
        impl<$( $x , )* Rets> TypedFunction<( $( $x ),* ), Rets>
        where
            $( $x: FromToNativeWasmType, )*
            Rets: WasmTypeList,
        {
            /// Call the typed func and return results.
            #[allow(unused_mut)]
            #[allow(clippy::too_many_arguments)]
            pub fn call(&self, store: &mut impl AsStoreMut, $( $x: $x, )* ) -> Result<Rets, RuntimeError> where $( $x: FromToNativeWasmType, )*

            {
                $(
                    let [<p_ $x>] = $x;
                )*
                match store.as_store_mut().inner.store {
                    #[cfg(feature = "sys")]
                    BackendStore::Sys(_) => self.call_sys(store, $([<p_ $x>]),*),
                    #[cfg(feature = "v8")]
                    BackendStore::V8(_) => self.call_v8(store, $([<p_ $x>]),*),
                    #[cfg(feature = "js")]
                    BackendStore::Js(_) => self.call_js(store, $([<p_ $x>]),*),

                }
            }

            /// Call the typed func through a [`CallWrapper`], which decides
            /// when and how many times the function is entered.
            #[allow(clippy::too_many_arguments)]
            pub fn call_wrapped(&self, store: &mut impl AsStoreMut, wrapper: &mut dyn CallWrapper, $( $x: $x, )* ) -> Result<Rets, RuntimeError> where $( $x: FromToNativeWasmType, )*
            {
                $(
                    let [<p_ $x>] = $x;
                )*
                match store.as_store_mut().inner.store {
                    #[cfg(feature = "sys")]
                    BackendStore::Sys(_) => self.call_wrapped_sys(store, Some(wrapper), $([<p_ $x>]),*),
                    #[cfg(feature = "v8")]
                    BackendStore::V8(_) => self.call_wrapped_v8(store, Some(wrapper), $([<p_ $x>]),*),
                    #[cfg(feature = "js")]
                    BackendStore::Js(_) => self.call_wrapped_js(store, Some(wrapper), $([<p_ $x>]),*),
                }
            }

            /// Call the typed func asynchronously.
            ///
            /// See [`Function::call_async`] for what `store` can be.
            #[cfg(feature = "experimental-async")]
            #[allow(clippy::too_many_arguments)]
            pub fn call_async<'a, S: AsyncCallStore>(
                &'a self,
                store: &'a mut S,
                $( $x: $x, )*
            ) -> S::Call<'a, Result<Rets, RuntimeError>>
            where
                $( $x: FromToNativeWasmType + 'static, )*
                Rets: 'static,
            {
                $(
                    let [<p_ $x>] = $x;
                )*
                // Safety: a `TypedFunction` is only made for a function whose
                // signature its `Args` and `Rets` match, and the closure makes
                // one raw value of each argument's type.
                unsafe { store.start_typed_call(&self.func, move |store: &mut StoreMut<'_>| {
                    // Ensure all parameters come from the same context.
                    if $(!FromToNativeWasmType::is_from_store(&[<p_ $x>], store) ||)* false {
                        return Err(RuntimeError::new(
                            "cross-`Store` values are not supported",
                        ));
                    }
                    let args: [RawValue; _] = [ $( [<p_ $x>].to_native().into_raw(store) ),* ];
                    Ok(args)
                }) }
            }

            /// Calls the function with arguments that are already raw values.
            ///
            /// Returns an error if `params_list` does not have one value per
            /// argument, or if the backend has no raw calls (only `sys` does).
            ///
            /// # Safety
            ///
            /// Each value must be a valid raw value of the type of its
            /// argument, as [`FromToNativeWasmType`] makes it, and a reference
            /// must come from `store`.
            #[doc(hidden)]
            pub unsafe fn call_raw(&self, store: &mut impl AsStoreMut, params_list: Vec<RawValue> ) -> Result<Rets, RuntimeError> {
                crate::entities::function::check_raw_arity(
                    <( $( $x ),* ) as WasmTypeList>::size() as usize,
                    params_list.len(),
                )?;
                match store.as_store_mut().inner.store {
                    #[cfg(feature = "sys")]
                    BackendStore::Sys(_) => unsafe { self.call_raw_sys(store, params_list) },
                    #[cfg(feature = "v8")]
                    BackendStore::V8(_) => unsafe { self.call_raw_v8(store, params_list) },
                    #[cfg(feature = "js")]
                    BackendStore::Js(_) => unsafe { self.call_raw_js(store, params_list) },
                }
            }
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

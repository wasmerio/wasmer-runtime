//! This module defines traits to handle abstractions created by the runtimes.

mod impls;

use crate::{VMExternToExtern, macros::backend::gen_rt_ty};
use wasmer_types::RawValue;

macro_rules! define_vm_like {
    ($name:ident $(, $derives:ident)*) => {
        paste::paste! {
            gen_rt_ty! {
                #[derive($($derives,)*)]
                pub [<VM $name>](vm::[<VM $name>]);
            }
        }
    };
}

define_vm_like!(Extern);
define_vm_like!(ExternFunction, Debug);
define_vm_like!(ExternGlobal);
define_vm_like!(ExternTag);
define_vm_like!(ExternMemory);
define_vm_like!(ExternTable);
//define_vm_like!(ExternObj, Debug);
define_vm_like!(FunctionCallback, Debug);
define_vm_like!(FunctionBody);
define_vm_like!(FunctionEnvironment, Debug);
define_vm_like!(Instance, Debug);
define_vm_like!(Trampoline, Debug);

//define_vm_like!(Config);
define_vm_like!(Function, Debug);
define_vm_like!(Global, Debug);
define_vm_like!(Tag, Debug);
define_vm_like!(Memory, Debug);
define_vm_like!(SharedMemory);
define_vm_like!(Table, Debug);

define_vm_like!(ExceptionRef);
define_vm_like!(ExternRef);
define_vm_like!(FuncRef);

#[cfg(feature = "v8")]
impl VMExtern {
    /// Imports a raw wee8 extern into the named actual Store.
    ///
    /// # Safety
    /// `raw` must be a nonnull, live wee8 wasm_extern_t created in this exact
    /// Store on its original thread, with its wee8 reference retained for the
    /// Store's lifetime. A foreign, expired or arbitrary address is not valid.
    /// This constructor cannot discover ownership by querying the raw pointer.
    #[doc(hidden)]
    pub unsafe fn from_raw_v8_unchecked(
        store: &impl crate::AsStoreRef,
        raw: *mut std::ffi::c_void,
    ) -> Result<Self, crate::RuntimeError> {
        let origin = crate::backend::v8::origin::Origin::of(store)?;
        if raw.is_null() {
            return Err(crate::RuntimeError::new("null V8 extern"));
        }
        let view = store.as_store_ref();
        let reference = unsafe { crate::backend::v8::bindings::wasm_extern_as_ref(raw.cast()) };
        let retained = view
            .inner
            .store
            .as_v8()
            .reference_roots
            .copy(reference, false);
        let raw = unsafe { crate::backend::v8::bindings::wasm_ref_as_extern(retained) };
        Ok(Self::V8(crate::backend::v8::origin::Handle::new(
            raw, origin,
        )))
    }
}

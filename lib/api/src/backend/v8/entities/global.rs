//! Data types, functions and traits for `v8` runtime's `Global` implementation.
use wasmer_types::{GlobalType, Mutability};

use crate::{
    AsStoreMut, AsStoreRef, RuntimeError, Value,
    v8::{
        bindings::{
            self, wasm_global_as_extern, wasm_global_get, wasm_global_new, wasm_global_set,
            wasm_global_type, wasm_globaltype_content, wasm_globaltype_mutability,
            wasm_globaltype_new, wasm_mutability_t, wasm_valtype_new,
        },
        utils::convert::{IntoCApiType, IntoCApiValue, IntoWasmerType, IntoWasmerValue},
        vm::VMGlobal,
    },
    vm::{VMExtern, VMExternGlobal},
};

use super::check_isolate;
use crate::backend::v8::origin::{Handle, Origin};

#[derive(Debug, Clone, PartialEq, Eq)]
/// A WebAssembly `global` in the `v8` runtime.
pub struct Global {
    pub(crate) handle: VMGlobal,
}

unsafe impl Send for Global {}
unsafe impl Sync for Global {}

// Global can't be Send in js because it doesn't support `structuredClone`
// https://developer.mozilla.org/en-US/docs/Web/API/structuredClone
// unsafe impl Send for Global {}

impl Global {
    pub(crate) fn to_vm_extern(&self) -> VMExtern {
        let extern_ = unsafe { wasm_global_as_extern(self.handle.raw()) };
        assert!(
            !extern_.is_null(),
            "Returned null Global extern from wasm-c-api"
        );
        VMExtern::V8(Handle::new(extern_, self.handle.origin.clone()))
    }

    /// Create a `Global` with the initial value [`Value`] and the provided [`Mutability`].
    pub(crate) fn from_value(
        store: &mut impl AsStoreMut,
        val: Value,
        mutability: Mutability,
    ) -> Result<Self, RuntimeError> {
        check_isolate(store);
        if !val.is_from_store(store) {
            return Err(RuntimeError::new("foreign V8 global initializer"));
        }
        let store = store.as_store_mut();
        let v8_store = store.inner.store.as_v8();
        let v8_type = val.ty().into_ct();
        let v8_value = val.into_cv();
        let v8_mutability = if mutability.is_mutable() {
            bindings::wasm_mutability_enum_WASM_VAR
        } else {
            bindings::wasm_mutability_enum_WASM_CONST
        } as wasm_mutability_t;

        let v8_global_type =
            unsafe { wasm_globaltype_new(wasm_valtype_new(v8_type), v8_mutability) };

        Ok(Self {
            handle: Handle::new(
                unsafe { wasm_global_new(v8_store.inner, v8_global_type, &v8_value) },
                v8_store.origin.capture(),
            ),
        })
    }

    pub fn ty(&self, store: &impl AsStoreRef) -> GlobalType {
        check_isolate(store);
        self.handle
            .origin
            .check_store(store)
            .expect("foreign V8 handle");
        let store = store.as_store_ref();
        let r#type = unsafe { wasm_global_type(self.handle.raw()) };
        let mutability = unsafe { wasm_globaltype_mutability(&*r#type) };
        let valtype = unsafe { wasm_globaltype_content(r#type) };
        let wasmer_type = valtype.into_wt();

        GlobalType::new(
            wasmer_type,
            if mutability == bindings::wasm_mutability_enum_WASM_VAR as u8 {
                Mutability::Var
            } else {
                Mutability::Const
            },
        )
    }

    pub fn get(&self, store: &mut impl AsStoreMut) -> Value {
        check_isolate(store);
        self.handle
            .origin
            .check_store(store)
            .expect("foreign V8 handle");
        let store = store.as_store_ref();
        let mut out = unsafe { std::mem::zeroed() };
        unsafe { wasm_global_get(self.handle.raw(), &mut out) };
        out.into_wv(&self.handle.origin)
    }

    pub fn set(&self, store: &mut impl AsStoreMut, val: Value) -> Result<(), RuntimeError> {
        check_isolate(store);
        self.handle.origin.check_store(store)?;
        if !val.is_from_store(store) {
            return Err(RuntimeError::new("foreign V8 global value"));
        }

        if val.ty() != self.ty(store).ty {
            return Err(RuntimeError::new(format!(
                "Incompatible types: {} != {}",
                val.ty(),
                self.ty(store)
            )));
        }

        if self.ty(store).mutability == Mutability::Const {
            return Err(RuntimeError::new("The global is immutable".to_owned()));
        }

        let value = val.into_cv();

        unsafe { wasm_global_set(self.handle.raw(), &value) };

        Ok(())
    }

    pub(crate) fn from_vm_extern(store: &mut impl AsStoreMut, vm_global: VMExternGlobal) -> Self {
        check_isolate(store);
        let handle = vm_global.unwrap_v_8();
        handle
            .origin
            .check_store(store)
            .expect("foreign V8 global export");
        Self { handle }
    }

    pub fn is_from_store(&self, store: &impl AsStoreRef) -> bool {
        self.handle.origin.check_store(store).is_ok()
    }
}

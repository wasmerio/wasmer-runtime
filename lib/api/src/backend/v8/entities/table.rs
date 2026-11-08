//! Data types, functions and traits for `v8` runtime's `Table` implementation.
use wasmer_types::TableType;

use crate::{
    AsStoreMut, AsStoreRef, BackendTable, RuntimeError, Value,
    v8::{
        bindings::{self, *},
        utils::convert::{IntoCApiType, IntoCApiValue, IntoWasmerType, IntoWasmerValue},
        vm::VMTable,
    },
    vm::{VMExtern, VMExternTable},
};

use super::check_isolate;
use crate::backend::v8::{origin::Handle, references::OwnedValue};

#[derive(Debug, Clone, PartialEq, Eq)]
/// A WebAssembly `table` in the `v8` runtime.
pub struct Table {
    pub(crate) handle: VMTable,
}

unsafe impl Send for Table {}
unsafe impl Sync for Table {}

impl Table {
    pub(crate) fn type_to_v8(ty: TableType) -> *mut wasm_tabletype_t {
        let valtype = unsafe { wasm_valtype_new(ty.ty.into_ct()) };

        let limits = Box::into_raw(Box::new(wasm_limits_t {
            min: ty.minimum,
            max: ty.maximum.unwrap_or(wasm_limits_max_default),
            shared: false,
        }));

        unsafe { wasm_tabletype_new(valtype, limits) }
    }

    pub fn new(
        store: &mut impl AsStoreMut,
        ty: TableType,
        init: Value,
    ) -> Result<Self, RuntimeError> {
        check_isolate(store);
        if !init.is_from_store(store) {
            return Err(RuntimeError::new("foreign V8 table initializer"));
        }
        let store_mut = store.as_store_mut();
        let v8_store = store_mut.inner.store.as_v8();

        let engine = store_mut.engine();

        if !matches!(
            ty.ty,
            wasmer_types::Type::FuncRef | wasmer_types::Type::ExternRef
        ) {
            return Err(RuntimeError::new(
                "V8 table requires a declared reference type",
            ));
        }
        let init: wasm_val_t = init.into_cv(ty.ty)?;
        let wasm_tablety = Self::type_to_v8(ty);

        Ok(Self {
            handle: Handle::new(
                v8_store
                    .reference_roots
                    .table(unsafe { wasm_table_new(v8_store.inner, wasm_tablety, init.of.ref_) }),
                v8_store.origin.capture(),
            ),
        })
    }

    pub fn to_vm_extern(&self) -> VMExtern {
        VMExtern::V8(Handle::new(
            unsafe { wasm_table_as_extern(self.handle.raw()) },
            self.handle.origin.clone(),
        ))
    }

    pub fn ty(&self, store: &impl AsStoreRef) -> TableType {
        check_isolate(store);
        self.handle
            .origin
            .check_store(store)
            .expect("foreign V8 table");

        let store = store.as_store_ref();

        let table_type: *mut wasm_tabletype_t = unsafe { wasm_table_type(self.handle.raw()) };
        let table_limits = unsafe { wasm_tabletype_limits(table_type) };
        let table_type = unsafe { wasm_tabletype_element(table_type) };

        TableType {
            ty: table_type.into_wt(),
            minimum: unsafe { (*table_limits).min },
            maximum: unsafe {
                if (*table_limits).max == wasm_limits_max_default {
                    None
                } else {
                    Some((*table_limits).max)
                }
            },
            readonly: false,
        }
    }

    pub fn get(&self, store: &mut impl AsStoreMut, index: u32) -> Option<Value> {
        if self.handle.origin.check_store(store).is_err() {
            return None;
        }
        let expected = self.ty(store).ty;

        if index >= self.size(store) {
            return None;
        }

        unsafe {
            let ref_ = wasm_table_get(self.handle.raw(), index);

            if ref_.is_null() {
                return match expected {
                    wasmer_types::Type::ExternRef => Some(Value::ExternRef(None)),
                    wasmer_types::Type::FuncRef => Some(Value::FuncRef(None)),
                    ty => panic!("unsupported table type: {ty:?}"),
                };
            }

            let kind = match expected {
                wasmer_types::Type::ExternRef => wasm_valkind_enum_WASM_EXTERNREF,
                wasmer_types::Type::FuncRef => wasm_valkind_enum_WASM_FUNCREF,
                ty => panic!("unsupported table type: {ty:?}"),
            } as u8;

            let value = OwnedValue::adopt(wasm_val_t {
                kind,
                of: bindings::wasm_val_t__bindgen_ty_1 { ref_ },
            });

            Some(
                (*value.value())
                    .into_wv(expected, store)
                    .expect("V8 table value disagrees with its checked element type"),
            )
        }
    }

    pub fn set(
        &self,
        store: &mut impl AsStoreMut,
        index: u32,
        val: Value,
    ) -> Result<(), RuntimeError> {
        self.handle.origin.check_store(store)?;
        if !val.is_from_store(store) {
            return Err(RuntimeError::new("foreign V8 table value"));
        }
        let expected = self.ty(store).ty;
        if !matches!(
            expected,
            wasmer_types::Type::FuncRef | wasmer_types::Type::ExternRef
        ) {
            return Err(RuntimeError::new(
                "V8 table requires a declared reference type",
            ));
        }
        let value = val.clone().into_cv(expected)?;
        if !unsafe { wasm_table_set(self.handle.raw(), index, value.of.ref_) } {
            return Err(RuntimeError::new(format!(
                "Could not set value {val:?} table at index {index}"
            )));
        }
        Ok(())
    }

    pub fn size(&self, store: &impl AsStoreRef) -> u32 {
        check_isolate(store);
        self.handle
            .origin
            .check_store(store)
            .expect("foreign V8 handle");
        let store = store.as_store_ref();
        unsafe { wasm_table_size(self.handle.raw()) }
    }

    pub fn grow(
        &self,
        store: &mut impl AsStoreMut,
        delta: u32,
        init: Value,
    ) -> Result<u32, RuntimeError> {
        self.handle.origin.check_store(store)?;
        if !init.is_from_store(store) {
            return Err(RuntimeError::new("foreign V8 table grow value"));
        }
        let expected = self.ty(store).ty;
        if !matches!(
            expected,
            wasmer_types::Type::FuncRef | wasmer_types::Type::ExternRef
        ) {
            return Err(RuntimeError::new(
                "V8 table requires a declared reference type",
            ));
        }
        let value = init.into_cv(expected)?;
        let size = unsafe { wasm_table_size(self.handle.raw()) };
        if !unsafe { wasm_table_grow(self.handle.raw(), delta, value.of.ref_) } {
            return Err(RuntimeError::new("Could not grow table"));
        }
        Ok(size)
    }

    pub fn copy(
        _store: &mut impl AsStoreMut,
        _dst_table: &Self,
        _dst_index: u32,
        _src_table: &Self,
        _src_index: u32,
        _len: u32,
    ) -> Result<(), RuntimeError> {
        Err(RuntimeError::new(
            "Could Copying tables is currently not implemented",
        ))
    }

    pub(crate) fn from_vm_extern(store: &mut impl AsStoreMut, vm_extern: VMExternTable) -> Self {
        check_isolate(store);
        let store_mut = store.as_store_mut();

        let handle = vm_extern.unwrap_v_8();
        handle
            .origin
            .check_store(store)
            .expect("foreign V8 table export");
        Self { handle }
    }

    pub fn is_from_store(&self, store: &impl AsStoreRef) -> bool {
        self.handle.origin.check_store(store).is_ok()
    }
}

impl crate::Table {
    /// Consume [`self`] into [`crate::backend::v8::table::Table`].
    pub fn into_v8(self) -> crate::backend::v8::table::Table {
        match self.0 {
            BackendTable::V8(s) => s,
            _ => panic!("Not a `v8` table!"),
        }
    }

    /// Convert a reference to [`self`] into a reference [`crate::backend::v8::table::Table`].
    pub fn as_v8(&self) -> &crate::backend::v8::table::Table {
        match &self.0 {
            BackendTable::V8(s) => s,
            _ => panic!("Not a `v8` table!"),
        }
    }

    /// Convert a mutable reference to [`self`] into a mutable reference [`crate::backend::v8::table::Table`].
    pub fn as_v8_mut(&mut self) -> &mut crate::backend::v8::table::Table {
        match &mut self.0 {
            BackendTable::V8(s) => s,
            _ => panic!("Not a `v8` table!"),
        }
    }
}

impl crate::BackendTable {
    /// Consume [`self`] into [`crate::backend::v8::table::Table`].
    pub fn into_v8(self) -> crate::backend::v8::table::Table {
        match self {
            Self::V8(s) => s,
            _ => panic!("Not a `v8` table!"),
        }
    }

    /// Convert a reference to [`self`] into a reference [`crate::backend::v8::table::Table`].
    pub fn as_v8(&self) -> &crate::backend::v8::table::Table {
        match self {
            Self::V8(s) => s,
            _ => panic!("Not a `v8` table!"),
        }
    }

    /// Convert a mutable reference to [`self`] into a mutable reference [`crate::backend::v8::table::Table`].
    pub fn as_v8_mut(&mut self) -> &mut crate::backend::v8::table::Table {
        match self {
            Self::V8(s) => s,
            _ => panic!("Not a `v8` table!"),
        }
    }
}

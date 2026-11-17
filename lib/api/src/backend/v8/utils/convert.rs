use crate::backend::v8::origin::Handle;
use wasmer_types::{ExternType, FunctionType, Mutability, Type};

/// Utilities to convert between `v8` and `wasmer` values
use crate::{
    BackendFunction, Function, Value,
    v8::{
        bindings::{self, *},
        function,
    },
};

pub trait IntoCApiValue {
    /// Borrow a value only after validating the canonical declared type.
    fn into_cv(self, expected: Type) -> Result<wasm_val_t, crate::RuntimeError>;
}

impl IntoCApiValue for Value {
    fn into_cv(self, expected: Type) -> Result<wasm_val_t, crate::RuntimeError> {
        // Null carries no callable/object identity. Preserve the C API's
        // generic null-table initializer while stamping its declared family.
        if matches!(&self, Value::FuncRef(None) | Value::ExternRef(None))
            && matches!(expected, Type::FuncRef | Type::ExternRef)
        {
            return Ok(wasm_val_t {
                kind: if expected == Type::FuncRef {
                    bindings::wasm_valkind_enum_WASM_FUNCREF
                } else {
                    bindings::wasm_valkind_enum_WASM_EXTERNREF
                } as _,
                of: wasm_val_t__bindgen_ty_1 {
                    ref_: std::ptr::null_mut(),
                },
            });
        }
        if self.ty() != expected {
            return Err(crate::RuntimeError::new(
                "V8 value does not match its declared type",
            ));
        }
        Ok(match self {
            Self::I32(value) => wasm_val_t {
                kind: bindings::wasm_valkind_enum_WASM_I32 as _,
                of: wasm_val_t__bindgen_ty_1 { i32_: value },
            },
            Self::I64(value) => wasm_val_t {
                kind: bindings::wasm_valkind_enum_WASM_I64 as _,
                of: wasm_val_t__bindgen_ty_1 { i64_: value },
            },
            Self::F32(value) => wasm_val_t {
                kind: bindings::wasm_valkind_enum_WASM_F32 as _,
                of: wasm_val_t__bindgen_ty_1 { f32_: value },
            },
            Self::F64(value) => wasm_val_t {
                kind: bindings::wasm_valkind_enum_WASM_F64 as _,
                of: wasm_val_t__bindgen_ty_1 { f64_: value },
            },
            Self::FuncRef(Some(value)) => {
                let BackendFunction::V8(function) = value.0 else {
                    return Err(crate::RuntimeError::new(
                        "V8 function reference has a foreign backend",
                    ));
                };
                function.handle.origin.check_alive()?;
                wasm_val_t {
                    kind: bindings::wasm_valkind_enum_WASM_FUNCREF as _,
                    of: wasm_val_t__bindgen_ty_1 {
                        ref_: unsafe { wasm_func_as_ref(function.handle.raw()) },
                    },
                }
            }
            Self::FuncRef(None) => wasm_val_t {
                kind: bindings::wasm_valkind_enum_WASM_FUNCREF as _,
                of: wasm_val_t__bindgen_ty_1 {
                    ref_: std::ptr::null_mut(),
                },
            },
            Self::ExternRef(None) => wasm_val_t {
                kind: bindings::wasm_valkind_enum_WASM_EXTERNREF as _,
                of: wasm_val_t__bindgen_ty_1 {
                    ref_: std::ptr::null_mut(),
                },
            },
            Self::V128(value) => wasm_val_t {
                kind: bindings::wasm_valkind_enum_WASM_V128 as _,
                of: wasm_val_t__bindgen_ty_1 {
                    v128: wasm_v128_t {
                        bytes: value.to_le_bytes(),
                    },
                },
            },
            Self::ExternRef(Some(_)) | Self::ExceptionRef(_) => {
                return Err(crate::RuntimeError::new(
                    "V8 value reference type is not supported",
                ));
            }
        })
    }
}

pub(crate) fn validate_values(
    values: &[Value],
    expected: &[Type],
) -> Result<(), crate::RuntimeError> {
    if values.len() != expected.len() {
        return Err(crate::RuntimeError::new(
            "V8 value count does not match its signature",
        ));
    }
    for (value, expected) in values.iter().zip(expected) {
        if value.ty() != *expected {
            return Err(crate::RuntimeError::new(
                "V8 value does not match its declared type",
            ));
        }
        if matches!(value, Value::ExternRef(Some(_)) | Value::ExceptionRef(_)) {
            return Err(crate::RuntimeError::new(
                "V8 value reference type is not supported",
            ));
        }
    }
    Ok(())
}

pub(crate) fn validate_raw_values(
    values: &[wasm_val_t],
    expected: &[Type],
) -> Result<(), crate::RuntimeError> {
    if values.len() != expected.len() {
        return Err(crate::RuntimeError::new(
            "V8 value count does not match its signature",
        ));
    }
    for (value, expected) in values.iter().zip(expected) {
        validate_raw_value(value, *expected)?;
    }
    Ok(())
}

fn validate_raw_value(value: &wasm_val_t, expected: Type) -> Result<(), crate::RuntimeError> {
    let kind = value.kind as bindings::wasm_valkind_enum;
    let valid = match expected {
        Type::FuncRef | Type::ExternRef => matches!(
            kind,
            bindings::wasm_valkind_enum_WASM_FUNCREF | bindings::wasm_valkind_enum_WASM_EXTERNREF
        ),
        Type::I32 => kind == bindings::wasm_valkind_enum_WASM_I32,
        Type::I64 => kind == bindings::wasm_valkind_enum_WASM_I64,
        Type::F32 => kind == bindings::wasm_valkind_enum_WASM_F32,
        Type::F64 => kind == bindings::wasm_valkind_enum_WASM_F64,
        Type::V128 => kind == bindings::wasm_valkind_enum_WASM_V128,
        _ => false,
    };
    if !valid {
        return Err(crate::RuntimeError::new(
            "V8 value does not match its declared type",
        ));
    }
    if expected == Type::ExternRef && !unsafe { value.of.ref_ }.is_null() {
        return Err(crate::RuntimeError::new(
            "V8 nonnull externref is not supported",
        ));
    }
    Ok(())
}

pub trait IntoWasmerValue {
    /// The pinned C++ Val(own<Ref>) uses EXTERNREF for both reference families.
    /// Only the already-validated signature supplies the actual Wasm type.
    fn into_wv(
        self,
        expected: Type,
        store: &impl crate::AsStoreRef,
    ) -> Result<Value, crate::RuntimeError>;
}

impl IntoWasmerValue for wasm_val_t {
    fn into_wv(
        self,
        expected: Type,
        store: &impl crate::AsStoreRef,
    ) -> Result<Value, crate::RuntimeError> {
        validate_raw_value(&self, expected)?;
        let view = store.as_store_ref();
        if !view.inner.store.is_v8() {
            return Err(crate::RuntimeError::new(
                "V8 value requires its actual V8 Store",
            ));
        }
        let backend = view.inner.store.as_v8();
        let origin = backend.origin.capture();
        origin.check_alive()?;
        if matches!(expected, Type::FuncRef | Type::ExternRef) {
            let raw = unsafe { self.of.ref_ };
            return match expected {
                Type::FuncRef if raw.is_null() => Ok(Value::FuncRef(None)),
                Type::FuncRef => {
                    // The checked producer's declared FuncRef type, not the
                    // generic C++ Val tag, certifies this callable reference.
                    let retained = backend.reference_roots.copy(raw, true);
                    let function = unsafe { wasm_ref_as_func(retained) };
                    Ok(Value::FuncRef(Some(Function(BackendFunction::V8(
                        function::Function {
                            handle: Handle::new(function, origin),
                        },
                    )))))
                }
                Type::ExternRef => Ok(Value::ExternRef(None)),
                _ => unreachable!(),
            };
        }
        Ok(match expected {
            Type::I32 => Value::I32(unsafe { self.of.i32_ }),
            Type::I64 => Value::I64(unsafe { self.of.i64_ }),
            Type::F32 => Value::F32(unsafe { self.of.f32_ }),
            Type::F64 => Value::F64(unsafe { self.of.f64_ }),
            Type::V128 => Value::V128(u128::from_le_bytes(unsafe { self.of.v128.bytes })),
            _ => unreachable!(),
        })
    }
}

pub trait IntoWasmerType {
    /// Consume [`self`] to produce a [`Type`].
    fn into_wt(self) -> Type;
}

impl IntoWasmerType for wasm_valkind_t {
    fn into_wt(self) -> Type {
        match self as _ {
            bindings::wasm_valkind_enum_WASM_I32 => Type::I32,
            bindings::wasm_valkind_enum_WASM_I64 => Type::I64,
            bindings::wasm_valkind_enum_WASM_F32 => Type::F32,
            bindings::wasm_valkind_enum_WASM_F64 => Type::F64,
            bindings::wasm_valkind_enum_WASM_EXTERNREF => Type::ExternRef,
            bindings::wasm_valkind_enum_WASM_FUNCREF => Type::FuncRef,
            bindings::wasm_valkind_enum_WASM_V128 => Type::V128,
            _ => unreachable!("v8 kind {self:?} has no matching wasmer type"),
        }
    }
}

pub trait IntoCApiType {
    /// Consume [`self`] to produce a [`wasm_valkind_t`].
    fn into_ct(self) -> wasm_valkind_t;
}

impl IntoCApiType for Type {
    fn into_ct(self) -> wasm_valkind_t {
        match self as _ {
            Self::I32 => bindings::wasm_valkind_enum_WASM_I32 as _,
            Self::I64 => bindings::wasm_valkind_enum_WASM_I64 as _,
            Self::F32 => bindings::wasm_valkind_enum_WASM_F32 as _,
            Self::F64 => bindings::wasm_valkind_enum_WASM_F64 as _,
            Self::FuncRef => bindings::wasm_valkind_enum_WASM_FUNCREF as _,
            Self::ExternRef => bindings::wasm_valkind_enum_WASM_EXTERNREF as _,
            Self::V128 => bindings::wasm_valkind_enum_WASM_V128 as _,
            Self::ExceptionRef => panic!("v8 currently does not support exnrefs"),
        }
    }
}

impl IntoWasmerType for wasm_valtype_t {
    fn into_wt(self) -> Type {
        let type_: wasm_valkind_t = unsafe { wasm_valtype_kind(&self as *const _) };
        type_.into_wt()
    }
}

impl IntoWasmerType for *const wasm_valtype_t {
    fn into_wt(self) -> Type {
        let type_: wasm_valkind_t = unsafe { wasm_valtype_kind(self as _) };
        type_.into_wt()
    }
}

impl IntoWasmerType for *mut wasm_valtype_t {
    fn into_wt(self) -> Type {
        let type_: wasm_valkind_t = unsafe { wasm_valtype_kind(self as _) };
        type_.into_wt()
    }
}

pub trait IntoWasmerExternType {
    /// Consume [`self`] to produce a [`wasm_valkind_t`].
    unsafe fn into_wextt(self) -> Result<ExternType, String>;
}

impl IntoWasmerExternType for wasm_externtype_t {
    unsafe fn into_wextt(mut self) -> Result<ExternType, String> {
        unsafe { (&self as *const Self).into_wextt() }
    }
}

impl IntoWasmerExternType for *mut wasm_externtype_t {
    unsafe fn into_wextt(self) -> Result<ExternType, String> {
        unsafe { (self as *const wasm_externtype_t).into_wextt() }
    }
}

impl IntoWasmerExternType for *const wasm_externtype_t {
    unsafe fn into_wextt(self) -> Result<ExternType, String> {
        let ret = unsafe {
            let kind = wasm_externtype_kind(self);
            match kind as _ {
                bindings::wasm_externkind_enum_WASM_EXTERN_FUNC => {
                    let functype = wasm_externtype_as_functype_const(self);
                    let params = wasm_functype_params(functype);
                    let params = if params.is_null() {
                        vec![]
                    } else {
                        std::slice::from_raw_parts((*params).data, (*params).size)
                            .to_vec()
                            .into_iter()
                            .map(|v| v.into_wt())
                            .collect::<Vec<_>>()
                    };

                    let returns = wasm_functype_results(functype);
                    let returns = if returns.is_null() {
                        vec![]
                    } else {
                        std::slice::from_raw_parts((*returns).data, (*returns).size)
                            .to_vec()
                            .into_iter()
                            .map(|v| v.into_wt())
                            .collect::<Vec<_>>()
                    };

                    ExternType::Function(FunctionType::new(params, returns))
                }
                bindings::wasm_externkind_enum_WASM_EXTERN_GLOBAL => {
                    let globaltype = wasm_externtype_as_globaltype_const(self);
                    let valtype = wasm_globaltype_content(globaltype);
                    let mutability = if wasm_globaltype_mutability(globaltype)
                        == bindings::wasm_mutability_enum_WASM_CONST as u8
                    {
                        Mutability::Const
                    } else {
                        Mutability::Var
                    };
                    ExternType::Global(wasmer_types::GlobalType {
                        ty: valtype.into_wt(),
                        mutability,
                    })
                }

                bindings::wasm_externkind_enum_WASM_EXTERN_TABLE => {
                    let tabletype = wasm_externtype_as_tabletype_const(self);
                    let valtype = wasm_tabletype_element(tabletype);
                    let limits = *wasm_tabletype_limits(tabletype);
                    ExternType::Table(wasmer_types::TableType {
                        ty: valtype.into_wt(),
                        minimum: limits.min,
                        maximum: if limits.max == 0 || limits.max == u32::MAX {
                            None
                        } else {
                            Some(limits.max)
                        },
                        readonly: false,
                    })
                }
                bindings::wasm_externkind_enum_WASM_EXTERN_MEMORY => {
                    let memorytype = wasm_externtype_as_memorytype_const(self);
                    let limits = *wasm_memorytype_limits(memorytype);
                    ExternType::Memory(wasmer_types::MemoryType {
                        minimum: wasmer_types::Pages(limits.min),
                        maximum: if limits.max == 0 || limits.max == u32::MAX {
                            None
                        } else {
                            Some(wasmer_types::Pages(limits.max))
                        },
                        shared: limits.shared,
                    })
                }
                bindings::wasm_externkind_enum_WASM_EXTERN_TAG => {
                    let tagtype = wasm_externtype_as_tagtype_const(self);
                    let params = wasm_tagtype_params(tagtype);
                    let params = if params.is_null() {
                        vec![]
                    } else {
                        std::slice::from_raw_parts((*params).data, (*params).size)
                            .to_vec()
                            .into_iter()
                            .map(|v| v.into_wt())
                            .collect::<Vec<_>>()
                    };

                    ExternType::Tag(wasmer_types::TagType {
                        kind: wasmer_types::TagKind::Exception,
                        params: params.into(),
                    })
                }
                _ => return Err(String::from("Unsupported extern kind!")),
            }
        };

        Ok(ret)
    }
}

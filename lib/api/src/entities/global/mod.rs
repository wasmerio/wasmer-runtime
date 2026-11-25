use crate::{
    ExportError, Exportable, Extern,
    error::RuntimeError,
    store::{AsStoreMut, AsStoreRef, StoreMut, StoreRef},
    value::Value,
    vm::{VMExtern, VMExternGlobal},
};
use wasmer_types::{GlobalType, Mutability};

pub(crate) mod inner;
pub(crate) use inner::*;

/// A WebAssembly `global` instance.
///
/// A global instance is the runtime representation of a global variable.
/// It consists of an individual value and a flag indicating whether it is mutable.
///
/// Spec: <https://webassembly.github.io/spec/core/exec/runtime.html#global-instances>
#[derive(Debug, Clone, PartialEq, Eq, derive_more::From)]
#[cfg_attr(feature = "artifact-size", derive(loupe::MemoryUsage))]
pub struct Global(pub(crate) BackendGlobal);

impl Global {
    /// Create a new global with the initial [`Value`].
    ///
    /// # Example
    ///
    /// ```
    /// # use wasmer::{Global, Mutability, Store, Value};
    /// # let mut store = Store::default();
    /// #
    /// let g = Global::new(&mut store, Value::I32(1));
    ///
    /// assert_eq!(g.get(&mut store), Value::I32(1));
    /// assert_eq!(g.ty(&mut store).mutability, Mutability::Const);
    /// ```
    ///
    /// # Panics
    ///
    /// Panics when [`Global::try_new`] returns an error, that is when `val`:
    ///
    /// - is a reference (a `funcref`, an `externref` or an exception
    ///   reference) of another store;
    /// - is of a type the backend of `store` cannot create a global of: an
    ///   `externref` or an exception reference on `v8`; a reference, a `v128`
    ///   or an `i64` on `js`.
    ///
    /// Use [`Global::try_new`] for a value you do not control.
    ///
    /// ```should_panic
    /// # use wasmer::{Function, Global, Store, Value};
    /// # let mut store = Store::default();
    /// # let mut other_store = Store::default();
    /// #
    /// let f = Function::new_typed(&mut other_store, || {});
    ///
    /// // This panics: `f` is a function of another store.
    /// Global::new(&mut store, Value::FuncRef(Some(f)));
    /// ```
    pub fn new(store: &mut impl AsStoreMut, val: Value) -> Self {
        Self::try_new(store, val).unwrap()
    }

    /// Create a new global with the initial [`Value`], or return an error
    /// where [`Global::new`] panics.
    ///
    /// # Example
    ///
    /// ```
    /// # use wasmer::{Global, Mutability, Store, Value};
    /// # let mut store = Store::default();
    /// #
    /// let g = Global::try_new(&mut store, Value::I32(1)).unwrap();
    ///
    /// assert_eq!(g.get(&mut store), Value::I32(1));
    /// assert_eq!(g.ty(&mut store).mutability, Mutability::Const);
    /// ```
    ///
    /// # Errors
    ///
    /// Trying to create a global with a reference of another store will
    /// raise an error:
    ///
    /// ```
    /// # use wasmer::{Function, Global, Store, Value};
    /// # let mut store = Store::default();
    /// # let mut other_store = Store::default();
    /// #
    /// let f = Function::new_typed(&mut other_store, || {});
    ///
    /// assert!(Global::try_new(&mut store, Value::FuncRef(Some(f))).is_err());
    /// ```
    ///
    /// So will a value of a type the backend of `store` cannot create a
    /// global of: an `externref` or an exception reference on `v8`; a
    /// reference, a `v128` or an `i64` on `js`.
    pub fn try_new(store: &mut impl AsStoreMut, val: Value) -> Result<Self, RuntimeError> {
        Self::from_value(store, val, Mutability::Const)
    }

    /// Create a mutable global with the initial [`Value`].
    ///
    /// # Example
    ///
    /// ```
    /// # use wasmer::{Global, Mutability, Store, Value};
    /// # let mut store = Store::default();
    /// #
    /// let g = Global::new_mut(&mut store, Value::I32(1));
    ///
    /// assert_eq!(g.get(&mut store), Value::I32(1));
    /// assert_eq!(g.ty(&mut store).mutability, Mutability::Var);
    /// ```
    ///
    /// # Panics
    ///
    /// Panics when [`Global::try_new_mut`] returns an error, on the same
    /// values as [`Global::new`]. Use [`Global::try_new_mut`] for a value you
    /// do not control.
    pub fn new_mut(store: &mut impl AsStoreMut, val: Value) -> Self {
        Self::try_new_mut(store, val).unwrap()
    }

    /// Create a mutable global with the initial [`Value`], or return an error
    /// where [`Global::new_mut`] panics.
    ///
    /// # Example
    ///
    /// ```
    /// # use wasmer::{Global, Mutability, Store, Value};
    /// # let mut store = Store::default();
    /// #
    /// let g = Global::try_new_mut(&mut store, Value::I32(1)).unwrap();
    ///
    /// assert_eq!(g.get(&mut store), Value::I32(1));
    /// assert_eq!(g.ty(&mut store).mutability, Mutability::Var);
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error on the same values as [`Global::try_new`]:
    ///
    /// ```
    /// # use wasmer::{Function, Global, Store, Value};
    /// # let mut store = Store::default();
    /// # let mut other_store = Store::default();
    /// #
    /// let f = Function::new_typed(&mut other_store, || {});
    ///
    /// assert!(Global::try_new_mut(&mut store, Value::FuncRef(Some(f))).is_err());
    /// ```
    pub fn try_new_mut(store: &mut impl AsStoreMut, val: Value) -> Result<Self, RuntimeError> {
        Self::from_value(store, val, Mutability::Var)
    }

    /// Create a global with the initial [`Value`] and the provided [`Mutability`].
    fn from_value(
        store: &mut impl AsStoreMut,
        val: Value,
        mutability: Mutability,
    ) -> Result<Self, RuntimeError> {
        BackendGlobal::from_value(store, val, mutability).map(Self)
    }

    /// Returns the [`GlobalType`] of the global.
    ///
    /// # Example
    ///
    /// ```
    /// # use wasmer::{Global, Mutability, Store, Type, Value, GlobalType};
    /// # let mut store = Store::default();
    /// #
    /// let c = Global::new(&mut store, Value::I32(1));
    /// let v = Global::new_mut(&mut store, Value::I64(1));
    ///
    /// assert_eq!(c.ty(&mut store), GlobalType::new(Type::I32, Mutability::Const));
    /// assert_eq!(v.ty(&mut store), GlobalType::new(Type::I64, Mutability::Var));
    /// ```
    pub fn ty(&self, store: &impl AsStoreRef) -> GlobalType {
        self.0.ty(store)
    }

    /// Retrieves the current value [`Value`] that the global has.
    ///
    /// # Example
    ///
    /// ```
    /// # use wasmer::{Global, Store, Value};
    /// # let mut store = Store::default();
    /// #
    /// let g = Global::new(&mut store, Value::I32(1));
    ///
    /// assert_eq!(g.get(&mut store), Value::I32(1));
    /// ```
    pub fn get(&self, store: &mut impl AsStoreMut) -> Value {
        self.0.get(store)
    }

    /// Sets a custom [`Value`] to the runtime global.
    ///
    /// # Example
    ///
    /// ```
    /// # use wasmer::{Global, Store, Value};
    /// # let mut store = Store::default();
    /// #
    /// let g = Global::new_mut(&mut store, Value::I32(1));
    ///
    /// assert_eq!(g.get(&mut store), Value::I32(1));
    ///
    /// g.set(&mut store, Value::I32(2));
    ///
    /// assert_eq!(g.get(&mut store), Value::I32(2));
    /// ```
    ///
    /// # Errors
    ///
    /// Trying to mutate a immutable global will raise an error:
    ///
    /// ```should_panic
    /// # use wasmer::{Global, Store, Value};
    /// # let mut store = Store::default();
    /// #
    /// let g = Global::new(&mut store, Value::I32(1));
    ///
    /// g.set(&mut store, Value::I32(2)).unwrap();
    /// ```
    ///
    /// Trying to set a value of a incompatible type will raise an error:
    ///
    /// ```should_panic
    /// # use wasmer::{Global, Store, Value};
    /// # let mut store = Store::default();
    /// #
    /// let g = Global::new(&mut store, Value::I32(1));
    ///
    /// // This results in an error: `RuntimeError`.
    /// g.set(&mut store, Value::I64(2)).unwrap();
    /// ```
    pub fn set(&self, store: &mut impl AsStoreMut, val: Value) -> Result<(), RuntimeError> {
        self.0.set(store, val)
    }

    pub(crate) fn from_vm_extern(store: &mut impl AsStoreMut, vm_extern: VMExternGlobal) -> Self {
        Self(BackendGlobal::from_vm_extern(store, vm_extern))
    }

    /// Checks whether this global can be used with the given context.
    pub fn is_from_store(&self, store: &impl AsStoreRef) -> bool {
        self.0.is_from_store(store)
    }

    /// Create a [`VMExtern`] from self.
    pub(crate) fn to_vm_extern(&self) -> VMExtern {
        self.0.to_vm_extern()
    }
}

impl<'a> Exportable<'a> for Global {
    fn get_self_from_extern(_extern: &'a Extern) -> Result<&'a Self, ExportError> {
        match _extern {
            Extern::Global(global) => Ok(global),
            _ => Err(ExportError::IncompatibleType),
        }
    }
}

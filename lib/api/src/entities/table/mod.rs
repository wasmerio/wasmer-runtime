use wasmer_types::TableType;

pub(crate) mod inner;
pub(crate) use inner::*;

use crate::{
    AsStoreMut, AsStoreRef, ExportError, Exportable, Extern, StoreMut, StoreRef, Value,
    error::RuntimeError,
    store::BackendStore,
    vm::{VMExtern, VMExternTable},
};

/// A WebAssembly `table` instance.
///
/// The `Table` struct is an array-like structure representing a WebAssembly Table,
/// which stores function references.
///
/// A table created by the host or in WebAssembly code will be accessible and
/// mutable from both host and WebAssembly.
///
/// Spec: <https://webassembly.github.io/spec/core/exec/runtime.html#table-instances>
#[derive(Debug, Clone, PartialEq, Eq, derive_more::From)]
#[cfg_attr(feature = "artifact-size", derive(loupe::MemoryUsage))]
pub struct Table(pub(crate) BackendTable);

impl Table {
    /// Creates a new table with the provided [`TableType`] definition.
    ///
    /// All the elements in the table will be set to the `init` value.
    ///
    /// This function will construct the table using the store `BaseTunables`.
    pub fn new(
        store: &mut impl AsStoreMut,
        ty: TableType,
        init: Value,
    ) -> Result<Self, RuntimeError> {
        BackendTable::new(store, ty, init).map(Self)
    }

    /// Returns the [`TableType`] of the table.
    pub fn ty(&self, store: &impl AsStoreRef) -> TableType {
        self.0.ty(store)
    }

    /// Retrieves an element of the table at the provided `index`.
    pub fn get(&self, store: &mut impl AsStoreMut, index: u32) -> Option<Value> {
        self.0.get(store, index)
    }

    /// Retrieves the function at the provided `index`, if its signature is
    /// known.
    ///
    /// Returns `None` if `index` is out of bounds, if the element is null or
    /// not a function, and on the `js` backend if the function carries no
    /// signature. JavaScript does not tell the signature of a WebAssembly
    /// function, so Wasmer only knows it for an export of an instance and,
    /// with the `wasm-types-polyfill` feature, for a function that an active
    /// element segment put in the table: not for one that a guest stored
    /// there as it ran. [`Table::get`] returns such a function as a
    /// `() -> ()` one, which this method tells from a function that really
    /// has that signature. The other backends know the signature of every
    /// function.
    ///
    /// # Example
    ///
    /// ```
    /// # use wasmer::{FunctionType, Instance, Module, Store, Type, imports};
    /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
    /// # let mut store = Store::default();
    /// let module = Module::new(&store, r#"(module
    ///   (table (export "table") 2 funcref)
    ///   (func $double (param i32) (result i32) (i32.mul (local.get 0) (i32.const 2)))
    ///   (elem (i32.const 0) $double))"#)?;
    /// let instance = Instance::new(&mut store, &module, &imports! {})?;
    /// let table = instance.exports.get_table("table")?;
    ///
    /// let double = table.get_typed_function(&mut store, 0).unwrap();
    /// assert_eq!(double.ty(&store), FunctionType::new([Type::I32], [Type::I32]));
    /// // A null element, and an index outside the table.
    /// assert!(table.get_typed_function(&mut store, 1).is_none());
    /// assert!(table.get_typed_function(&mut store, 2).is_none());
    /// # Ok(())
    /// # }
    /// ```
    pub fn get_typed_function(
        &self,
        store: &mut impl AsStoreMut,
        index: u32,
    ) -> Option<crate::Function> {
        self.0.get_typed_function(store, index)
    }

    /// Sets an element `val` in the Table at the provided `index`.
    pub fn set(
        &self,
        store: &mut impl AsStoreMut,
        index: u32,
        val: Value,
    ) -> Result<(), RuntimeError> {
        self.0.set(store, index, val)
    }

    /// Retrieves the size of the `Table` (in elements)
    pub fn size(&self, store: &impl AsStoreRef) -> u32 {
        self.0.size(store)
    }

    /// Grows the size of the `Table` by `delta`, initializing
    /// the elements with the provided `init` value.
    ///
    /// It returns the previous size of the `Table` in case is able
    /// to grow the Table successfully.
    ///
    /// # Errors
    ///
    /// Returns an error if the `delta` is out of bounds for the table.
    pub fn grow(
        &self,
        store: &mut impl AsStoreMut,
        delta: u32,
        init: Value,
    ) -> Result<u32, RuntimeError> {
        self.0.grow(store, delta, init)
    }

    /// Copies the `len` elements of `src_table` starting at `src_index`
    /// to the destination table `dst_table` at index `dst_index`.
    ///
    /// # Errors
    ///
    /// Returns an error if the range is out of bounds of either the source or
    /// destination tables.
    pub fn copy(
        store: &mut impl AsStoreMut,
        dst_table: &Self,
        dst_index: u32,
        src_table: &Self,
        src_index: u32,
        len: u32,
    ) -> Result<(), RuntimeError> {
        if !src_table.is_from_store(store) {
            return Err(RuntimeError::new(
                "source table does not belong to the provided Store",
            ));
        }
        if !dst_table.is_from_store(store) {
            return Err(RuntimeError::new(
                "destination table does not belong to the provided Store",
            ));
        }

        let dst_ty = dst_table.ty(store).ty;
        let src_ty = src_table.ty(store).ty;
        if dst_ty != src_ty {
            return Err(RuntimeError::new(format!(
                "source table element type {src_ty} does not match destination table element type {dst_ty}"
            )));
        }

        BackendTable::copy(store, &dst_table.0, dst_index, &src_table.0, src_index, len)
    }

    pub(crate) fn from_vm_extern(store: &mut impl AsStoreMut, ext: VMExternTable) -> Self {
        Self(BackendTable::from_vm_extern(store, ext))
    }

    /// Checks whether this `Table` can be used with the given context.
    pub fn is_from_store(&self, store: &impl AsStoreRef) -> bool {
        self.0.is_from_store(store)
    }

    pub(crate) fn to_vm_extern(&self) -> VMExtern {
        self.0.to_vm_extern()
    }
}

impl<'a> Exportable<'a> for Table {
    fn get_self_from_extern(ext: &'a Extern) -> Result<&'a Self, ExportError> {
        match ext {
            Extern::Table(table) => Ok(table),
            _ => Err(ExportError::IncompatibleType),
        }
    }
}

#[cfg(test)]
mod test {
    /// Check the example from <https://github.com/wasmerio/wasmer/issues/3197>.
    #[test]
    #[cfg_attr(
        feature = "v8",
        ignore = "growing tables in v8 is not currently supported"
    )]
    fn table_grow_issue_3197() {
        use crate::{Instance, Module, Store, Table, TableType, Type, Value, imports};

        const WAT: &str = r#"(module (table (import "env" "table") 100 funcref))"#;

        // Tests that the table type of `table` is compatible with the export in the WAT
        // This tests that `wasmer_types::types::is_table_compatible` works as expected.
        let mut store = Store::default();
        let module = Module::new(&store, WAT).unwrap();
        let ty = TableType::new(Type::FuncRef, 0, None);
        let table = Table::new(&mut store, ty, Value::FuncRef(None)).unwrap();
        table.grow(&mut store, 100, Value::FuncRef(None)).unwrap();
        assert_eq!(table.ty(&store).minimum, 0);
        let imports = imports! {"env" => {"table" => table}};
        let _instance = Instance::new(&mut store, &module, &imports).unwrap();
    }
}

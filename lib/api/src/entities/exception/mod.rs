pub(crate) mod inner;
pub(crate) use inner::*;

use crate::{AsStoreMut, AsStoreRef, RuntimeError, Tag, Value, vm::VMExceptionRef};

/// A WebAssembly `exception` instance.
///
/// An exception is an internal construct in WebAssembly that represents a runtime object that can
/// be thrown. A WebAssembly exception consists of an exception tag and its runtime arguments.
///
/// Spec: <https://github.com/WebAssembly/exception-handling/blob/main/proposals/exception-handling/Exceptions.md#exceptions>
#[derive(Debug, Clone, PartialEq, Eq, derive_more::From)]
#[cfg_attr(feature = "artifact-size", derive(loupe::MemoryUsage))]
pub struct Exception(pub(crate) BackendException);

impl Exception {
    /// Create a new exception with the given tag and payload, and also creates
    /// a reference to it, returning the reference.
    ///
    /// ```
    /// # use wasmer::{Exception, Store, Tag, Type, Value};
    /// let mut store = Store::default();
    /// let tag = Tag::new(&mut store, [Type::I32])?;
    ///
    /// let exception = Exception::new(&mut store, &tag, &[Value::I32(42)])?;
    /// assert_eq!(exception.payload(&mut store), vec![Value::I32(42)]);
    ///
    /// // The payload must match the parameters of the tag.
    /// assert!(Exception::new(&mut store, &tag, &[Value::I64(42)]).is_err());
    /// # Ok::<(), wasmer::RuntimeError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns an error if `tag` or a value of `payload` comes from another
    /// store, if `payload` does not have one value of the type of each
    /// parameter of `tag`, or if the backend has no exceptions (only `sys`
    /// does).
    pub fn new(
        store: &mut impl AsStoreMut,
        tag: &Tag,
        payload: &[Value],
    ) -> Result<Self, RuntimeError> {
        BackendException::new(store, tag, payload).map(Self)
    }

    /// Check whether this `Exception` comes from the given store.
    pub fn is_from_store(&self, store: &impl AsStoreRef) -> bool {
        self.0.is_from_store(store)
    }

    /// Get the exception tag.
    ///
    /// # Panics
    ///
    /// Panics if the exception comes from another store, as other objects do.
    pub fn tag(&self, store: &impl AsStoreRef) -> Tag {
        self.0.tag(store)
    }

    /// Get the exception payload values.
    ///
    /// # Panics
    ///
    /// Panics if the exception comes from another store, as other objects do.
    pub fn payload(&self, store: &mut impl AsStoreMut) -> Vec<Value> {
        self.0.payload(store)
    }

    /// Get the `VMExceptionRef` corresponding to this `Exception`.
    pub(crate) fn vm_exceptionref(&self) -> VMExceptionRef {
        self.0.vm_exceptionref()
    }

    /// Create an `Exception` from a `VMExceptionRef`, which is not checked.
    pub(crate) fn from_vm_exceptionref(exnref: VMExceptionRef) -> Self {
        Self(BackendException::from_vm_exceptionref(exnref))
    }
}

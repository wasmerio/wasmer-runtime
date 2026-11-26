//! Data types, functions and traits for `js` runtime's `Exception` implementation.

use crate::js::vm::VMException;

#[derive(Debug, Clone, PartialEq, Eq)]
/// A WebAssembly exception in the `js` runtime.
pub(crate) struct Exception {
    pub(crate) handle: VMException,
}

unsafe impl Send for Exception {}
unsafe impl Sync for Exception {}

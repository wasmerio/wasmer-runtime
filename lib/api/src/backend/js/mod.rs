//! Data types, functions and traits for the `js` backend.

pub(crate) mod entities;
pub(crate) mod error;
#[cfg(feature = "async")]
pub(crate) mod boundary;
#[cfg(feature = "async")]
pub(crate) mod guard;
#[cfg(feature = "async")]
pub(crate) mod jspi;
pub(crate) mod lent;
pub(crate) mod utils;
pub(crate) mod vm;

pub(crate) use entities::*;
pub use memory::JsSharedMemoryExt;
pub use utils::*;

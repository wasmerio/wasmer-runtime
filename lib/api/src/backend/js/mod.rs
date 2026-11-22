//! Data types, functions and traits for the `js` backend.

#[cfg(feature = "experimental-async")]
pub(crate) mod async_runtime;
pub(crate) mod entities;
pub(crate) mod error;
pub(crate) mod guest_guard;
#[cfg(feature = "experimental-async")]
pub(crate) mod jspi;
pub(crate) mod lent;
pub(crate) mod utils;
pub(crate) mod vm;

pub use entities::*;
pub use utils::*;

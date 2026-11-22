//! Data types, functions and traits for the `v8` backend.

#[cfg(feature = "experimental-async")]
pub(crate) mod async_call;
#[cfg(feature = "experimental-async")]
pub(crate) use async_call::guest_entry;

/// One guest entry through V8's C API.
#[cfg(not(feature = "experimental-async"))]
#[inline]
pub(crate) fn guest_entry<R>(_store: &mut impl crate::AsStoreMut, enter: impl FnOnce() -> R) -> R {
    enter()
}

pub(crate) mod bindings;
pub(crate) mod entities;
pub(crate) mod error;
pub(crate) mod utils;
pub(crate) mod vm;

pub use entities::{engine::Engine as V8, *};

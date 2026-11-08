//! Data types, functions and traits for the `v8` backend.

pub(crate) mod bindings;
pub(crate) mod entities;
pub(crate) mod error;
pub(crate) mod utils;
pub(crate) mod vm;

pub use entities::{engine::Engine as V8, *};

#[cfg(feature = "experimental-async")]
pub(crate) mod async_runtime;
#[cfg(feature = "experimental-async")]
pub use async_runtime::{BlockingExecutor, set_blocking_executor};

pub(crate) mod origin;

//! Data types, functions and traits for the `sys` runtime.

#[cfg(feature = "experimental-async")]
pub(crate) mod async_runtime;
#[cfg(feature = "experimental-async")]
#[doc(hidden)]
pub use async_runtime::{
    NativeAncestrySettlement, NativeCleanupExitBoundary, NativeCleanupFence, NativeCleanupGuard,
    NativeCleanupOwner, NativeFrameGuard, NativeInvocationScope, NativeStoreDestruction,
    NativeStoreDestructionObserver, WeakNativeInvocation, call_async_with_local_owner,
    call_native_descendant, context_switch_supported, current_native_local_owner,
    current_native_store_loan, enroll_native_ancestry_tail, native_cleanup_owner,
};
mod native_frame;
#[doc(hidden)]
pub use native_frame::{
    NativeEngineEntryIdentity, NativeEngineFrame, NativeEngineInvocation, NativeEngineStackBounds,
    NativeSynchronousCallback,
};
#[cfg(feature = "experimental-async")]
#[doc(hidden)]
pub use native_frame::{native_host_work_can_suspend, run_native_host_future};

pub(crate) mod entities;
pub(crate) mod error;
pub(crate) mod tunables;
pub mod vm;

pub use engine::NativeEngineExt;
pub use entities::*;
pub use tunables::*;

#[cfg(feature = "compiler")]
pub use wasmer_compiler::{
    CompilerConfig, FunctionMiddleware, MiddlewareReaderState, ModuleMiddleware, wasmparser,
};

pub use wasmer_compiler::{Artifact, EngineBuilder, Features, FunctionExtent, Tunables};

pub use wasmer_types::MiddlewareError;
pub use wasmer_types::target::{Architecture, CpuFeature, OperatingSystem, Target, Triple};

#[cfg(feature = "cranelift")]
pub use wasmer_compiler_cranelift::{Cranelift, CraneliftOptLevel};
#[cfg(feature = "llvm")]
pub use wasmer_compiler_llvm::{LLVM, LLVMOptLevel};
#[cfg(feature = "singlepass")]
pub use wasmer_compiler_singlepass::Singlepass;

#[doc(hidden)]
pub use wasmer_vm::{
    HostMemoryAccess, HostMemoryAllocation, HostMemoryMappingChange, HostMemoryPin,
};

#[doc(hidden)]
pub use entities::function::env::FunctionEnvFramePin;

#[doc(hidden)]
pub use entities::memory::with_memory_grow_callback;

//! Defines the [`Store`] data type and various useful traits and data types to interact with a
//! store.

/// Defines [`SharedStore`] and how async calls reach their store.
#[cfg(feature = "experimental-async")]
mod async_;
#[cfg(all(test, feature = "experimental-async", feature = "sys"))]
pub(crate) use async_::is_published;
#[cfg(feature = "experimental-async")]
pub(crate) use async_::{CallStore, PollScope, PollStore, SharedState, StoreSlice, WithSlice};
#[cfg(feature = "experimental-async")]
pub use async_::{SharedStore, StoreBusy, WeakSharedStore};
#[cfg(all(feature = "experimental-async", feature = "js"))]
pub(crate) use async_::{publish_store, published_store};

/// Defines the [`StoreContext`] type.
mod context;

/// Defines the [`StoreInner`] data type.
mod inner;

/// Create temporary handles to engines.
mod store_ref;

use std::{
    boxed::Box,
    ops::{Deref, DerefMut},
};

pub use store_ref::*;

mod obj;
pub use obj::*;

use crate::{AsEngineRef, BackendEngine, Engine, EngineRef};
pub(crate) use context::*;
pub(crate) use inner::*;
use wasmer_types::StoreId;

#[cfg(feature = "sys")]
use wasmer_vm::TrapHandlerFn;

/// The store represents all global state that can be manipulated by
/// WebAssembly programs. It consists of the runtime representation
/// of all instances of functions, tables, memories, and globals that
/// have been allocated during the lifetime of the abstract machine.
///
/// The [`Store`] is tied to the underlying [`Engine`] that is — among many things — used to
/// compile the Wasm bytes into a valid module artifact.
///
/// For more information, check out the [related WebAssembly specification]
/// [related WebAssembly specification]: <https://webassembly.github.io/spec/core/exec/runtime.html#store>
pub struct Store {
    pub(crate) inner: StoreBox,
}

/// What owns a store's allocation. On JavaScript, a guest can outlive its
/// call, so the owner cooperates with the guest guard.
#[cfg(not(all(feature = "js", feature = "experimental-async")))]
pub(crate) type StoreBox = Box<StoreInner>;
#[cfg(all(feature = "js", feature = "experimental-async"))]
pub(crate) type StoreBox = crate::backend::js::guard::GuardedStore;

impl Store {
    /// Creates a new `Store` with a specific [`Engine`].
    pub fn new(engine: impl Into<Engine>) -> Self {
        let engine: Engine = engine.into();

        let store = match engine.be {
            #[cfg(feature = "sys")]
            BackendEngine::Sys(_) => {
                BackendStore::Sys(crate::backend::sys::entities::store::Store::new(engine))
            }
            #[cfg(feature = "v8")]
            BackendEngine::V8(_) => {
                BackendStore::V8(crate::backend::v8::entities::store::Store::new(engine))
            }
            #[cfg(feature = "js")]
            BackendEngine::Js(_) => {
                BackendStore::Js(crate::backend::js::entities::store::Store::new(engine))
            }
        };

        let objects = StoreObjects::from_store_ref(&store);
        #[cfg(all(
            feature = "sys",
            feature = "experimental-async",
            feature = "experimental-host-interrupt"
        ))]
        let _teardown = crate::backend::sys::async_runtime::StoreTeardown(objects.id());
        let inner = Box::new(StoreInner {
            objects,
            store,
            #[cfg(all(
                feature = "sys",
                feature = "experimental-async",
                feature = "experimental-host-interrupt"
            ))]
            _teardown,
            #[cfg(feature = "experimental-async")]
            shared: std::rc::Weak::new(),
            #[cfg(all(feature = "js", feature = "experimental-async"))]
            guard: Default::default(),
        });
        Self {
            #[cfg(not(all(feature = "js", feature = "experimental-async")))]
            inner,
            #[cfg(all(feature = "js", feature = "experimental-async"))]
            inner: StoreBox::new(inner),
        }
    }

    #[cfg(feature = "sys")]
    /// Set the [`TrapHandlerFn`] for this store.
    ///
    /// # Note
    ///
    /// Not every implementor allows changing the trap handler. In those store that
    /// don't allow it, this function has no effect.
    pub fn set_trap_handler(&mut self, handler: Option<Box<TrapHandlerFn<'static>>>) {
        use crate::backend::sys::entities::store::NativeStoreExt;
        #[allow(irrefutable_let_patterns)]
        if let BackendStore::Sys(ref mut s) = self.inner.store {
            s.set_trap_handler(handler)
        }
    }

    /// Returns the [`Engine`].
    pub fn engine(&self) -> &Engine {
        self.inner.store.engine()
    }

    /// Returns mutable reference to [`Engine`].
    pub fn engine_mut(&mut self) -> &mut Engine {
        self.inner.store.engine_mut()
    }

    /// Checks whether two stores are identical. A store is considered
    /// equal to another store if both have the same engine.
    pub fn same(a: &Self, b: &Self) -> bool {
        a.id() == b.id()
    }

    /// Returns the ID of this store
    pub fn id(&self) -> StoreId {
        self.inner.objects.id()
    }

    /// Runs `f` with the store this thread is currently executing, if that
    /// store has been lent out with [`StoreMut::parked`].
    ///
    /// Host code called from Wasm normally receives the store as a
    /// [`FunctionEnvMut`](crate::FunctionEnvMut), but code the host in turn
    /// calls into cannot always be handed one: an embedded engine invoking a
    /// callback, a C library's allocator hook, anything reached through frames
    /// that carry no Rust references. This is how such code gets the store the
    /// current call is running under, so it can use the ordinary store-taking
    /// APIs — [`Memory::grow`](crate::Memory::grow) and friends — instead of
    /// reaching around them.
    ///
    /// Returns `None` unless a store is executing on this thread *and* every
    /// borrow of it on this thread's stack has been parked. Holding a
    /// [`StoreMut`] and calling this would alias that borrow, so the answer
    /// there is `None` rather than a second handle to the same store; parking
    /// is what makes the outstanding borrow provably unreachable.
    ///
    /// A store starts executing when a call enters it, which only the `sys`
    /// backend tracks — so on the others this is `None` inside a host function.
    /// [`StoreMut::parked`] works everywhere, though, so an embedder can always
    /// lend a store it owns, whether or not a call is running.
    ///
    /// Nothing guarantees the store is the one the caller expects — a thread
    /// may run several — so check any handle before using it, with
    /// [`Memory::is_from_store`](crate::Memory::is_from_store) or by comparing
    /// [`StoreObjects::id`]. Using a handle from another store panics.
    pub fn with_current<R>(f: impl FnOnce(&mut StoreMut<'_>) -> R) -> Option<R> {
        let mut store = StoreContext::try_get_current_unborrowed()?;
        Some(f(&mut store.as_mut()))
    }

    /// Builds an [`Interrupter`] for this store. Calling [`Interrupter::interrupt`]
    /// will cause running WASM code to terminate immediately with a
    /// [`HostInterrupt`](crate::backend::sys::vm::TrapCode::HostInterrupt) trap.
    ///
    /// Best effort is made to ensure interrupts are handled. However, there is no
    /// guarantee; under rare circumstances, it is possible for the interrupt to be
    /// missed. One such case is when the target thread is about to call WASM code
    /// but has not yet made the call.
    ///
    /// To make sure the code is interrupted, the target thread should notify
    /// the signalling thread that it has finished running in some way, and
    /// the signalling thread must wait for that notification and retry the
    /// interrupt if the notification is not received after some time. Embedders
    /// are expected to implement this logic.
    ///
    /// If an interrupt is delivered while an imported function is running,
    /// the interrupt will simply be stored and processed only when the
    /// imported function returns control to WASM code. No effort is made
    /// to interrupt running imported functions. Embedders are expected to
    /// implement support for interruption of long-running or blocking
    /// imported functions separately.
    #[cfg(all(unix, feature = "experimental-host-interrupt"))]
    pub fn interrupter(&self) -> Interrupter {
        self.inner.objects.interrupter()
    }

    /// Makes this store shareable by several guest stacks.
    ///
    /// Most embedders do not need this: [`Function::call_async`](crate::Function::call_async)
    /// on a plain store already suspends and resumes. A [`SharedStore`] is for
    /// a guest that runs several stacks at once. Two stacks on one instance
    /// share its globals and memory, so this is only correct for guests built
    /// for it.
    #[cfg(feature = "experimental-async")]
    pub fn into_shared(self) -> SharedStore {
        #[cfg(not(feature = "js"))]
        let inner = self.inner;
        #[cfg(feature = "js")]
        let inner = self.inner.into_box();
        SharedStore::new(inner)
    }
}

impl PartialEq for Store {
    fn eq(&self, other: &Self) -> bool {
        Self::same(self, other)
    }
}

impl Default for Store {
    fn default() -> Self {
        Self::new(Engine::default())
    }
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("Store").finish()
    }
}

impl AsEngineRef for Store {
    fn as_engine_ref(&self) -> EngineRef<'_> {
        self.inner.store.as_engine_ref()
    }

    fn maybe_as_store(&self) -> Option<StoreRef<'_>> {
        Some(self.as_store_ref())
    }
}

impl AsStoreRef for Store {
    fn as_store_ref(&self) -> StoreRef<'_> {
        StoreRef::new(&self.inner)
    }
}
impl AsStoreMut for Store {
    fn as_store_mut(&mut self) -> StoreMut<'_> {
        StoreMut::new(&mut self.inner)
    }

    fn objects_mut(&mut self) -> &mut StoreObjects {
        &mut self.inner.objects
    }
}

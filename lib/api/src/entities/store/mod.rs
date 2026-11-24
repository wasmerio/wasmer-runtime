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
#[cfg(feature = "experimental-async")]
#[allow(unused_imports)]
pub(crate) use async_::{publish_store, published_store};

/// Defines the [`StoreContext`] type.
mod context;

/// Defines the [`StoreInner`] data type.
mod inner;

/// Create temporary handles to engines.
mod store_ref;

use std::{
    boxed::Box,
    mem::ManuallyDrop,
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

/// What owns a store's allocation, with what the store's backend keeps
/// beside it. The keeper is asked before the allocation is reached, and is
/// given the allocation when its owner is gone: on a backend where a call
/// can outlive its borrow, host code may still be using the store.
pub(crate) struct StoreBox {
    inner: ManuallyDrop<Box<StoreInner>>,
    keeper: BackendStoreKeeper,
}

impl StoreBox {
    pub(crate) fn new(inner: Box<StoreInner>) -> Self {
        let keeper = inner.store.keeper();
        Self {
            inner: ManuallyDrop::new(inner),
            keeper,
        }
    }

    /// Gives the allocation up to another owner.
    pub(crate) fn into_box(self) -> Box<StoreInner> {
        self.keeper.check_owner();
        let mut this = ManuallyDrop::new(self);
        // Safety: `this` is never dropped, so each field is taken once.
        unsafe {
            std::ptr::drop_in_place(&mut this.keeper);
            ManuallyDrop::take(&mut this.inner)
        }
    }
}

impl Deref for StoreBox {
    type Target = StoreInner;

    #[inline]
    fn deref(&self) -> &StoreInner {
        self.keeper.check_owner();
        &self.inner
    }
}

impl DerefMut for StoreBox {
    #[inline]
    fn deref_mut(&mut self) -> &mut StoreInner {
        self.keeper.check_owner();
        &mut self.inner
    }
}

impl Drop for StoreBox {
    fn drop(&mut self) {
        // Safety: dropped once, here.
        let inner = unsafe { ManuallyDrop::take(&mut self.inner) };
        self.keeper.dispose(inner);
    }
}

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
        });
        Self {
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

    /// Builds an [`Interrupter`] for this store. An interrupt requests a
    /// [`HostInterrupt`](crate::backend::sys::vm::TrapCode::HostInterrupt) trap
    /// for calls that have already entered the store. New entries are refused
    /// until all installed entries exit. A request on an idle store has no
    /// effect. A forgotten suspended call can prevent the flag from clearing.
    ///
    /// With Cranelift, code compiled with `experimental-host-interrupt` checks
    /// at Wasm function entry and loop headers. Other compilers do not emit
    /// these checks. An interrupt cannot preempt straight-line Wasm code,
    /// blocking `memory.atomic.wait`, or running host imports. A suspended
    /// async call can observe the interrupt on its next poll.
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
        SharedStore::new(self.inner.into_box())
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

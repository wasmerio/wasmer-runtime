use crate::{
    AsStoreMut, AsStoreRef, StoreRef,
    entities::{
        engine::{AsEngineRef, Engine},
        store::{StoreMut, StoreObjects},
    },
    macros::backend::{gen_rt_ty, match_rt},
};

/// We require the context to have a fixed memory address for its lifetime since
/// various bits of the VM have raw pointers that point back to it. Hence we
/// wrap the actual context in a box.
pub(crate) struct StoreInner {
    pub(crate) objects: StoreObjects,
    pub(crate) store: BackendStore,
    /// The shared handle of this store, if it has one: how a host function
    /// finds it. A back-reference here keeps `T` in a `FunctionEnv` `Send`.
    #[cfg(feature = "async")]
    pub(crate) shared: std::rc::Weak<super::SharedState>,
    /// Releases what forgotten calls left registered for this store.
    #[cfg(all(
        feature = "sys",
        feature = "async",
        feature = "experimental-host-interrupt"
    ))]
    pub(crate) _teardown: crate::backend::sys::async_runtime::StoreTeardown,
}

impl std::fmt::Debug for StoreInner {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("StoreInner")
            .field("objects", &self.objects)
            .field("store", &self.store)
            .finish()
    }
}

impl AsStoreRef for StoreInner {
    fn as_store_ref(&self) -> StoreRef<'_> {
        StoreRef::new(self)
    }
}
impl AsStoreMut for StoreInner {
    fn as_store_mut(&mut self) -> StoreMut<'_> {
        StoreMut::new(self)
    }

    fn objects_mut(&mut self) -> &mut StoreObjects {
        &mut self.objects
    }
}

gen_rt_ty! {
    #[derive(derive_more::From, Debug)]
    pub BackendStore(entities::store::Store);
}

gen_rt_ty! {
    /// What the backend of a store keeps beside the store's allocation: in
    /// its owner, and in every shared handle. It has a say in who may use the
    /// store, and in when its memory goes.
    #[derive(derive_more::From, Clone)]
    pub(crate) BackendStoreKeeper(entities::store::StoreKeeper);
}

/// What the `StoreKeeper` of a backend answers. By default it keeps nothing:
/// on most backends a call cannot outlive the borrow of the store it was made
/// with, and nothing unwinds through Rust frames without running them.
pub(crate) trait KeepsStore {
    /// The owner of the store, or a handle taken from it, is about to use
    /// it. This cannot reach the store: host code may be using it.
    #[inline]
    fn check_owner(&self) {}

    /// Whether a call holds the store while no slice of it runs.
    #[cfg(feature = "async")]
    #[inline]
    fn holds_store(&self) -> bool {
        false
    }

    /// Takes the allocation of a store whose last owner is gone.
    #[inline]
    fn dispose(&self, inner: Box<StoreInner>) {
        drop(inner);
    }

    /// Runs `f`, and then drops `guard`, whatever the host of the backend
    /// lets unwind through `f`.
    #[inline]
    fn holding<G, R>(&self, mut guard: G, f: impl FnOnce(&mut G) -> R) -> R {
        f(&mut guard)
    }
}

impl BackendStoreKeeper {
    /// See [`KeepsStore::check_owner`].
    #[inline]
    pub(crate) fn check_owner(&self) {
        match_rt!(on self => k {
            k.check_owner()
        })
    }

    /// See [`KeepsStore::holds_store`].
    #[cfg(feature = "async")]
    #[inline]
    pub(crate) fn holds_store(&self) -> bool {
        match_rt!(on self => k {
            k.holds_store()
        })
    }

    /// See [`KeepsStore::dispose`].
    #[inline]
    pub(crate) fn dispose(&self, inner: Box<StoreInner>) {
        match_rt!(on self => k {
            k.dispose(inner)
        })
    }

    /// See [`KeepsStore::holding`].
    #[inline]
    pub(crate) fn holding<G, R>(&self, guard: G, f: impl FnOnce(&mut G) -> R) -> R {
        match_rt!(on self => k {
            k.holding(guard, f)
        })
    }
}

impl BackendStore {
    /// What the backend keeps beside this store's allocation.
    #[inline]
    pub(crate) fn keeper(&self) -> BackendStoreKeeper {
        match_rt!(on self => s {
            s.keeper().clone().into()
        })
    }

    /// See [`KeepsStore::check_owner`]. Unlike the keeper beside the
    /// allocation, this is reached through the store.
    #[inline]
    pub(crate) fn check_owner(&self) {
        match_rt!(on self => s {
            s.keeper().check_owner()
        })
    }

    #[inline]
    pub(crate) fn engine(&self) -> &Engine {
        match_rt!(on self => s {
            s.engine()
        })
    }

    #[inline]
    pub(crate) fn engine_mut(&mut self) -> &mut Engine {
        match_rt!(on self => s {
            s.engine_mut()
        })
    }
}

impl AsEngineRef for BackendStore {
    #[inline]
    fn as_engine_ref(&self) -> crate::EngineRef<'_> {
        match_rt!(on self => s {
            s.as_engine_ref()
        })
    }
}

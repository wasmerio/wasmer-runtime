use std::ops::{Deref, DerefMut};

use super::{StoreObjects, inner::StoreInner};
use crate::entities::engine::{AsEngineRef, Engine, EngineRef};
use wasmer_types::ExternType;
//use wasmer_vm::{StoreObjects, TrapHandlerFn};

#[cfg(feature = "sys")]
use wasmer_vm::TrapHandlerFn;

/// How a handle reached its store.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct HandleOrigin {
    /// Lent for one synchronous slice of a call, rather than taken from the
    /// store's owner. A slice's handle cannot outlive that slice.
    from_slice: bool,
    /// From [`FunctionEnvMut::data_and_store`](crate::FunctionEnvMut::data_and_store),
    /// whose `&mut T` is live: it must not reach host state.
    view_only: bool,
}

/// A temporary handle to a [`crate::Store`].
#[derive(Debug)]
pub struct StoreRef<'a> {
    pub(crate) inner: &'a StoreInner,
    origin: HandleOrigin,
}

impl<'a> StoreRef<'a> {
    pub(crate) fn new(inner: &'a StoreInner) -> Self {
        Self {
            inner,
            origin: HandleOrigin::default(),
        }
    }

    /// A handle lent for one slice of a call.
    pub(crate) fn from_slice(inner: &'a StoreInner) -> Self {
        Self {
            inner,
            origin: HandleOrigin {
                from_slice: true,
                view_only: false,
            },
        }
    }

    /// This handle, without access to host state.
    pub(crate) fn view_only(mut self) -> Self {
        self.origin.view_only = true;
        self
    }

    pub(crate) fn objects(&self) -> &'a StoreObjects {
        &self.inner.objects
    }

    /// The objects, for reading a `FunctionEnv`.
    ///
    /// `AsStoreMut` is a public trait, so a wrapper can forward the view of
    /// [`FunctionEnvMut::data_and_store`](crate::FunctionEnvMut::data_and_store)
    /// through it. The origin travels with the handle, so that view still
    /// cannot reach the `T` whose `&mut` is live.
    ///
    /// # Panics
    /// Panics on a handle from `data_and_store`.
    pub(crate) fn host_state_objects(&self) -> &'a StoreObjects {
        assert!(
            !self.origin.view_only,
            "the store view of `FunctionEnvMut::data_and_store` cannot read host state"
        );
        &self.inner.objects
    }

    /// Returns the [`Engine`].
    pub fn engine(&self) -> &Engine {
        self.inner.store.engine()
    }

    /// Checks whether two stores are identical. A store is considered
    /// equal to another store if both have the same engine.
    pub fn same(a: &Self, b: &Self) -> bool {
        StoreObjects::same(&a.inner.objects, &b.inner.objects)
    }

    /// The signal handler
    #[cfg(feature = "sys")]
    #[inline]
    pub fn signal_handler(&self) -> Option<*const TrapHandlerFn<'static>> {
        use crate::backend::sys::entities::store::NativeStoreExt;
        self.inner.store.as_sys().signal_handler()
    }
}

/// A temporary handle to a [`crate::Store`].
pub struct StoreMut<'a> {
    pub(crate) inner: &'a mut StoreInner,
    origin: HandleOrigin,
}

impl<'a> StoreMut<'a> {
    pub(crate) fn new(inner: &'a mut StoreInner) -> Self {
        Self {
            inner,
            origin: HandleOrigin::default(),
        }
    }
}

impl StoreMut<'_> {
    /// Returns the [`Engine`].
    pub fn engine(&self) -> &Engine {
        self.inner.store.engine()
    }

    /// Checks whether two stores are identical. A store is considered
    /// equal to another store if both have the same engine.
    pub fn same(a: &Self, b: &Self) -> bool {
        StoreObjects::same(&a.inner.objects, &b.inner.objects)
    }

    #[allow(unused)]
    pub(crate) fn as_raw(&self) -> *mut StoreInner {
        self.inner as *const StoreInner as *mut StoreInner
    }

    /// A handle lent for one slice of a call.
    ///
    /// # Safety
    /// The caller holds the store exclusively for as long as this handle.
    #[allow(unused)]
    pub(crate) unsafe fn from_raw(raw: *mut StoreInner) -> Self {
        Self {
            inner: unsafe { &mut *raw },
            origin: HandleOrigin {
                from_slice: true,
                view_only: false,
            },
        }
    }

    #[allow(unused)]
    pub(crate) fn engine_and_objects_mut(&mut self) -> (&Engine, &mut StoreObjects) {
        (self.inner.store.engine(), &mut self.inner.objects)
    }

    /// Parks this borrow of the store for the duration of `f`, lending the
    /// store to code that runs inside `f` without reaching it through Rust:
    /// [`Store::with_current`](crate::Store::with_current) hands the store back to any frame `f` reaches,
    /// however many foreign frames deep.
    ///
    /// This is how host code lends the store across an FFI boundary. An
    /// embedded engine that calls back into the host — a JS engine's allocator
    /// hook, say — cannot be handed a Rust reference through its own C frames,
    /// and cannot be given one out of band either, because the imported
    /// function it was called from is still holding the only borrow. Parking
    /// that borrow, which the `&mut self` receiver here makes unreachable for
    /// exactly as long as `f` runs, is what makes lending it sound rather than
    /// aliasing.
    ///
    /// Parks nest, and a store nobody parked stays unlendable:
    /// [`Store::with_current`](crate::Store::with_current) returns `None`
    /// unless every borrow on this thread's stack has been parked this way.
    ///
    /// ```
    /// use wasmer::{AsStoreMut, FunctionEnvMut, Store};
    ///
    /// // A host function lending its store to code it calls into.
    /// fn host_call(mut env: FunctionEnvMut<'_, ()>) {
    ///     // `env` holds the store, so nobody else may have it.
    ///     assert!(Store::with_current(|_| ()).is_none());
    ///
    ///     env.as_store_mut().parked(|| {
    ///         // ...but code reached from here can pick it back up.
    ///         assert!(Store::with_current(|_| ()).is_some());
    ///     });
    /// }
    /// ```
    pub fn parked<R>(&mut self, f: impl FnOnce() -> R) -> R {
        // This is the same thing `Function::call` does before entering Wasm:
        // install this borrow as the store executing on the thread, so that
        // code which reaches the store through the context — rather than
        // through a reference it was handed — picks up a borrow derived from
        // *this* one. The new entry starts unborrowed, which is what makes the
        // store lendable for as long as it is on top.
        let ptr: *mut StoreInner = &mut *self.inner;
        // SAFETY: `ptr` comes from `&mut *self.inner`, which `&mut self` keeps
        // alive and unreachable for every frame on this thread until `f`
        // returns and the guard is dropped.
        let guard = unsafe { super::StoreContext::install(ptr) };
        holding(guard, |_| f())
    }
}

/// Runs `f` and then drops `guard`, also if a JavaScript exception unwinds
/// through `f`, which skips the destructors of the frames it crosses.
#[inline]
pub(crate) fn holding<G, R>(mut guard: G, f: impl FnOnce(&mut G) -> R) -> R {
    #[cfg(all(feature = "js", feature = "experimental-async", target_arch = "wasm32"))]
    {
        let result = crate::backend::js::boundary::catching(|| f(&mut guard));
        drop(guard);
        match result {
            Ok(value) => value,
            Err(error) => wasm_bindgen::throw_val(error),
        }
    }
    #[cfg(not(all(feature = "js", feature = "experimental-async", target_arch = "wasm32")))]
    f(&mut guard)
}

/// Helper trait for a value that is convertible to a [`StoreRef`].
pub trait AsStoreRef {
    /// Returns a `StoreRef` pointing to the underlying context.
    fn as_store_ref(&self) -> StoreRef<'_>;
}

/// Helper trait for a value that is convertible to a [`StoreMut`].
pub trait AsStoreMut: AsStoreRef {
    /// Returns a `StoreMut` pointing to the underlying context.
    fn as_store_mut(&mut self) -> StoreMut<'_>;

    /// Returns the ObjectMutable
    fn objects_mut(&mut self) -> &mut StoreObjects;
}

/// On JavaScript, a handle from the store's owner must not reach the store
/// while the guest of a forgotten call can still run. A handle lent to a slice
/// of that call can, and cannot outlive the slice.
#[inline]
fn check_guest_guard(_inner: &StoreInner, _origin: HandleOrigin) {
    #[cfg(all(feature = "js", feature = "experimental-async"))]
    if !_origin.from_slice {
        _inner.guard.check_owner();
    }
}

impl AsStoreRef for StoreRef<'_> {
    fn as_store_ref(&self) -> StoreRef<'_> {
        check_guest_guard(self.inner, self.origin);
        StoreRef {
            inner: self.inner,
            origin: self.origin,
        }
    }
}

impl AsEngineRef for StoreRef<'_> {
    fn as_engine_ref(&self) -> EngineRef<'_> {
        self.inner.store.as_engine_ref()
    }
}

impl AsStoreRef for StoreMut<'_> {
    fn as_store_ref(&self) -> StoreRef<'_> {
        check_guest_guard(self.inner, self.origin);
        StoreRef {
            inner: self.inner,
            origin: self.origin,
        }
    }
}
impl AsStoreMut for StoreMut<'_> {
    fn as_store_mut(&mut self) -> StoreMut<'_> {
        check_guest_guard(self.inner, self.origin);
        StoreMut {
            inner: self.inner,
            origin: self.origin,
        }
    }

    fn objects_mut(&mut self) -> &mut StoreObjects {
        check_guest_guard(self.inner, self.origin);
        &mut self.inner.objects
    }
}

impl<P> AsStoreRef for P
where
    P: Deref,
    P::Target: AsStoreRef,
{
    fn as_store_ref(&self) -> StoreRef<'_> {
        (**self).as_store_ref()
    }
}

impl<P> AsStoreMut for P
where
    P: DerefMut,
    P::Target: AsStoreMut,
{
    fn as_store_mut(&mut self) -> StoreMut<'_> {
        (**self).as_store_mut()
    }

    fn objects_mut(&mut self) -> &mut StoreObjects {
        (**self).objects_mut()
    }
}

impl AsEngineRef for StoreMut<'_> {
    fn as_engine_ref(&self) -> EngineRef<'_> {
        self.inner.store.as_engine_ref()
    }
}

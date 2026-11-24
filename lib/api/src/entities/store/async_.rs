//! How async calls reach their store.
//!
//! A host future reaches the store only in a synchronous slice, while the call
//! that owns it is being polled. That poll publishes the store here, and
//! [`AsyncEnv::with`](crate::AsyncEnv::with) takes it for one closure. Nothing
//! suspends while it holds the store, so a thread can only find its store busy
//! through re-entrancy, and waiting could never succeed: a busy store is an
//! immediate error, and there is no lock to wait on.

use std::{
    cell::Cell,
    marker::PhantomData,
    mem::ManuallyDrop,
    rc::{Rc, Weak},
};

use wasmer_types::StoreId;

use super::BackendStoreKeeper;
use crate::{Store, StoreContext, StoreInner, StoreMut};

/// How one poll reaches the store of its call.
#[derive(Clone, Copy)]
enum StoreAccess {
    /// The pointer a call derived from its exclusive borrow, or the one the
    /// enclosing synchronous entry holds.
    Exclusive(*mut StoreInner),
    /// A store whose slices are handed out one at a time.
    Shared(*const SharedState),
}

/// The store of the call being polled.
#[derive(Clone, Copy)]
pub(crate) struct PollStore {
    id: StoreId,
    access: StoreAccess,
}

/// The store published on this thread: for a backend whose host can unwind
/// through Rust frames without running them, to put it back.
#[allow(dead_code)]
pub(crate) fn published_store() -> Option<PollStore> {
    POLL_STORE.get()
}

/// Puts back what [`published_store`] returned.
#[allow(dead_code)]
pub(crate) fn publish_store(store: Option<PollStore>) {
    POLL_STORE.set(store);
}

/// Whether a store is published for a host future on this thread.
#[cfg(all(test, feature = "sys"))]
pub(crate) fn is_published() -> bool {
    POLL_STORE.get().is_some()
}

thread_local! {
    /// The store of the call being polled on this thread. Empty inside a
    /// slice, so that a slice cannot be taken twice.
    static POLL_STORE: Cell<Option<PollStore>> = const { Cell::new(None) };
}

impl PollStore {
    /// The store for a poll made by a synchronous entry, whose host frame
    /// holds `ptr` for as long as the poll runs.
    #[inline]
    pub(crate) fn exclusive(id: StoreId, ptr: *mut StoreInner) -> Self {
        Self {
            id,
            access: StoreAccess::Exclusive(ptr),
        }
    }

    /// Takes the store for one slice.
    ///
    /// # Panics
    /// Panics if a shared store is already in a slice.
    #[inline]
    pub(crate) fn enter_slice(self) -> StoreSlice {
        match self.access {
            StoreAccess::Exclusive(ptr) => StoreSlice { ptr, shared: None },
            StoreAccess::Shared(state) => {
                // Safety: whoever published this access holds a handle to the
                // state for at least as long as the poll.
                let state_ref = unsafe { &*state };
                assert!(
                    !state_ref.busy.replace(true),
                    "the shared store is already running a slice on this thread"
                );
                StoreSlice {
                    ptr: state_ref.ptr,
                    shared: Some(state),
                }
            }
        }
    }
}

/// What a backend needs of the store of a call when its engine, and not
/// `poll`, runs the call: who holds the store, and how to put its state back.
#[allow(dead_code)]
impl PollStore {
    /// Whether a guest with this access holds the store through a borrow,
    /// rather than slice by slice through a shared handle.
    pub(crate) fn is_exclusive(self) -> bool {
        matches!(self.access, StoreAccess::Exclusive(_))
    }

    /// What the backend keeps beside this store's allocation.
    pub(crate) fn keeper(self) -> BackendStoreKeeper {
        // Safety: the store outlives whoever published this access, and no
        // other reference to it is in use during the call's own poll.
        unsafe {
            match self.access {
                StoreAccess::Exclusive(ptr) => (*ptr).store.keeper(),
                StoreAccess::Shared(state) => (*state).keeper.clone(),
            }
        }
    }

    /// A handle that keeps a shared store alive, if this is one.
    pub(crate) fn shared(self) -> Option<SharedStore> {
        match self.access {
            StoreAccess::Exclusive(_) => None,
            // Safety: as for `keeper`.
            StoreAccess::Shared(state) => {
                unsafe { (*(*state).ptr).shared.upgrade() }.map(SharedStore)
            }
        }
    }

    /// Whether a shared store is in a slice. `None` for an exclusive one.
    pub(crate) fn busy(self) -> Option<bool> {
        match self.access {
            StoreAccess::Exclusive(_) => None,
            // Safety: as for `keeper`.
            StoreAccess::Shared(state) => Some(unsafe { &*state }.busy.get()),
        }
    }

    /// Puts back what [`Self::busy`] returned, after an unwind skipped the
    /// slice that would have.
    pub(crate) fn set_busy(self, busy: bool) {
        if let StoreAccess::Shared(state) = self.access {
            // Safety: as for `keeper`.
            unsafe { &*state }.busy.set(busy);
        }
    }

    /// [`Self::enter_slice`], or `None` if the store is in a slice.
    pub(crate) fn try_enter_slice(self) -> Option<StoreSlice> {
        if let StoreAccess::Shared(state) = self.access
            // Safety: as for `keeper`.
            && unsafe { &*state }.busy.get()
        {
            return None;
        }
        Some(self.enter_slice())
    }
}

/// One synchronous slice of a store. Ends when dropped.
pub(crate) struct StoreSlice {
    ptr: *mut StoreInner,
    shared: Option<*const SharedState>,
}

impl StoreSlice {
    pub(crate) fn ptr(&self) -> *mut StoreInner {
        self.ptr
    }
}

impl Drop for StoreSlice {
    #[inline]
    fn drop(&mut self) {
        if let Some(state) = self.shared {
            // Safety: see `PollStore::enter_slice`.
            unsafe { &*state }.busy.set(false);
        }
    }
}

/// Publishes the store of a call for the duration of a poll, or of the drop of
/// its host future.
pub(crate) struct PollScope {
    previous: Option<PollStore>,
}

impl PollScope {
    #[inline]
    pub(crate) fn enter(store: Option<PollStore>) -> Self {
        Self {
            previous: POLL_STORE.replace(store),
        }
    }
}

impl Drop for PollScope {
    #[inline]
    fn drop(&mut self) {
        POLL_STORE.set(self.previous);
    }
}

/// The slice of an [`AsyncEnv::with`](crate::AsyncEnv::with). While it lasts
/// the published store is empty, so nothing else can take it.
pub(crate) struct WithSlice {
    slice: ManuallyDrop<StoreSlice>,
    poll: PollStore,
}

impl WithSlice {
    /// # Panics
    /// Panics unless the call being polled is on the store `id`.
    #[inline]
    pub(crate) fn enter(id: StoreId) -> Self {
        let Some(poll) = POLL_STORE.take() else {
            panic!(
                "AsyncEnv::with can only be used while its host function is being polled, \
                 and not inside another `with`"
            );
        };
        if poll.id != id {
            POLL_STORE.set(Some(poll));
            panic!("AsyncEnv::with was used during a poll of another store's call");
        }
        // Restores the published store if the slice cannot be taken.
        struct Restore(PollStore);
        impl Drop for Restore {
            fn drop(&mut self) {
                POLL_STORE.set(Some(self.0));
            }
        }
        let restore = Restore(poll);
        let slice = poll.enter_slice();
        std::mem::forget(restore);
        Self {
            slice: ManuallyDrop::new(slice),
            poll,
        }
    }

    #[inline]
    pub(crate) fn ptr(&self) -> *mut StoreInner {
        self.slice.ptr()
    }
}

impl Drop for WithSlice {
    #[inline]
    fn drop(&mut self) {
        // Safety: dropped once, here.
        unsafe { ManuallyDrop::drop(&mut self.slice) };
        POLL_STORE.set(Some(self.poll));
    }
}

/// The store an async call runs on.
pub(crate) enum CallStore<'a> {
    /// The call holds `&'a mut` of the store. The pointer is derived once,
    /// when the call is made.
    Exclusive {
        id: StoreId,
        ptr: *mut StoreInner,
        _borrow: PhantomData<&'a mut StoreInner>,
    },
    /// The call runs inside an async host function, whose call publishes the
    /// store for each poll. `'a` holds the `AsyncEnv`.
    Env {
        id: StoreId,
        _borrow: PhantomData<&'a mut ()>,
    },
    /// The call keeps its own handle, and takes the store slice by slice.
    Shared(SharedStore),
}

impl CallStore<'_> {
    pub(crate) fn id(&self) -> StoreId {
        match self {
            Self::Exclusive { id, .. } | Self::Env { id, .. } => *id,
            Self::Shared(store) => store.0.id,
        }
    }

    /// The store for this poll.
    ///
    /// # Panics
    /// Panics if the call is polled from inside a slice of its own store, or
    /// a nested call outside the poll of the host function that made it.
    pub(crate) fn resolve(&self) -> PollStore {
        match self {
            Self::Shared(store) => {
                assert!(
                    !store.0.busy.get(),
                    "a task of a shared store was polled from inside a slice of that \
                     store; an executor whose waker polls inline must defer that poll"
                );
                store.poll_store()
            }
            _ => self.try_resolve().unwrap_or_else(|| {
                panic!(
                    "a nested async call can only be polled while the host function \
                     that made it is being polled"
                )
            }),
        }
    }

    /// The store for dropping a host future, if it can still be reached.
    pub(crate) fn try_resolve(&self) -> Option<PollStore> {
        match self {
            Self::Exclusive { id, ptr, .. } => Some(PollStore::exclusive(*id, *ptr)),
            Self::Env { id, .. } => POLL_STORE.get().filter(|poll| poll.id == *id),
            Self::Shared(store) => Some(store.poll_store()),
        }
    }
}

/// The state behind every handle of a [`SharedStore`].
pub(crate) struct SharedState {
    id: StoreId,
    /// From `Box::into_raw`, so that every slice derives from one pointer.
    ptr: *mut StoreInner,
    /// A slice is running.
    busy: Cell<bool>,
    /// What the backend keeps beside the allocation: kept here, and not
    /// reached through the store, because host code may be using the store.
    keeper: BackendStoreKeeper,
}

impl Drop for SharedState {
    fn drop(&mut self) {
        // Safety: the last handle is gone, and every slice holds one.
        let inner = unsafe { Box::from_raw(self.ptr) };
        self.keeper.dispose(inner);
    }
}

/// A store that several guest stacks can use, one synchronous slice at a time.
///
/// Create one with [`Store::into_shared`]. `SharedStore` is `Clone` and
/// `!Send`. A call made with it keeps its own handle, so its future borrows
/// nothing and can be spawned on a local task set.
#[derive(Clone)]
pub struct SharedStore(pub(crate) Rc<SharedState>);

/// A handle to a [`SharedStore`] that does not keep it alive.
#[derive(Clone)]
pub struct WeakSharedStore(Weak<SharedState>);

/// The store is running a slice on this thread. There is nothing to wait for:
/// the holder is below the caller on the same stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoreBusy;

impl std::fmt::Display for StoreBusy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the store is busy on this thread")
    }
}

impl std::error::Error for StoreBusy {}

impl SharedStore {
    pub(crate) fn new(inner: Box<StoreInner>) -> Self {
        let id = inner.objects.id();
        let keeper = inner.store.keeper();
        let ptr = Box::into_raw(inner);
        Self(Rc::new_cyclic(|weak| {
            // Safety: `ptr` was just made and nothing else holds it.
            unsafe { (*ptr).shared = weak.clone() };
            SharedState {
                id,
                ptr,
                busy: Cell::new(false),
                keeper,
            }
        }))
    }

    fn poll_store(&self) -> PollStore {
        PollStore {
            id: self.0.id,
            access: StoreAccess::Shared(Rc::as_ptr(&self.0)),
        }
    }

    /// Runs `f` with the store.
    ///
    /// Fails at once if the store is busy, because there is nothing to wait
    /// for. Inside [`FunctionEnvMut::release`](crate::FunctionEnvMut::release)
    /// the store is lent to this call.
    pub fn try_with<R>(&self, f: impl FnOnce(StoreMut<'_>) -> R) -> Result<R, StoreBusy> {
        if self.is_held() {
            let lent = StoreContext::try_get_lent(self.0.id).ok_or(StoreBusy)?;
            return Ok(self.0.keeper.holding(lent, |lent| f(lent.as_mut())));
        }
        let slice = self.poll_store().enter_slice();
        // Safety: the slice is the only holder of the store until it drops,
        // after `f` and every borrow `f` was given are gone.
        Ok(self
            .0
            .keeper
            .holding(slice, |slice| f(unsafe { StoreMut::from_raw(slice.ptr()) })))
    }

    /// Whether a slice holds the store, or a call of its backend does while
    /// none runs.
    fn is_held(&self) -> bool {
        self.0.busy.get() || self.0.keeper.holds_store()
    }

    /// Takes the [`Store`] back, if this is its last handle and no slice is
    /// running. A task that has not finished holds a handle.
    pub fn into_store(self) -> Result<Store, Self> {
        if self.is_held() {
            return Err(self);
        }
        match Rc::try_unwrap(self.0) {
            Ok(state) => {
                let state = ManuallyDrop::new(state);
                // Safety: this was the last handle, and `state` is not
                // dropped, so the box has one owner again.
                let mut inner = unsafe { Box::from_raw(state.ptr) };
                inner.shared = Weak::new();
                Ok(Store {
                    inner: crate::StoreBox::new(inner),
                })
            }
            Err(state) => Err(Self(state)),
        }
    }

    /// Makes a handle that does not keep the store alive.
    pub fn downgrade(&self) -> WeakSharedStore {
        WeakSharedStore(Rc::downgrade(&self.0))
    }
}

impl std::fmt::Debug for SharedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SharedStore")
            .field("id", &self.0.id)
            .finish()
    }
}

impl WeakSharedStore {
    /// The store, if a handle to it is still alive.
    pub fn upgrade(&self) -> Option<SharedStore> {
        self.0.upgrade().map(SharedStore)
    }
}

impl std::fmt::Debug for WeakSharedStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WeakSharedStore")
    }
}

/// Replays the slices of an async call at the level of the store, which is
/// the part that has nothing to do with switching stacks. Natively these
/// always pass; they earn their keep under Miri:
///
/// ```text
/// cargo +nightly miri test -p wasmer --features sys,headless,experimental-async --lib store::async_
/// ```
#[cfg(all(test, feature = "sys"))]
mod tests {
    use super::*;
    use crate::{AsStoreMut, FunctionEnv};

    fn sys_store() -> Store {
        Store::new(crate::backend::sys::entities::engine::default_engine())
    }

    /// A host future's slices derive from the call's pointer, and the owner
    /// of the borrow goes on using it once the call is over.
    #[test]
    fn with_slices_derive_from_the_published_pointer() {
        let mut store = sys_store();
        let env = FunctionEnv::new(&mut store, 1i32);
        let id = store.id();
        {
            let borrow = store.as_store_mut();
            let ptr: *mut StoreInner = borrow.inner;
            let poll = PollStore::exclusive(id, ptr);
            let _scope = PollScope::enter(Some(poll));
            for _ in 0..2 {
                let slice = WithSlice::enter(id);
                let mut slice_store = unsafe { StoreMut::from_raw(slice.ptr()) };
                *env.as_mut(&mut slice_store) += 1;
            }
            // The guest's slice, between the host future's.
            let slice = poll.enter_slice();
            let mut guest = unsafe { StoreMut::from_raw(slice.ptr()) };
            *env.as_mut(&mut guest) += 1;
        }
        assert_eq!(*env.as_ref(&store), 4);
    }

    /// `release` lends the store to one `try_with` at a time, and the lender
    /// goes on with the borrow it held throughout.
    #[test]
    fn a_released_slice_nests_in_its_lender() {
        let mut store = sys_store();
        let env = FunctionEnv::new(&mut store, 1i32);
        let shared = store.into_shared();
        shared
            .try_with(|mut outer| {
                *env.as_mut(&mut outer) += 1;
                assert_eq!(shared.try_with(|_| ()).err(), Some(StoreBusy));
                outer.parked(|| {
                    shared
                        .try_with(|mut lent| {
                            *env.as_mut(&mut lent) += 1;
                            assert_eq!(shared.try_with(|_| ()).err(), Some(StoreBusy));
                        })
                        .unwrap();
                });
                *env.as_mut(&mut outer) += 1;
            })
            .unwrap();
        let value = shared.try_with(|store| *env.as_ref(&store)).unwrap();
        assert_eq!(value, 4);
        let store = shared.into_store().expect("the last handle");
        assert_eq!(*env.as_ref(&store), 4);
    }

    /// A slice is given back when its closure panics.
    #[test]
    fn a_panicking_slice_frees_the_store() {
        let shared = sys_store().into_shared();
        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _ = shared.try_with(|_| panic!("in a slice"));
        }));
        assert!(caught.is_err());
        assert!(shared.try_with(|_| ()).is_ok());
    }
}

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
    ptr,
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

/// Where this thread's store is published, for a guard that takes it and
/// puts it back to look it up once.
///
/// The pointer is valid for as long as the thread runs: the thread-local has
/// no destructor. A guard that keeps it holds a pointer to its store as well,
/// so it is not `Send`, and it is dropped before the poll it was made in
/// returns: it never leaves the thread.
///
/// This is `LocalKey::with`, written with `try_with`, which is `#[inline]`
/// where `with` is not (see `with_top` in `context.rs`).
#[inline]
fn published() -> *const Cell<Option<PollStore>> {
    POLL_STORE
        .try_with(ptr::from_ref)
        .expect("the published store of a thread was used during or after its destruction")
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
        match self.try_enter_slice() {
            Some(slice) => slice,
            None => slice_is_running(),
        }
    }

    /// [`Self::enter_slice`], or `None` if a shared store is already in a
    /// slice. That slice goes on holding the store.
    #[inline]
    pub(crate) fn try_enter_slice(self) -> Option<StoreSlice> {
        match self.access {
            StoreAccess::Exclusive(ptr) => Some(StoreSlice { ptr, shared: None }),
            StoreAccess::Shared(state) => {
                // Safety: whoever published this access holds a handle to the
                // state for at least as long as the poll.
                let state_ref = unsafe { &*state };
                if state_ref.busy.replace(true) {
                    return None;
                }
                Some(StoreSlice {
                    ptr: state_ref.ptr,
                    shared: Some(state),
                })
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
    /// See [`published`].
    published: *const Cell<Option<PollStore>>,
}

impl PollScope {
    #[inline]
    pub(crate) fn enter(store: Option<PollStore>) -> Self {
        let published = published();
        Self {
            // Safety: see `published`.
            previous: unsafe { &*published }.replace(store),
            published,
        }
    }
}

impl Drop for PollScope {
    #[inline]
    fn drop(&mut self) {
        // Safety: see `published`.
        unsafe { &*self.published }.set(self.previous);
    }
}

/// The slice of an [`AsyncEnv::with`](crate::AsyncEnv::with). While it lasts
/// the published store is empty, so nothing else can take it.
pub(crate) struct WithSlice {
    slice: ManuallyDrop<StoreSlice>,
    poll: PollStore,
    /// See [`published`].
    published: *const Cell<Option<PollStore>>,
}

/// The panics of a slice. Out of line, so that the slice of an
/// `AsyncEnv::with`, which is all that most host futures do with the store,
/// is small enough to be inlined into it.
#[cold]
#[inline(never)]
fn not_published() -> ! {
    panic!(
        "AsyncEnv::with can only be used while its host function is being polled, \
         and not inside another `with`"
    )
}

#[cold]
#[inline(never)]
fn published_for_another_store() -> ! {
    panic!("AsyncEnv::with was used during a poll of another store's call")
}

#[cold]
#[inline(never)]
fn slice_is_running() -> ! {
    panic!("the shared store is already running a slice on this thread")
}

/// The panic of a task of a shared store that is polled where a slice holds
/// its store, and the store is not published for that poll.
#[cold]
#[inline(never)]
fn polled_in_a_slice() -> ! {
    panic!(
        "a task of a shared store was polled from inside a slice of that store; an \
         executor whose waker polls inline must defer that poll"
    )
}

impl WithSlice {
    /// # Panics
    /// Panics unless the call being polled is on the store `id`, and if that
    /// store is a shared one that is already in a slice.
    #[inline]
    pub(crate) fn enter(id: StoreId) -> Self {
        match Self::try_enter(id) {
            Some(slice) => slice,
            None => slice_is_running(),
        }
    }

    /// [`Self::enter`], or `None` if the store is a shared one that is
    /// already in a slice: the store stays published, and that slice goes on.
    ///
    /// # Panics
    /// Panics unless the call being polled is on the store `id`.
    #[inline]
    pub(crate) fn try_enter(id: StoreId) -> Option<Self> {
        let published = published();
        // Safety: see `published`.
        let cell = unsafe { &*published };
        let Some(poll) = cell.take() else {
            not_published()
        };
        // The published store is put back before a panic: it is not this
        // slice's to keep.
        if poll.id != id {
            cell.set(Some(poll));
            published_for_another_store()
        }
        let Some(slice) = poll.try_enter_slice() else {
            cell.set(Some(poll));
            return None;
        };
        Some(Self {
            slice: ManuallyDrop::new(slice),
            poll,
            published,
        })
    }

    /// [`Self::try_enter`], or `None` where that panics as well: unless the
    /// call being polled is on the store `id`. What is published stays as it
    /// was.
    #[inline]
    pub(crate) fn enter_if_in_reach(id: StoreId) -> Option<Self> {
        let published = published();
        // Safety: see `published`.
        let cell = unsafe { &*published };
        let poll = cell.take()?;
        let slice = if poll.id == id {
            poll.try_enter_slice()
        } else {
            None
        };
        let Some(slice) = slice else {
            cell.set(Some(poll));
            return None;
        };
        Some(Self {
            slice: ManuallyDrop::new(slice),
            poll,
            published,
        })
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
        // Safety: see `published`.
        unsafe { &*self.published }.set(Some(self.poll));
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
    #[cfg_attr(
        not(any(
            feature = "experimental-host-interrupt",
            feature = "js",
            feature = "v8"
        )),
        allow(dead_code)
    )]
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
    /// while a call of its backend holds the store with no slice (see
    /// [`SharedStore::try_with`]), or a nested call outside the poll of the
    /// host function that made it.
    pub(crate) fn resolve(&self) -> PollStore {
        match self {
            Self::Shared(store) => {
                if store.is_held() {
                    polled_in_a_slice()
                }
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

    /// [`Self::resolve`], for a backend that asks at every poll and keeps the
    /// answer no longer than that poll.
    ///
    /// A task of a shared store that is polled where its store is lent (see
    /// [`lent_under_a_guest`]) runs with the store that is published there,
    /// as a call made with an `AsyncEnv` does. The task is polled by the host
    /// function the store is published for, so the two do not overlap.
    ///
    /// # Panics
    /// As [`Self::resolve`], unless the store is lent to the poll this one is
    /// made in.
    #[cfg(feature = "sys")]
    pub(crate) fn resolve_in_reach(&self) -> PollStore {
        match self {
            Self::Shared(store) if store.0.busy.get() => {
                lent_under_a_guest(store.0.id).unwrap_or_else(|| polled_in_a_slice())
            }
            _ => self.resolve(),
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

/// The store `id` as it is published for an async host function that is being
/// polled under its own guest, if that guest is the one of a `call_async`.
///
/// On `sys` an async host function is polled for the first time where its
/// guest called it, while the guest still has its slice of the store, and the
/// store is published for the host function alone. The store is busy there,
/// though the host function would find it free in every later poll, which its
/// call makes with no slice held. So a handle of a shared store is let in
/// where the `AsyncEnv` of the host function is: through what is published,
/// which has one holder at a time.
///
/// A guest that was entered synchronously lends nothing: the host frame that
/// entered it holds the store, as it does for a synchronous host function.
#[cfg(feature = "sys")]
fn lent_under_a_guest(id: StoreId) -> Option<PollStore> {
    POLL_STORE
        .get()
        .filter(|poll| poll.id == id && poll.is_exclusive())
        .filter(|_| crate::backend::sys::async_runtime::guest_can_wait_here())
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
///
/// # Owning-thread execution
///
/// An embedder whose bindings must stay on one thread runs its guest there
/// with a `SharedStore`, and needs nothing more. On the native backend:
///
/// - **The thread that makes the store owns it.** [`Store`], `SharedStore`,
///   [`WeakSharedStore`] and the future of a call are `!Send`, so no other
///   thread can enter the store. An [`Engine`](crate::Engine) and a
///   [`Module`](crate::Module) are `Send`, so a guest can be compiled on any
///   thread, and is instantiated on the owning one. Host state is `Send`, so
///   what is bound to the thread, such as its tasks, goes in a thread-local.
/// - **Other threads ask the owning thread.** They send it requests over a
///   channel of the embedder's, and it enters the store for them: with
///   [`try_with`](Self::try_with) for a synchronous call, or by polling the
///   future of a [`call_async`](crate::Function::call_async), which needs no
///   multi-threaded runtime. A waker may fire on any thread, as long as all
///   it does is tell the owning thread to poll. Another thread ends a guest
///   that does not return with the store's interrupter
///   (`Store::interrupter`, with `experimental-host-interrupt` on Unix).
/// - **A host function re-enters the guest** with the store it was given.
///   Code it reaches through frames that carry no reference, such as a
///   callback of an embedded engine, uses a handle inside
///   [`FunctionEnvMut::release`](crate::FunctionEnvMut::release). A guest
///   entered from a synchronous host function cannot wait. An async one
///   passes its [`AsyncEnv`](crate::AsyncEnv) to `call_async`, and holds no
///   store between its slices: code it reaches through such frames asks for
///   the store with
///   [`AsyncEnv::with_if_in_reach`](crate::AsyncEnv::with_if_in_reach).
/// - **A guest can end while calls are still on the native stack.** Until
///   they return, the frames below hold the store: `try_with` fails with
///   [`StoreBusy`], and [`into_store`](Self::into_store) gives the handle
///   back. The error that ended the guest unwinds the calls innermost first,
///   and each host function goes on with its own borrow. A task that waits
///   has no frame of a host function on its stack, so it can be dropped at
///   any point, also from inside one of those calls: its host future is
///   dropped first, and its guest is then discarded without running. A
///   destructor that needs
///   the store uses a handle and `try_with`, because
///   [`AsyncEnv::with`](crate::AsyncEnv::with) panics in another slice of the
///   store. On `StoreBusy` it leaves its work for the owning thread to run
///   once that thread's own call has returned. The store is free from then
///   on: `into_store` succeeds as soon as every task and every other handle
///   is dropped, and a [`WeakSharedStore`] no longer upgrades once the store
///   is dropped.
///
/// Host code always runs on the owning thread, and on the stack of whoever
/// entered the guest: the caller of `call`, or whoever polls the future of a
/// `call_async`. The exception is a typed host function set to run on the
/// Wasm stack with
/// [`Function::set_calls_on_native_stack`](crate::Function::set_calls_on_native_stack),
/// which bindings that need the thread's own stack must not be.
///
/// A task that still waits when the owning thread exits is dropped like any
/// other: its host future is dropped, and its guest's stack is reset.
///
/// The thread that owns the store, serving a request of another thread:
///
/// ```
/// use std::{sync::mpsc, thread};
/// use wasmer::{Instance, Module, Store, Value, imports};
///
/// # fn main() -> anyhow::Result<()> {
/// // What another thread asks for, and where the answer goes.
/// type Request = (i32, mpsc::Sender<i32>);
/// let (requests, inbox) = mpsc::channel::<Request>();
///
/// let owner = thread::spawn(move || -> anyhow::Result<()> {
///     // The store is made here, so this thread owns it.
///     let mut store = Store::default();
///     let module = Module::new(
///         &store,
///         r#"(module (func (export "add_one") (param i32) (result i32)
///             (i32.add (local.get 0) (i32.const 1))))"#,
///     )?;
///     let instance = Instance::new(&mut store, &module, &imports! {})?;
///     let add_one = instance.exports.get_function("add_one")?.clone();
///     let store = store.into_shared();
///
///     for (x, reply) in inbox {
///         // No call of this thread is on the stack, so the store is free.
///         let results = store
///             .try_with(|mut store| add_one.call(&mut store, &[Value::I32(x)]))??;
///         reply.send(results[0].unwrap_i32())?;
///     }
///     // No task and no frame is left, so this is the last handle.
///     drop(store.into_store().expect("the last handle"));
///     Ok(())
/// });
///
/// let (reply, replied) = mpsc::channel();
/// requests.send((41, reply))?;
/// assert_eq!(replied.recv()?, 42);
/// drop(requests);
/// owner.join().unwrap()
/// # }
/// ```
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
    /// the store is lent to this call. On `sys` it is lent as well where an
    /// async host function is polled under its own guest, if a
    /// [`call_async`](crate::Function::call_async) runs that guest: this
    /// takes the store there as [`AsyncEnv::with`](crate::AsyncEnv::with)
    /// does, one holder at a time.
    pub fn try_with<R>(&self, f: impl FnOnce(StoreMut<'_>) -> R) -> Result<R, StoreBusy> {
        #[cfg(feature = "sys")]
        if self.0.busy.get()
            && lent_under_a_guest(self.0.id).is_some()
            && let Some(slice) = WithSlice::enter_if_in_reach(self.0.id)
        {
            // Safety: the slice is the only holder of the store until it
            // drops: it took what was published, as the slice of an
            // `AsyncEnv` does.
            return Ok(self
                .0
                .keeper
                .holding(slice, |slice| f(unsafe { StoreMut::from_raw(slice.ptr()) })));
        }
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

    /// Whether both handles are of the same store.
    ///
    /// It does not borrow the store, so it also answers inside a slice.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.0, &other.0)
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

    /// Whether both handles are of the same store, alive or not.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.0, &other.0)
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
/// cargo +nightly miri test -p wasmer --features sys,headless,async --lib store::async_
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

    /// A slice that is refused, as `AsyncEnv::try_with` is in a destructor run
    /// inside another slice, leaves the store to the slice that runs and
    /// published for the next, and the holder goes on with its borrow.
    #[test]
    fn a_refused_slice_leaves_the_running_one_in_place() {
        let mut store = sys_store();
        let env = FunctionEnv::new(&mut store, 1i32);
        let shared = store.into_shared();
        let id = shared.0.id;
        let _scope = PollScope::enter(Some(shared.poll_store()));
        shared
            .try_with(|mut outer| {
                *env.as_mut(&mut outer) += 1;
                assert!(WithSlice::try_enter(id).is_none());
                assert!(is_published());
                assert_eq!(shared.try_with(|_| ()).err(), Some(StoreBusy));
                *env.as_mut(&mut outer) += 1;
            })
            .unwrap();

        let slice = WithSlice::try_enter(id).expect("the store is free");
        assert!(!is_published(), "a slice hides the store it holds");
        let mut inner = unsafe { StoreMut::from_raw(slice.ptr()) };
        *env.as_mut(&mut inner) += 1;
        drop(slice);
        assert!(is_published());
        assert_eq!(shared.try_with(|store| *env.as_ref(&store)), Ok(4));
    }

    /// A slice asked for where the store may be out of reach, as
    /// `AsyncEnv::with_if_in_reach` asks: there is none unless the store is
    /// published and free, and asking changes nothing.
    #[test]
    fn a_slice_is_only_in_reach_of_a_poll_of_its_store() {
        let mut store = sys_store();
        let env = FunctionEnv::new(&mut store, 1i32);
        let shared = store.into_shared();
        let id = shared.0.id;
        let other = sys_store().into_shared();

        // No call is being polled.
        assert!(WithSlice::enter_if_in_reach(id).is_none());
        assert!(!is_published());

        // A call of another store is.
        {
            let _scope = PollScope::enter(Some(other.poll_store()));
            assert!(WithSlice::enter_if_in_reach(id).is_none());
            assert!(WithSlice::enter_if_in_reach(other.0.id).is_some());
            assert!(is_published());
        }

        let _scope = PollScope::enter(Some(shared.poll_store()));
        // The store is in a slice already.
        shared
            .try_with(|mut outer| {
                assert!(WithSlice::enter_if_in_reach(id).is_none());
                assert!(is_published());
                *env.as_mut(&mut outer) += 1;
            })
            .unwrap();

        let slice = WithSlice::enter_if_in_reach(id).expect("the store is in reach");
        // This slice hides the store, and a running guest does as well.
        assert!(WithSlice::enter_if_in_reach(id).is_none());
        let mut inner = unsafe { StoreMut::from_raw(slice.ptr()) };
        *env.as_mut(&mut inner) += 1;
        drop(slice);
        assert!(is_published());
        assert_eq!(shared.try_with(|store| *env.as_ref(&store)), Ok(3));
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

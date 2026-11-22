//! The JavaScript guest guard.
//!
//! The engine resumes a guest from a microtask, with no `poll` on the stack,
//! so the borrow of a call cannot protect what the guest's host code does
//! with the store. Dropping a call ends that: it gives the store up (see
//! `jspi`). A forgotten call ends its borrow while its host code can still
//! run with the pointer it derived from it. The guard keeps that safe with
//! two rules:
//!
//! 1. A call that is neither finished nor dropped co-owns the store's
//!    allocation, so dropping the [`Store`](crate::Store) cannot free memory
//!    under it.
//! 2. It also marks the store as held. If its borrow ended anyway, because
//!    the call was forgotten, every public store entry panics until its guest
//!    is done.

use std::{
    cell::{Cell, RefCell},
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    rc::Rc,
};

use crate::StoreInner;

/// One per store. The store and every call made with a borrow of it share it.
#[derive(Default)]
pub(crate) struct GuestGuard {
    /// Calls made with a borrow of the store, neither finished nor dropped.
    calls: Cell<u32>,
    /// Host code of a guest the engine resumed, or a host future, is running
    /// with the store, in no call that lends it (see `jspi::hold_running`).
    held: Cell<bool>,
    /// The store, if its owner dropped it while such a call was not over.
    orphan: RefCell<Option<Box<StoreInner>>>,
}

impl GuestGuard {
    /// Panics if a call that is neither finished nor dropped holds the store.
    /// Only the store's owner, and handles taken from it, come through here:
    /// while that call is alive they can be reached only if it was forgotten.
    #[inline]
    pub(crate) fn check_owner(&self) {
        if self.calls.get() != 0 {
            store_is_held();
        }
    }

    pub(crate) fn begin_call(&self) {
        self.calls.set(self.calls.get() + 1);
    }

    /// The call is over. Frees a store its owner dropped meanwhile, once no
    /// call can reach it.
    pub(crate) fn end_call(&self) {
        let calls = self.calls.get() - 1;
        self.calls.set(calls);
        if calls == 0 {
            let orphan = self.orphan.borrow_mut().take();
            drop(orphan);
        }
    }

    /// Whether an unfinished call made with a borrow of the store holds it.
    pub(crate) fn holds_store(&self) -> bool {
        self.calls.get() != 0
    }

    pub(crate) fn is_held(&self) -> bool {
        self.held.get()
    }

    pub(crate) fn set_held(&self, held: bool) {
        self.held.set(held);
    }

    /// Takes ownership of a store whose last owner is gone: it is freed now,
    /// or when the last call that can reach it is over.
    pub(crate) fn dispose(inner: Box<StoreInner>) {
        let guard = Rc::clone(&inner.guard);
        if guard.calls.get() > 0 {
            *guard.orphan.borrow_mut() = Some(inner);
        }
    }
}

#[cold]
#[inline(never)]
fn store_is_held() -> ! {
    panic!(
        "the store was used while the guest of a forgotten `call_async` can still run; \
         it is usable again once that guest is done"
    )
}

/// The owner of a store's allocation on this backend.
pub(crate) struct GuardedStore {
    inner: ManuallyDrop<Box<StoreInner>>,
    guard: Rc<GuestGuard>,
}

impl GuardedStore {
    pub(crate) fn new(inner: Box<StoreInner>) -> Self {
        let guard = Rc::clone(&inner.guard);
        Self {
            inner: ManuallyDrop::new(inner),
            guard,
        }
    }

    /// Gives the allocation up to another owner.
    pub(crate) fn into_box(self) -> Box<StoreInner> {
        self.guard.check_owner();
        let mut this = ManuallyDrop::new(self);
        // Safety: `this` is never dropped, so each field is taken once.
        unsafe {
            std::ptr::drop_in_place(&mut this.guard);
            ManuallyDrop::take(&mut this.inner)
        }
    }
}

impl Deref for GuardedStore {
    type Target = StoreInner;

    #[inline]
    fn deref(&self) -> &StoreInner {
        self.guard.check_owner();
        &self.inner
    }
}

impl DerefMut for GuardedStore {
    #[inline]
    fn deref_mut(&mut self) -> &mut StoreInner {
        self.guard.check_owner();
        &mut self.inner
    }
}

impl Drop for GuardedStore {
    fn drop(&mut self) {
        // Safety: dropped once, here.
        let inner = unsafe { ManuallyDrop::take(&mut self.inner) };
        GuestGuard::dispose(inner);
    }
}

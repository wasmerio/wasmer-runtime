//! The JavaScript guest guard.
//!
//! The engine resumes a guest from a microtask, with no `poll` on the stack,
//! so the borrow of a call cannot protect that slice. A forgotten call ends
//! its borrow while its guest can still run and reach the store. The guard
//! keeps this safe with three rules:
//!
//! 1. A started guest co-owns the store's allocation until it leaves the
//!    engine's stack, so dropping the [`Store`](crate::Store) cannot free
//!    memory under it.
//! 2. A call that is neither finished nor dropped marks the store as held. If
//!    its borrow ended anyway because the call was forgotten, every public
//!    store entry panics until the guest leaves.
//! 3. Dropping a call marks its guest cancelled. If the engine can no longer be
//!    stopped from resuming it, its host functions fail without touching the
//!    store, and the store is usable at once. A new guest call waits for that
//!    guest to leave the stack, or fails if it is synchronous.

use std::{
    cell::{Cell, RefCell},
    mem::ManuallyDrop,
    ops::{Deref, DerefMut},
    rc::Rc,
    task::Waker,
};

use crate::{RuntimeError, StoreInner};

/// One per store. The store and every guest started on it share it.
#[derive(Default)]
pub(crate) struct GuestGuard {
    /// Guests started and not yet off the engine's stack.
    guests: Cell<u32>,
    /// Exclusive calls neither finished nor dropped.
    calls: Cell<u32>,
    /// Slices of those calls running now: their code may use the store.
    slices: Cell<u32>,
    /// A guest is on the stack, or is about to be resumed there.
    holder: Cell<bool>,
    /// That guest's call was dropped.
    holder_cancelled: Cell<bool>,
    /// Calls waiting for that guest to leave.
    waiters: RefCell<Vec<Waker>>,
    /// The store, if its owner dropped it while a guest was still running.
    orphan: RefCell<Option<Box<StoreInner>>>,
}

impl GuestGuard {
    /// Panics if a forgotten call's guest can still reach the store.
    #[inline]
    pub(crate) fn check(&self) {
        if self.calls.get() != 0 && self.slices.get() == 0 {
            store_is_held();
        }
    }

    pub(crate) fn enter_guest(&self) {
        self.guests.set(self.guests.get() + 1);
    }

    /// The guest left the engine's stack. Frees a store its owner dropped
    /// meanwhile, once no guest can reach it.
    pub(crate) fn leave_guest(&self) {
        let guests = self.guests.get() - 1;
        self.guests.set(guests);
        if guests == 0 {
            let orphan = self.orphan.borrow_mut().take();
            drop(orphan);
        }
    }

    pub(crate) fn begin_call(&self) {
        self.calls.set(self.calls.get() + 1);
    }

    pub(crate) fn end_call(&self) {
        self.calls.set(self.calls.get() - 1);
    }

    /// Lets the code of a live call use the store until the mark is dropped.
    pub(crate) fn mark(self: &Rc<Self>) -> SliceMark {
        self.slices.set(self.slices.get() + 1);
        SliceMark(Rc::clone(self))
    }

    /// Whether no guest is on the stack.
    pub(crate) fn is_free(&self) -> bool {
        !self.holder.get()
    }

    pub(crate) fn acquire(&self) {
        debug_assert!(!self.holder.get(), "two guests run in one store");
        self.holder.set(true);
        self.holder_cancelled.set(false);
    }

    /// The guest left the stack: whoever waited for it goes on.
    pub(crate) fn release(&self) {
        self.holder.set(false);
        self.holder_cancelled.set(false);
        // Not woken under the borrow: a waker may do anything.
        let waiters = std::mem::take(&mut *self.waiters.borrow_mut());
        for waiter in waiters {
            waiter.wake();
        }
    }

    pub(crate) fn mark_cancelled(&self) {
        self.holder_cancelled.set(true);
    }

    /// Wakes `waker` when the guest on the stack leaves.
    pub(crate) fn wait(&self, waker: &Waker) {
        let mut waiters = self.waiters.borrow_mut();
        if !waiters.iter().any(|waiter| waiter.will_wake(waker)) {
            waiters.push(waker.clone());
        }
    }

    /// A synchronous call cannot wait for a cancelled guest to leave.
    pub(crate) fn refuse_sync_entry(&self) -> Result<(), RuntimeError> {
        if self.holder.get() && self.holder_cancelled.get() {
            return Err(RuntimeError::new(
                "a guest whose call was dropped is still on the stack of this store",
            ));
        }
        Ok(())
    }

    /// Takes ownership of a store whose last owner is gone: it is freed now,
    /// or when its last guest leaves.
    pub(crate) fn dispose(inner: Box<StoreInner>) {
        let guard = Rc::clone(&inner.guard);
        if guard.guests.get() > 0 {
            *guard.orphan.borrow_mut() = Some(inner);
        }
    }
}

#[cold]
#[inline(never)]
fn store_is_held() -> ! {
    panic!(
        "the store was used while the guest of a forgotten `call_async` can still run; \
         it is usable again once that guest leaves the stack"
    )
}

/// See [`GuestGuard::mark`].
pub(crate) struct SliceMark(Rc<GuestGuard>);

impl Drop for SliceMark {
    fn drop(&mut self) {
        self.0.slices.set(self.0.slices.get() - 1);
    }
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
        self.guard.check();
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
        self.guard.check();
        &self.inner
    }
}

impl DerefMut for GuardedStore {
    #[inline]
    fn deref_mut(&mut self) -> &mut StoreInner {
        self.guard.check();
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

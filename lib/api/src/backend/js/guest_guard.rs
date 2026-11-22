//! Keeps a JSPI guest alive independently of the Rust future that started it.
//!
//! A forgotten exclusive future ends its Rust borrow but not its JavaScript
//! stack. Public store access stays refused until that stack finishes. Internal
//! slices use the call's proven raw pointer; cancelling the call releases
//! the public hold immediately, while its engine activation retains ownership.

use std::{cell::Cell, rc::Rc};

#[derive(Debug, Default)]
pub(crate) struct GuestGuard {
    exclusive: Cell<usize>,
    running: Cell<usize>,
    released: Cell<usize>,
}

impl GuestGuard {
    pub(crate) fn assert_access(&self) {
        assert!(
            self.exclusive.get() == 0,
            "the store is held by an unfinished async JavaScript call"
        );
    }

    pub(crate) fn is_running(&self) -> bool {
        self.running.get() != 0 && self.released.get() == 0
    }

    pub(crate) fn release(&self) -> Released<'_> {
        self.released.set(self.released.get() + 1);
        Released(self)
    }

    pub(crate) fn released(&self) -> usize {
        self.released.get()
    }

    pub(crate) fn restore_released(&self, released: usize) {
        self.released.set(released);
    }

    pub(crate) fn begin(self: &Rc<Self>, exclusive: bool) -> Guest {
        if exclusive {
            self.exclusive.set(self.exclusive.get() + 1);
        }
        self.running.set(self.running.get() + 1);
        Guest {
            guard: self.clone(),
            holds: Cell::new(exclusive),
            running: Cell::new(true),
        }
    }
}

pub(crate) struct Guest {
    guard: Rc<GuestGuard>,
    holds: Cell<bool>,
    running: Cell<bool>,
}

impl Guest {
    pub(crate) fn suspend(&self) {
        if self.running.replace(false) {
            self.guard.running.set(self.guard.running.get() - 1);
        }
    }

    pub(crate) fn resume(&self) {
        if !self.running.replace(true) {
            self.guard.running.set(self.guard.running.get() + 1);
        }
    }

    pub(crate) fn finish(&self) {
        self.suspend();
        if self.holds.replace(false) {
            self.guard.exclusive.set(self.guard.exclusive.get() - 1);
        }
    }

    pub(crate) fn cancel(&self) {
        self.finish();
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Only shared slice acquisition is lent across a release, never public Store
/// access. The caller's `&mut FunctionEnvMut` keeps its borrow unreachable.
pub(crate) struct Released<'a>(&'a GuestGuard);
impl Drop for Released<'_> {
    fn drop(&mut self) {
        self.0.released.set(self.0.released.get() - 1);
    }
}

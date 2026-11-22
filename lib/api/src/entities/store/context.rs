//! Thread-local storage for the stores executing on a thread.
//!
//! When a function is called, a pointer to the [`StoreInner`] is placed in
//! this context, so that host function trampolines can get it back with just
//! the store ID: entering Wasm loses every Rust reference.
//!
//! The context is a stack because calls nest, on one store or several:
//!
//! ```text
//! call(store1, func1) -> wasm -> imported func -> call(store2, func2)
//! ```
//!
//! Each entry holds a pointer derived from the borrow its call was given, so
//! a nested entry's borrows are children of the borrow its caller still
//! holds. Only the top entry is ever used.
//!
//! A suspended `call_async` takes its entries with it: the switch to its
//! coroutine puts them back, and the switch away takes them off, so a call
//! never finds another call's entries, whichever order they resume in. See
//! `sys::async_runtime`.

use std::cell::RefCell;

use super::{StoreInner, StoreMut, StoreRef};

use wasmer_types::StoreId;

pub(crate) struct StoreContext {
    id: StoreId,
    /// How many borrows of this entry are live. A host function trampoline
    /// holds one while it runs. A guest call it makes pushes an entry of its
    /// own, so that the host function's borrow stays valid underneath.
    borrow_count: u32,
    ptr: *mut StoreInner,
}

pub(crate) struct StorePtrWrapper {
    store_ptr: *mut StoreInner,
    /// The store's id, so that giving the borrow back does not use `store_ptr`.
    id: StoreId,
}

pub(crate) struct StoreInstallGuard {
    store_id: StoreId,
}

thread_local! {
    static STORE_CONTEXT_STACK: RefCell<Vec<StoreContext>> = const { RefCell::new(Vec::new()) };
}

impl StoreContext {
    /// The depth of the stack, and the borrows of its top entry.
    #[cfg(all(feature = "js", feature = "experimental-async"))]
    pub(crate) fn checkpoint() -> (usize, u32) {
        STORE_CONTEXT_STACK.with(|cell| {
            let stack = cell.borrow();
            (stack.len(), stack.last().map_or(0, |top| top.borrow_count))
        })
    }

    /// Puts the stack back as [`Self::checkpoint`] found it, after a
    /// JavaScript exception skipped the frames that held its entries.
    #[cfg(all(feature = "js", feature = "experimental-async"))]
    pub(crate) fn restore_checkpoint((depth, borrow_count): (usize, u32)) {
        STORE_CONTEXT_STACK.with(|cell| {
            let mut stack = cell.borrow_mut();
            stack.truncate(depth);
            if let Some(top) = stack.last_mut() {
                top.borrow_count = borrow_count;
            }
        });
    }

    /// Whether the thread still has what unwinding a call suspended in Wasm
    /// needs of it: false once its thread locals are being destroyed, as it
    /// exits, and always with `baremetal`. A call suspended then is forgotten
    /// rather than unwound.
    #[cfg(all(feature = "sys", feature = "experimental-async"))]
    pub(crate) fn thread_can_unwind() -> bool {
        // With `baremetal`, what a guest frame raises leaves through the
        // embedder's unwinder, which would take the unwind of a suspended
        // coroutine off its stack: such a coroutine is forgotten instead.
        !cfg!(feature = "baremetal")
            && STORE_CONTEXT_STACK.try_with(|_| ()).is_ok()
            && wasmer_vm::ActivationState::thread_is_usable()
    }

    /// Puts the entries of a suspended call back on the thread, on top of
    /// whatever runs there, and returns where they start.
    ///
    /// `store` is the pointer the call holds for this slice. It replaces the
    /// entries' own, which were derived before other code last used the
    /// store. It is `None` when the coroutine is being discarded: that never
    /// touches the store, so the old pointers are only compared.
    #[cfg(all(feature = "sys", feature = "experimental-async"))]
    pub(crate) fn resume_entries(entries: &mut Vec<Self>, store: Option<*mut StoreInner>) -> usize {
        STORE_CONTEXT_STACK.with(|cell| {
            let mut stack = cell.borrow_mut();
            let base = stack.len();
            stack.extend(entries.drain(..).map(|mut entry| {
                if let Some(store) = store {
                    entry.ptr = store;
                }
                entry
            }));
            base
        })
    }

    /// Takes the entries of a call off the thread again: everything above
    /// `base`. Never panics, because it runs while a panic may be unwinding.
    #[cfg(all(feature = "sys", feature = "experimental-async"))]
    pub(crate) fn suspend_entries(entries: &mut Vec<Self>, base: usize) {
        let _ = STORE_CONTEXT_STACK.try_with(|cell| {
            if let Ok(mut stack) = cell.try_borrow_mut()
                && base <= stack.len()
            {
                entries.extend(stack.drain(base..));
            }
        });
    }

    /// Whether a coroutine whose entries start at `base` can suspend: it has
    /// its own call's entry and nothing above it. Another entry belongs to a
    /// synchronous call made by a host frame, which must not be parked.
    #[cfg(all(feature = "sys", feature = "experimental-async"))]
    pub(crate) fn can_suspend(base: usize) -> bool {
        STORE_CONTEXT_STACK.with(|cell| cell.borrow().len() == base + 1)
    }

    /// Install `store_ptr` as this thread's executing store until the returned
    /// guard is dropped.
    ///
    /// A store already executing here gets a *second* entry rather than
    /// re-using its first: the entries differ in which borrow their pointer was
    /// derived from, and that difference is load-bearing. Every re-acquisition
    /// ([`Self::get_current`] and friends) reborrows the top entry's pointer,
    /// so re-using an outer entry would make the inner borrow a *sibling* of
    /// the one the caller is still holding — creating it invalidates the
    /// caller's, and the caller's next use of its own store is undefined
    /// behaviour. Installing the pointer the caller just derived from its live
    /// borrow makes the inner borrow a child of it instead, which is what lets
    /// the caller carry on once the nested call returns. See the
    /// `borrow_provenance` tests, which fail under Miri if this re-uses the
    /// outer entry.
    ///
    /// # Safety
    /// `store_ptr` must be derived from a mutable borrow of the store that
    /// stays alive, and unreachable to every frame on this thread, until the
    /// guard is dropped.
    pub(crate) unsafe fn install(store_ptr: *mut StoreInner) -> StoreInstallGuard {
        let store_id = unsafe { store_ptr.as_ref().unwrap().objects.id() };
        STORE_CONTEXT_STACK.with(|cell| {
            let mut stack = cell.borrow_mut();
            // Nesting changes a pointer's provenance, never which store it
            // addresses, so the same store must arrive at the same address.
            debug_assert!(
                stack
                    .last()
                    .is_none_or(|top| top.id != store_id || top.ptr == store_ptr),
                "Store context pointer mismatch"
            );
            stack.push(Self {
                id: store_id,
                borrow_count: 0,
                ptr: store_ptr,
            });
        });
        StoreInstallGuard { store_id }
    }

    /// The active entry's store, if no frame on this thread's stack is holding
    /// a borrow of it.
    ///
    /// A non-zero borrow count means some frame further up still holds a
    /// `StoreMut` it can reach, so handing out a second one would alias it;
    /// the caller gets `None` instead. A frame that has lent its borrow out
    /// ([`StoreMut::parked`]) installed an entry of its own, whose count is
    /// zero until somebody takes it — which is how an imported function lends
    /// its store to code it calls into.
    pub(crate) fn try_get_current_unborrowed() -> Option<StorePtrWrapper> {
        STORE_CONTEXT_STACK.with(|cell| {
            let mut stack = cell.borrow_mut();
            let top = stack.last_mut()?;
            if top.borrow_count != 0 {
                return None;
            }
            top.borrow_count += 1;
            Some(StorePtrWrapper {
                store_ptr: top.ptr,
                id: top.id,
            })
        })
    }

    /// [`Self::try_get_current_unborrowed`], if the active entry is `id`'s.
    #[cfg(feature = "experimental-async")]
    pub(crate) fn try_get_lent(id: StoreId) -> Option<StorePtrWrapper> {
        let is_top =
            STORE_CONTEXT_STACK.with(|cell| cell.borrow().last().is_some_and(|top| top.id == id));
        if is_top {
            Self::try_get_current_unborrowed()
        } else {
            None
        }
    }

    /// Safety: This method lets you borrow multiple mutable references
    /// to the currently active store context. The caller must ensure that:
    ///   * there is only one mutable reference alive, or
    ///   * all but one mutable reference are inaccessible and passed
    ///     into a function that lost the reference (e.g. into WASM code)
    ///
    /// The intended, valid use-case for this method is from within
    /// imported function trampolines.
    pub(crate) unsafe fn get_current(id: StoreId) -> StorePtrWrapper {
        STORE_CONTEXT_STACK.with(|cell| {
            let mut stack = cell.borrow_mut();
            let top = stack
                .last_mut()
                .expect("No store context installed on this thread");
            assert_eq!(top.id, id, "Mismatched store context access");
            top.borrow_count += 1;
            StorePtrWrapper {
                store_ptr: top.ptr,
                id,
            }
        })
    }

    /// Safety: In addition to the safety requirements of [`Self::get_current`],
    /// the pointer returned from this function will become invalid if
    /// the store context is changed in any way (via installing or uninstalling
    /// a store context). The caller must ensure that the store context
    /// remains unchanged as long as the pointer is being accessed.
    #[cfg(feature = "sys")]
    pub(crate) unsafe fn get_current_transient(id: StoreId) -> *mut StoreInner {
        STORE_CONTEXT_STACK.with(|cell| {
            let stack = cell.borrow();
            let top = stack
                .last()
                .expect("No store context installed on this thread");
            assert_eq!(top.id, id, "Mismatched store context access");
            top.ptr
        })
    }
}

impl StorePtrWrapper {
    pub(crate) fn as_ref(&self) -> StoreRef<'_> {
        // Safety: the pointer is valid for as long as this wrapper holds its
        // borrow of the entry.
        StoreRef::from_slice(unsafe { &*self.store_ptr })
    }

    pub(crate) fn as_mut(&mut self) -> StoreMut<'_> {
        // Safety: as for `as_ref`.
        unsafe { StoreMut::from_raw(self.store_ptr) }
    }

    #[cfg(feature = "experimental-async")]
    pub(crate) fn as_ptr(&self) -> *mut StoreInner {
        self.store_ptr
    }
}

impl Drop for StorePtrWrapper {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // The borrow is given back while unwinding too, if its entry is
            // still the active one: a host frame further up that catches the
            // panic must find the count of its own borrow. An entry that is
            // no longer active is removed by its install guard, whatever its
            // count. Never panics: a destructor that panics while unwinding
            // aborts the process.
            let _ = STORE_CONTEXT_STACK.try_with(|cell| {
                if let Ok(mut stack) = cell.try_borrow_mut()
                    && let Some(top) = stack.last_mut()
                    && top.ptr == self.store_ptr
                {
                    top.borrow_count = top.borrow_count.saturating_sub(1);
                }
            });
            return;
        }
        let id = self.id;
        STORE_CONTEXT_STACK.with(|cell| {
            let mut stack = cell.borrow_mut();
            let top = stack
                .last_mut()
                .expect("No store context installed on this thread");
            assert_eq!(top.id, id, "Mismatched store context reinstall");
            top.borrow_count -= 1;
        })
    }
}

impl Drop for StoreInstallGuard {
    fn drop(&mut self) {
        STORE_CONTEXT_STACK.with(|cell| {
            let mut stack = cell.borrow_mut();
            match (stack.pop(), std::thread::panicking()) {
                (Some(top), false) => {
                    assert_eq!(top.id, self.store_id, "Mismatched store context uninstall");
                    assert_eq!(
                        top.borrow_count, 0,
                        "Cannot uninstall store context while it is still borrowed"
                    );
                }
                (Some(top), true) => {
                    // If we're panicking and there's a store ID mismatch, just
                    // put the store back in the hope that its own install guard
                    // take care of uninstalling it later.
                    if top.id != self.store_id {
                        stack.push(top);
                    }
                }
                (None, false) => panic!("Store context stack underflow"),
                (None, true) => {
                    // Nothing to do if we're panicking; panics can put the context
                    // in an invalid state, and we don't want to cause another panic here.
                }
            }
        })
    }
}

#[cfg(test)]
mod borrow_provenance {
    use super::*;
    use crate::{AsStoreMut, Store};

    /// Replays a recursive guest call at the level of the store context, which
    /// is the part of it that has nothing to do with executing Wasm:
    ///
    /// ```text
    /// Function::call(&mut store)          install(ptr from the caller's borrow)
    ///   wasm -> import                    get_current            -> the shim's borrow
    ///     shim: Function::call(&mut env)  install(ptr from *that* borrow)
    ///       wasm -> import                get_current            -> the inner borrow
    ///     shim carries on using its own borrow
    /// ```
    ///
    /// Natively this always passes; it earns its keep under Miri, which is
    /// where the last step is checked. If [`StoreContext::install`] ever goes
    /// back to re-using an entry that is already active, the inner borrow
    /// becomes a sibling of the shim's rather than a child of it, and this test
    /// reports the shim's own store as invalidated:
    ///
    /// ```text
    /// error: Undefined Behavior: trying to retag from <..> for Unique
    ///        permission, but that tag does not exist in the borrow stack
    ///   --> lib/api/src/entities/store/store_ref.rs   &mut self.inner.objects
    /// ```
    ///
    /// Run it with:
    ///
    /// ```text
    /// cargo +nightly miri test -p wasmer --features sys --lib borrow_provenance
    /// ```
    #[test]
    fn nested_call_keeps_the_outer_borrow_usable() {
        let mut store = Store::default();
        let id = store.id();

        // --- the embedder's Function::call(&mut store)
        let mut caller = store.as_store_mut();
        let install = unsafe { StoreContext::install(caller.as_store_mut().inner as *mut _) };

        // --- the import trampoline, handing the shim its store
        let mut wrapper = unsafe { StoreContext::get_current(id) };
        let mut shim = wrapper.as_mut();
        let _ = shim.objects_mut().id();

        {
            // --- the shim calls back into the guest
            let inner_install =
                unsafe { StoreContext::install(shim.as_store_mut().inner as *mut _) };

            // --- the inner import trampoline
            let mut inner_wrapper = unsafe { StoreContext::get_current(id) };
            let mut inner_shim = inner_wrapper.as_mut();
            let _ = inner_shim.objects_mut().id();
            drop(inner_wrapper);

            drop(inner_install);
        }

        // --- and goes on using the borrow it held throughout
        let _ = shim.objects_mut().id();

        drop(wrapper);
        drop(install);
    }

    /// The lending path, which is the nesting above with the inner entry
    /// installed by [`StoreMut::parked`] instead of by a nested call: the
    /// borrow handed out by [`crate::Store::with_current`] must not invalidate
    /// the one the lender is holding. Miri checks the last line.
    #[test]
    fn a_lend_keeps_the_lending_borrow_usable() {
        let mut store = Store::default();
        let id = store.id();

        let mut caller = store.as_store_mut();
        let install = unsafe { StoreContext::install(caller.as_store_mut().inner as *mut _) };

        let mut wrapper = unsafe { StoreContext::get_current(id) };
        let mut shim = wrapper.as_mut();
        let _ = shim.objects_mut().id();

        shim.parked(|| {
            crate::Store::with_current(|lent| {
                let _ = lent.objects_mut().id();
            })
            .expect("a parked store is lendable");
        });

        // The lender goes back to the borrow it held throughout.
        let _ = shim.objects_mut().id();

        drop(wrapper);
        drop(install);
    }

    /// A host function that catches a panic from the guest it called goes on
    /// with the borrow it held throughout, after the nested entry and
    /// the borrow of the host function that panicked were dropped while
    /// unwinding. Miri checks the last use.
    #[test]
    fn a_caught_panic_keeps_the_outer_borrow_usable() {
        let mut store = Store::default();
        let id = store.id();

        let mut caller = store.as_store_mut();
        let install = unsafe { StoreContext::install(caller.as_store_mut().inner as *mut _) };

        let mut wrapper = unsafe { StoreContext::get_current(id) };
        let mut shim = wrapper.as_mut();
        let _ = shim.objects_mut().id();

        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _inner_install =
                unsafe { StoreContext::install(shim.as_store_mut().inner as *mut _) };
            let mut inner_wrapper = unsafe { StoreContext::get_current(id) };
            let _ = inner_wrapper.as_mut().objects_mut().id();
            panic!("host function panicked");
        }));
        assert!(caught.is_err());

        let _ = shim.objects_mut().id();

        drop(wrapper);
        drop(install);
    }
}

#[cfg(test)]
mod unwinding {
    use super::*;
    use crate::{AsStoreMut, Store};

    fn active_borrows() -> u32 {
        STORE_CONTEXT_STACK.with(|cell| cell.borrow().last().unwrap().borrow_count)
    }

    /// A host function that catches a panic from a guest it called finds the
    /// count of its own borrow, with the inner entry gone.
    #[test]
    fn a_caught_panic_leaves_the_borrow_of_the_host_function_that_caught_it() {
        let mut store = Store::default();
        let id = store.id();
        let mut caller = store.as_store_mut();
        let install = unsafe { StoreContext::install(caller.as_store_mut().inner as *mut _) };
        let mut wrapper = unsafe { StoreContext::get_current(id) };
        assert_eq!(active_borrows(), 1);
        let mut shim = wrapper.as_mut();

        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _inner = unsafe { StoreContext::install(shim.as_store_mut().inner as *mut _) };
            let _inner_wrapper = unsafe { StoreContext::get_current(id) };
            assert_eq!(active_borrows(), 1);
            panic!("host function panicked");
        }));
        assert!(caught.is_err());
        assert_eq!(active_borrows(), 1);

        drop(wrapper);
        assert_eq!(active_borrows(), 0);
        drop(install);
    }

    /// A store lent with `parked` is given out once.
    #[cfg(feature = "experimental-async")]
    #[test]
    fn a_lent_store_has_one_holder() {
        let mut store = Store::default();
        let id = store.id();
        store.as_store_mut().parked(|| {
            let lent = StoreContext::try_get_lent(id).expect("lent");
            assert!(StoreContext::try_get_lent(id).is_none());
            drop(lent);
            assert!(StoreContext::try_get_lent(id).is_some());
        });
        assert!(StoreContext::try_get_lent(id).is_none());
    }
}

/// What a `call_async` switch does with the entries of its coroutine.
#[cfg(all(test, feature = "sys", feature = "experimental-async"))]
mod suspended {
    use super::*;
    use crate::{AsStoreMut, Store};

    fn depth() -> usize {
        STORE_CONTEXT_STACK.with(|cell| cell.borrow().len())
    }

    #[test]
    fn a_suspended_call_takes_its_entry_with_it() {
        let mut store = Store::default();
        let id = store.id();
        let ptr: *mut StoreInner = store.as_store_mut().inner;
        let mut saved = Vec::new();

        let base = StoreContext::resume_entries(&mut saved, Some(ptr));
        assert!(!StoreContext::can_suspend(base), "nothing entered yet");
        let guard = unsafe { StoreContext::install(ptr) };
        assert!(StoreContext::can_suspend(base));
        StoreContext::suspend_entries(&mut saved, base);
        assert_eq!((depth(), saved.len()), (base, 1));

        // Another store runs meanwhile, and the call resumes on top of it.
        let mut other = Store::default();
        let other_ptr: *mut StoreInner = other.as_store_mut().inner;
        let other_guard = unsafe { StoreContext::install(other_ptr) };
        let base = StoreContext::resume_entries(&mut saved, Some(ptr));
        assert_eq!(depth(), base + 1);
        assert_eq!(unsafe { StoreContext::get_current_transient(id) }, ptr);
        drop(guard);
        StoreContext::suspend_entries(&mut saved, base);
        assert!(saved.is_empty());
        drop(other_guard);
    }

    /// A synchronous call made by a host frame cannot be suspended.
    #[test]
    fn only_the_calls_own_entry_can_suspend() {
        let mut store = Store::default();
        let ptr: *mut StoreInner = store.as_store_mut().inner;
        let base = StoreContext::resume_entries(&mut Vec::new(), None);
        let outer = unsafe { StoreContext::install(ptr) };
        assert!(StoreContext::can_suspend(base));
        let nested = unsafe { StoreContext::install(ptr) };
        assert!(!StoreContext::can_suspend(base));
        drop(nested);
        assert!(StoreContext::can_suspend(base));
        drop(outer);
    }
}

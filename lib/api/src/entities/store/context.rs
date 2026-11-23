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
//! The entries live on the frames of the calls that install them, linked
//! from the top one, which is all the thread-local holds: it has no
//! destructor and no `RefCell`, and a borrow of an entry is given back through
//! the entry itself. An entry is installed for no longer than the frame that
//! holds it, and the entries above it are always taken off first, so the
//! links never point at a frame that is gone. A JavaScript exception, which
//! skips frames without running their destructors, is the one exception: the
//! boundary it is caught at puts the top back (see [`StoreContext::checkpoint`]).
//!
//! A `call_async` installs its entry for as long as its guest runs, and takes
//! it off when the guest waits or returns, so a call never finds another
//! call's entries, whichever order they resume in. See `sys::async_runtime`.

use std::cell::Cell;
use std::ptr::{self, NonNull};

use super::{StoreInner, StoreMut, StoreRef};

use wasmer_types::StoreId;

/// An entry of the store context: a store executing on this thread, and the
/// borrow its pointer was derived from. It is made on the frame of the call
/// that installs it, and stays there while it is installed: the guard that
/// [`Self::install`] returns borrows it.
pub(crate) struct StoreContext {
    id: StoreId,
    /// How many borrows of this entry are live. A host function trampoline
    /// holds one while it runs. A guest call it makes installs an entry of
    /// its own, so that the host function's borrow stays valid underneath.
    borrow_count: Cell<u32>,
    ptr: *mut StoreInner,
    /// The entry beneath this one while it is installed.
    prev: Cell<*const StoreContext>,
}

pub(crate) struct StorePtrWrapper {
    store_ptr: *mut StoreInner,
    /// The entry the borrow was taken from, which it is given back to. An
    /// entry stays installed, in place, until the call that installed it
    /// returns, and a borrow is given back within that call.
    entry: NonNull<StoreContext>,
    /// The store's id, so that giving the borrow back does not use `store_ptr`.
    #[cfg_attr(not(feature = "experimental-async"), allow(dead_code))]
    id: StoreId,
}

pub(crate) struct StoreInstallGuard<'a> {
    entry: &'a StoreContext,
}

/// The top of the stack, as [`StoreContext::checkpoint`] found it.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct StoreContextCheckpoint {
    top: *const StoreContext,
    borrow_count: u32,
}

thread_local! {
    /// The top entry of this thread's stack of entries, or null.
    static TOP: Cell<*const StoreContext> = const { Cell::new(ptr::null()) };
}

/// Runs `f` on the top of the thread's stack of entries.
///
/// This is `LocalKey::with`, written with `try_with`, which is `#[inline]`
/// where `with` is not: an instance of `with` is compiled apart from the
/// function that calls it, and is inlined into it or not depending on how the
/// crate happens to be split into codegen units. A host function looks its
/// store up here on every call.
#[inline]
fn with_top<R>(f: impl FnOnce(&Cell<*const StoreContext>) -> R) -> R {
    TOP.try_with(f)
        .expect("the store context of a thread was used during or after its destruction")
}

/// The top entry. The reference is valid for as long as the entry stays
/// installed, which outlasts any use the callers make of it.
#[inline]
fn top_entry<'a>(top: &Cell<*const StoreContext>) -> Option<&'a StoreContext> {
    // Safety: an installed entry lives, in place, on the frame of the call
    // that installed it, until that call takes it off.
    unsafe { top.get().as_ref() }
}

/// The panic of a lookup that finds another store's entry on top. Out of
/// line, and given the ids by value, so that a host function does not keep
/// them in memory on every call to have their addresses ready.
#[cold]
#[inline(never)]
#[track_caller]
fn mismatched(found: StoreId, expected: StoreId) -> ! {
    panic!("Mismatched store context access: found {found:?}, expected {expected:?}")
}

impl StoreContext {
    /// An entry for `store_ptr`, to be installed with [`Self::install`].
    ///
    /// # Safety
    /// `store_ptr` must point to a live store.
    #[inline]
    pub(crate) unsafe fn new(store_ptr: *mut StoreInner) -> Self {
        Self {
            id: unsafe { (*store_ptr).objects.id() },
            borrow_count: Cell::new(0),
            ptr: store_ptr,
            prev: Cell::new(ptr::null()),
        }
    }

    /// The top of the stack, and the borrows of its top entry: for a backend
    /// whose host can unwind through Rust frames without running them.
    #[allow(dead_code)]
    pub(crate) fn checkpoint() -> StoreContextCheckpoint {
        with_top(|top| StoreContextCheckpoint {
            top: top.get(),
            borrow_count: top_entry(top).map_or(0, |entry| entry.borrow_count.get()),
        })
    }

    /// Puts the stack back as [`Self::checkpoint`] found it, after an unwind
    /// skipped the frames that held its entries: those entries are gone, and
    /// nothing may point at them any more.
    #[allow(dead_code)]
    pub(crate) fn restore_checkpoint(checkpoint: StoreContextCheckpoint) {
        with_top(|top| {
            top.set(checkpoint.top);
            if let Some(entry) = top_entry(top) {
                entry.borrow_count.set(checkpoint.borrow_count);
            }
        });
    }

    /// Install this entry as this thread's executing store until the returned
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
    /// The entry's pointer must be derived from a mutable borrow of the store
    /// that stays alive, and unreachable to every frame on this thread, until
    /// the guard is dropped.
    #[inline]
    pub(crate) unsafe fn install(&self) -> StoreInstallGuard<'_> {
        with_top(|top| {
            let prev = top.get();
            // Nesting changes a pointer's provenance, never which store it
            // addresses, so the same store must arrive at the same address.
            debug_assert!(
                top_entry(top).is_none_or(|prev| prev.id != self.id || prev.ptr == self.ptr),
                "Store context pointer mismatch"
            );
            debug_assert!(!ptr::eq(prev, self), "Store context entry installed twice");
            self.prev.set(prev);
            top.set(self);
        });
        StoreInstallGuard { entry: self }
    }

    /// A borrow of the top entry, counted in it.
    #[inline]
    fn borrow(&self) -> StorePtrWrapper {
        self.borrow_count.set(self.borrow_count.get() + 1);
        StorePtrWrapper {
            store_ptr: self.ptr,
            entry: NonNull::from(self),
            id: self.id,
        }
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
        with_top(|top| {
            let entry = top_entry(top)?;
            (entry.borrow_count.get() == 0).then(|| entry.borrow())
        })
    }

    /// [`Self::try_get_current_unborrowed`], if the active entry is `id`'s.
    #[cfg(feature = "experimental-async")]
    pub(crate) fn try_get_lent(id: StoreId) -> Option<StorePtrWrapper> {
        with_top(|top| {
            let entry = top_entry(top)?;
            (entry.id == id && entry.borrow_count.get() == 0).then(|| entry.borrow())
        })
    }

    /// Safety: This method lets you borrow multiple mutable references
    /// to the currently active store context. The caller must ensure that:
    ///   * there is only one mutable reference alive, or
    ///   * all but one mutable reference are inaccessible and passed
    ///     into a function that lost the reference (e.g. into WASM code)
    ///
    /// The intended, valid use-case for this method is from within
    /// imported function trampolines.
    #[inline]
    pub(crate) unsafe fn get_current(id: StoreId) -> StorePtrWrapper {
        with_top(|top| {
            let entry = top_entry(top).expect("No store context installed on this thread");
            if entry.id != id {
                mismatched(entry.id, id);
            }
            entry.borrow()
        })
    }

    /// Safety: In addition to the safety requirements of [`Self::get_current`],
    /// the pointer returned from this function will become invalid if
    /// the store context is changed in any way (via installing or uninstalling
    /// a store context). The caller must ensure that the store context
    /// remains unchanged as long as the pointer is being accessed.
    #[cfg(feature = "sys")]
    pub(crate) unsafe fn get_current_transient(id: StoreId) -> *mut StoreInner {
        with_top(|top| {
            let entry = top_entry(top).expect("No store context installed on this thread");
            if entry.id != id {
                mismatched(entry.id, id);
            }
            entry.ptr
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

    /// The pointer the borrow was taken through, for a trampoline to use the
    /// store on the Wasm stack once the borrow is given back.
    #[cfg(feature = "sys")]
    pub(crate) fn as_ptr(&self) -> *mut StoreInner {
        self.store_ptr
    }

    #[cfg(feature = "experimental-async")]
    pub(crate) fn id(&self) -> StoreId {
        self.id
    }
}

impl Drop for StorePtrWrapper {
    #[inline]
    fn drop(&mut self) {
        // Safety: the entry outlives the borrow (see the field). The borrow is
        // given back while unwinding too: a host frame further up that catches
        // the panic must find the count of its own borrow.
        let entry = unsafe { self.entry.as_ref() };
        entry.borrow_count.set(entry.borrow_count.get() - 1);
        debug_assert!(
            std::thread::panicking() || with_top(|top| ptr::eq(top.get(), entry)),
            "Mismatched store context reinstall"
        );
    }
}

impl Drop for StoreInstallGuard<'_> {
    // Inlined into the call that installed the entry, which then looks the
    // thread-local up once for both.
    #[inline]
    fn drop(&mut self) {
        let entry = self.entry;
        // Taken off before anything is checked, so that the thread never keeps
        // an entry whose frame is about to go. Entries still above it were
        // left by frames an unwind skipped: they go with it.
        let was_top = with_top(|top| {
            let was_top = ptr::eq(top.get(), entry);
            top.set(entry.prev.get());
            was_top
        });
        if !std::thread::panicking() {
            assert!(was_top, "Mismatched store context uninstall");
            assert_eq!(
                entry.borrow_count.get(),
                0,
                "Cannot uninstall store context while it is still borrowed"
            );
        }
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
        let entry = unsafe { StoreContext::new(caller.as_store_mut().inner as *mut _) };
        let install = unsafe { entry.install() };

        // --- the import trampoline, handing the shim its store
        let mut wrapper = unsafe { StoreContext::get_current(id) };
        let mut shim = wrapper.as_mut();
        let _ = shim.objects_mut().id();

        {
            // --- the shim calls back into the guest
            let inner_entry = unsafe { StoreContext::new(shim.as_store_mut().inner as *mut _) };
            let inner_install = unsafe { inner_entry.install() };

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
        let entry = unsafe { StoreContext::new(caller.as_store_mut().inner as *mut _) };
        let install = unsafe { entry.install() };

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
        let entry = unsafe { StoreContext::new(caller.as_store_mut().inner as *mut _) };
        let install = unsafe { entry.install() };

        let mut wrapper = unsafe { StoreContext::get_current(id) };
        let mut shim = wrapper.as_mut();
        let _ = shim.objects_mut().id();

        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let inner_entry = unsafe { StoreContext::new(shim.as_store_mut().inner as *mut _) };
            let _inner_install = unsafe { inner_entry.install() };
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
        with_top(|top| top_entry(top).unwrap().borrow_count.get())
    }

    /// A host function that catches a panic from a guest it called finds the
    /// count of its own borrow, with the inner entry gone.
    #[test]
    fn a_caught_panic_leaves_the_borrow_of_the_host_function_that_caught_it() {
        let mut store = Store::default();
        let id = store.id();
        let mut caller = store.as_store_mut();
        let entry = unsafe { StoreContext::new(caller.as_store_mut().inner as *mut _) };
        let install = unsafe { entry.install() };
        let mut wrapper = unsafe { StoreContext::get_current(id) };
        assert_eq!(active_borrows(), 1);
        let mut shim = wrapper.as_mut();

        let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let inner_entry = unsafe { StoreContext::new(shim.as_store_mut().inner as *mut _) };
            let _inner = unsafe { inner_entry.install() };
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

    /// Frames that an unwind skips without running their destructors, as a
    /// JavaScript exception does, leave their entries installed and their
    /// borrows counted. The checkpoint taken before them puts the top back,
    /// so nothing points at those entries once their frames are gone.
    #[test]
    fn a_checkpoint_drops_the_entries_of_skipped_frames() {
        let mut store = Store::default();
        let id = store.id();
        let mut caller = store.as_store_mut();
        let entry = unsafe { StoreContext::new(caller.as_store_mut().inner as *mut _) };
        let install = unsafe { entry.install() };
        let checkpoint = StoreContext::checkpoint();
        {
            // A host function that calls back into the guest, skipped.
            let mut wrapper = unsafe { StoreContext::get_current(id) };
            let mut shim = wrapper.as_mut();
            let inner_entry = unsafe { StoreContext::new(shim.as_store_mut().inner as *mut _) };
            std::mem::forget(unsafe { inner_entry.install() });
            std::mem::forget(unsafe { StoreContext::get_current(id) });
            std::mem::forget(wrapper);
        }
        StoreContext::restore_checkpoint(checkpoint);
        assert_eq!(active_borrows(), 0);
        let lent = StoreContext::try_get_current_unborrowed().expect("the outer entry is on top");
        assert_eq!(active_borrows(), 1);
        drop(lent);
        drop(install);
        assert!(StoreContext::try_get_current_unborrowed().is_none());
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

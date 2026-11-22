//! Thread-local storage for storing the current store context,
//! i.e. the currently active [`Store`]s. When
//! a function is called, a pointer to the [`StoreInner`] is placed
//! inside the store context so it can be retrieved when needed.
//! This lets code that needs access to the store get it with
//! just the store ID.
//!
//! The currently active store context can be a sync or async
//! context.
//!
//! For sync contexts, we just store a raw pointer
//! to the `StoreInner`, which is owned by the embedder's stack.
//!
//! An async poll installs one raw context for its coroutine resume. The root
//! coroutine call uses that entry directly. Host trampolines relinquish their
//! store borrows before suspension; synchronous nested entries cannot suspend.
//!
//! We maintain a stack because it is technically possible to
//! have nested `Function::call` invocations that use different
//! stores, such as:
//!     call(store1, func1) -> wasm code -> imported func ->
//!     call(store2, func2)
//!
//! Note that this stack is maintained by both function
//! calls and the async_runtime to reflect the exact WASM
//! functions running on a given thread at any moment in
//! time. If a function suspends, its store context is
//! cleared and later reinstalled when it resumes. This keeps
//! thread-local state specific to the running stack. Async calls remain
//! bound to their thread through their non-Send store capabilities.
//!
//! When something needs the "currently active" store context,
//! they will only look at the top entry in the stack. It is
//! always an error for code to try to access a store that's
//! "paused", i.e. not the top entry. This should be impossible
//! due to how the function call code is structured, but we
//! guard against it anyway.

use std::{
    borrow::BorrowMut,
    cell::{RefCell, UnsafeCell},
    mem::MaybeUninit,
};

use super::{AsStoreMut, AsStoreRef, StoreInner, StoreMut, StoreRef};

use wasmer_types::StoreId;

enum StoreContextEntry {
    Sync(*mut StoreInner),

    #[cfg(feature = "experimental-async")]
    BorrowedAsync {
        ptr: *mut StoreInner,
    },
}

impl StoreContextEntry {
    fn as_ptr(&self) -> *mut StoreInner {
        match self {
            Self::Sync(ptr) => *ptr,
            #[cfg(feature = "experimental-async")]
            Self::BorrowedAsync { ptr, .. } => *ptr,
        }
    }
}

pub(crate) struct StoreContext {
    id: StoreId,
    // Cached outside StoreInner's borrow tree, so safe callback admission can
    // inspect the slice flag without accessing a store already borrowed.
    allocation: *const super::AllocationInner,

    // Host trampolines recover the loan passed into Wasm through this entry.
    // The count prevents public callbacks from borrowing a loan already held
    // by a host frame. A nested call installs its own derived pointer, leaving
    // the outer loan unreachable until that call returns.
    borrow_count: u32,
    entry: UnsafeCell<StoreContextEntry>,
}

pub(crate) struct CurrentStoreGuard {
    store: StorePtrWrapper,
    allocation: *const super::AllocationInner,
}

impl CurrentStoreGuard {
    pub(crate) fn as_mut(&mut self) -> StoreMut<'_> {
        self.store.as_mut()
    }
}

impl Drop for CurrentStoreGuard {
    fn drop(&mut self) {
        // The installed context keeps its store allocation alive through this
        // callback. No user code runs between releasing the two borrow checks.
        unsafe {
            (*self.allocation).busy.set(false);
        }
    }
}

pub(crate) struct StorePtrWrapper {
    store_ptr: *mut StoreInner,
    /// The store's id, so giving the borrow back never reborrows the store
    /// through `store_ptr` while another frame may hold its derived loan.
    id: StoreId,
}

pub(crate) struct ForcedStoreInstallGuard {
    store_id: StoreId,
}

pub(crate) enum StoreInstallGuard {
    /// An entry of its own was pushed for the store.
    Pushed(StoreId),
}

thread_local! {
    static STORE_CONTEXT_STACK: RefCell<Vec<StoreContext>> = const { RefCell::new(Vec::new()) };
}

impl StoreContext {
    /// Snapshot synchronous callback loans before entering a JavaScript host
    /// function. JavaScript exceptions may skip the Rust guards inside it.
    #[cfg(all(feature = "js", feature = "experimental-async"))]
    pub(crate) fn checkpoint() -> (usize, u32) {
        STORE_CONTEXT_STACK.with(|cell| {
            let stack = cell.borrow();
            (
                stack.len(),
                stack.last().map_or(0, |entry| entry.borrow_count),
            )
        })
    }

    /// Recover callback loans after a JavaScript host function exits.
    ///
    /// # Safety
    /// The checkpoint belongs to this thread and the enclosing JavaScript
    /// `finally` boundary. All Rust frames and references created inside that
    /// boundary have returned or been unwound before this is called. Frames
    /// below it, including the entry at `depth - 1`, remain alive and unchanged.
    #[cfg(all(feature = "js", feature = "experimental-async"))]
    pub(crate) unsafe fn restore_checkpoint(depth: usize, borrow_count: u32) {
        STORE_CONTEXT_STACK.with(|cell| {
            let mut stack = cell.borrow_mut();
            assert!(
                stack.len() >= depth,
                "mismatched JavaScript store checkpoint"
            );
            stack.truncate(depth);
            if let Some(entry) = stack.last_mut() {
                entry.borrow_count = borrow_count;
            }
        });
    }

    /// Whether `id`'s entry is the active one.
    pub(crate) fn is_active(id: StoreId) -> bool {
        STORE_CONTEXT_STACK.with(|cell| {
            let stack = cell.borrow();
            stack.last().is_some_and(|ctx| ctx.id == id)
        })
    }

    /// Whether `id` has an entry on the thread's stack, active or not: a call
    /// on this thread, beneath whatever runs, holds the store.
    #[cfg(all(feature = "sys", feature = "experimental-async"))]
    pub(crate) fn is_held(id: StoreId) -> bool {
        STORE_CONTEXT_STACK.with(|cell| cell.borrow().iter().any(|ctx| ctx.id == id))
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

    fn push(id: StoreId, entry: StoreContextEntry) {
        // SAFETY: every entry is installed from a live store loan or owner.
        // This pointer names allocation metadata, not the borrowed StoreInner.
        let allocation = unsafe { (*entry.as_ptr()).allocation.as_ptr() };
        STORE_CONTEXT_STACK.with(|cell| {
            let mut stack = cell.borrow_mut();
            stack.push(Self {
                id,
                allocation,
                borrow_count: 0,
                entry: UnsafeCell::new(entry),
            });
        })
    }

    /// Returns true if there are no active store context entries.
    pub(crate) fn is_empty() -> bool {
        STORE_CONTEXT_STACK.with(|cell| {
            let stack = cell.borrow();
            stack.is_empty()
        })
    }

    /// Install an exclusively lent async slice. No lock or waiter is involved.
    ///
    /// # Safety
    /// The caller owns the slice until the guard is dropped. No host frame
    /// may retain a reference when the coroutine suspends.
    #[cfg(feature = "experimental-async")]
    pub(crate) unsafe fn install_async_raw(ptr: *mut StoreInner) -> ForcedStoreInstallGuard {
        let store_id = unsafe { (*ptr).objects.id() };
        Self::push(store_id, StoreContextEntry::BorrowedAsync { ptr });
        ForcedStoreInstallGuard { store_id }
    }

    /// Whether the entry on top of the thread's stack, the store that is
    /// running, is held by an async context. A store entered synchronously
    /// cannot be suspended, not even from a `call_async` coroutine further
    /// down the stack: its caller holds a borrow of it across the call.
    #[cfg(feature = "experimental-async")]
    pub(crate) fn top_is_async() -> bool {
        STORE_CONTEXT_STACK.with(|cell| {
            cell.borrow().last().is_some_and(|top| {
                matches!(
                    unsafe { top.entry.get().as_ref().unwrap() },
                    StoreContextEntry::BorrowedAsync { .. }
                )
            })
        })
    }

    /// Whether the call running on top of the thread's stack can suspend, and
    /// give its store up while it waits: the top entry is held by an async
    /// context, and no host function holds a borrow of it across the guest
    /// call that is running. A host function that calls its guest back holds
    /// one, which it goes on with once the call returns: whoever used the
    /// store while the call waited would have aliased it.
    #[cfg(feature = "experimental-async")]
    pub(crate) fn top_can_suspend() -> bool {
        STORE_CONTEXT_STACK.with(|cell| {
            cell.borrow().last().is_some_and(|top| {
                top.borrow_count == 0
                    && matches!(
                        unsafe { top.entry.get().as_ref().unwrap() },
                        StoreContextEntry::BorrowedAsync { .. }
                    )
            })
        })
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
    /// The root async coroutine uses the poller's raw entry directly. Every
    /// synchronous nested call pushes a normal entry, so it cannot suspend.
    ///
    /// # Safety
    /// `store_ptr` must be derived from a mutable borrow of the store that
    /// stays alive, and unreachable to every frame on this thread, until the
    /// guard is dropped.
    pub(crate) unsafe fn install(store_ptr: *mut StoreInner) -> StoreInstallGuard {
        let store_id = unsafe { store_ptr.as_ref().unwrap().objects.id() };
        // Nesting changes a pointer's provenance, never which store it
        // addresses, so the same store must arrive at the same address.
        debug_assert!(
            !Self::is_active(store_id)
                || STORE_CONTEXT_STACK.with(|cell| {
                    let stack = cell.borrow();
                    let active =
                        unsafe { stack.last().unwrap().entry.get().as_ref().unwrap().as_ptr() };
                    active == store_ptr
                }),
            "Store context pointer mismatch"
        );
        Self::push(store_id, StoreContextEntry::Sync(store_ptr));
        StoreInstallGuard::Pushed(store_id)
    }

    /// Install a parked synchronous loan as a separate entry, even underneath
    /// an async call. The parked borrow is unreachable, and the new entry's
    /// count starts at zero so callbacks can acquire its derived loan.
    ///
    /// # Safety
    /// `ptr` is an exclusive store borrow unreachable until the guard drops.
    pub(crate) unsafe fn install_lent(ptr: *mut StoreInner) -> StoreInstallGuard {
        let id = unsafe { (*ptr).objects.id() };
        Self::push(id, StoreContextEntry::Sync(ptr));
        StoreInstallGuard::Pushed(id)
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
    pub(crate) fn try_get_current_unborrowed() -> Option<CurrentStoreGuard> {
        STORE_CONTEXT_STACK.with(|cell| {
            let mut stack = cell.borrow_mut();
            let top = stack.last_mut()?;
            if top.borrow_count != 0 {
                return None;
            }
            // Inspect cached allocation metadata before producing a StoreMut.
            // SharedStore and AsyncEnv slices use the same admission flag.
            let allocation = unsafe { &*top.allocation };
            if allocation.busy.replace(true) {
                return None;
            }
            top.borrow_count += 1;
            Some(CurrentStoreGuard {
                store: StorePtrWrapper {
                    store_ptr: unsafe { top.entry.get().as_mut().unwrap().as_ptr() },
                    id: top.id,
                },
                allocation: top.allocation,
            })
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
    pub(crate) unsafe fn get_current(id: StoreId) -> StorePtrWrapper {
        STORE_CONTEXT_STACK.with(|cell| {
            let mut stack = cell.borrow_mut();
            let top = stack
                .last_mut()
                .expect("No store context installed on this thread");
            assert_eq!(top.id, id, "Mismatched store context access");
            top.borrow_count += 1;
            StorePtrWrapper {
                store_ptr: unsafe { top.entry.get().as_mut().unwrap().as_ptr() },
                id,
            }
        })
    }

    /// Safety: In addition to the safety requirements of [`Self::get_current`],
    /// the pointer returned from this function will become invalid if
    /// the store context is changed in any way (via installing or uninstalling
    /// a store context). The caller must ensure that the store context
    /// remains unchanged as long as the pointer is being accessed.
    pub(crate) unsafe fn get_current_transient(id: StoreId) -> *mut StoreInner {
        STORE_CONTEXT_STACK.with(|cell| {
            let mut stack = cell.borrow_mut();
            let top = stack
                .last_mut()
                .expect("No store context installed on this thread");
            assert_eq!(top.id, id, "Mismatched store context access");
            unsafe { top.entry.get().as_mut().unwrap().as_ptr() }
        })
    }

    /// Safety: See [`Self::get_current`].
    pub(crate) unsafe fn try_get_current(id: StoreId) -> Option<StorePtrWrapper> {
        STORE_CONTEXT_STACK.with(|cell| {
            let mut stack = cell.borrow_mut();
            let top = stack.last_mut()?;
            if top.id != id {
                return None;
            }
            top.borrow_count += 1;
            Some(StorePtrWrapper {
                store_ptr: unsafe { top.entry.get().as_mut().unwrap().as_ptr() },
                id: top.id,
            })
        })
    }
}

impl StorePtrWrapper {
    pub(crate) fn as_ref(&self) -> StoreRef<'_> {
        // Safety: the store_mut is always initialized unless the StoreMutWrapper
        // is dropped, at which point it's impossible to call this function
        unsafe { self.store_ptr.as_ref().unwrap().as_store_ref() }
    }

    pub(crate) fn as_mut(&mut self) -> StoreMut<'_> {
        // Safety: the store_mut is always initialized unless the StoreMutWrapper
        // is dropped, at which point it's impossible to call this function
        unsafe { self.store_ptr.as_mut().unwrap().as_store_mut() }
    }
}

impl Clone for StorePtrWrapper {
    fn clone(&self) -> Self {
        STORE_CONTEXT_STACK.with(|cell| {
            let mut stack = cell.borrow_mut();
            let top = stack
                .last_mut()
                .expect("No store context installed on this thread");
            match unsafe { top.entry.get().as_ref().unwrap() } {
                StoreContextEntry::Sync(ptr) if *ptr == self.store_ptr => (),
                _ => panic!("Mismatched store context access"),
            }
            top.borrow_count += 1;
            Self {
                store_ptr: self.store_ptr,
                id: self.id,
            }
        })
    }
}

/// Runs `f` on the active entry while a panic unwinds, if there is one. Never
/// panics: a destructor that panics while unwinding aborts the process.
fn with_active_while_unwinding(f: impl FnOnce(&mut StoreContext)) {
    let _ = STORE_CONTEXT_STACK.try_with(|cell| {
        if let Ok(mut stack) = cell.try_borrow_mut()
            && let Some(top) = stack.last_mut()
        {
            f(top);
        }
    });
}

impl Drop for StorePtrWrapper {
    fn drop(&mut self) {
        if std::thread::panicking() {
            // A host frame that catches a panic must find its own borrow
            // count restored. Entries already unwound need no adjustment.
            with_active_while_unwinding(|top| {
                if unsafe { top.entry.get().as_ref().unwrap() }.as_ptr() == self.store_ptr {
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
        let Self::Pushed(store_id) = *self;
        STORE_CONTEXT_STACK.with(|cell| {
            let mut stack = cell.borrow_mut();
            match (stack.pop(), std::thread::panicking()) {
                (Some(top), false) => {
                    assert_eq!(top.id, store_id, "Mismatched store context uninstall");
                    assert_eq!(
                        top.borrow_count, 0,
                        "Cannot uninstall store context while it is still borrowed"
                    );
                }
                (Some(top), true) => {
                    // If we're panicking and there's a store ID mismatch, just
                    // put the store back in the hope that its own install guard
                    // take care of uninstalling it later.
                    if top.id != store_id {
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

impl Drop for ForcedStoreInstallGuard {
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
    /// with the borrow it held throughout, after the nested entry and the
    /// borrow of the host function that panicked were dropped while
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

#[cfg(all(test, feature = "experimental-async"))]
mod unwinding {
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use super::*;
    use crate::Store;

    fn active_borrows() -> u32 {
        STORE_CONTEXT_STACK.with(|cell| cell.borrow().last().unwrap().borrow_count)
    }

    /// A nested synchronous guest call has its own context entry. Unwinding
    /// that entry restores the outer async entry with its host borrow intact,
    /// so a host function that catches the panic can keep using its loan.
    #[test]
    fn a_caught_panic_leaves_the_borrow_of_the_host_function_that_caught_it() {
        let mut store = Store::default();
        let id = store.id();
        let install =
            unsafe { StoreContext::install_async_raw(store.as_store_mut().inner as *mut _) };

        // --- the host function that catches the panic
        let mut wrapper = unsafe { StoreContext::get_current(id) };
        assert_eq!(active_borrows(), 1);
        let caught = catch_unwind(AssertUnwindSafe(|| {
            // --- its guest call
            let _install = unsafe { StoreContext::install(wrapper.as_mut().inner as *mut _) };

            assert_eq!(active_borrows(), 0);
            // --- a host function the guest reaches, which panics
            let _inner_wrapper = unsafe { StoreContext::get_current(id) };
            assert_eq!(active_borrows(), 1);
            panic!("host function panicked");
        }));
        assert!(caught.is_err());
        assert_eq!(active_borrows(), 1);

        let _ = wrapper.as_mut().objects_mut().id();
        drop(wrapper);
        drop(install);
        assert!(StoreContext::is_empty());
    }

    /// A synchronous host function of a call in an async context calls its
    /// guest back, and goes on with its store once that returns, as one of
    /// a synchronous call does (see `borrow_provenance`): the borrows of the
    /// host functions that guest call reaches are children of its own. Miri
    /// checks the last write.
    #[test]
    fn a_nested_call_keeps_the_outer_borrow_usable() {
        // Writes the store's objects back as they are: a write through
        // `store`, which Tree Borrows needs to see a sibling's.
        fn write(store: &mut StoreMut<'_>) {
            let objects = store.objects_mut();
            unsafe { std::ptr::write(objects, std::ptr::read(objects)) };
        }

        let mut store = Store::default();
        let id = store.id();
        let install =
            unsafe { StoreContext::install_async_raw(store.as_store_mut().inner as *mut _) };

        // --- the host function, handed its store
        let mut wrapper = unsafe { StoreContext::get_current(id) };
        let mut outer = wrapper.as_mut();
        write(&mut outer);
        {
            // --- its guest call
            let _install = unsafe { StoreContext::install(outer.as_store_mut().inner as *mut _) };

            // --- a host function that guest call reaches
            let mut inner = unsafe { StoreContext::get_current(id) };
            write(&mut inner.as_mut());
        }
        // --- the host function goes on with its store
        write(&mut outer);

        drop(wrapper);
        drop(install);
        assert!(StoreContext::is_empty());
    }

    /// A guest call can suspend, and give its store up, only where no host
    /// function holds a borrow of the store across it: the coroutine's own
    /// call can, which nothing holds a borrow across, and the guest call of a
    /// host function cannot, until it has returned. A panic caught by that
    /// host function restores its original context entry as well.
    #[test]
    fn only_a_call_no_host_function_holds_can_suspend() {
        let mut store = Store::default();
        let id = store.id();
        let install =
            unsafe { StoreContext::install_async_raw(store.as_store_mut().inner as *mut _) };

        // The root coroutine uses its poller's context directly.
        assert!(StoreContext::top_can_suspend());

        // --- a host function the guest reaches, and its guest call
        let mut host = unsafe { StoreContext::get_current(id) };
        assert!(!StoreContext::top_can_suspend());
        {
            let _call = unsafe { StoreContext::install(host.as_mut().inner as *mut _) };
            assert!(!StoreContext::top_is_async());
            assert!(
                !StoreContext::top_can_suspend(),
                "the host function holds its borrow across the call"
            );
        }
        let caught = catch_unwind(AssertUnwindSafe(|| {
            let _call = unsafe { StoreContext::install(host.as_mut().inner as *mut _) };

            panic!("a host function of the inner call panicked");
        }));
        assert!(caught.is_err());
        assert_eq!(active_borrows(), 1);
        assert!(!StoreContext::top_can_suspend());
        drop(host);
        assert!(StoreContext::top_can_suspend());

        drop(install);
        assert!(StoreContext::is_empty());
    }

    /// A panic inside the same entry gives back only the borrow acquired
    /// by the panicking host function.
    #[test]
    fn a_caught_panic_without_a_guest_call_leaves_the_count_alone() {
        let mut store = Store::default();
        let id = store.id();
        let install =
            unsafe { StoreContext::install_async_raw(store.as_store_mut().inner as *mut _) };

        let wrapper = unsafe { StoreContext::get_current(id) };
        let caught = catch_unwind(AssertUnwindSafe(|| {
            let _inner_wrapper = unsafe { StoreContext::get_current(id) };
            assert_eq!(active_borrows(), 2);
            panic!("host function panicked");
        }));
        assert!(caught.is_err());
        assert_eq!(active_borrows(), 1);

        drop(wrapper);
        drop(install);
        assert!(StoreContext::is_empty());
    }

    /// The same for the borrow an async host function takes while it reads
    /// its arguments.
    #[test]
    fn a_caught_panic_gives_back_the_borrow_of_an_async_host_function() {
        let mut store = Store::default();
        let id = store.id();
        let install =
            unsafe { StoreContext::install_async_raw(store.as_store_mut().inner as *mut _) };

        let wrapper = unsafe { StoreContext::get_current(id) };
        let caught = catch_unwind(AssertUnwindSafe(|| {
            let _inner = unsafe { StoreContext::get_current(id) };
            assert_eq!(active_borrows(), 2);
            panic!("async host function panicked");
        }));
        assert!(caught.is_err());
        assert_eq!(active_borrows(), 1);

        drop(wrapper);
        drop(install);
        assert!(StoreContext::is_empty());
    }
}

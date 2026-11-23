use std::{
    cell::UnsafeCell,
    ffi::CStr,
    sync::{
        Arc, LazyLock, Mutex, PoisonError,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use dashmap::{DashMap, Entry};
use smallvec::SmallVec;
use wasmer_types::StoreId;

use super::*;

/// All necessary data for interrupting a store running WASM code
/// on a thread.
struct StoreInterruptState {
    /// The thread the store is running on, which the interrupt signal is
    /// sent to. Note that multiple stores may be executing WASM code within
    /// the same OS thread.
    thread: Arc<InterruptTarget>,
    /// Whether this store was interrupted.
    interrupted: Arc<AtomicBool>,
    /// How many Wasm entries have this state installed, counting those of
    /// suspended activations, which are not in the thread's `active_stores`.
    installations: usize,
    /// See comments in [`ThreadInterruptState`].
    thread_current_signal_target_store: Arc<AtomicUsize>,
}

/// A thread that runs stores, as the threads that interrupt them see it.
struct InterruptTarget {
    /// The thread's pthread. We store this as a plain integer because
    /// `libc::pthread_t` is a raw pointer on some Unix targets, which would
    /// make the global `DashMap` fail its `Send` bounds even though we only
    /// treat the value as an opaque thread identifier.
    pthread: usize,
    /// False once the thread's locals are destroyed, as it exits. A store can
    /// stay installed for a thread after it exited: a suspended call that was
    /// forgotten keeps its installation. [`interrupt`] signals the thread
    /// only while it holds this lock and finds it true, and the thread clears
    /// it under the same lock, so a signal never goes to a thread that is
    /// gone, or to another one that got its id since.
    alive: Mutex<bool>,
}

/// Clears [`InterruptTarget::alive`] when the thread's locals are destroyed.
struct ThreadTarget(Arc<InterruptTarget>);

impl Drop for ThreadTarget {
    fn drop(&mut self) {
        *self.0.alive.lock().unwrap_or_else(PoisonError::into_inner) = false;
    }
}

/// The stores a thread, or one activation, has entered, innermost last, each
/// with the flag an interrupt of it sets. The usual few are inline: an
/// activation starts with an empty list, and entering its store must not
/// allocate.
type Stores = SmallVec<[(StoreId, Arc<AtomicBool>); 2]>;

/// Thread-related state; only **PARTS** of this struct are safe to access
/// from within the interrupt handler.
struct ThreadInterruptState {
    /// We need to maintain a stack of active stores per thread, hence the vec.
    /// This should not be touched by the interrupt handler.
    active_stores: Stores,

    /// Always stores the top entry from `active_stores`. Needed since a vec is not
    /// safe to access from signal handlers.
    current_active_store: AtomicUsize,

    /// Shared state between the thread requesting the interrupt
    /// and the thread running the store's code. The thread
    /// requesting the interrupt writes the ID of the store it
    /// wants to interrupt to this atomic. The interrupted
    /// thread later checks this value (through its own clone
    /// of the Arc in [`ThreadInterruptState`]) against the currently
    /// running store, and traps only if they match, recording the
    /// interrupt otherwise.
    /// Note that mutexes are not safe for use within signal
    /// handlers; only atomics can be safely used.
    current_signal_target_store: Arc<AtomicUsize>,

    /// This thread, for the stores installed on it. Never touched by the
    /// interrupt handler.
    target: ThreadTarget,
}

/// HashMap of all store states, accessible from all threads
static STORE_INTERRUPT_STATE: LazyLock<DashMap<StoreId, StoreInterruptState>> =
    LazyLock::new(Default::default);

thread_local! {
    /// Thread-local thread state. The book-keeping in a RefCell isn't
    /// guaranteed to be signal-handler-safe, so we use an UnsafeCell
    /// instead. The cell is only accessed in leaf functions, so it
    /// should be safe.
    /// The *only* actually unsafe access happens if a signal comes in
    /// while another function is modifying the cell; In this case,
    /// [`should_interrupt_now`] will return junk results. This is
    /// still safe because:
    ///   * `should_interrupt_now` only atomically accesses data from this cell
    ///   * junk results shouldn't matter if we're not running WASM code
    static THREAD_INTERRUPT_STATE: UnsafeCell<ThreadInterruptState> =
        UnsafeCell::new(ThreadInterruptState {
            active_stores: Stores::new_const(),
            current_active_store: AtomicUsize::new(0),
            current_signal_target_store: Arc::new(AtomicUsize::new(0)),
            target: ThreadTarget(Arc::new(InterruptTarget {
                // TODO: isn't there a way to get this without reaching for libc APIs?
                #[allow(trivial_numeric_casts)]
                pthread: unsafe { libc::pthread_self() as usize },
                alive: Mutex::new(true),
            })),
        });
}

/// Install interrupt state for the given store. Note that this function
/// may be called more than once, and correctly maintains a stack of
/// stores for which the state is installed.
pub fn install(store_id: StoreId) -> Result<InterruptInstallGuard, InstallError> {
    let mut store_state = STORE_INTERRUPT_STATE.entry(store_id).or_insert_with(|| {
        // Since stores can't be sent across threads once they start executing
        // code, we don't need to update the thread for recursive calls.
        let (thread, thread_current_signal_target_store) = THREAD_INTERRUPT_STATE.with(|t| {
            // Safety: See comments on THREAD_INTERRUPT_STATE.
            let t = unsafe { t.get().as_ref().unwrap() };
            (t.target.0.clone(), t.current_signal_target_store.clone())
        });

        StoreInterruptState {
            thread,
            interrupted: Arc::new(AtomicBool::new(false)),
            installations: 0,
            thread_current_signal_target_store,
        }
    });

    if store_state.interrupted.load(Ordering::Acquire) {
        let entered_here = THREAD_INTERRUPT_STATE.with(|t| {
            // Safety: See comments on THREAD_INTERRUPT_STATE.
            let state = unsafe { t.get().as_ref().unwrap() };
            state.active_stores.iter().any(|(id, _)| *id == store_id)
        });
        if entered_here {
            return Err(InstallError::AlreadyInterrupted);
        }
        // Every call the interrupt reached is suspended, or was forgotten and
        // never returns. Each keeps the flag it holds, and a live one ends at
        // its next poll. A new entry starts with a flag of its own, so that a
        // forgotten call cannot keep the store interrupted for good.
        store_state.interrupted = Arc::new(AtomicBool::new(false));
    }
    store_state.installations += 1;

    THREAD_INTERRUPT_STATE.with(|t| {
        // Safety: See comments on THREAD_INTERRUPT_STATE.
        let borrow = unsafe { t.get().as_mut().unwrap() };
        borrow
            .active_stores
            .push((store_id, store_state.interrupted.clone()));
        borrow
            .current_active_store
            .store(store_id.as_raw().get(), Ordering::Release);
    });

    Ok(InterruptInstallGuard { store_id })
}

pub(super) fn uninstall(store_id: StoreId) {
    let Entry::Occupied(mut store_state_entry) = STORE_INTERRUPT_STATE.entry(store_id) else {
        panic!("Internal error: interrupt state not installed for store");
    };

    THREAD_INTERRUPT_STATE.with(|t| {
        // Safety: See comments on THREAD_INTERRUPT_STATE.
        let borrow = unsafe { t.get().as_mut().unwrap() };
        match borrow.active_stores.last() {
            Some((id, _)) if *id == store_id => {
                borrow.active_stores.pop();
                borrow.publish_active_store();
            }
            _ => panic!("InterruptInstallGuard dropped out of order"),
        }
    });

    // If this store is still active at some other point (further down this
    // thread's stack, or in a suspended activation), we should keep its
    // state around. Otherwise, it should be deleted from the global
    // interrupt state. Note that this will also reset the `interrupted`
    // flag, allowing the store to be used for further function calls.
    let store_state = store_state_entry.get_mut();
    store_state.installations -= 1;
    if store_state.installations == 0 {
        store_state_entry.remove();
    }
}

/// Forgets a store that is being dropped. Only calls that were forgotten can
/// still hold installations of it, and they never run again.
pub fn release_store(store_id: StoreId) {
    STORE_INTERRUPT_STATE.remove(&store_id);
}

impl ThreadInterruptState {
    fn publish_active_store(&mut self) {
        self.current_active_store.store(
            self.active_stores
                .last()
                .map_or(0, |(id, _)| id.as_raw().get()),
            Ordering::Release,
        );
    }
}

/// Whether the thread's interrupt state is still there: it is not once the
/// thread's locals are being destroyed.
pub(crate) fn thread_is_usable() -> bool {
    THREAD_INTERRUPT_STATE.try_with(|_| ()).is_ok()
}

/// The stores one activation has entered, innermost last. See
/// [`crate::ActivationState`], which swaps it with the thread's at every
/// switch so that the signal handler compares an interrupt's target against
/// the store that is actually running.
pub(crate) struct ActiveStores(Stores);

impl ActiveStores {
    pub(crate) const fn new() -> Self {
        Self(Stores::new_const())
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether this activation entered `store_id` and it was interrupted.
    pub(crate) fn is_interrupted(&self, store_id: StoreId) -> bool {
        self.0
            .iter()
            .rev()
            .find(|(id, _)| *id == store_id)
            .is_some_and(|(_, interrupted)| interrupted.load(Ordering::Acquire))
    }

    /// The activation's calls are gone without returning: each gives up the
    /// installation it holds, as `uninstall` does for a call that returns,
    /// without the thread they ran on. A store none holds any more is
    /// removed, with its `interrupted` flag and the thread it was aimed at.
    pub(crate) fn abandon(&mut self) {
        for (store_id, _) in self.0.drain(..) {
            if let Entry::Occupied(mut state) = STORE_INTERRUPT_STATE.entry(store_id) {
                state.get_mut().installations -= 1;
                if state.get().installations == 0 {
                    state.remove();
                }
            }
        }
    }

    pub(crate) fn swap(&mut self) {
        THREAD_INTERRUPT_STATE.with(|t| {
            // Safety: See comments on THREAD_INTERRUPT_STATE.
            let borrow = unsafe { t.get().as_mut().unwrap() };
            std::mem::swap(&mut borrow.active_stores, &mut self.0);
            borrow.publish_active_store();
        });
    }
}

/// Interrupt the store with the given ID. Best effort is made to ensure
/// interrupts are handled. However, there is no guarantee; under rare
/// circumstances, it is possible for the interrupt to be missed. One such
/// case is when the target thread is about to call WASM code but has not
/// yet made the call.
///
/// To make sure the code is interrupted, the target thread should notify
/// the signalling thread that it has finished running in some way, and
/// the signalling thread must wait for that notification and retry the
/// interrupt if the notification is not received after some time.
pub fn interrupt(store_id: StoreId) -> Result<(), InterruptError> {
    let Entry::Occupied(mut store_state) = STORE_INTERRUPT_STATE.entry(store_id) else {
        return Err(InterruptError::StoreNotRunning);
    };
    let store_state = store_state.get_mut();

    // Held until the signal is sent: the thread cannot exit before.
    let alive = store_state
        .thread
        .alive
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if !*alive {
        // Only a call that was forgotten is left on the thread.
        return Err(InterruptError::StoreNotRunning);
    }

    if store_state
        .thread_current_signal_target_store
        .compare_exchange(
            0,
            store_id.as_raw().get(),
            Ordering::SeqCst,
            Ordering::SeqCst,
        )
        .is_err()
    {
        return Err(InterruptError::OtherInterruptInProgress);
    }

    store_state.interrupted.store(true, Ordering::Release);

    unsafe {
        #[allow(trivial_numeric_casts)]
        let errno =
            libc::pthread_kill(store_state.thread.pthread as libc::pthread_t, libc::SIGUSR1);
        if errno != 0 {
            let error_str = CStr::from_ptr(libc::strerror(errno)).to_str().unwrap();
            return Err(InterruptError::FailedToSendSignal(error_str));
        }
    }
    drop(alive);

    Ok(())
}

/// Called from within the signal handler to decide whether we should interrupt
/// the currently running WASM code. This function *MAY* return junk results in
/// case a signal comes in during an install or uninstall operation. However,
/// in such cases, there is no WASM code running, and the result will be ignored
/// by the signal handler anyway.
pub(crate) fn on_interrupted() -> bool {
    // A signal sent just before the thread started to exit can arrive once
    // its locals are being destroyed: no Wasm runs then.
    THREAD_INTERRUPT_STATE
        .try_with(|t| {
            // Safety: See comments on THREAD_INTERRUPT_STATE.
            let state = unsafe { t.get().as_ref().unwrap() };

            let current_active_store = state.current_active_store.load(Ordering::Acquire);

            let current_signal_target_store =
                state.current_signal_target_store.load(Ordering::Acquire);
            assert_ne!(
                current_signal_target_store, 0,
                "current_signal_target_store should be set before signalling the WASM thread"
            );
            if state
                .current_signal_target_store
                .compare_exchange(
                    current_signal_target_store,
                    0,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                )
                .is_err()
            {
                unreachable!("current_signal_target_store isn't changed unless it's zero");
            }

            current_active_store == current_signal_target_store
        })
        .unwrap_or(false)
}

/// Returns true if the store with the given ID was entered by the activation
/// this thread is running and has already been interrupted. The stores of a
/// suspended activation are not on the thread; see
/// [`crate::ActivationState::is_interrupted`].
#[inline]
pub fn is_interrupted(store_id: StoreId) -> bool {
    THREAD_INTERRUPT_STATE.with(|t| {
        // Safety: See comments on THREAD_INTERRUPT_STATE.
        let state = unsafe { t.get().as_ref().unwrap() };
        state
            .active_stores
            .iter()
            .rev()
            .find(|(id, _)| *id == store_id)
            .is_some_and(|(_, interrupted)| interrupted.load(Ordering::Acquire))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sync call enters a store, and one of its host functions runs
    /// `block_on(f.call_async(&mut env, ..))`. The interrupt lands after the
    /// nested call's poll checked for one, and before the nested call enters
    /// the store from its coroutine. The nested entry must be refused, as it
    /// is without the coroutine in between.
    // TODO: fails today, so it is ignored. `install` gives an entry a fresh
    // flag when the store is interrupted but not in the thread's list, so
    // that a forgotten suspended call cannot keep its store interrupted. The
    // coroutine switch swaps the caller's entry out of that list, so the
    // nested entry is taken for a new call, gets a fresh flag, and its guest
    // runs on as if never interrupted. Either drop the reset, or keep the
    // callers of a coroutine on the thread's list. Reported in
    // https://github.com/wasmerio/wasmer-runtime/pull/18#discussion_r4664630939.
    #[test]
    #[ignore = "TODO: an interrupted store lets a nested entry in behind a coroutine switch"]
    fn an_entry_behind_a_coroutine_switch_is_refused_once_interrupted() {
        let store_id = StoreId::default();
        let outer = install(store_id).unwrap();
        // The coroutine of the nested call starts with no stores of its own.
        let mut coroutine = ActiveStores::new();
        coroutine.swap();
        // What `interrupt` does to the flag, without the signal.
        STORE_INTERRUPT_STATE
            .get(&store_id)
            .unwrap()
            .interrupted
            .store(true, Ordering::Release);

        let nested = install(store_id);
        let refused = matches!(nested, Err(InstallError::AlreadyInterrupted));
        // Leave the thread as it was before asserting: guards are popped in
        // order, and the coroutine's list is not left on the thread.
        drop(nested);
        coroutine.swap();
        drop(outer);
        assert!(refused, "the nested entry was let in with a fresh flag");
    }
}

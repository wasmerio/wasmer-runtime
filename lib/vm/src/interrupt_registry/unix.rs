use std::sync::{
    Arc, LazyLock, Weak,
    atomic::{AtomicBool, Ordering},
};

use dashmap::{DashMap, Entry};
use wasmer_types::StoreId;

use super::*;
use crate::StoreObjects;

struct StoreInterruptState {
    /// A forgotten call can outlive its thread but cannot resume once it exits.
    thread: Weak<()>,
    interrupted: Arc<AtomicBool>,
    /// Includes entries on suspended activations.
    installations: usize,
}

static STORE_INTERRUPT_STATE: LazyLock<DashMap<StoreId, StoreInterruptState>> =
    LazyLock::new(Default::default);

thread_local! {
    static THREAD_LIFETIME: Arc<()> = Arc::new(());
}

/// Register a store entry unless the store is already interrupted.
pub fn install(objects: &StoreObjects) -> Result<InterruptInstallGuard, InstallError> {
    let store_id = objects.id();
    let mut state = STORE_INTERRUPT_STATE
        .entry(store_id)
        .or_insert_with(|| StoreInterruptState {
            thread: THREAD_LIFETIME.with(Arc::downgrade),
            interrupted: objects.interrupted.clone(),
            installations: 0,
        });
    if state.interrupted.load(Ordering::Acquire) {
        return Err(InstallError::AlreadyInterrupted);
    }
    state.installations += 1;
    Ok(InterruptInstallGuard { store_id })
}

pub(super) fn uninstall(store_id: StoreId) {
    if let Entry::Occupied(mut state) = STORE_INTERRUPT_STATE.entry(store_id) {
        state.get_mut().installations -= 1;
        if state.get().installations == 0 {
            state.get().interrupted.store(false, Ordering::Release);
            state.remove();
        }
    }
}

/// Remove registrations left by forgotten calls when their store is dropped.
pub fn release_store(store_id: StoreId) {
    STORE_INTERRUPT_STATE.remove(&store_id);
}

/// Request an interrupt for all installed entries. New entries are also
/// refused until the last installed entry exits.
pub fn interrupt(store_id: StoreId) -> Result<(), InterruptError> {
    let state = STORE_INTERRUPT_STATE
        .get(&store_id)
        .ok_or(InterruptError::StoreNotRunning)?;
    if state.thread.strong_count() == 0 {
        return Err(InterruptError::StoreNotRunning);
    }
    state.interrupted.store(true, Ordering::Release);
    Ok(())
}

/// Whether the store is currently interrupted.
pub fn is_interrupted(store_id: StoreId) -> bool {
    STORE_INTERRUPT_STATE
        .get(&store_id)
        .is_some_and(|state| state.interrupted.load(Ordering::Acquire))
}

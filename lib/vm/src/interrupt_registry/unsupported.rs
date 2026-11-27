use wasmer_types::StoreId;

use super::*;

/// Install interrupt state for the given store.
///
/// On unsupported platforms this is a no-op.
pub fn install(objects: &crate::StoreObjects) -> Result<InterruptInstallGuard, InstallError> {
    Ok(InterruptInstallGuard {
        store_id: objects.id(),
    })
}

pub(super) fn uninstall(_store_id: StoreId) {}

/// On unsupported platforms no store is ever registered.
pub fn release_store(_store_id: StoreId) {}

/// Interrupt the given store.
///
/// On unsupported platforms this is a no-op.
pub fn interrupt(_store_id: StoreId) -> Result<(), InterruptError> {
    Ok(())
}

/// Returns whether the given store has been interrupted.
///
/// On unsupported platforms interrupts are not tracked.
pub fn is_interrupted(_store_id: StoreId) -> bool {
    false
}

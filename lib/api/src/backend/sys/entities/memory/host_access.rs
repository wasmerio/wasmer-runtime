use std::marker::PhantomData;

use wasmer_vm::{HostMemoryMapping, LinearMemory, MemoryError};

use super::MemoryBuffer;
use crate::{
    BackendMemory, MemoryAccessError, entities::store::AsStoreRef, vm::VMSharedMemory,
};

/// Reads and writes a memory from host code that cannot reach its store.
///
/// A [`MemoryView`](crate::MemoryView) borrows the store of its memory. That
/// is out of reach for host code that serves one guest and has to look into
/// the memory of another: the other store may be borrowed by a call further
/// down the stack, or be running on another thread. This is what an embedder
/// that runs guests as processes needs for `ptrace` peek and poke,
/// `process_vm_readv` and `process_vm_writev`, `/proc/<pid>/mem`, or the core
/// dump of another guest.
///
/// The handle is `Send`, `Sync` and `Clone`, and works for shared and
/// non-shared memories. Get one with
/// [`Memory::host_memory_access`](crate::Memory::host_memory_access) or
/// [`SharedMemory::host_memory_access`](crate::SharedMemory::host_memory_access).
///
/// # What an access can observe
///
/// - The handle does not keep the memory alive. Once the memory is dropped,
///   with its store or with the last handle of a shared memory, every method
///   returns [`MemoryAccessError::MemoryDropped`].
/// - Each call checks its range against the size the memory has at that
///   moment, so it follows `memory.grow`, including one that moves the memory
///   to another address.
/// - A call excludes `memory.grow` while it copies, and `memory.grow` excludes
///   the calls. A grow therefore waits for the copies in progress: copy a
///   large range, such as a core dump, in chunks.
/// - Nothing excludes the guest. If it runs while the host reads, the bytes
///   read may mix states from before and after a guest write. If it runs
///   while the host writes, it sees the new bytes at some later point, and
///   the guest of a non-shared memory may go on using values it has already
///   loaded. This is what `process_vm_readv` and `process_vm_writev` give for
///   a process that is running, and it never breaks the host or the sandbox.
///   Stop the guest first, as `ptrace` does, to see or set a consistent state.
///
/// # Example
///
/// ```
/// # use wasmer::{Memory, MemoryAccessError, MemoryType, Store};
/// # let mut store = Store::default();
/// let memory = Memory::new(&mut store, MemoryType::new(1, None, false)).unwrap();
/// let access = memory.host_memory_access(&store).unwrap();
///
/// // From here on no store is needed, on any thread.
/// std::thread::spawn({
///     let access = access.clone();
///     move || {
///         access.write(8, b"poke").unwrap();
///
///         let mut peek = [0; 4];
///         access.read(8, &mut peek).unwrap();
///         assert_eq!(&peek, b"poke");
///     }
/// })
/// .join()
/// .unwrap();
///
/// assert_eq!(memory.view(&store).read_u8(8).unwrap(), b'p');
///
/// drop(store);
/// assert!(matches!(
///     access.data_size(),
///     Err(MemoryAccessError::MemoryDropped)
/// ));
/// ```
#[derive(Debug, Clone)]
pub struct HostMemoryAccess {
    mapping: HostMemoryMapping,
}

impl HostMemoryAccess {
    fn new(mapping: HostMemoryMapping) -> Self {
        Self { mapping }
    }

    /// Returns the size (in bytes) the memory has right now.
    ///
    /// The memory can grow before the next call, and
    /// [`Memory::reset`](crate::Memory::reset) can shrink it.
    pub fn data_size(&self) -> Result<u64, MemoryAccessError> {
        self.with_buffer(|buffer| buffer.len as u64)
    }

    /// Reads bytes from the memory at the given offset.
    ///
    /// The full buffer is filled, otherwise a [`MemoryAccessError`] says
    /// whether the range is outside the memory or the memory was dropped.
    ///
    /// This method is safe (from the host side) in the face of concurrent
    /// writes, also by a guest that is running.
    pub fn read(&self, offset: u64, buf: &mut [u8]) -> Result<(), MemoryAccessError> {
        self.with_buffer(|buffer| buffer.read(offset, buf))?
    }

    /// Writes bytes to the memory at the given offset.
    ///
    /// Nothing is written if the range is outside the memory or the memory
    /// was dropped, and a [`MemoryAccessError`] says which.
    ///
    /// This method is safe (from the host side) in the face of concurrent
    /// reads and writes, also by a guest that is running.
    pub fn write(&self, offset: u64, data: &[u8]) -> Result<(), MemoryAccessError> {
        self.with_buffer(|buffer| buffer.write(offset, data))?
    }

    /// Runs `f` with the bytes the memory has right now.
    fn with_buffer<R>(
        &self,
        f: impl FnOnce(MemoryBuffer<'_>) -> R,
    ) -> Result<R, MemoryAccessError> {
        // `MemoryBuffer` trusts its base and length. The mapping keeps both in
        // place until the closure returns, and the buffer cannot leave it.
        let with_bytes = |base, len| {
            f(MemoryBuffer {
                base,
                len,
                marker: PhantomData,
            })
        };
        self.mapping
            .with_bytes(with_bytes)
            .ok_or(MemoryAccessError::MemoryDropped)
    }
}

impl crate::Memory {
    /// Returns a handle that reads and writes this memory without its store,
    /// on any thread. See [`HostMemoryAccess`] for what it can observe.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::UnsupportedOperation`] for a memory of another
    /// backend, and for a custom memory that has no
    /// [`LinearMemory::host_mapping`].
    pub fn host_memory_access(
        &self,
        store: &impl AsStoreRef,
    ) -> Result<HostMemoryAccess, MemoryError> {
        match &self.0 {
            BackendMemory::Sys(memory) => memory
                .handle
                .get(store.as_store_ref().objects().as_sys())
                .host_mapping()
                .map(HostMemoryAccess::new),
            #[allow(unreachable_patterns)]
            _ => Err(MemoryError::UnsupportedOperation {
                message: "host memory access is unsupported by this backend".into(),
            }),
        }
    }
}

impl crate::SharedMemory {
    /// Returns a handle that reads and writes this memory without a store,
    /// on any thread, and that does not keep the memory alive. See
    /// [`HostMemoryAccess`] for what it can observe.
    ///
    /// # Errors
    ///
    /// Returns [`MemoryError::UnsupportedOperation`] for a memory of another
    /// backend.
    pub fn host_memory_access(&self) -> Result<HostMemoryAccess, MemoryError> {
        match &self.memory {
            VMSharedMemory::Sys(memory) => memory.host_mapping().map(HostMemoryAccess::new),
            #[allow(unreachable_patterns)]
            _ => Err(MemoryError::UnsupportedOperation {
                message: "host memory access is unsupported by this backend".into(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn ensure_host_memory_access_is_send_and_sync() {
        fn assert_send<T: Send>() {}
        fn assert_sync<T: Sync>() {}
        assert_send::<super::HostMemoryAccess>();
        assert_sync::<super::HostMemoryAccess>();
    }
}

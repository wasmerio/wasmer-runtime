// This file contains code from external sources.
// Attributions: https://github.com/wasmerio/wasmer/blob/main/docs/ATTRIBUTIONS.md

//! Memory management for linear memories.
//!
//! `Memory` is to WebAssembly linear memories what `Table` is to WebAssembly tables.

use crate::threadconditions::ThreadConditions;
pub use crate::threadconditions::{NotifyLocation, WaiterError};
use crate::trap::Trap;
use crate::{
    mmap::{Mmap, MmapType},
    store::MaybeInstanceOwned,
    threadconditions::ExpectedValue,
    vmcontext::VMMemoryDefinition,
};
use more_asserts::assert_ge;
use std::cell::UnsafeCell;
use std::convert::TryInto;
use std::ptr::NonNull;
use std::slice;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use wasmer_types::{Bytes, MemoryError, MemoryStyle, MemoryType, Pages, WASM_PAGE_SIZE};

// The memory mapped area
#[derive(Debug)]
struct WasmMmap {
    // Our OS allocation of mmap'd memory.
    alloc: Mmap,
    // The current logical size in wasm pages of this linear memory.
    size: Pages,
    /// The owned memory definition used by the generated code
    vm_memory_definition: MaybeInstanceOwned<VMMemoryDefinition>,
    // Where host code that cannot reach the store finds the bytes above.
    host_mapping: HostMemoryMapping,
}

/// # SAFETY: Not safe by rust standards, since guest code may do weird things
/// with its memory. However, this is still safe to send across threads as
/// far as the WASM spec is concerned.
unsafe impl Send for WasmMmap {}
/// # SAFETY: see above.
unsafe impl Sync for WasmMmap {}

impl WasmMmap {
    fn get_vm_memory_definition(&self) -> NonNull<VMMemoryDefinition> {
        self.vm_memory_definition.as_ptr()
    }

    fn size(&self) -> Pages {
        unsafe {
            let md_ptr = self.get_vm_memory_definition();
            let md = md_ptr.as_ref();
            Bytes::from(md.current_length).try_into().unwrap()
        }
    }

    fn grow(&mut self, delta: Pages, conf: VMMemoryConfig) -> Result<Pages, MemoryError> {
        // Optimization of memory.grow 0 calls.
        if delta.0 == 0 {
            return Ok(self.size);
        }

        let new_pages = self
            .size
            .checked_add(delta)
            .ok_or(MemoryError::CouldNotGrow {
                current: self.size,
                attempted_delta: delta,
            })?;
        let prev_pages = self.size;

        if let Some(maximum) = conf.maximum
            && new_pages > maximum
        {
            return Err(MemoryError::CouldNotGrow {
                current: self.size,
                attempted_delta: delta,
            });
        }

        // Wasm linear memories are never allowed to grow beyond what is
        // indexable. If the memory has no maximum, enforce the greatest
        // limit here.
        if new_pages > Pages::max_value() {
            // Linear memory size would exceed the index range.
            return Err(MemoryError::CouldNotGrow {
                current: self.size,
                attempted_delta: delta,
            });
        }

        let delta_bytes = delta.bytes().0;
        let prev_bytes = prev_pages.bytes().0;
        let new_bytes = new_pages.bytes().0;

        // A host access holds this lock while it copies, so none is inside the
        // mapping while it moves, and none writes to bytes already copied out.
        let mut mapped = self.host_mapping.lock_for_change();

        if new_bytes > self.alloc.len() - conf.offset_guard_size {
            // If the new size is within the declared maximum, but needs more memory than we
            // have on hand, it's a dynamic heap and it can move.
            let guard_bytes = conf.offset_guard_size;
            let request_bytes =
                new_bytes
                    .checked_add(guard_bytes)
                    .ok_or_else(|| MemoryError::CouldNotGrow {
                        current: new_pages,
                        attempted_delta: Bytes(guard_bytes).try_into().unwrap(),
                    })?;

            let mut new_mmap =
                Mmap::accessible_reserved(new_bytes, request_bytes, None, MmapType::Private, true)
                    .map_err(MemoryError::Region)?;

            let copy_len = self.alloc.len() - conf.offset_guard_size;
            new_mmap.as_mut_slice()[..copy_len].copy_from_slice(&self.alloc.as_slice()[..copy_len]);

            // The old mapping is released below. Forget it first: this lock
            // is not poisoned by a panic, which would leave it recorded.
            *mapped = None;
            self.alloc = new_mmap;
        } else if delta_bytes > 0 {
            // Make the newly allocated pages accessible.
            self.alloc
                .make_accessible(prev_bytes, delta_bytes)
                .map_err(MemoryError::Region)?;
        }

        self.size = new_pages;

        // update memory definition
        unsafe {
            let mut md_ptr = self.vm_memory_definition.as_ptr();
            let md = md_ptr.as_mut();
            md.current_length = new_pages.bytes().0;
            md.base = self.alloc.as_mut_ptr() as _;
        }
        *mapped = Some(MappedBytes::new(&self.alloc, new_bytes));

        Ok(prev_pages)
    }

    /// Grows the memory to at least a minimum size. If the memory is already big enough
    /// for the min size then this function does nothing
    fn grow_at_least(&mut self, min_size: u64, conf: VMMemoryConfig) -> Result<(), MemoryError> {
        let cur_size = self.size.bytes().0 as u64;
        if cur_size < min_size {
            let growth = min_size - cur_size;
            let growth_pages = ((growth - 1) / WASM_PAGE_SIZE as u64) + 1;
            self.grow(Pages(growth_pages as u32), conf)?;
        }

        Ok(())
    }

    /// Resets the memory down to a zero size
    fn reset(&mut self) -> Result<(), MemoryError> {
        let mut mapped = self.host_mapping.lock_for_change();
        self.size.0 = 0;
        // update memory definition
        unsafe {
            let mut md_ptr = self.vm_memory_definition.as_ptr();
            let md = md_ptr.as_mut();
            md.current_length = 0;
        }
        *mapped = Some(MappedBytes::new(&self.alloc, 0));
        Ok(())
    }

    /// Copies the memory
    /// (in this case it performs a copy-on-write to save memory)
    pub fn copy(&self) -> Result<Self, MemoryError> {
        let mem_length = self.size.bytes().0;
        let mut alloc = self
            .alloc
            .copy(Some(mem_length))
            .map_err(MemoryError::Generic)?;
        let base_ptr = alloc.as_mut_ptr();
        Ok(Self {
            vm_memory_definition: MaybeInstanceOwned::Host(Box::new(UnsafeCell::new(
                VMMemoryDefinition {
                    base: base_ptr,
                    current_length: mem_length,
                },
            ))),
            host_mapping: HostMemoryMapping::new(MappedBytes::new(&alloc, mem_length)),
            alloc,
            size: self.size,
        })
    }
}

impl Drop for WasmMmap {
    fn drop(&mut self) {
        // The mapping is released once this returns. Waiting for the lock lets
        // the accesses in progress finish, and no later one finds the bytes.
        *self.host_mapping.lock_for_change() = None;
    }
}

/// The host bytes of a linear memory: what generated code reads from its
/// `VMMemoryDefinition`.
#[derive(Debug, Clone, Copy)]
struct MappedBytes {
    // A `usize`, as in `Mmap`, so that a handle is `Send` and `Sync`.
    base: usize,
    len: usize,
}

impl MappedBytes {
    fn new(alloc: &Mmap, len: usize) -> Self {
        Self {
            base: alloc.as_ptr() as usize,
            len,
        }
    }
}

/// Where a linear memory is mapped in the host, for code that cannot reach
/// the store that owns the memory.
///
/// A store is the only way to a non-shared memory, and a store can be busy on
/// another thread or borrowed further down the stack. This handle is not tied
/// to it: it is `Send`, `Sync` and `Clone`, it does not keep the memory alive,
/// and it follows the memory when [`LinearMemory::grow`] moves it.
///
/// Get one with [`LinearMemory::host_mapping`].
#[derive(Debug, Clone)]
pub struct HostMemoryMapping {
    // `None` once the memory is dropped. An access holds the read side for as
    // long as it uses the bytes; whatever moves, resizes or unmaps them holds
    // the write side. Generated code takes neither.
    //
    // This lock keeps new readers out as soon as a writer waits. The one of
    // `std` lets readers that keep coming overtake it on some platforms, so a
    // host that polls a memory would stall its `memory.grow`.
    bytes: Arc<parking_lot::RwLock<Option<MappedBytes>>>,
}

impl HostMemoryMapping {
    fn new(bytes: MappedBytes) -> Self {
        Self {
            bytes: Arc::new(parking_lot::RwLock::new(Some(bytes))),
        }
    }

    /// Runs `f` with the address and the length in bytes of the memory, or
    /// returns `None` if the memory was dropped.
    ///
    /// The bytes stay mapped, at that address and with that length, until `f`
    /// returns: growing, resetting and dropping the memory wait for it. So `f`
    /// must be short, and must neither do any of the three nor use a handle of
    /// this memory again, which could deadlock.
    ///
    /// The guest, and other host code, may read and write the same bytes while
    /// `f` runs. Access them as raw, racing memory, with volatile reads and
    /// writes, and never through a `&[u8]` or `&mut [u8]`.
    pub fn with_bytes<R>(&self, f: impl FnOnce(*mut u8, usize) -> R) -> Option<R> {
        let bytes = self.bytes.read();
        (*bytes).map(|bytes| f(bytes.base as *mut u8, bytes.len))
    }

    /// Waits for the accesses in progress, and keeps new ones out until the
    /// returned guard is dropped.
    fn lock_for_change(&self) -> parking_lot::RwLockWriteGuard<'_, Option<MappedBytes>> {
        self.bytes.write()
    }
}

/// A linear memory instance.
#[derive(Debug, Clone)]
struct VMMemoryConfig {
    // The optional maximum size in wasm pages of this linear memory.
    maximum: Option<Pages>,
    /// The WebAssembly linear memory description.
    memory: MemoryType,
    /// Our chosen implementation style.
    style: MemoryStyle,
    // Size in bytes of extra guard pages after the end to optimize loads and stores with
    // constant offsets.
    offset_guard_size: usize,
}

impl VMMemoryConfig {
    fn ty(&self, minimum: Pages) -> MemoryType {
        let mut out = self.memory;
        out.minimum = minimum;

        out
    }

    fn style(&self) -> MemoryStyle {
        self.style
    }
}

/// A linear memory instance.
#[derive(Debug)]
pub struct VMOwnedMemory {
    // The underlying allocation.
    mmap: WasmMmap,
    // Configuration of this memory
    config: VMMemoryConfig,
}

unsafe impl Send for VMOwnedMemory {}
unsafe impl Sync for VMOwnedMemory {}

impl VMOwnedMemory {
    /// Create a new linear memory instance with specified minimum and maximum number of wasm pages.
    ///
    /// This creates a `Memory` with owned metadata: this can be used to create a memory
    /// that will be imported into Wasm modules.
    pub fn new(memory: &MemoryType, style: &MemoryStyle) -> Result<Self, MemoryError> {
        unsafe { Self::new_internal(memory, style, None, None, MmapType::Private) }
    }

    /// Create a new linear memory instance with specified minimum and maximum number of wasm pages
    /// that is backed by a memory file. When set to private the file will be remaining in memory and
    /// never flush to disk, when set to shared the memory will be flushed to disk.
    ///
    /// This creates a `Memory` with owned metadata: this can be used to create a memory
    /// that will be imported into Wasm modules.
    pub fn new_with_file(
        memory: &MemoryType,
        style: &MemoryStyle,
        backing_file: std::path::PathBuf,
        memory_type: MmapType,
    ) -> Result<Self, MemoryError> {
        unsafe { Self::new_internal(memory, style, None, Some(backing_file), memory_type) }
    }

    /// Create a new linear memory instance with specified minimum and maximum number of wasm pages.
    ///
    /// This creates a `Memory` with metadata owned by a VM, pointed to by
    /// `vm_memory_location`: this can be used to create a local memory.
    ///
    /// # Safety
    /// - `vm_memory_location` must point to a valid location in VM memory.
    pub unsafe fn from_definition(
        memory: &MemoryType,
        style: &MemoryStyle,
        vm_memory_location: NonNull<VMMemoryDefinition>,
    ) -> Result<Self, MemoryError> {
        unsafe {
            Self::new_internal(
                memory,
                style,
                Some(vm_memory_location),
                None,
                MmapType::Private,
            )
        }
    }

    /// Create a new linear memory instance with specified minimum and maximum number of wasm pages
    /// that is backed by a file. When set to private the file will be remaining in memory and
    /// never flush to disk, when set to shared the memory will be flushed to disk.
    ///
    /// This creates a `Memory` with metadata owned by a VM, pointed to by
    /// `vm_memory_location`: this can be used to create a local memory.
    ///
    /// # Safety
    /// - `vm_memory_location` must point to a valid location in VM memory.
    pub unsafe fn from_definition_with_file(
        memory: &MemoryType,
        style: &MemoryStyle,
        vm_memory_location: NonNull<VMMemoryDefinition>,
        backing_file: Option<std::path::PathBuf>,
        memory_type: MmapType,
    ) -> Result<Self, MemoryError> {
        unsafe {
            Self::new_internal(
                memory,
                style,
                Some(vm_memory_location),
                backing_file,
                memory_type,
            )
        }
    }

    /// Build a `Memory` with either self-owned or VM owned metadata.
    unsafe fn new_internal(
        memory: &MemoryType,
        style: &MemoryStyle,
        vm_memory_location: Option<NonNull<VMMemoryDefinition>>,
        backing_file: Option<std::path::PathBuf>,
        memory_type: MmapType,
    ) -> Result<Self, MemoryError> {
        unsafe {
            if memory.minimum > Pages::max_value() {
                return Err(MemoryError::MinimumMemoryTooLarge {
                    min_requested: memory.minimum,
                    max_allowed: Pages::max_value(),
                });
            }
            // `maximum` cannot be set to more than `65536` pages.
            if let Some(max) = memory.maximum {
                if max > Pages::max_value() {
                    return Err(MemoryError::MaximumMemoryTooLarge {
                        max_requested: max,
                        max_allowed: Pages::max_value(),
                    });
                }
                if max < memory.minimum {
                    return Err(MemoryError::InvalidMemory {
                        reason: format!(
                            "the maximum ({} pages) is less than the minimum ({} pages)",
                            max.0, memory.minimum.0
                        ),
                    });
                }
            }

            let offset_guard_bytes = usize::try_from(style.offset_guard_size()).map_err(|e| {
                MemoryError::Generic(format!("cannot install memory guard page: {e}"))
            })?;

            let minimum_pages = match style {
                MemoryStyle::Dynamic { .. } => memory.minimum,
                MemoryStyle::Static => {
                    let bound = MemoryStyle::static_bound();
                    assert_ge!(bound, memory.minimum);
                    bound
                }
            };
            let minimum_bytes = minimum_pages.bytes().0;
            let request_bytes = minimum_bytes.checked_add(offset_guard_bytes).unwrap();
            let mapped_pages = memory.minimum;
            let mapped_bytes = mapped_pages.bytes();

            let mut alloc = Mmap::accessible_reserved(
                mapped_bytes.0,
                request_bytes,
                backing_file,
                memory_type,
                true,
            )
            .map_err(MemoryError::Region)?;

            let base_ptr = alloc.as_mut_ptr();
            let mem_length = memory
                .minimum
                .bytes()
                .0
                .max(alloc.as_slice_accessible().len());
            let mmap = WasmMmap {
                vm_memory_definition: if let Some(mem_loc) = vm_memory_location {
                    {
                        let mut ptr = mem_loc;
                        let md = ptr.as_mut();
                        md.base = base_ptr;
                        md.current_length = mem_length;
                    }
                    MaybeInstanceOwned::Instance(mem_loc)
                } else {
                    MaybeInstanceOwned::Host(Box::new(UnsafeCell::new(VMMemoryDefinition {
                        base: base_ptr,
                        current_length: mem_length,
                    })))
                },
                host_mapping: HostMemoryMapping::new(MappedBytes::new(&alloc, mem_length)),
                alloc,
                size: Bytes::from(mem_length).try_into().unwrap(),
            };

            Ok(Self {
                mmap,
                config: VMMemoryConfig {
                    maximum: memory.maximum,
                    offset_guard_size: offset_guard_bytes,
                    memory: *memory,
                    style: *style,
                },
            })
        }
    }

    /// Converts this owned memory into shared memory
    pub fn to_shared(self) -> VMSharedMemory {
        VMSharedMemory {
            mmap: Arc::new(RwLock::new(self.mmap)),
            config: self.config,
            conditions: ThreadConditions::new(),
        }
    }

    /// Copies this memory to a new memory
    pub fn copy(&self) -> Result<Self, MemoryError> {
        Ok(Self {
            mmap: self.mmap.copy()?,
            config: self.config.clone(),
        })
    }
}

// TODO: why doesn't this support wait/notify? wait should block indefinitely if you ask me
impl LinearMemory for VMOwnedMemory {
    /// Returns the type for this memory.
    fn ty(&self) -> MemoryType {
        let minimum = self.mmap.size();
        self.config.ty(minimum)
    }

    /// Returns the size of the memory in pages
    fn size(&self) -> Pages {
        self.mmap.size()
    }

    /// Returns the memory style for this memory.
    fn style(&self) -> MemoryStyle {
        self.config.style()
    }

    /// Grow memory by the specified amount of wasm pages.
    ///
    /// Returns `None` if memory can't be grown by the specified amount
    /// of wasm pages.
    fn grow(&mut self, delta: Pages) -> Result<Pages, MemoryError> {
        self.mmap.grow(delta, self.config.clone())
    }

    /// Grows the memory to at least a minimum size. If the memory is already big enough
    /// for the min size then this function does nothing
    fn grow_at_least(&mut self, min_size: u64) -> Result<(), MemoryError> {
        self.mmap.grow_at_least(min_size, self.config.clone())
    }

    /// Resets the memory down to a zero size
    fn reset(&mut self) -> Result<(), MemoryError> {
        self.mmap.reset()?;
        Ok(())
    }

    /// Return a `VMMemoryDefinition` for exposing the memory to compiled wasm code.
    fn vmmemory(&self) -> NonNull<VMMemoryDefinition> {
        self.mmap.vm_memory_definition.as_ptr()
    }

    /// Owned memory can not be cloned (this will always return None)
    fn try_clone(&self) -> Result<Box<dyn LinearMemory + Send + Sync + 'static>, MemoryError> {
        Err(MemoryError::MemoryNotShared)
    }

    /// Copies this memory to a new memory
    fn copy(&self) -> Result<Box<dyn LinearMemory + Send + Sync + 'static>, MemoryError> {
        let forked = Self::copy(self)?;
        Ok(Box::new(forked))
    }

    /// Return a concrete shared memory handle for detached API sharing.
    fn as_shared(&self) -> Result<VMSharedMemory, MemoryError> {
        Err(MemoryError::MemoryNotShared)
    }

    fn host_mapping(&self) -> Result<HostMemoryMapping, MemoryError> {
        Ok(self.mmap.host_mapping.clone())
    }
}

/// A shared linear memory instance.
#[derive(Debug, Clone)]
pub struct VMSharedMemory {
    // The underlying allocation.
    mmap: Arc<RwLock<WasmMmap>>,
    // Configuration of this memory
    config: VMMemoryConfig,
    // waiters list for this memory
    conditions: ThreadConditions,
}

impl VMSharedMemory {
    /// Create a new linear memory instance with specified minimum and maximum number of wasm pages.
    ///
    /// This creates a `Memory` with owned metadata: this can be used to create a memory
    /// that will be imported into Wasm modules.
    pub fn new(memory: &MemoryType, style: &MemoryStyle) -> Result<Self, MemoryError> {
        Ok(VMOwnedMemory::new(memory, style)?.to_shared())
    }

    /// Create a new linear memory instance with specified minimum and maximum number of wasm pages
    /// that is backed by a file. When set to private the file will be remaining in memory and
    /// never flush to disk, when set to shared the memory will be flushed to disk.
    ///
    /// This creates a `Memory` with owned metadata: this can be used to create a memory
    /// that will be imported into Wasm modules.
    pub fn new_with_file(
        memory: &MemoryType,
        style: &MemoryStyle,
        backing_file: std::path::PathBuf,
        memory_type: MmapType,
    ) -> Result<Self, MemoryError> {
        Ok(VMOwnedMemory::new_with_file(memory, style, backing_file, memory_type)?.to_shared())
    }

    /// Create a new linear memory instance with specified minimum and maximum number of wasm pages.
    ///
    /// This creates a `Memory` with metadata owned by a VM, pointed to by
    /// `vm_memory_location`: this can be used to create a local memory.
    ///
    /// # Safety
    /// - `vm_memory_location` must point to a valid location in VM memory.
    pub unsafe fn from_definition(
        memory: &MemoryType,
        style: &MemoryStyle,
        vm_memory_location: NonNull<VMMemoryDefinition>,
    ) -> Result<Self, MemoryError> {
        unsafe {
            Ok(VMOwnedMemory::from_definition(memory, style, vm_memory_location)?.to_shared())
        }
    }

    /// Create a new linear memory instance with specified minimum and maximum number of wasm pages
    /// that is backed by a file. When set to private the file will be remaining in memory and
    /// never flush to disk, when set to shared the memory will be flushed to disk.
    ///
    /// This creates a `Memory` with metadata owned by a VM, pointed to by
    /// `vm_memory_location`: this can be used to create a local memory.
    ///
    /// # Safety
    /// - `vm_memory_location` must point to a valid location in VM memory.
    pub unsafe fn from_definition_with_file(
        memory: &MemoryType,
        style: &MemoryStyle,
        vm_memory_location: NonNull<VMMemoryDefinition>,
        backing_file: Option<std::path::PathBuf>,
        memory_type: MmapType,
    ) -> Result<Self, MemoryError> {
        unsafe {
            Ok(VMOwnedMemory::from_definition_with_file(
                memory,
                style,
                vm_memory_location,
                backing_file,
                memory_type,
            )?
            .to_shared())
        }
    }

    /// Copies this memory to a new memory
    pub fn copy(&self) -> Result<Self, MemoryError> {
        let guard = self.mmap.read().unwrap();
        Ok(Self {
            mmap: Arc::new(RwLock::new(guard.copy()?)),
            config: self.config.clone(),
            conditions: ThreadConditions::new(),
        })
    }
}

impl LinearMemory for VMSharedMemory {
    /// Returns the type for this memory.
    fn ty(&self) -> MemoryType {
        let minimum = {
            let guard = self.mmap.read().unwrap();
            guard.size()
        };
        self.config.ty(minimum)
    }

    /// Returns the size of the memory in pages
    fn size(&self) -> Pages {
        let guard = self.mmap.read().unwrap();
        guard.size()
    }

    /// Returns the memory style for this memory.
    fn style(&self) -> MemoryStyle {
        self.config.style()
    }

    /// Grow memory by the specified amount of wasm pages.
    ///
    /// Returns `None` if memory can't be grown by the specified amount
    /// of wasm pages.
    fn grow(&mut self, delta: Pages) -> Result<Pages, MemoryError> {
        let mut guard = self.mmap.write().unwrap();
        guard.grow(delta, self.config.clone())
    }

    /// Grows the memory to at least a minimum size. If the memory is already big enough
    /// for the min size then this function does nothing
    fn grow_at_least(&mut self, min_size: u64) -> Result<(), MemoryError> {
        let mut guard = self.mmap.write().unwrap();
        guard.grow_at_least(min_size, self.config.clone())
    }

    /// Resets the memory down to a zero size
    fn reset(&mut self) -> Result<(), MemoryError> {
        let mut guard = self.mmap.write().unwrap();
        guard.reset()?;
        Ok(())
    }

    /// Return a `VMMemoryDefinition` for exposing the memory to compiled wasm code.
    fn vmmemory(&self) -> NonNull<VMMemoryDefinition> {
        let guard = self.mmap.read().unwrap();
        guard.vm_memory_definition.as_ptr()
    }

    /// Shared memory can always be cloned
    fn try_clone(&self) -> Result<Box<dyn LinearMemory + Send + Sync + 'static>, MemoryError> {
        Ok(Box::new(self.clone()))
    }

    /// Copies this memory to a new memory
    fn copy(&self) -> Result<Box<dyn LinearMemory + Send + Sync + 'static>, MemoryError> {
        let forked = Self::copy(self)?;
        Ok(Box::new(forked))
    }

    /// Return a concrete shared memory handle for detached API sharing.
    fn as_shared(&self) -> Result<VMSharedMemory, MemoryError> {
        Ok(self.clone())
    }

    fn host_mapping(&self) -> Result<HostMemoryMapping, MemoryError> {
        Ok(self.mmap.read().unwrap().host_mapping.clone())
    }

    // Add current thread to waiter list
    unsafe fn do_wait(
        &mut self,
        dst: u32,
        expected: ExpectedValue,
        timeout: Option<Duration>,
    ) -> Result<u32, WaiterError> {
        let dst = NotifyLocation {
            address: dst,
            memory_base: self.mmap.read().unwrap().alloc.as_ptr() as *mut _,
        };
        unsafe { self.conditions.do_wait(dst, expected, timeout) }
    }

    /// Notify waiters from the wait list. Return the number of waiters notified
    fn do_notify(&mut self, dst: u32, count: u32) -> u32 {
        self.conditions.do_notify(dst, count)
    }

    fn thread_conditions(&self) -> Option<&ThreadConditions> {
        Some(&self.conditions)
    }
}

impl From<VMOwnedMemory> for VMMemory {
    fn from(mem: VMOwnedMemory) -> Self {
        Self(Box::new(mem))
    }
}

impl From<VMSharedMemory> for VMMemory {
    fn from(mem: VMSharedMemory) -> Self {
        Self(Box::new(mem))
    }
}

/// Represents linear memory that can be either owned or shared
#[derive(Debug)]
pub struct VMMemory(pub Box<dyn LinearMemory + Send + Sync + 'static>);

impl From<Box<dyn LinearMemory + Send + Sync + 'static>> for VMMemory {
    fn from(mem: Box<dyn LinearMemory + Send + Sync + 'static>) -> Self {
        Self(mem)
    }
}

impl LinearMemory for VMMemory {
    /// Returns the type for this memory.
    fn ty(&self) -> MemoryType {
        self.0.ty()
    }

    /// Returns the size of the memory in pages
    fn size(&self) -> Pages {
        self.0.size()
    }

    /// Grow memory by the specified amount of wasm pages.
    ///
    /// Returns `None` if memory can't be grown by the specified amount
    /// of wasm pages.
    fn grow(&mut self, delta: Pages) -> Result<Pages, MemoryError> {
        self.0.grow(delta)
    }

    /// Grows the memory to at least a minimum size. If the memory is already big enough
    /// for the min size then this function does nothing
    fn grow_at_least(&mut self, min_size: u64) -> Result<(), MemoryError> {
        self.0.grow_at_least(min_size)
    }

    /// Resets the memory down to a zero size
    fn reset(&mut self) -> Result<(), MemoryError> {
        self.0.reset()?;
        Ok(())
    }

    /// Returns the memory style for this memory.
    fn style(&self) -> MemoryStyle {
        self.0.style()
    }

    /// Return a `VMMemoryDefinition` for exposing the memory to compiled wasm code.
    fn vmmemory(&self) -> NonNull<VMMemoryDefinition> {
        self.0.vmmemory()
    }

    /// Attempts to clone this memory (if its cloneable)
    fn try_clone(&self) -> Result<Box<dyn LinearMemory + Send + Sync + 'static>, MemoryError> {
        self.0.try_clone()
    }

    /// Initialize memory with data
    unsafe fn initialize_with_data(&self, start: usize, data: &[u8]) -> Result<(), Trap> {
        unsafe { self.0.initialize_with_data(start, data) }
    }

    /// Copies this memory to a new memory
    fn copy(&self) -> Result<Box<dyn LinearMemory + Send + Sync + 'static>, MemoryError> {
        self.0.copy()
    }

    fn as_shared(&self) -> Result<VMSharedMemory, MemoryError> {
        self.0.as_shared()
    }

    fn host_mapping(&self) -> Result<HostMemoryMapping, MemoryError> {
        self.0.host_mapping()
    }

    // Add current thread to waiter list
    unsafe fn do_wait(
        &mut self,
        dst: u32,
        expected: ExpectedValue,
        timeout: Option<Duration>,
    ) -> Result<u32, WaiterError> {
        unsafe { self.0.do_wait(dst, expected, timeout) }
    }

    /// Notify waiters from the wait list. Return the number of waiters notified
    fn do_notify(&mut self, dst: u32, count: u32) -> u32 {
        self.0.do_notify(dst, count)
    }

    fn thread_conditions(&self) -> Option<&ThreadConditions> {
        self.0.thread_conditions()
    }
}

impl VMMemory {
    /// Creates a new linear memory instance of the correct type with specified
    /// minimum and maximum number of wasm pages.
    ///
    /// This creates a `Memory` with owned metadata: this can be used to create a memory
    /// that will be imported into Wasm modules.
    pub fn new(memory: &MemoryType, style: &MemoryStyle) -> Result<Self, MemoryError> {
        Ok(if memory.shared {
            Self(Box::new(VMSharedMemory::new(memory, style)?))
        } else {
            Self(Box::new(VMOwnedMemory::new(memory, style)?))
        })
    }

    /// Returns the number of pages in the allocated memory block
    pub fn get_runtime_size(&self) -> u32 {
        self.0.size().0
    }

    /// Create a new linear memory instance with specified minimum and maximum number of wasm pages.
    ///
    /// This creates a `Memory` with metadata owned by a VM, pointed to by
    /// `vm_memory_location`: this can be used to create a local memory.
    ///
    /// # Safety
    /// - `vm_memory_location` must point to a valid location in VM memory.
    pub unsafe fn from_definition(
        memory: &MemoryType,
        style: &MemoryStyle,
        vm_memory_location: NonNull<VMMemoryDefinition>,
    ) -> Result<Self, MemoryError> {
        unsafe {
            Ok(if memory.shared {
                Self(Box::new(VMSharedMemory::from_definition(
                    memory,
                    style,
                    vm_memory_location,
                )?))
            } else {
                Self(Box::new(VMOwnedMemory::from_definition(
                    memory,
                    style,
                    vm_memory_location,
                )?))
            })
        }
    }

    /// Creates VMMemory from a custom implementation - the following into implementations
    /// are natively supported
    /// - VMOwnedMemory -> VMMemory
    /// - Box<dyn LinearMemory + 'static> -> VMMemory
    pub fn from_custom<IntoVMMemory>(memory: IntoVMMemory) -> Self
    where
        IntoVMMemory: Into<Self>,
    {
        memory.into()
    }

    /// Copies this memory to a new memory
    pub fn copy(&self) -> Result<Box<dyn LinearMemory + Send + Sync + 'static>, MemoryError> {
        LinearMemory::copy(self)
    }

    /// Attempts to clone this memory handle.
    pub fn try_clone(&self) -> Result<Self, MemoryError> {
        LinearMemory::try_clone(self).map(Self)
    }
}

#[doc(hidden)]
/// Default implementation to initialize memory with data
pub unsafe fn initialize_memory_with_data(
    memory: &VMMemoryDefinition,
    start: usize,
    data: &[u8],
) -> Result<(), Trap> {
    unsafe {
        let mem_slice = slice::from_raw_parts_mut(memory.base, memory.current_length);
        let end = start + data.len();
        let to_init = &mut mem_slice[start..end];
        to_init.copy_from_slice(data);

        Ok(())
    }
}

/// Represents memory that is used by the WebAssembly module
pub trait LinearMemory
where
    Self: std::fmt::Debug + Send,
{
    /// Returns the type for this memory.
    fn ty(&self) -> MemoryType;

    /// Returns the size of the memory in pages
    fn size(&self) -> Pages;

    /// Returns the memory style for this memory.
    fn style(&self) -> MemoryStyle;

    /// Grow memory by the specified amount of wasm pages.
    ///
    /// Returns `None` if memory can't be grown by the specified amount
    /// of wasm pages.
    fn grow(&mut self, delta: Pages) -> Result<Pages, MemoryError>;

    /// Grows the memory to at least a minimum size. If the memory is already big enough
    /// for the min size then this function does nothing
    fn grow_at_least(&mut self, _min_size: u64) -> Result<(), MemoryError> {
        Err(MemoryError::UnsupportedOperation {
            message: "grow_at_least() is not supported".to_string(),
        })
    }

    /// Resets the memory back to zero length
    fn reset(&mut self) -> Result<(), MemoryError> {
        Err(MemoryError::UnsupportedOperation {
            message: "reset() is not supported".to_string(),
        })
    }

    /// Return a `VMMemoryDefinition` for exposing the memory to compiled wasm code.
    fn vmmemory(&self) -> NonNull<VMMemoryDefinition>;

    /// Attempts to clone this memory (if its cloneable)
    fn try_clone(&self) -> Result<Box<dyn LinearMemory + Send + Sync + 'static>, MemoryError>;

    #[doc(hidden)]
    /// # Safety
    /// This function is unsafe because WebAssembly specification requires that data is always set at initialization time.
    /// It should be the implementors responsibility to make sure this respects the spec
    unsafe fn initialize_with_data(&self, start: usize, data: &[u8]) -> Result<(), Trap> {
        unsafe {
            let memory = self.vmmemory().as_ref();

            initialize_memory_with_data(memory, start, data)
        }
    }

    /// Copies this memory to a new memory
    fn copy(&self) -> Result<Box<dyn LinearMemory + Send + Sync + 'static>, MemoryError>;

    /// Returns a concrete shared memory handle if this memory is shared.
    fn as_shared(&self) -> Result<VMSharedMemory, MemoryError> {
        Err(MemoryError::MemoryNotShared)
    }

    /// Returns a handle to the host bytes of this memory that works without
    /// its store, on any thread, and that does not keep the memory alive.
    ///
    /// The memories of this crate keep the handle in step with every change
    /// of the address or the length of their bytes. A custom memory has none
    /// by default, and can return the handle of a memory it wraps.
    fn host_mapping(&self) -> Result<HostMemoryMapping, MemoryError> {
        Err(MemoryError::UnsupportedOperation {
            message: "host_mapping() is not supported".to_string(),
        })
    }

    /// Add current thread to the waiter hash, and wait until notified or timeout.
    /// Return 0 if the waiter has been notified, 1 if there was a value mismatch,
    /// or 2 if the timeout occurred.
    ///
    /// # Safety
    /// the destination address must be a valid offset within this memory. It must also
    /// be properly aligned for the expected value type; either 4-byte aligned for
    /// `ExpectedValue::U32` or 8-byte aligned for `ExpectedValue::u64`.
    unsafe fn do_wait(
        &mut self,
        _dst: u32,
        _expected: ExpectedValue,
        _timeout: Option<Duration>,
    ) -> Result<u32, WaiterError> {
        Err(WaiterError::Unimplemented)
    }

    /// Notify waiters from the wait list. Return the number of waiters notified
    fn do_notify(&mut self, _dst: u32, _count: u32) -> u32 {
        0
    }

    /// Access the internal atomics handler.
    ///
    /// Will be [`None`] if the memory does not support atomics.
    fn thread_conditions(&self) -> Option<&ThreadConditions> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{
        HostMemoryMapping, LinearMemory, MappedBytes, VMMemory, VMOwnedMemory, VMSharedMemory,
    };
    use wasmer_types::{MemoryStyle, MemoryType, Pages, WASM_PAGE_SIZE};

    /// A style whose memories have no room to grow in place.
    const DYNAMIC: MemoryStyle = MemoryStyle::Dynamic {
        offset_guard_size: WASM_PAGE_SIZE as u64,
    };

    fn owned(style: MemoryStyle, maximum: u32) -> VMOwnedMemory {
        VMOwnedMemory::new(&MemoryType::new(1, Some(maximum), false), &style).unwrap()
    }

    fn bytes(mapping: &HostMemoryMapping) -> Option<(usize, usize)> {
        mapping.with_bytes(|base, len| (base as usize, len))
    }

    fn peek(mapping: &HostMemoryMapping, offset: usize) -> Option<u8> {
        mapping.with_bytes(|base, len| {
            assert!(offset < len);
            unsafe { base.add(offset).read_volatile() }
        })
    }

    fn poke(mapping: &HostMemoryMapping, offset: usize, value: u8) -> Option<()> {
        mapping.with_bytes(|base, len| {
            assert!(offset < len);
            unsafe { base.add(offset).write_volatile(value) }
        })
    }

    #[test]
    fn host_mapping_keeps_its_base_when_a_static_memory_grows() {
        let mut memory = owned(MemoryStyle::Static, 4);
        let mapping = memory.host_mapping().unwrap();
        let (base, len) = bytes(&mapping).unwrap();
        assert_eq!(len, WASM_PAGE_SIZE);

        memory.grow(Pages(2)).unwrap();

        assert_eq!(bytes(&mapping), Some((base, 3 * WASM_PAGE_SIZE)));
        poke(&mapping, 3 * WASM_PAGE_SIZE - 1, 7).unwrap();
    }

    #[test]
    fn host_mapping_follows_a_dynamic_memory_that_moves() {
        let mut memory = owned(DYNAMIC, 4);
        let mapping = memory.host_mapping().unwrap();
        let (base, _) = bytes(&mapping).unwrap();
        poke(&mapping, 11, 42).unwrap();

        memory.grow(Pages(1)).unwrap();

        let (moved, len) = bytes(&mapping).unwrap();
        assert_ne!(moved, base);
        assert_eq!(moved, unsafe { memory.vmmemory().as_ref() }.base as usize);
        assert_eq!(len, 2 * WASM_PAGE_SIZE);
        assert_eq!(peek(&mapping, 11), Some(42));
    }

    #[test]
    fn host_mapping_is_left_alone_by_a_grow_that_fails() {
        let mut memory = owned(DYNAMIC, 1);
        let mapping = memory.host_mapping().unwrap();
        let before = bytes(&mapping);

        memory.grow(Pages(1)).unwrap_err();

        assert_eq!(bytes(&mapping), before);
    }

    #[test]
    fn host_mapping_is_empty_after_a_reset() {
        let mut memory = owned(MemoryStyle::Static, 1);
        let mapping = memory.host_mapping().unwrap();

        memory.reset().unwrap();

        assert_eq!(bytes(&mapping).unwrap().1, 0);
    }

    #[test]
    fn host_mapping_ends_with_its_memory() {
        let memory = VMMemory::from(owned(MemoryStyle::Static, 1));
        let mapping = memory.host_mapping().unwrap();
        assert!(bytes(&mapping).is_some());

        drop(memory);

        assert_eq!(bytes(&mapping), None);
        assert_eq!(poke(&mapping, 0, 1), None);
    }

    #[test]
    fn host_mapping_is_shared_by_the_clones_of_a_shared_memory() {
        let ty = MemoryType::new(1, Some(4), true);
        let mut memory = VMSharedMemory::new(&ty, &MemoryStyle::Static).unwrap();
        let clone = memory.clone();
        let mapping = clone.host_mapping().unwrap();
        poke(&mapping, 0, 9).unwrap();

        memory.grow(Pages(1)).unwrap();
        assert_eq!(bytes(&mapping).unwrap().1, 2 * WASM_PAGE_SIZE);
        assert_eq!(peek(&memory.host_mapping().unwrap(), 0), Some(9));

        // A copy is another memory, with bytes of its own.
        let copy = memory.copy().unwrap();
        poke(&copy.host_mapping().unwrap(), 0, 1).unwrap();
        assert_eq!(peek(&mapping, 0), Some(9));

        drop(memory);
        assert!(bytes(&mapping).is_some());
        drop(clone);
        assert_eq!(bytes(&mapping), None);
    }

    /// Reads the memory from end to end, to stay in the mapping for a while.
    fn scan(base: *mut u8, len: usize) -> usize {
        for offset in (0..len).step_by(64) {
            unsafe { base.add(offset).read_volatile() };
        }
        len
    }

    /// The handle alone, over bytes of the heap that move the way `grow`
    /// moves a memory: no mapping is involved, so Miri can run this one.
    #[test]
    fn host_mapping_keeps_accesses_out_of_bytes_that_move() {
        let record = |bytes: &mut Vec<u8>| MappedBytes {
            base: bytes.as_mut_ptr() as usize,
            len: bytes.len(),
        };
        let mut bytes = vec![0; 64];
        let mapping = HostMemoryMapping::new(record(&mut bytes));

        std::thread::scope(|scope| {
            scope.spawn(|| {
                let mut written = 0;
                while let Some(read) = peek(&mapping, 0) {
                    assert_eq!(read, written);
                    written = written.wrapping_add(1);
                    if poke(&mapping, 0, written).is_none() {
                        break;
                    }
                }
            });

            for _ in 0..8 {
                let mut mapped = mapping.lock_for_change();
                let mut moved = vec![0; bytes.len() + 64];
                moved[..bytes.len()].copy_from_slice(&bytes);
                *mapped = None;
                bytes = moved;
                *mapped = Some(record(&mut bytes));
            }
            *mapping.lock_for_change() = None;
            drop(bytes);
        });
        assert_eq!(peek(&mapping, 0), None);
    }

    /// Accesses on other threads never find a mapping that is gone, and never
    /// lose a write, while the memory moves at every grow and is then dropped.
    #[test]
    fn host_mapping_survives_grows_and_the_drop_under_concurrent_accesses() {
        const PAGES: u32 = 24;

        for _ in 0..20 {
            let mut memory = owned(DYNAMIC, PAGES);
            let mapping = memory.host_mapping().unwrap();

            std::thread::scope(|scope| {
                for _ in 0..2 {
                    scope.spawn(|| {
                        let mut last_len = 0;
                        while let Some(len) = mapping.with_bytes(scan) {
                            assert!(len >= last_len);
                            last_len = len;
                        }
                    });
                }

                // The only writer of this byte, so it reads back what it wrote
                // unless a move dropped the write.
                scope.spawn(|| {
                    let mut written = 0;
                    while let Some(read) = peek(&mapping, 0) {
                        assert_eq!(read, written);
                        written = written.wrapping_add(1);
                        if poke(&mapping, 0, written).is_none() {
                            break;
                        }
                    }
                });

                for _ in 1..PAGES {
                    memory.grow(Pages(1)).unwrap();
                }
                drop(memory);
            });
        }
    }
}

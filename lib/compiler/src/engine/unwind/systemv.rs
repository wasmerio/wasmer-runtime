// This file contains code from external sources.
// Attributions: https://github.com/wasmerio/wasmer/blob/main/docs/ATTRIBUTIONS.md

//! Module for System V ABI unwind registry.

use core::sync::atomic::{AtomicBool, Ordering};
use gimli::{BaseAddresses, CieOrFde, EhFrame, NativeEndian, UnwindSection};
use std::sync::Once;

/// Represents a registry of function unwind information for System V ABI.
pub struct UnwindRegistry {
    published: bool,
    eh_frames_published: bool,
    registrations: Vec<*const u8>,
    #[cfg(target_os = "macos")]
    compact_unwind_mgr: compact_unwind::CompactUnwindManager,
}

unsafe extern "C" {
    // libunwind import
    fn __register_frame(fde: *const u8);
    fn __deregister_frame(fde: *const u8);
    fn wasmer_using_libunwind() -> bool;
}

// Apple-specific unwind functions - the following is taken from LLVM's libunwind itself.
#[cfg(target_os = "macos")]
mod compact_unwind;

/// There are two primary unwinders on Unix platforms: libunwind and libgcc.
///
/// Unfortunately their interface to `__register_frame` is different. The
/// libunwind library takes a pointer to an individual FDE while libgcc takes a
/// null-terminated `.eh_frame` section. This means we need to know what
/// unwinder is being used at runtime.
///
/// This detection is done currently by looking for a libunwind-specific symbol.
/// This specific symbol was somewhat recommended by LLVM's
/// "RTDyldMemoryManager.cpp" file which says:
///
/// > We use the presence of __unw_add_dynamic_fde to detect libunwind.
///
/// I'll note that there's also a different libunwind project at
/// https://www.nongnu.org/libunwind/ but that doesn't appear to have
/// `__register_frame` so I don't think that interacts with this.
#[allow(dead_code)]
fn using_libunwind() -> bool {
    // dlsym (e.g. on musl target) cannot distinguish reliably LLVM libunwind and the libgcc
    // if the target binary is statically linked. Probe a
    // weak symbol in C so the final link determines the registration ABI.
    //
    // On macOS the libgcc interface is never used so libunwind is always used.

    cfg!(target_os = "macos") || unsafe { wasmer_using_libunwind() }
}

impl UnwindRegistry {
    /// Creates a new unwind registry with the given base address.
    pub fn new() -> Self {
        // Register atexit handler that will tell us if exit has been called.
        static INIT: Once = Once::new();
        INIT.call_once(|| unsafe {
            let result = libc::atexit(atexit_handler);
            assert_eq!(result, 0, "libc::atexit must succeed");
        });
        assert!(
            !EXIT_CALLED.load(Ordering::SeqCst),
            "Cannot register unwind information during the process exit"
        );

        Self {
            published: false,
            eh_frames_published: false,
            registrations: Vec::new(),
            #[cfg(target_os = "macos")]
            compact_unwind_mgr: Default::default(),
        }
    }

    /// Publishes all registered functions (coming from .eh_frame sections).
    pub fn publish_eh_frame(&mut self, eh_frame: Option<&[u8]>) -> Result<(), String> {
        if self.eh_frames_published {
            return Err("unwind registry has already been published".to_string());
        }

        unsafe {
            if let Some(eh_frame) = eh_frame {
                self.register_eh_frames(eh_frame)?;
            }
        }

        self.eh_frames_published = true;

        Ok(())
    }

    unsafe fn register_eh_frames(&mut self, eh_frame: &[u8]) -> Result<(), String> {
        let start = eh_frame.as_ptr() as usize;

        // We're going to make a validation even for libgcc where we generally pass the entire
        // input slice.
        let section = EhFrame::new(eh_frame, NativeEndian);
        let bases = BaseAddresses::default().set_eh_frame(start as u64);
        let mut entries = section.entries(&bases);
        let mut records = Vec::new();

        if using_libunwind() {
            while let Some(entry) = entries
                .next()
                .map_err(|err| format!("invalid `.eh_frame` entry: {err}"))?
            {
                if let CieOrFde::Fde(fde) = entry {
                    records.push((start + fde.offset()) as *const u8);
                }
            }

            for record in records {
                unsafe { __register_frame(record) };
                self.registrations.push(record);
            }
        } else {
            while entries
                .next()
                .map_err(|err| format!("invalid `.eh_frame` entry: {err}"))?
                .is_some()
            {}

            if !eh_frame.ends_with(&[0u8; 4]) {
                return Err(
                    "The `.eh_frame` section must end with a four-byte zero terminator".to_string(),
                );
            }

            let start = start as *const u8;
            unsafe { __register_frame(start) };
            self.registrations.push(start);
        }

        Ok(())
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn publish_unwind_info(
        &mut self,
        image_range: core::ops::Range<usize>,
        section: &[u8],
    ) -> Result<(), String> {
        if self.published {
            return Err("unwind registry has already been published".to_string());
        }
        self.compact_unwind_mgr
            .publish_unwind_info(image_range, section);
        self.published = true;
        Ok(())
    }
}

/// Global flag indicating whether the program exit has been initiated.
/// Set to true by an atexit handler to prevent crashes during shutdown
/// when deregistering unwind frames. Accesses use `Ordering::SeqCst`
/// to ensure correct memory ordering across threads.
pub static EXIT_CALLED: AtomicBool = AtomicBool::new(false);

extern "C" fn atexit_handler() {
    EXIT_CALLED.store(true, Ordering::SeqCst);
}

impl Drop for UnwindRegistry {
    fn drop(&mut self) {
        if self.published || self.eh_frames_published {
            // libgcc stores the frame entries as a linked list in decreasing sort order
            // based on the PC value of the registered entry.
            //
            // As we store the registrations in increasing order, it would be O(N^2) to
            // deregister in that order.
            //
            // To ensure that we just pop off the first element in the list upon every
            // deregistration, walk our list of registrations backwards.
            for registration in self.registrations.iter().rev() {
                // We don't want to deregister frames in UnwindRegistry::Drop as that could be called during
                // program shutdown and can collide with release_registered_frames and lead to
                // crashes.
                if EXIT_CALLED.load(Ordering::SeqCst) {
                    return;
                }

                unsafe {
                    __deregister_frame(*registration as *const _);
                }
            }

            #[cfg(target_os = "macos")]
            {
                if EXIT_CALLED.load(Ordering::SeqCst) {
                    return;
                }
                self.compact_unwind_mgr.deregister();
            }
        }
    }
}

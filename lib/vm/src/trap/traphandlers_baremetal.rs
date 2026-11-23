//! Signal-free trap handling (`baremetal` feature).
//!
//! Mirrors the `traphandlers.rs` API without OS signals or coroutine stacks.
//! Traps are routed through a host-installed callback (`install_unwinder`).
//! Without one, they become panics. std is still required (`Mutex`, `thread_local`).
//!
//! Unlike the OS backend, the unwinder does a naked non-local exit, skipping
//! destructors for everything above its landing site — not just Wasm frames.
//! Drop all locals before calling the raise functions.

use super::trap::UnwindReason;
use crate::Trap;
use corosensei::{Coroutine, CoroutineResult, Yielder};
use std::{
    any::Any, cell::Cell, convert::Infallible, error::Error, marker::PhantomData, ptr::NonNull,
    sync::Mutex,
};

/// Uninhabited type - no OS signals exist, so no handler can be constructed.
pub enum TrapHandlerFn<'a> {
    #[doc(hidden)]
    /// Uninhabited variant; carries the lifetime. Cannot be constructed.
    _Uninhabited(Infallible, PhantomData<&'a ()>),
}

/// Configuration for the runtime VM
pub struct VMConfig {
    /// Ignored in baremetal mode. Kept for API compatibility.
    pub wasm_stack_size: Option<usize>,
}

/// Always 0 in baremetal mode — no coroutine stacks to bound.
pub const MAX_STACK_SIZE: usize = 0;

/// No-op.
pub fn drain_stack_pool() {}

/// Not supported in baremetal mode.
pub fn set_stack_size(_size: usize) {
    panic!("set_stack_size is not supported in baremetal mode");
}

/// Not supported in baremetal mode.
pub fn get_stack_size() -> usize {
    panic!("get_stack_size is not supported in baremetal mode");
}

/// No-op — no signal handlers to install.
pub fn init_traps() {}

/// The thread-local state of one activation; see the signal-based backend.
/// There is no trap handler and no separate Wasm stack here, so only the
/// interrupt registry's active stores follow the activation.
pub struct ActivationState {
    #[cfg(feature = "experimental-host-interrupt")]
    active_stores: crate::interrupt_registry::ActiveStores,
    /// The state belongs to one thread, as on the signal-based backend.
    _not_send: PhantomData<*mut ()>,
}

impl Default for ActivationState {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivationState {
    /// The state of an activation that has not entered Wasm.
    pub const fn new() -> Self {
        Self {
            #[cfg(feature = "experimental-host-interrupt")]
            active_stores: crate::interrupt_registry::ActiveStores::new(),
            _not_send: PhantomData,
        }
    }

    /// Exchanges this state with the calling thread's.
    ///
    /// # Safety
    ///
    /// Same contract as the signal-based backend's: calls come in pairs on
    /// one thread, immediately before switching to the stack that owns this
    /// state and immediately after control comes back from it.
    pub unsafe fn swap(&mut self) {
        #[cfg(feature = "experimental-host-interrupt")]
        self.active_stores.swap();
    }

    /// Whether this activation, while it is not running, has entered
    /// `store_id` and the store was interrupted.
    #[cfg(feature = "experimental-host-interrupt")]
    pub fn is_interrupted(&self, store_id: wasmer_types::StoreId) -> bool {
        self.active_stores.is_interrupted(store_id)
    }

    /// Whether nothing is installed.
    pub fn is_empty(&self) -> bool {
        #[cfg(feature = "experimental-host-interrupt")]
        if !self.active_stores.is_empty() {
            return false;
        }
        true
    }

    /// Whether the calling thread still has what swapping a state in needs
    /// of it; see the signal-based backend. Only the interrupt registry keeps
    /// thread locals here.
    pub fn thread_is_usable() -> bool {
        #[cfg(feature = "experimental-host-interrupt")]
        return crate::interrupt_registry::thread_is_usable();
        #[cfg(not(feature = "experimental-host-interrupt"))]
        true
    }

    /// Forgets the state of an activation whose stack is forgotten rather
    /// than unwound; see the signal-based backend.
    pub fn abandon(&mut self) {
        #[cfg(feature = "experimental-host-interrupt")]
        self.active_stores.abandon();
    }
}

/// Runs `f` in-place (there is only one stack).
pub fn on_host_stack<F: FnOnce() -> T, T>(f: F) -> T {
    f()
}

/// See the `std` variant: there is no Wasm stack to switch from here, so `f`
/// is always given back.
pub fn try_on_host_stack<F: FnOnce() -> T, T>(f: F) -> Result<T, F> {
    Err(f)
}

/// Runs `f` in-place (there is only one stack).
pub fn on_host_stack_from_host<F: FnOnce() -> T, T>(f: F) -> T {
    f()
}

/// See the `std` variant. Host code always runs where it is called here, so
/// the reserve is never measured.
pub const HOST_CALL_RESERVE: usize = 256 * 1024;

/// See the `std` variant. Host code always runs where it is called here.
#[must_use = "the host call ends when this is dropped"]
pub struct HostCallInPlace(());

/// See the `std` variant. Host code always runs where it is called here: the
/// guard does nothing.
#[inline(always)]
pub fn host_call_in_place() -> Option<HostCallInPlace> {
    Some(HostCallInPlace(()))
}

/// Just run the `closure` and return its result wrapped in `Ok()`. Nothing is
/// caught in `baremetal` mode.
///
/// # Safety
///
/// The closure must not rely on its destructor running if a trap is raised;
/// the unwinder's non-local exit skips all destructors above its landing site.
pub unsafe fn catch_traps<F, R: 'static>(
    _trap_handler: Option<*const TrapHandlerFn<'static>>,
    _config: &VMConfig,
    closure: F,
) -> Result<R, Trap>
where
    F: FnOnce() -> R + 'static,
{
    // A call made from here cannot be suspended: its caller is beneath it.
    let outer = SUSPENDABLE.with(|cell| cell.replace(None));
    let result = closure();
    SUSPENDABLE.with(|cell| cell.set(outer));
    Ok(result)
}

thread_local! {
    /// The yielder of the [`SuspendableCall`] that is running on this thread
    /// and may be suspended: one that no synchronous call was made in since.
    static SUSPENDABLE: Cell<Option<NonNull<WasmYielder>>> = const { Cell::new(None) };
}

type WasmYielder = Yielder<(), ()>;

/// How many values a [`SuspendableCall`] keeps on its own stack.
pub const INLINE_CALL_VALUES: usize = 4;

/// Where the results of a [`SuspendableCall`] whose values are
/// [`CallValues::Inline`] go; see the signal-based backend.
pub type CallResults = [wasmer_types::RawValue; INLINE_CALL_VALUES];

/// The arguments of a [`SuspendableCall`], which its trampoline overwrites
/// with the results; see the signal-based backend.
#[derive(Clone, Copy)]
pub enum CallValues {
    /// This many values, read from here when the call starts and kept on the
    /// stack of the call; the results are written to the [`CallResults`] of
    /// the resumption in which the guest returns.
    Inline(*const wasmer_types::RawValue, usize),
    /// Kept by whoever makes the call, where they stay until the call is
    /// finished or dropped, and where the results are.
    At(*mut wasmer_types::RawValue),
}

/// What a [`SuspendableCall`] did when it was resumed.
pub enum CallStep {
    /// It waits: an import it called suspended it (see
    /// [`suspend_wasm_call`]). It goes on when it is resumed again.
    Suspended,
    /// It is over: it returned, with its results where its values said. A
    /// trap leaves through the unwinder.
    Finished(Result<(), Trap>),
}

/// A Wasm call on a stack of its own that an import can suspend, and its
/// caller resume later; see the signal-based backend. This is the one place
/// where `baremetal` switches stacks.
///
/// Dropping a suspended call resets its stack: none of it runs again.
pub struct SuspendableCall {
    coroutine: Option<Coroutine<(), (), Option<CallResults>>>,
    /// The coroutine's yielder, once it has suspended.
    yielder: Option<NonNull<WasmYielder>>,
}

/// What [`SuspendableCall::start`] did.
pub enum CallStarted {
    /// The guest waits: it goes on when the call is resumed.
    Suspended(SuspendableCall),
    /// It is over: it returned, with its results where its values said.
    Finished(Result<(), Trap>),
}

impl SuspendableCall {
    /// Calls `callee` through `trampoline`, until its guest waits or is over.
    ///
    /// # Safety
    /// All pointer arguments, and the pointer of [`CallValues::At`], must be
    /// valid until the call is finished or dropped.
    pub unsafe fn start(
        config: &VMConfig,
        vmctx: crate::VMFunctionContext,
        trampoline: crate::VMTrampoline,
        callee: *const crate::VMFunctionBody,
        values: CallValues,
        results: *mut CallResults,
        trap_handler: Option<*const TrapHandlerFn<'static>>,
    ) -> CallStarted {
        // The values are read now: the coroutine starts within this call.
        let mut call = unsafe { Self::new(config, vmctx, trampoline, callee, values) };
        match unsafe { call.resume(results, trap_handler) } {
            CallStep::Suspended => CallStarted::Suspended(call),
            CallStep::Finished(result) => CallStarted::Finished(result),
        }
    }

    unsafe fn new(
        _config: &VMConfig,
        vmctx: crate::VMFunctionContext,
        trampoline: crate::VMTrampoline,
        callee: *const crate::VMFunctionBody,
        values: CallValues,
    ) -> Self {
        let coroutine = Coroutine::new(move |yielder: &WasmYielder, ()| {
            let yielder = NonNull::from(yielder);
            SUSPENDABLE.with(|cell| cell.set(Some(yielder)));
            // In this frame, on the call's own stack, from here on.
            let mut inline = [wasmer_types::RawValue { i32: 0 }; INLINE_CALL_VALUES];
            let values_vec = match values {
                CallValues::Inline(values, len) => {
                    for (i, slot) in inline.iter_mut().enumerate().take(len) {
                        *slot = unsafe { *values.add(i) };
                    }
                    inline.as_mut_ptr()
                }
                CallValues::At(values) => values,
            };
            unsafe {
                // pointer types are ABI-compatible; transmute to unsafe fn to preserve unsafety
                std::mem::transmute::<
                    unsafe extern "C" fn(
                        *mut crate::VMContext,
                        *const crate::VMFunctionBody,
                        *mut wasmer_types::RawValue,
                    ),
                    unsafe extern "C" fn(
                        crate::VMFunctionContext,
                        *const crate::VMFunctionBody,
                        *mut u8,
                    ),
                >(trampoline)(vmctx, callee, values_vec.cast());
            }
            matches!(values, CallValues::Inline(..)).then_some(inline)
        });
        Self {
            coroutine: Some(coroutine),
            yielder: None,
        }
    }

    /// Runs a call whose guest waits until it waits again or is over.
    ///
    /// # Safety
    /// Called on the thread the call was made on, and not again once it is
    /// finished.
    pub unsafe fn resume(
        &mut self,
        results: *mut CallResults,
        _trap_handler: Option<*const TrapHandlerFn<'static>>,
    ) -> CallStep {
        let coroutine = self.coroutine.as_mut().expect("the call is not over");
        let outer = SUSPENDABLE.with(|cell| cell.replace(self.yielder));
        let resumed = coroutine.resume(());
        // The coroutine published its yielder when it started.
        self.yielder = SUSPENDABLE.with(|cell| cell.replace(outer));
        match resumed {
            CoroutineResult::Yield(()) => CallStep::Suspended,
            CoroutineResult::Return(returned) => {
                if let Some(returned) = returned {
                    unsafe { results.write(returned) };
                }
                CallStep::Finished(Ok(()))
            }
        }
    }
}

impl Drop for SuspendableCall {
    fn drop(&mut self) {
        let Some(mut coroutine) = self.coroutine.take() else {
            return;
        };
        if !coroutine.started() {
            // Never resumed: there is only its function to drop.
            coroutine.force_unwind();
        } else if !coroutine.done() {
            // Safety: a suspended stack holds nothing to release, and is
            // never resumed.
            unsafe { coroutine.force_reset() };
        }
    }
}

/// Whether the Wasm call that is running is a [`SuspendableCall`] that an
/// import called from it may suspend.
pub fn wasm_call_is_suspendable() -> bool {
    SUSPENDABLE.with(|cell| cell.get()).is_some()
}

/// Suspends the Wasm call that is running, from an import it called, until
/// it is resumed.
///
/// # Safety
/// Only if [`wasm_call_is_suspendable`], with nothing on the stack that a
/// destructor would have to release: the call may be dropped while it waits.
pub unsafe fn suspend_wasm_call() {
    let yielder = SUSPENDABLE
        .with(|cell| cell.get())
        .expect("no Wasm call that can be suspended is running");
    unsafe { yielder.as_ref().suspend(()) };
}

/// Registered unwinder, if any.
static UNWINDER: Mutex<Option<Box<dyn Fn(UnwindReason) + Send>>> = Mutex::new(None);

/// Register (or clear) the trap unwinder.
///
/// The callback **must not return** — it should perform a non-local exit
/// (abort, longjmp, …). Returning is treated as `unreachable!`.
///
/// If no unwinder is installed, traps forward as Rust panics.
/// Install before starting Wasm execution — swapping the unwinder concurrently
/// with a live trap is non-deterministic.
pub fn install_unwinder(unwinder: Option<Box<dyn Fn(UnwindReason) + Send>>) {
    *UNWINDER
        .lock()
        .expect("baremetal unwinder mutex poisoned in install_unwinder") = unwinder;
}

thread_local! {
    static UNWINDING: Cell<bool> = const { Cell::new(false) };
}

struct UnwindingGuard;

impl UnwindingGuard {
    fn acquire() -> Option<Self> {
        UNWINDING.with(|u| {
            if u.replace(true) {
                None
            } else {
                Some(UnwindingGuard)
            }
        })
    }
}

impl Drop for UnwindingGuard {
    fn drop(&mut self) {
        UNWINDING.with(|u| u.set(false));
    }
}

fn unwind_with(reason: UnwindReason) -> ! {
    // Re-entrance guard: if the unwinder itself triggers a trap, panic instead
    // of recursing. RAII resets the flag even if this function panics.
    let Some(_guard) = UnwindingGuard::acquire() else {
        panic!("wasm trap raised inside the baremetal unwinder (re-entrant unwinding): {reason:?}");
    };

    // Move the callback out while holding the lock, then let the lock drop
    // before invoking. The callback performs a non-local exit that skips
    // destructors, so a MutexGuard held across the call would never drop,
    // deadlocking any future unwind_with / install_unwinder call.
    let f = UNWINDER
        .lock()
        .expect("baremetal unwinder mutex poisoned in unwind_with")
        .take();

    match f {
        Some(f) => {
            let display = format!("{reason:?}");
            f(reason);
            unreachable!("baremetal unwinder must not return (trap was: {display})");
        }
        None => panic!("wasm trap with no baremetal unwinder installed: {reason:?}"),
    }
}

/// Raise a trap from a Wasm libcall.
///
/// # Safety
///
/// Drop all locally-owned values before calling — the unwinder skips destructors
/// for every frame above its landing site (see module docs).
pub unsafe fn raise_lib_trap(trap: Trap) -> ! {
    unwind_with(UnwindReason::LibTrap(trap))
}

/// Raise a user-defined trap.
///
/// # Safety
///
/// Drop all locally-owned values before calling (see [`raise_lib_trap`]).
pub unsafe fn raise_user_trap(data: Box<dyn Error + Send + Sync>) -> ! {
    unwind_with(UnwindReason::UserTrap(data))
}

/// Forward a previously caught Rust panic to the unwinder.
///
/// # Safety
///
/// `payload` must be the original panic payload and must not have been
/// partially moved since it was caught.
pub unsafe fn resume_panic(payload: Box<dyn Any + Send>) -> ! {
    unwind_with(UnwindReason::Panic(payload))
}

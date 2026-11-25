use std::{
    any::Any,
    cell::Cell,
    error::Error,
    ffi::{CStr, c_void},
    panic::resume_unwind,
};

use crate::{
    AsStoreMut,
    v8::{bindings::*, vm::VMExceptionRef},
};

#[derive(Debug)]
enum InnerTrap {
    User(Box<dyn Error + Send + Sync>),
    CApi(*mut wasm_trap_t),
}

/// A struct representing a Trap
#[derive(Debug)]
pub struct Trap {
    inner: InnerTrap,
}

unsafe impl Send for Trap {}
unsafe impl Sync for Trap {}

impl Trap {
    pub fn user(error: Box<dyn Error + Send + Sync>) -> Self {
        Self {
            inner: InnerTrap::User(error),
        }
    }

    /// Attempts to downcast the `Trap` to a concrete type.
    pub fn downcast<T: Error + 'static>(self) -> Result<T, Self> {
        match self.inner {
            // We only try to downcast user errors
            InnerTrap::User(err) if err.is::<T>() => Ok(*err.downcast::<T>().unwrap()),
            _ => Err(self),
        }
    }

    /// Attempts to downcast the `Trap` to a concrete type.
    pub fn downcast_ref<T: Error + 'static>(&self) -> Option<&T> {
        match &self.inner {
            // We only try to downcast user errors
            InnerTrap::User(err) if err.is::<T>() => err.downcast_ref::<T>(),
            _ => None,
        }
    }

    /// Returns true if the `Trap` is the same as T
    pub fn is<T: Error + 'static>(&self) -> bool {
        match &self.inner {
            InnerTrap::User(err) => err.is::<T>(),
            _ => false,
        }
    }

    /// Returns true if the `Trap` is an exception
    pub fn is_exception(&self) -> bool {
        false
    }

    /// If the `Trap` is an uncaught exception, returns it.
    pub fn to_exception_ref(&self) -> Option<VMExceptionRef> {
        None
    }

    /// The trap V8 is given for this one: the trap itself if it came from
    /// V8, and for the error of a host function a trap that carries it.
    pub unsafe fn into_wasm_trap(self, store: &mut impl AsStoreMut) -> *mut wasm_trap_t {
        match self.inner {
            InnerTrap::CApi(t) => t,
            InnerTrap::User(error) => unsafe {
                trap_carrying(store, "a host function failed", Payload::Error(error))
            },
        }
    }

    /// The trap V8 is given for a host function that panicked. A C callback
    /// cannot unwind, so the trap carries the panic past V8's frames, to the
    /// Rust caller of the guest: [`Trap::from`] resumes it there.
    pub(crate) unsafe fn from_panic(
        store: &mut impl AsStoreMut,
        panic: Box<dyn Any + Send>,
    ) -> *mut wasm_trap_t {
        unsafe { trap_carrying(store, "a host function panicked", Payload::Panic(panic)) }
    }
}

/// What the trap of a host function carries past V8's frames, as the host
/// info of the trap. A guest cannot catch a trap that a host function
/// returns, so it reaches the Rust caller of the guest, which takes it.
enum Payload {
    /// The error the host function returned.
    Error(Box<dyn Error + Send + Sync>),
    /// The panic of the host function.
    Panic(Box<dyn Any + Send>),
}

/// The host info of such a trap: its payload, until it is taken.
type Carried = Cell<Option<Payload>>;

/// Frees the host info of a trap, when V8 collects the trap.
unsafe extern "C" fn drop_carried(info: *mut c_void) {
    drop(unsafe { Box::from_raw(info.cast::<Carried>()) });
}

/// A trap of `store` that carries `payload`.
unsafe fn trap_carrying(
    store: &mut impl AsStoreMut,
    message: &str,
    payload: Payload,
) -> *mut wasm_trap_t {
    let store = store.as_store_mut();
    unsafe {
        let mut data = std::mem::zeroed();
        wasm_byte_vec_new(&mut data, message.len(), message.as_ptr() as _);
        let trap = wasm_trap_new(store.inner.store.as_v8().inner, &data);
        wasm_byte_vec_delete(&mut data);
        let carried: *mut Carried = Box::into_raw(Box::new(Cell::new(Some(payload))));
        wasm_trap_set_host_info_with_finalizer(trap, carried.cast(), Some(drop_carried));
        trap
    }
}

impl From<*mut wasm_trap_t> for Trap {
    /// The trap of a guest, as V8 returned it to the Rust caller. Only a
    /// trap that a host function made has host info: what it carries is taken
    /// here, and its panic resumes, now that V8's frames are gone.
    fn from(value: *mut wasm_trap_t) -> Self {
        let carried = unsafe { wasm_trap_get_host_info(value) }.cast::<Carried>();
        match unsafe { carried.as_ref() }.and_then(Cell::take) {
            Some(Payload::Error(error)) => Self {
                inner: InnerTrap::User(error),
            },
            Some(Payload::Panic(panic)) => resume_unwind(panic),
            None => Self {
                inner: InnerTrap::CApi(value),
            },
        }
    }
}

impl std::error::Error for Trap {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.inner {
            InnerTrap::User(err) => Some(&**err),
            _ => None,
        }
    }
}

impl std::fmt::Display for Trap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            InnerTrap::User(e) => write!(f, "{e}"),
            InnerTrap::CApi(value) => {
                // let message: wasm_message_t;
                // wasm_trap_message(value, &mut message);
                let mut out = unsafe {
                    let mut vec: wasm_byte_vec_t = Default::default();
                    wasm_byte_vec_new_empty(&mut vec);
                    &mut vec as *mut _
                };
                unsafe { wasm_trap_message(*value, out) };
                let cstr = unsafe { CStr::from_ptr((*out).data) };
                write!(f, "wasm-c-api trap: {}", cstr.to_str().unwrap())
            }
        }
    }
}

impl From<Trap> for crate::RuntimeError {
    fn from(trap: Trap) -> Self {
        if trap.is::<Self>() {
            return trap.downcast::<Self>().unwrap();
        }

        Self::new_from_source(crate::BackendTrap::V8(trap), vec![], None)
    }
}

use std::{
    any::Any,
    cell::{Cell, OnceCell, RefCell},
    error::Error,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    rc::{Rc, Weak},
};

use crate::{
    AsStoreMut, AsStoreRef, RuntimeError,
    backend::v8::{origin::Origin, references::OwnedValue},
    v8::{bindings::*, vm::VMExceptionRef},
};

type UserBox = Box<dyn Error + Send + Sync>;
pub(crate) type Panic = Box<dyn Any + Send>;
pub(crate) type FailureCell = Rc<RefCell<Option<Panic>>>;

pub(crate) fn record_failure(slot: &RefCell<Option<Panic>>, mut panic: Panic, unwinding: bool) {
    loop {
        if !unwinding {
            let mut first = slot.borrow_mut();
            if first.is_none() {
                *first = Some(panic);
                return;
            }
        }
        // Finite secondary disposal may reenter and consume the first value.
        // Publish the next original caught value before any later destructor.
        match catch_unwind(AssertUnwindSafe(|| drop(panic))) {
            Ok(()) => return,
            Err(next) => panic = next,
        }
    }
}
fn retire(slot: &RefCell<Option<Panic>>, unwinding: bool, action: impl FnOnce()) {
    if let Err(panic) = catch_unwind(AssertUnwindSafe(action)) {
        record_failure(slot, panic, unwinding);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrapMode {
    BorrowedSync,
    #[cfg(feature = "experimental-async")]
    GuardFreeAsync,
}
#[derive(Default)]
pub(crate) struct TrapCallState {
    active: RefCell<Weak<TrapCallFrame>>,
}
struct TrapCallFrame {
    origin: Origin,
    backing: usize,
    raw: *mut wasm_store_t,
    mode: TrapMode,
    live: Cell<bool>,
    rows: RefCell<Vec<Rc<UserTrapRow>>>,
    failure: FailureCell,
}
struct UserTrapRow {
    key: OnceCell<OwnedValue>,
    error: RefCell<Option<UserBox>>,
    failure: Weak<RefCell<Option<Panic>>>,
}
impl Drop for UserTrapRow {
    fn drop(&mut self) {
        // Sole immutable-key disposer, after the last lexical Rc comparison.
        let error = self.error.get_mut().take();
        let key = self.key.take();
        let unwinding = std::thread::panicking();
        let local = RefCell::new(None);
        let failure = self.failure.upgrade();
        let slot = failure.as_deref().unwrap_or(&local);
        retire(slot, unwinding, || drop(error));
        retire(slot, unwinding, || drop(key));
        if let Some(panic) = local.into_inner() {
            resume_unwind(panic);
        }
    }
}
pub(crate) struct TrapCallScope {
    state: Rc<TrapCallState>,
    frame: Rc<TrapCallFrame>,
    previous: Weak<TrapCallFrame>,
    finished: Cell<bool>,
    owns_failure: bool,
}
#[derive(Clone)]
pub(crate) struct TrapPublisher(Rc<TrapCallFrame>);
impl TrapCallState {
    pub(crate) fn publisher(
        state: &Weak<Self>,
        origin: &Origin,
        backing: usize,
        raw: *mut wasm_store_t,
    ) -> Result<TrapPublisher, RuntimeError> {
        origin.check_alive()?;
        let state = state
            .upgrade()
            .ok_or_else(|| RuntimeError::new("V8 trap metadata expired"))?;
        let frame = state.active.borrow().upgrade();
        let frame =
            frame.ok_or_else(|| RuntimeError::new("V8 callback has no actual call scope"))?;
        if !frame.live.get()
            || !frame.origin.same(origin)
            || frame.backing != backing
            || frame.raw != raw
        {
            return Err(RuntimeError::new(
                "V8 callback has a different actual call scope",
            ));
        }
        Ok(TrapPublisher(frame))
    }
}
impl TrapPublisher {
    pub(crate) fn mode(&self) -> TrapMode {
        self.0.mode
    }
    pub(crate) fn record(&self, panic: Panic, unwinding: bool) {
        record_failure(&self.0.failure, panic, unwinding);
    }
    pub(crate) fn has_panic(&self) -> bool {
        self.0.failure.borrow().is_some()
    }
    pub(crate) fn emit_user(&self, error: UserBox) -> *mut wasm_trap_t {
        // Exact live publisher captured BEFORE user work; the enclosing actual
        // synchronous C++ call cannot end while this callback is executing.
        let row = Rc::new(UserTrapRow {
            key: OnceCell::new(),
            error: RefCell::new(Some(error)),
            failure: Rc::downgrade(&self.0.failure),
        });
        self.0.rows.borrow_mut().push(row.clone());
        let unwinding = std::thread::panicking();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let raw = new_message(self.0.raw, "V8 host function returned an error");
            let mut owner = OwnedValue::reference(unsafe { wasm_trap_as_ref(raw) }, false);
            let key = OwnedValue::copy(owner.value());
            assert!(row.key.set(key).is_ok(), "V8 trap key committed twice");
            // The same Box and independent identity are committed FIRST.
            unsafe { wasm_ref_as_trap(owner.take().of.ref_) }
        }));
        match result {
            Ok(raw) => raw,
            Err(panic) => {
                self.record(panic, unwinding);
                // The enrolled row still owns the same error if no exception
                // was published. Never unwind through this C callback.
                new_message(self.0.raw, "V8 host trap construction panicked")
            }
        }
    }
}
impl TrapCallScope {
    pub(crate) fn enter(
        store: &impl AsStoreRef,
        mode: TrapMode,
        shared_failure: Option<FailureCell>,
    ) -> Result<Self, RuntimeError> {
        let origin = Origin::of(store)?;
        let store = store.as_store_ref();
        let state = store.inner.store.as_v8().trap_calls.clone();
        let owns_failure = shared_failure.is_none();
        let frame = Rc::new(TrapCallFrame {
            origin,
            backing: store.inner as *const crate::StoreInner as usize,
            raw: store.inner.store.as_v8().inner,
            mode,
            live: Cell::new(true),
            rows: RefCell::new(Vec::new()),
            failure: shared_failure.unwrap_or_default(),
        });
        let previous = state.active.replace(Rc::downgrade(&frame));
        Ok(Self {
            state,
            frame,
            previous,
            finished: Cell::new(false),
            owns_failure,
        })
    }
    pub(crate) fn from_owned(&self, raw: *mut wasm_trap_t) -> Trap {
        if raw.is_null() {
            return Trap {
                inner: InnerTrap::Message("V8 call failed without an exception".into()),
            };
        }
        let owner = OwnedValue::reference(unsafe { wasm_trap_as_ref(raw) }, false);
        let reference = unsafe { owner.value().of.ref_ };
        let rows = self.frame.rows.borrow().clone();
        let mut error = None;
        // ONLY this explicitly supplied owning scope, never an ancestor.
        for row in rows {
            let matched = row
                .key
                .get()
                .is_some_and(|key| unsafe { wasm_ref_same(reference, key.value().of.ref_) });
            if matched {
                error = row.error.borrow_mut().take();
                let removed = {
                    let mut current = self.frame.rows.borrow_mut();
                    current
                        .iter()
                        .position(|value| Rc::ptr_eq(value, &row))
                        .map(|index| current.remove(index))
                };
                retire(&self.frame.failure, std::thread::panicking(), || {
                    drop(removed)
                });
                break;
            }
        } // every lexical row snapshot ends before any later finish
        let trap = match error {
            Some(error) => Trap::user(error),
            None => Trap {
                inner: InnerTrap::Message(snapshot_message(raw)),
            },
        };
        retire(&self.frame.failure, std::thread::panicking(), || {
            drop(owner)
        });
        trap
    }
    pub(crate) fn has_panic(&self) -> bool {
        self.frame.failure.borrow().is_some()
    }
    pub(crate) fn dispose(&self, action: impl FnOnce()) {
        retire(&self.frame.failure, std::thread::panicking(), action);
    }
    pub(crate) fn catch<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        let unwinding = std::thread::panicking();
        match catch_unwind(AssertUnwindSafe(action)) {
            Ok(value) => Some(value),
            Err(panic) => {
                record_failure(&self.frame.failure, panic, unwinding);
                None
            }
        }
    }
    pub(crate) fn finish(&self) {
        if self.finished.replace(true) {
            return;
        }
        self.frame.live.set(false);
        let rows = self.frame.rows.take();
        let unwinding = std::thread::panicking();
        for row in rows {
            retire(&self.frame.failure, unwinding, || drop(row));
        }
        // Private query snapshots cannot escape their physical operation.
        // All actual row resources end before restoration or failure take.
        self.state.active.replace(self.previous.clone());
    }
    pub(crate) fn finish_outcome<T>(&self, outcome: T) -> T {
        self.finish();
        if self.owns_failure && self.frame.failure.borrow().is_some() {
            retire(&self.frame.failure, std::thread::panicking(), || {
                drop(outcome)
            });
            let panic = self.frame.failure.borrow_mut().take().unwrap();
            resume_unwind(panic);
        }
        outcome
    }
}
impl Drop for TrapCallScope {
    fn drop(&mut self) {
        self.finish();
        if self.owns_failure {
            let panic = self.frame.failure.borrow_mut().take();
            if let Some(panic) = panic {
                if std::thread::panicking() {
                    record_failure(&RefCell::new(None), panic, true);
                } else {
                    resume_unwind(panic);
                }
            }
        }
    }
}

#[derive(Debug)]
enum InnerTrap {
    User(UserBox),
    Message(String),
}
/// An owned Rust snapshot, without a queryable C++ handle after escape.
#[derive(Debug)]
pub struct Trap {
    inner: InnerTrap,
}
impl Trap {
    pub fn user(error: UserBox) -> Self {
        Self {
            inner: InnerTrap::User(error),
        }
    }
    pub fn downcast<T: Error + 'static>(self) -> Result<T, Self> {
        match self.inner {
            InnerTrap::User(error) if error.is::<T>() => Ok(*error.downcast::<T>().unwrap()),
            _ => Err(self),
        }
    }
    pub fn downcast_ref<T: Error + 'static>(&self) -> Option<&T> {
        match &self.inner {
            InnerTrap::User(error) => error.downcast_ref::<T>(),
            _ => None,
        }
    }
    pub fn is<T: Error + 'static>(&self) -> bool {
        self.downcast_ref::<T>().is_some()
    }
    pub fn is_exception(&self) -> bool {
        false
    }
    pub fn to_exception_ref(&self) -> Option<VMExceptionRef> {
        None
    }
    /// Recreate an owned trap in an actual live destination Store.
    ///
    /// # Safety
    /// A User trap requires a matching live actual-call scope on this Store.
    /// Preserve that scope through publication and consume the exception with
    /// its explicit owning scope to recover the typed Box. Message snapshots
    /// require only the supplied actual live Store.
    pub unsafe fn into_wasm_trap(self, store: &mut impl AsStoreMut) -> *mut wasm_trap_t {
        match self.try_into_wasm_trap(store) {
            Ok(raw) => raw,
            Err(unadmitted) => {
                drop(unadmitted);
                panic!("V8 User trap requires its matching live actual-call scope");
            }
        }
    }
    pub(crate) fn try_into_wasm_trap(
        self,
        store: &mut impl AsStoreMut,
    ) -> Result<*mut wasm_trap_t, Self> {
        let Ok(origin) = Origin::of(store) else {
            return Err(self);
        };
        let view = store.as_store_ref();
        let backend = view.inner.store.as_v8();
        match self.inner {
            InnerTrap::Message(message) => Ok(new_message(backend.inner, &message)),
            InnerTrap::User(error) => {
                let publisher = TrapCallState::publisher(
                    &Rc::downgrade(&backend.trap_calls),
                    &origin,
                    view.inner as *const crate::StoreInner as usize,
                    backend.inner,
                );
                match publisher {
                    Ok(publisher) => Ok(publisher.emit_user(error)),
                    Err(_) => Err(Self::user(error)),
                }
            }
        }
    }
}
pub(crate) fn checked_message(
    origin: &Origin,
    raw: *mut wasm_store_t,
    message: &str,
) -> *mut wasm_trap_t {
    if origin.check_alive().is_err() {
        return std::ptr::null_mut();
    }
    new_message(raw, message)
}
fn new_message(store: *mut wasm_store_t, message: &str) -> *mut wasm_trap_t {
    let mut bytes = Default::default();
    unsafe { wasm_byte_vec_new(&mut bytes, message.len(), message.as_ptr().cast()) };
    let trap = unsafe { wasm_trap_new(store, &bytes) };
    unsafe { wasm_byte_vec_delete(&mut bytes) };
    trap
}
fn snapshot_message(raw: *mut wasm_trap_t) -> String {
    let mut bytes = Default::default();
    unsafe { wasm_trap_message(raw, &mut bytes) };
    let message = if bytes.size == 0 {
        String::new()
    } else {
        let raw = unsafe { std::slice::from_raw_parts(bytes.data.cast::<u8>(), bytes.size) };
        String::from_utf8_lossy(raw.strip_suffix(&[0]).unwrap_or(raw)).into_owned()
    };
    unsafe { wasm_byte_vec_delete(&mut bytes) };
    message
}
impl From<*mut wasm_trap_t> for Trap {
    /// Consumes a trusted OWN raw trap as a MESSAGE ONLY snapshot. Typed Rust
    /// custody requires the explicit owning scope; text never grants it.
    fn from(raw: *mut wasm_trap_t) -> Self {
        if raw.is_null() {
            return Self {
                inner: InnerTrap::Message("V8 call failed without an exception".into()),
            };
        }
        let owner = OwnedValue::reference(unsafe { wasm_trap_as_ref(raw) }, false);
        let message = snapshot_message(raw);
        drop(owner);
        Self {
            inner: InnerTrap::Message(message),
        }
    }
}
impl Error for Trap {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.inner {
            InnerTrap::User(error) => Some(&**error),
            _ => None,
        }
    }
}
impl std::fmt::Display for Trap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.inner {
            InnerTrap::User(error) => write!(f, "{error}"),
            InnerTrap::Message(message) => write!(f, "wasm-c-api trap: {message}"),
        }
    }
}
impl From<Trap> for RuntimeError {
    fn from(trap: Trap) -> Self {
        if trap.is::<Self>() {
            return trap.downcast::<Self>().unwrap();
        }
        Self::new_from_source(crate::BackendTrap::V8(trap), vec![], None)
    }
}

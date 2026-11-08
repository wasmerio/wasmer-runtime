//! Passive provenance for handles. This metadata neither pins nor grants access
//! to a V8 Store, and contains no owner-local executor, future, or payload.

use crate::{AsStoreRef, RuntimeError};
use std::{
    sync::{Arc, Weak},
    thread::ThreadId,
};

#[derive(Debug)]
pub(crate) struct OriginOwner {
    thread: ThreadId,
    token: Option<Arc<()>>,
}
impl OriginOwner {
    pub(crate) fn new() -> Self {
        Self {
            thread: std::thread::current().id(),
            token: Some(Arc::new(())),
        }
    }
    pub(crate) fn capture(&self) -> Origin {
        Origin {
            thread: self.thread,
            token: self.token.as_ref().map(Arc::downgrade).unwrap_or_default(),
        }
    }
    pub(crate) fn invalidate(&mut self) {
        self.token = None;
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Origin {
    thread: ThreadId,
    token: Weak<()>,
}
impl Origin {
    pub(crate) fn of(store: &impl AsStoreRef) -> Result<Self, RuntimeError> {
        let view = store.as_store_ref();
        if !view.inner.store.is_v8() {
            return Err(RuntimeError::new("V8 handle requires a V8 Store"));
        }
        let origin = view.inner.store.as_v8().origin.capture();
        origin.check_alive()?;
        Ok(origin)
    }
    pub(crate) fn check_alive(&self) -> Result<(), RuntimeError> {
        if self.thread != std::thread::current().id() {
            return Err(RuntimeError::new("V8 handle belongs to another thread"));
        }
        // Only the actual Store owns a strong unit token. Never upgrade this
        // Weak and accidentally keep metadata alive beyond actual deletion.
        if self.token.strong_count() == 0 {
            return Err(RuntimeError::new("V8 handle's Store has expired"));
        }
        Ok(())
    }
    pub(crate) fn same(&self, other: &Self) -> bool {
        self.thread == other.thread && self.token.ptr_eq(&other.token)
    }
    pub(crate) fn check_store(&self, store: &impl AsStoreRef) -> Result<(), RuntimeError> {
        self.check_alive()?;
        let view = store.as_store_ref();
        let actual = Self::of(&view)?;
        if !self.same(&actual) {
            return Err(RuntimeError::new(
                "V8 handle belongs to another Store backing",
            ));
        }
        Ok(())
    }
}

/// Opaque V8 VM handle with inert provenance. Clone/Drop never call wee8 or
/// access local Store state, including on another thread or after expiration.
/// Raw handles can be imported only through VMExtern::from_raw_v8_unchecked.
pub struct Handle<T> {
    raw: *mut T,
    pub(crate) origin: Origin,
}
impl<T> Clone for Handle<T> {
    fn clone(&self) -> Self {
        Self {
            raw: self.raw,
            origin: self.origin.clone(),
        }
    }
}
impl<T> std::fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("V8Handle")
            .field("raw", &self.raw)
            .finish_non_exhaustive()
    }
}
impl<T> PartialEq for Handle<T> {
    fn eq(&self, other: &Self) -> bool {
        self.raw == other.raw
    }
}
impl<T> Eq for Handle<T> {}
impl<T> Handle<T> {
    // Trusted internal sources must carry their actual source's origin. This
    // constructor makes no raw query and never infers origin from StoreId.
    pub(crate) fn new(raw: *mut T, origin: Origin) -> Self {
        Self { raw, origin }
    }
    pub(crate) fn raw(&self) -> *mut T {
        self.origin
            .check_alive()
            .expect("invalid V8 handle provenance");
        self.raw
    }
    pub(crate) fn checked_raw(&self, store: &impl AsStoreRef) -> Result<*mut T, RuntimeError> {
        self.origin.check_store(store)?;
        Ok(self.raw)
    }
}

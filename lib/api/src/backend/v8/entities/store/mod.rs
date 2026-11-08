//! Data types, functions and traits for `v8` runtime's `Store` implementation.
use std::thread::ThreadId;

use crate::{
    AsStoreRef, BackendStore, StoreRef,
    backend::v8::bindings::{wasm_store_new, wasm_store_t},
    engine::{AsEngineRef, Engine, EngineRef},
};

// Private adapter compiled against the exact paired pinned C++ public header.
unsafe extern "C" {
    fn wasmer_v8_owned_store_delete(actual_store: *mut wasm_store_t);
}

mod obj;
pub use obj::*;

/// A WebAssembly `store` in the `v8` runtime.
pub struct Store {
    pub(crate) engine: Engine,
    pub(crate) inner: *mut wasm_store_t,
    pub(crate) thread_id: ThreadId,
    pub(crate) origin: crate::backend::v8::origin::OriginOwner,
    pub(crate) reference_roots: crate::backend::v8::references::ReferenceRoots,
    #[cfg(feature = "experimental-async")]
    pub(crate) host_callbacks: crate::backend::v8::async_runtime::HostCallbacks,
    #[cfg(feature = "experimental-async")]
    pub(crate) async_binding: std::rc::Rc<crate::backend::v8::async_runtime::AsyncBinding>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("engine", &self.engine)
            .finish()
    }
}

impl Store {
    pub(crate) fn new(engine: crate::engine::Engine) -> Self {
        let inner: *mut wasm_store_t = unsafe { wasm_store_new(engine.as_v8().inner.engine) };
        let thread_id = std::thread::current().id();
        Self {
            inner,
            engine,
            thread_id,
            origin: crate::backend::v8::origin::OriginOwner::new(),
            reference_roots: Default::default(),
            #[cfg(feature = "experimental-async")]
            host_callbacks: Default::default(),
            #[cfg(feature = "experimental-async")]
            async_binding: crate::backend::v8::async_runtime::AsyncBinding::new(),
        }
    }

    pub(crate) fn engine(&self) -> &Engine {
        &self.engine
    }

    pub(crate) fn engine_mut(&mut self) -> &mut Engine {
        &mut self.engine
    }
}

impl Drop for Store {
    fn drop(&mut self) {
        use crate::backend::v8::references::retire_step;
        let unwinding = std::thread::panicking();
        self.origin.invalidate();
        // Take actual resources callback-free. Only empty fields remain after
        // the C++ Store-delete attempt, including when a prior stage panics.
        #[cfg(feature = "experimental-async")]
        let callbacks = std::mem::take(&mut self.host_callbacks);
        let mut roots = std::mem::take(&mut self.reference_roots);
        let raw = std::mem::replace(&mut self.inner, std::ptr::null_mut());
        let mut first = None;
        #[cfg(feature = "experimental-async")]
        retire_step(&mut first, unwinding, || drop(callbacks));
        retire_step(&mut first, unwinding, || roots.drain());
        retire_step(&mut first, unwinding, || unsafe {
            wasmer_v8_owned_store_delete(raw)
        });
        if let Some(first) = first {
            std::panic::resume_unwind(first);
        }
    }
}

impl AsEngineRef for Store {
    fn as_engine_ref(&self) -> EngineRef<'_> {
        EngineRef::new(&self.engine)
    }
}

impl crate::BackendStore {
    /// Consume [`self`] into [`crate::backend::v8::store::Store`].
    pub fn into_v8(self) -> crate::backend::v8::store::Store {
        match self {
            Self::V8(s) => s,
            _ => panic!("Not a `v8` store!"),
        }
    }

    /// Convert a reference to [`self`] into a reference [`crate::backend::v8::store::Store`].
    pub fn as_v8(&self) -> &crate::backend::v8::store::Store {
        match self {
            Self::V8(s) => s,
            _ => panic!("Not a `v8` store!"),
        }
    }

    /// Convert a mutable reference to [`self`] into a mutable reference [`crate::backend::v8::store::Store`].
    pub fn as_v8_mut(&mut self) -> &mut crate::backend::v8::store::Store {
        match self {
            Self::V8(s) => s,
            _ => panic!("Not a `v8` store!"),
        }
    }

    /// Return true if [`self`] is a store from the `v8` runtime.
    pub fn is_v8(&self) -> bool {
        matches!(self, Self::V8(_))
    }
}

impl crate::Store {
    /// Consume [`self`] into [`crate::backend::v8::store::Store`].
    pub(crate) fn into_v8(self) -> crate::backend::v8::store::Store {
        self.inner.store.into_v8()
    }

    /// Convert a reference to [`self`] into a reference [`crate::backend::v8::store::Store`].
    pub(crate) fn as_v8(&self) -> &crate::backend::v8::store::Store {
        self.inner.store.as_v8()
    }

    /// Convert a mutable reference to [`self`] into a mutable reference [`crate::backend::v8::store::Store`].
    pub(crate) fn as_v8_mut(&mut self) -> &mut crate::backend::v8::store::Store {
        self.inner.store.as_v8_mut()
    }

    /// Return true if [`self`] is a store from the `v8` runtime.
    pub fn is_v8(&self) -> bool {
        self.inner.store.is_v8()
    }
}

#[cfg(all(test, feature = "experimental-async"))]
mod tests {
    use crate::{AsStoreRef, Function, FunctionType, Store, Value, WeakStoreAsync};
    use std::{
        cell::RefCell,
        rc::Rc,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    struct Payload {
        name: &'static str,
        drops: Arc<AtomicUsize>,
        next: Option<Box<Payload>>,
    }
    impl Drop for Payload {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::SeqCst);
            if let Some(next) = self.next.take() {
                std::panic::resume_unwind(next);
            }
        }
    }
    struct Capture {
        weak: WeakStoreAsync,
        target: Function,
        origin: crate::backend::v8::origin::Origin,
        drops: Arc<AtomicUsize>,
        reentries: Arc<AtomicUsize>,
        payload: Option<Box<Payload>>,
    }
    impl Capture {
        fn touch(&self) {
            assert!(self.payload.is_some());
        }
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            assert!(
                self.origin.check_alive().is_err(),
                "invalidate dying A before user capture teardown"
            );
            self.drops.fetch_add(1, Ordering::SeqCst);
            let source = self
                .weak
                .upgrade()
                .expect("the separate original S remains live");
            let values =
                futures::executor::block_on(self.target.call_async(&source, vec![])).unwrap();
            assert_eq!(values[0].i32(), Some(29));
            self.reentries.fetch_add(1, Ordering::SeqCst);
            std::panic::resume_unwind(self.payload.take().unwrap());
        }
    }

    #[test]
    fn owned_cpp_store_drop_runs_actual_host_info_finalizer_after_rust_captures() {
        use crate::backend::v8::bindings::wasm_func_set_host_info_with_finalizer;
        use std::sync::atomic::AtomicBool;

        struct RustCapture(Arc<AtomicUsize>);
        impl RustCapture {
            fn touch(&self) {
                let _ = self.0.load(Ordering::SeqCst);
            }
        }
        impl Drop for RustCapture {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        struct Finalizer {
            rust_drops: Arc<AtomicUsize>,
            cpp_calls: Arc<AtomicUsize>,
            rust_before_cpp: Arc<AtomicBool>,
        }
        unsafe extern "C" fn finalize(data: *mut std::ffi::c_void) {
            // Sole actual C++ managed-data owner; never panic across C ABI.
            let data = unsafe { Box::from_raw(data.cast::<Finalizer>()) };
            data.rust_before_cpp.store(
                data.rust_drops.load(Ordering::SeqCst) == 1,
                Ordering::SeqCst,
            );
            data.cpp_calls.fetch_add(1, Ordering::SeqCst);
        }

        let rust_drops = Arc::new(AtomicUsize::new(0));
        let cpp_calls = Arc::new(AtomicUsize::new(0));
        let rust_before_cpp = Arc::new(AtomicBool::new(false));
        let mut store = crate::Store::new(crate::v8::V8::new());
        let capture = RustCapture(rust_drops.clone());
        let function = Function::new_async(&mut store, FunctionType::new([], []), move |_| {
            capture.touch();
            async { Ok(Vec::<Value>::new()) }
        });
        let finalizer = Box::new(Finalizer {
            rust_drops: rust_drops.clone(),
            cpp_calls: cpp_calls.clone(),
            rust_before_cpp: rust_before_cpp.clone(),
        });
        unsafe {
            wasm_func_set_host_info_with_finalizer(
                function.as_v8().handle.raw(),
                Box::into_raw(finalizer).cast(),
                Some(finalize),
            );
        }
        // This finite source-backed discriminator releases real reference
        // roots first. Merely invalidating a weak token or returning from the
        // C facade delete cannot satisfy its actual C++ finalizer assertion.
        store.as_v8_mut().reference_roots.drain();
        assert_eq!(cpp_calls.load(Ordering::SeqCst), 0);
        assert_eq!(rust_drops.load(Ordering::SeqCst), 0);
        drop(store);
        assert_eq!(cpp_calls.load(Ordering::SeqCst), 1);
        assert_eq!(rust_drops.load(Ordering::SeqCst), 1);
        assert!(rust_before_cpp.load(Ordering::SeqCst));
        // Expired Function destruction remains inert metadata-only.
        drop(function);
        let mut source = crate::Store::new(crate::v8::V8::new());
        let target = Function::new_typed_async(&mut source, || async { 31i32 });
        let source = source.into_async();
        let values = futures::executor::block_on(target.call_async(&source, vec![])).unwrap();
        assert_eq!(values[0].i32(), Some(31));
    }

    #[test]
    fn backend_arena_drains_actual_captures_and_preserves_first_or_active_panic() {
        for active in [false, true] {
            let mut source = Store::new(crate::v8::V8::new());
            let target = Function::new_typed_async(&mut source, || async { 29i32 });
            let source = source.into_async();
            let drops = Arc::new(AtomicUsize::new(0));
            let reentries = Arc::new(AtomicUsize::new(0));
            let first_drops = Arc::new(AtomicUsize::new(0));
            let second_drops = Arc::new(AtomicUsize::new(0));
            let nested_drops = Arc::new(AtomicUsize::new(0));
            let active_drops = Arc::new(AtomicUsize::new(0));
            let first = Box::new(Payload {
                name: "P",
                drops: first_drops.clone(),
                next: None,
            });
            let first_address = (&*first) as *const Payload as usize;
            let second = Box::new(Payload {
                name: "Q",
                drops: second_drops.clone(),
                next: Some(Box::new(Payload {
                    name: "N",
                    drops: nested_drops.clone(),
                    next: None,
                })),
            });
            let primary = active.then(|| {
                Box::new(Payload {
                    name: "G",
                    drops: active_drops.clone(),
                    next: None,
                })
            });
            let expected_address = primary
                .as_ref()
                .map(|value| (&**value) as *const Payload as usize)
                .unwrap_or(first_address);
            let saved = Rc::new(RefCell::new(None));
            let weak_slot = saved.clone();
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut store = Store::new(crate::v8::V8::new());
                let origin = store.as_store_ref().inner.store.as_v8().origin.capture();
                for payload in [first, second] {
                    let capture = Capture {
                        weak: source.downgrade(),
                        target: target.clone(),
                        origin: origin.clone(),
                        drops: drops.clone(),
                        reentries: reentries.clone(),
                        payload: Some(payload),
                    };
                    let _function =
                        Function::new_async(&mut store, FunctionType::new([], []), move |_| {
                            capture.touch();
                            async { Ok(Vec::<Value>::new()) }
                        });
                }
                let store = store.into_async();
                *weak_slot.borrow_mut() = Some(store.downgrade());
                if let Some(primary) = primary {
                    std::panic::resume_unwind(primary);
                }
                drop(store);
            }))
            .unwrap_err();
            assert_eq!(drops.load(Ordering::SeqCst), 2);
            assert_eq!(reentries.load(Ordering::SeqCst), 2);
            assert!(
                saved.borrow().as_ref().unwrap().upgrade().is_none(),
                "no Store/arena/Binding cycle"
            );
            assert_eq!(second_drops.load(Ordering::SeqCst), 1);
            assert_eq!(nested_drops.load(Ordering::SeqCst), 1);
            let first = outcome.downcast::<Payload>().unwrap();
            assert_eq!((&*first) as *const Payload as usize, expected_address);
            assert_eq!(first.name, if active { "G" } else { "P" });
            assert_eq!(first_drops.load(Ordering::SeqCst), usize::from(active));
            assert_eq!(active_drops.load(Ordering::SeqCst), 0);
            drop(first);
            assert_eq!(first_drops.load(Ordering::SeqCst), 1);
            assert_eq!(active_drops.load(Ordering::SeqCst), usize::from(active));
        }
    }
}

//! Data types, functions and traits for `sys` runtime's `Engine` implementation.
use crate::{
    BackendEngine,
    backend::v8::bindings::{wasm_engine_delete, wasm_engine_new, wasm_engine_t},
};
use std::sync::Arc;
use wasmer_types::{Features, target::Target};

// A handle to an engine, which we want to unsafely mark as Sync.
struct EngineCapsule(*mut wasm_engine_t);

impl Drop for EngineCapsule {
    fn drop(&mut self) {
        unsafe { wasm_engine_delete(self.0) }
    }
}

unsafe impl Sync for EngineCapsule {}
unsafe impl Send for EngineCapsule {}

static ENGINE: std::sync::OnceLock<std::sync::Mutex<EngineCapsule>> = std::sync::OnceLock::new();

#[derive(Debug)]
pub(crate) struct CApiEngine {
    pub(crate) engine: *mut wasm_engine_t,
}

impl Default for CApiEngine {
    fn default() -> Self {
        let engine = ENGINE
            .get_or_init(|| unsafe { std::sync::Mutex::new(EngineCapsule(wasm_engine_new())) });
        let engine = unsafe { engine.lock().unwrap().0 };
        Self { engine }
    }
}

impl Drop for CApiEngine {
    fn drop(&mut self) {}
}

/// The V8 engine.
#[derive(Clone, Debug, Default)]
pub struct Engine {
    pub(crate) inner: Arc<CApiEngine>,
}

impl Engine {
    /// Create a new instance of the `V8` engine.
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn deterministic_id(&self) -> String {
        String::from("v8")
    }

    pub(crate) fn artifact_format(&self) -> String {
        String::from("v8")
    }

    /// Returns the WebAssembly features supported by the V8 engine.
    pub fn supported_features() -> Features {
        // V8-specific features
        let mut features = Features::default();
        features.bulk_memory(true);
        features.reference_types(true);
        features.simd(true);
        features.threads(true);
        features.multi_value(true);
        features.exceptions(true);
        features
    }

    /// Returns the default features for the V8 engine.
    pub fn default_features() -> Features {
        Self::supported_features()
    }
}

unsafe impl Send for Engine {}
unsafe impl Sync for Engine {}

/// Returns the default engine for the JS engine
pub(crate) fn default_engine() -> Engine {
    Engine::default()
}

impl crate::Engine {
    /// Consume [`self`] into a [`crate::backend::v8::engine::Engine`].
    pub fn into_v8(self) -> crate::backend::v8::engine::Engine {
        match self.be {
            BackendEngine::V8(s) => s,
            _ => panic!("Not a `v8` engine!"),
        }
    }

    /// Convert a reference to [`self`] into a reference [`crate::backend::v8::engine::Engine`].
    pub fn as_v8(&self) -> &crate::backend::v8::engine::Engine {
        match &self.be {
            BackendEngine::V8(s) => s,
            _ => panic!("Not a `v8` engine!"),
        }
    }

    /// Convert a mutable reference to [`self`] into a mutable reference [`crate::backend::v8::engine::Engine`].
    pub fn as_v8_mut(&mut self) -> &mut crate::backend::v8::engine::Engine {
        match self.be {
            BackendEngine::V8(ref mut s) => s,
            _ => panic!("Not a `v8` engine!"),
        }
    }

    /// Return true if [`self`] is an engine from the `v8` runtime.
    pub fn is_v8(&self) -> bool {
        matches!(self.be, BackendEngine::V8(_))
    }
}

impl From<Engine> for crate::Engine {
    fn from(value: Engine) -> Self {
        Self {
            be: BackendEngine::V8(value),
            id: Self::atomic_next_engine_id(),
        }
    }
}

#[cfg(test)]
mod lifetime_tests {
    use super::*;
    use crate::{Function, Imports, Instance, Module, Store, Value};

    fn guest(engine: Engine) -> (Store, Module, Instance, Function) {
        let mut store = Store::new(engine);
        let bytes = wat::parse_str(
            "(module (func (export \"run\") (param i32) (result i32) local.get 0 i32.const 7 i32.add))",
        ).unwrap();
        let module = Module::new(&store, bytes).unwrap();
        let instance = Instance::new(&mut store, &module, &Imports::new()).unwrap();
        let run = instance.exports.get_function("run").unwrap().clone();
        (store, module, instance, run)
    }

    #[test]
    fn process_engine_survives_independent_facades_stores_and_engine_mut_replacement() {
        const TEST: &str = "backend::v8::entities::engine::lifetime_tests::process_engine_survives_independent_facades_stores_and_engine_mut_replacement";
        const CHILD: &str = "WASMER_V8_PROCESS_ENGINE_LIFETIME_CHILD";
        if std::env::var_os(CHILD).is_none() {
            struct Owned(std::process::Child);
            impl Drop for Owned {
                fn drop(&mut self) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
            let mut child = Owned(
                std::process::Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture"])
                    .env(CHILD, "1")
                    .spawn()
                    .unwrap(),
            );
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
            loop {
                match child.0.try_wait() {
                    Ok(Some(status)) => {
                        assert!(status.success(), "Engine lifetime child failed: {status}");
                        return;
                    }
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    status => panic!("Engine lifetime did not finish: {status:?}"),
                }
            }
        }
        let first = Engine::new();
        let second = Engine::new();
        let actual = first.inner.engine;
        assert!(!actual.is_null());
        assert!(
            !Arc::ptr_eq(&first.inner, &second.inner),
            "independent Rust facade owners"
        );
        assert_eq!(actual, second.inner.engine, "one actual process Engine");
        let (mut a, module_a, instance_a, run_a) = guest(first.clone());
        let (mut b, module_b, instance_b, run_b) = guest(second.clone());
        drop(first);
        drop(second);
        assert_eq!(
            run_a.call(&mut a, &[Value::I32(5)]).unwrap()[0].i32(),
            Some(12)
        );
        assert_eq!(
            run_b.call(&mut b, &[Value::I32(11)]).unwrap()[0].i32(),
            Some(18)
        );
        drop(instance_a);
        drop(module_a);
        drop(a);
        drop(run_a);
        assert_eq!(
            run_b.call(&mut b, &[Value::I32(6)]).unwrap()[0].i32(),
            Some(13)
        );

        let replacement = Engine::new();
        assert_eq!(replacement.inner.engine, actual);
        *b.engine_mut() = replacement.into();
        assert_eq!(
            run_b.call(&mut b, &[Value::I32(19)]).unwrap()[0].i32(),
            Some(26)
        );
        drop(instance_b);
        drop(module_b);
        drop(b);
        drop(run_b);

        // Every earlier facade and Store is gone. A later facade still borrows
        // the SAME intentional process owner, not a recreated V8 platform.
        let later = Engine::new();
        assert_eq!(later.inner.engine, actual);
        let (mut c, module_c, instance_c, run_c) = guest(later);
        assert_eq!(
            run_c.call(&mut c, &[Value::I32(26)]).unwrap()[0].i32(),
            Some(33)
        );
        drop(instance_c);
        drop(module_c);
        drop(c);
        drop(run_c);
        let last_facade = Engine::new();
        assert_eq!(last_facade.inner.engine, actual);
        drop(last_facade);
        assert_eq!(ENGINE.get().unwrap().lock().unwrap().0, actual);
        // This is wrapper/Store reuse evidence, not process-global disposal.
    }
}

//! Actual C++ reference owners. Transferable handles retain only inert metadata;
//! these roots move with their one real backend Store and die before its isolate.

use super::bindings::*;
use std::cell::RefCell;

pub(crate) struct OwnedValue(Option<wasm_val_t>);
impl OwnedValue {
    /// Takes the caller's actual owned value, never a borrowed callback value.
    pub(crate) fn adopt(value: wasm_val_t) -> Self {
        Self(Some(value))
    }
    pub(crate) fn reference(raw: *mut wasm_ref_t, function: bool) -> Self {
        Self::adopt(wasm_val_t {
            kind: if function {
                wasm_valkind_enum_WASM_FUNCREF
            } else {
                wasm_valkind_enum_WASM_EXTERNREF
            } as _,
            of: wasm_val_t__bindgen_ty_1 { ref_: raw },
        })
    }
    pub(crate) fn copy(value: &wasm_val_t) -> Self {
        let mut copied = zero();
        unsafe { wasm_val_copy(&mut copied, value) };
        let owned = Self::adopt(copied);
        if matches!(
            value.kind as _,
            wasm_valkind_enum_WASM_FUNCREF | wasm_valkind_enum_WASM_EXTERNREF
        ) && !unsafe { value.of.ref_ }.is_null()
        {
            assert!(
                !owned.reference_ptr().is_null(),
                "V8 reference copy returned null"
            );
        }
        owned
    }
    pub(crate) fn value(&self) -> &wasm_val_t {
        self.0.as_ref().unwrap()
    }
    pub(crate) fn take(&mut self) -> wasm_val_t {
        self.0
            .take()
            .expect("V8 owned value was already transferred")
    }
    fn reference_ptr(&self) -> *mut wasm_ref_t {
        unsafe { self.0.as_ref().unwrap().of.ref_ }
    }
    pub(crate) fn replace(mut self, destination: &mut wasm_val_t) {
        // Publish the SAME owner before disposing any previous owned value.
        let old = std::mem::replace(destination, self.take());
        drop(Self::adopt(old));
    }
}
impl Drop for OwnedValue {
    fn drop(&mut self) {
        if let Some(mut value) = self.0.take() {
            // The generated ref_delete uses C++ delete x. The verified Val
            // route instead invokes own<Ref> -> Ref::destroy -> RefImpl dtor.
            // wasm_val_delete does not clear its input: disarm FIRST.
            unsafe { wasm_val_delete(&mut value) };
        }
    }
}

fn zero() -> wasm_val_t {
    wasm_val_t {
        kind: wasm_valkind_enum_WASM_I32 as _,
        of: wasm_val_t__bindgen_ty_1 { i32_: 0 },
    }
}

#[derive(Default)]
pub(crate) struct ReferenceRoots(RefCell<Vec<OwnedValue>>);
impl ReferenceRoots {
    fn existing(&self, raw: *mut wasm_ref_t) -> Option<*mut wasm_ref_t> {
        // Never hold a RefCell borrow across C++ operations or finalizers.
        let candidates = self
            .0
            .borrow()
            .iter()
            .map(OwnedValue::reference_ptr)
            .collect::<Vec<_>>();
        candidates
            .into_iter()
            .find(|candidate| unsafe { wasm_ref_same(*candidate, raw) })
    }
    pub(crate) fn adopt(&self, value: OwnedValue) -> *mut wasm_ref_t {
        let raw = value.reference_ptr();
        assert!(!raw.is_null(), "cannot retain a null V8 reference");
        if let Some(existing) = self.existing(raw) {
            return existing;
        }
        self.0.borrow_mut().push(value);
        raw
    }
    pub(crate) fn copy(&self, raw: *mut wasm_ref_t, function: bool) -> *mut wasm_ref_t {
        assert!(!raw.is_null(), "cannot copy a null V8 reference");
        if let Some(existing) = self.existing(raw) {
            return existing;
        }
        // Copy before publication; the actual copy remains RAII-owned through
        // deduplication, including any finite reentry while C++ runs.
        let borrowed = wasm_val_t {
            kind: if function {
                wasm_valkind_enum_WASM_FUNCREF
            } else {
                wasm_valkind_enum_WASM_EXTERNREF
            } as _,
            of: wasm_val_t__bindgen_ty_1 { ref_: raw },
        };
        self.adopt(OwnedValue::copy(&borrowed))
    }
    pub(crate) fn drain(&mut self) {
        let values = std::mem::take(self.0.get_mut());
        let unwinding = std::thread::panicking();
        let mut first = None;
        for value in values {
            retire_step(&mut first, unwinding, || drop(value));
        }
        if let Some(first) = first {
            std::panic::resume_unwind(first);
        }
    }
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.borrow().len()
    }
}

macro_rules! owned_handle {
    ($($method:ident, $ty:ty, $as_ref:ident, $from_ref:ident, $function:expr;)*) => { $(
        impl ReferenceRoots {
            pub(crate) fn $method(&self, owned: *mut $ty) -> *mut $ty {
                assert!(!owned.is_null(), "V8 owned constructor returned null");
                let reference = unsafe { $as_ref(owned) };
                let retained = self.adopt(OwnedValue::reference(reference, $function));
                unsafe { $from_ref(retained) }
            }
        }
    )* };
}
owned_handle! {
    function, wasm_func_t, wasm_func_as_ref, wasm_ref_as_func, true;
    global, wasm_global_t, wasm_global_as_ref, wasm_ref_as_global, false;
    table, wasm_table_t, wasm_table_as_ref, wasm_ref_as_table, false;
    memory, wasm_memory_t, wasm_memory_as_ref, wasm_ref_as_memory, false;
    tag, wasm_tag_t, wasm_tag_as_ref, wasm_ref_as_tag, false;
}

// A finite owner teardown keeps its exact first value outside each subsequent
// callback. It is not a Store-wide receiver and cannot be consumed by reentry.
pub(crate) fn retire_step(
    first: &mut Option<Box<dyn std::any::Any + Send>>,
    unwinding: bool,
    step: impl FnOnce(),
) {
    if let Err(mut panic) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(step)) {
        if first.is_none() && !unwinding {
            *first = Some(panic);
        } else {
            loop {
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(panic))) {
                    Ok(()) => break,
                    Err(next) => panic = next,
                }
            }
        }
    }
}

/// Owns its actual vector and every contained value, with explicit safe empty
/// slots before a trap/panic or an InvokeAgain can leave partial results.
pub(crate) struct Values(pub(crate) wasm_val_vec_t);
impl Values {
    pub(crate) fn empty(length: usize) -> Self {
        let mut raw = Default::default();
        unsafe { wasm_val_vec_new_uninitialized(&mut raw, length) };
        let mut values = Self(raw);
        for slot in values.as_mut_slice() {
            *slot = zero();
        }
        values
    }
    pub(crate) fn from_values(
        values: &[crate::Value],
        expected: &[wasmer_types::Type],
    ) -> Result<Self, crate::RuntimeError> {
        use super::utils::convert::IntoCApiValue;
        super::utils::convert::validate_values(values, expected)?;
        let mut result = Self::empty(values.len());
        for ((slot, value), ty) in result.as_mut_slice().iter_mut().zip(values).zip(expected) {
            OwnedValue::copy(&value.clone().into_cv(*ty)?).replace(slot);
        }
        Ok(result)
    }
    pub(crate) fn as_slice(&self) -> &[wasm_val_t] {
        if self.0.size == 0 {
            &[]
        } else {
            unsafe { std::slice::from_raw_parts(self.0.data, self.0.size) }
        }
    }
    pub(crate) fn as_mut_slice(&mut self) -> &mut [wasm_val_t] {
        if self.0.size == 0 {
            &mut []
        } else {
            unsafe { std::slice::from_raw_parts_mut(self.0.data, self.0.size) }
        }
    }
    /// Pin-specific: b0a's wasm_func_call adopts even its const input vector.
    /// Finish all fallible work first, then transfer immediately to that call.
    pub(crate) fn into_call_arguments(self) -> wasm_val_vec_t {
        let owner = std::mem::ManuallyDrop::new(self);
        wasm_val_vec_t {
            size: owner.0.size,
            data: owner.0.data,
        }
    }
    pub(crate) fn clear(&mut self) {
        for slot in self.as_mut_slice() {
            let old = std::mem::replace(slot, zero());
            drop(OwnedValue::adopt(old));
        }
    }
}
impl Drop for Values {
    fn drop(&mut self) {
        // The verified vec<Val> destruction also uses Val::reset/Ref::destroy.
        unsafe { wasm_val_vec_delete(&mut self.0) };
    }
}

pub(crate) struct Exports(pub(crate) wasm_extern_vec_t);
impl Exports {
    pub(crate) fn take(&mut self, index: usize) -> OwnedValue {
        let slot = unsafe { &mut *self.0.data.add(index) };
        let raw = std::mem::replace(slot, std::ptr::null_mut());
        assert!(!raw.is_null(), "V8 export was already transferred");
        OwnedValue::reference(unsafe { wasm_extern_as_ref(raw) }, false)
    }
}
impl Drop for Exports {
    fn drop(&mut self) {
        for index in 0..self.0.size {
            let slot = unsafe { &mut *self.0.data.add(index) };
            let raw = std::mem::replace(slot, std::ptr::null_mut());
            if !raw.is_null() {
                drop(OwnedValue::reference(
                    unsafe { wasm_extern_as_ref(raw) },
                    false,
                ));
            }
        }
        // Every element has been disarmed; only vector storage remains.
        unsafe { wasm_extern_vec_delete(&mut self.0) };
    }
}

// C++ represents an empty vector with a null data pointer. Rust slices still
// require a nonnull aligned pointer at length zero, so preserve that boundary.
pub(crate) fn borrowed_values(raw: &wasm_val_vec_t) -> &[wasm_val_t] {
    if raw.size == 0 {
        &[]
    } else {
        unsafe { std::slice::from_raw_parts(raw.data, raw.size) }
    }
}
pub(crate) fn borrowed_values_mut(raw: &mut wasm_val_vec_t) -> &mut [wasm_val_t] {
    if raw.size == 0 {
        &mut []
    } else {
        unsafe { std::slice::from_raw_parts_mut(raw.data, raw.size) }
    }
}

#[cfg(all(test, feature = "experimental-async"))]
mod tests {
    use super::*;
    use crate::{
        AsStoreAsync, AsStoreRef, Function, FunctionType, Instance, Module, Store, StoreAsync,
        Type, Value,
    };
    use std::{
        cell::RefCell,
        rc::Rc,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    fn roots(store: &StoreAsync) -> usize {
        let guard = futures::executor::block_on(store.read_lock());
        guard
            .as_store_ref()
            .inner
            .store
            .as_v8()
            .reference_roots
            .len()
    }
    fn call(function: &Function, store: &StoreAsync, values: Vec<Value>) -> Box<[Value]> {
        futures::executor::block_on(function.call_async(store, values)).unwrap()
    }
    fn fixture() -> (Store, Instance) {
        let mut store = Store::new(crate::v8::V8::new());
        let module = Module::new(
            &store,
            wat::parse_str(
                r#"(module
            (func $target (export "target") (result i32) i32.const 37)
            (table (export "table") 1 funcref)
            (elem (i32.const 0) $target)
            (global (export "global") (mut funcref) (ref.func $target))
            (func (export "get") (result funcref) ref.func $target)
            (func (export "pass") (param funcref) (result funcref) local.get 0)
            (func (export "none") (result funcref) ref.null func)
            (func (export "multi") (result funcref i32 funcref)
                ref.func $target i32.const 42 ref.func $target)
            (func (export "trap") (param funcref) (result funcref) unreachable))"#,
            )
            .unwrap(),
        )
        .unwrap();
        let instance = Instance::new(&mut store, &module, &crate::Imports::new()).unwrap();
        (store, instance)
    }

    #[test]
    fn returned_table_global_and_argument_funcrefs_keep_one_canonical_owned_root() {
        let (store, instance) = fixture();
        let target = instance.exports.get_function("target").unwrap().clone();
        let get = instance.exports.get_function("get").unwrap().clone();
        let pass = instance.exports.get_function("pass").unwrap().clone();
        let none = instance.exports.get_function("none").unwrap().clone();
        let multi = instance.exports.get_function("multi").unwrap().clone();
        let table = instance.exports.get_table("table").unwrap().clone();
        let global = instance.exports.get_global("global").unwrap().clone();
        let store = store.into_async();
        let baseline = roots(&store);
        for _ in 0..32 {
            let returned = call(&get, &store, vec![])[0]
                .unwrap_funcref()
                .as_ref()
                .unwrap()
                .clone();
            // The result vector has already been destroyed at this point.
            assert_eq!(returned, target);
            assert_eq!(call(&returned, &store, vec![])[0].i32(), Some(37));
            let passed = call(&pass, &store, vec![Value::FuncRef(Some(target.clone()))]);
            assert_eq!(passed[0].unwrap_funcref().as_ref().unwrap(), &target);
            // Argument vector destruction must not delete target's own wrapper.
            assert_eq!(call(&target, &store, vec![])[0].i32(), Some(37));
            assert!(call(&none, &store, vec![])[0].unwrap_funcref().is_none());
            let multiple = call(&multi, &store, vec![]);
            assert_eq!(multiple[1].i32(), Some(42));
            assert_eq!(multiple[0].unwrap_funcref(), multiple[2].unwrap_funcref());
            {
                let mut guard = futures::executor::block_on(store.write_lock());
                let entry = table.get(&mut guard, 0).unwrap();
                let current = global.get(&mut guard);
                assert_eq!(entry.unwrap_funcref().as_ref().unwrap(), &target);
                assert_eq!(current.unwrap_funcref().as_ref().unwrap(), &target);
            }
            assert_eq!(
                roots(&store),
                baseline,
                "no per-call/return reference history"
            );
        }
        drop(instance);
        let origin = target.as_v8().handle.origin.clone();
        drop(store);
        assert!(origin.check_alive().is_err());
        // Metadata may still move/clone/drop; no C++ operation occurs here.
        let stale = std::thread::spawn(move || target.clone()).join().unwrap();
        let _copy = stale.clone();
    }

    #[test]
    fn callback_argument_escape_survives_input_cleanup_and_trap_and_first_panic() {
        let (mut store, instance) = fixture();
        let target = instance.exports.get_function("target").unwrap().clone();
        let trapped = instance.exports.get_function("trap").unwrap().clone();
        let saved = Rc::new(RefCell::new(None));
        let destination = saved.clone();
        let capture = Function::new_async(
            &mut store,
            FunctionType::new([Type::FuncRef], []),
            move |values| {
                *destination.borrow_mut() = values[0].unwrap_funcref().clone();
                async { Ok(vec![]) }
            },
        );
        struct Payload(Arc<AtomicUsize>);
        impl Drop for Payload {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let payload = Box::new(Payload(drops.clone()));
        let address = (&*payload) as *const Payload as usize;
        let pending = Rc::new(RefCell::new(Some(payload)));
        let failing = Function::new_async(
            &mut store,
            FunctionType::new([Type::FuncRef], [Type::FuncRef]),
            move |_| {
                let mut payload = pending.borrow_mut().take();
                std::future::poll_fn(
                    move |_| -> std::task::Poll<Result<Vec<Value>, crate::RuntimeError>> {
                        std::panic::resume_unwind(payload.take().unwrap())
                    },
                )
            },
        );
        let store = store.into_async();
        let baseline = roots(&store);
        call(&capture, &store, vec![Value::FuncRef(Some(target.clone()))]);
        let escaped = saved.borrow().as_ref().unwrap().clone();
        assert_eq!(call(&escaped, &store, vec![])[0].i32(), Some(37));
        assert!(
            futures::executor::block_on(
                trapped.call_async(&store, vec![Value::FuncRef(Some(target.clone()))])
            )
            .is_err()
        );
        assert_eq!(call(&target, &store, vec![])[0].i32(), Some(37));
        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            futures::executor::block_on(
                failing.call_async(&store, vec![Value::FuncRef(Some(target.clone()))]),
            )
        }))
        .unwrap_err();
        let payload = panic.downcast::<Payload>().unwrap();
        assert_eq!((&*payload) as *const Payload as usize, address);
        assert_eq!(drops.load(Ordering::SeqCst), 0);
        drop(payload);
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(call(&escaped, &store, vec![])[0].i32(), Some(37));
        assert_eq!(roots(&store), baseline);
        drop(instance);
    }

    fn replay_once(store: &mut impl crate::AsStoreMut, callbacks: Arc<AtomicUsize>) {
        store.as_store_mut().on_called(move |mut current| {
            callbacks.fetch_add(1, Ordering::SeqCst);
            current.on_called(move |_| {
                callbacks.fetch_add(1, Ordering::SeqCst);
                Ok(wasmer_types::OnCalledAction::Finish)
            });
            Ok(wasmer_types::OnCalledAction::InvokeAgain)
        });
    }

    #[test]
    fn each_physical_sync_typed_and_async_replay_transfers_fresh_owned_arguments() {
        use crate::AsStoreMut;
        let mut store = Store::new(crate::v8::V8::new());
        let module = Module::new(
            &store,
            wat::parse_str(
                r#"(module
            (global $runs (export "runs") (mut i32) (i32.const 0))
            (func $tick global.get $runs i32.const 1 i32.add global.set $runs)
            (func $target (export "target") (result i32) i32.const 37)
            (elem declare func $target)
            (func (export "scalar") (param i32) (result i32) call $tick local.get 0)
            (func (export "reference") (param funcref) (result funcref) call $tick local.get 0)
            (func (export "multiple") (param i32 funcref funcref) (result i32 funcref funcref)
                call $tick local.get 0 local.get 1 local.get 2)
            (func (export "typed") (param i32 i32) (result i32 i32)
                call $tick local.get 1 local.get 0))"#,
            )
            .unwrap(),
        )
        .unwrap();
        let instance = Instance::new(&mut store, &module, &crate::Imports::new()).unwrap();
        let scalar = instance.exports.get_function("scalar").unwrap().clone();
        let reference = instance.exports.get_function("reference").unwrap().clone();
        let multiple = instance.exports.get_function("multiple").unwrap().clone();
        let target = instance.exports.get_function("target").unwrap().clone();
        let runs = instance.exports.get_global("runs").unwrap().clone();
        let typed = instance
            .exports
            .get_typed_function::<(i32, i32), (i32, i32)>(&store, "typed")
            .unwrap();
        let callbacks = Arc::new(AtomicUsize::new(0));
        replay_once(&mut store, callbacks.clone());
        assert_eq!(
            scalar.call(&mut store, &[Value::I32(12)]).unwrap()[0].i32(),
            Some(12)
        );
        assert_eq!(runs.get(&mut store).i32(), Some(2));
        replay_once(&mut store, callbacks.clone());
        assert_eq!(typed.call(&mut store, 11, 17).unwrap(), (17, 11));
        assert_eq!(runs.get(&mut store).i32(), Some(4));
        let store = store.into_async();
        let baseline = roots(&store);
        for (function, arguments) in [
            (&reference, vec![Value::FuncRef(None)]),
            (&reference, vec![Value::FuncRef(Some(target.clone()))]),
            (
                &multiple,
                vec![
                    Value::I32(7),
                    Value::FuncRef(None),
                    Value::FuncRef(Some(target.clone())),
                ],
            ),
        ] {
            {
                let mut guard = futures::executor::block_on(store.write_lock());
                replay_once(&mut guard, callbacks.clone());
            }
            let result = call(function, &store, arguments.clone());
            assert_eq!(result.as_ref(), arguments.as_slice());
            assert_eq!(roots(&store), baseline);
            assert_eq!(call(&target, &store, vec![])[0].i32(), Some(37));
        }
        let mut guard = futures::executor::block_on(store.write_lock());
        assert_eq!(runs.get(&mut guard).i32(), Some(10));
        assert_eq!(callbacks.load(Ordering::SeqCst), 10);
        drop(guard);
        drop(instance);
    }

    #[test]
    fn checked_origins_reject_wrong_backings_before_raw_conversions_and_imports() {
        use crate::{
            AsStoreMut, Extern, Global, Memory, MemoryType, Table, TableType, Tag, VMExternToExtern,
        };
        let mut a = Store::new(crate::v8::V8::new());
        let mut b = Store::new(crate::v8::V8::new());
        let function = Function::new_typed(&mut a, || 17i32);
        let table = Table::new(
            &mut a,
            TableType::new(Type::FuncRef, 1, Some(1)),
            Value::FuncRef(Some(function.clone())),
        )
        .unwrap();
        let global = Global::new_mut(&mut a, Value::FuncRef(Some(function.clone())));
        let memory = Memory::new(&mut a, MemoryType::new(1, Some(1), false)).unwrap();
        let tag = Tag::new(&mut a, [Type::I32]);
        let copied = Extern::Function(function.clone());
        let foreign = copied.to_vm_extern();
        assert!(Extern::try_from_vm_extern(&mut b, foreign).is_err());
        // A duplicate StoreId cannot make this an actual V8 backing match.
        let id = a.id();
        b.as_store_mut().objects_mut().as_v8_mut().set_id(id);
        assert!(Extern::try_from_vm_extern(&mut b, copied.to_vm_extern()).is_err());
        let legitimate = copied.to_vm_extern().to_extern(&mut a);
        assert_eq!(legitimate, Extern::Function(function.clone()));
        let raw = Extern::Function(function.clone())
            .to_vm_extern()
            .unwrap_v_8();
        let imported =
            unsafe { crate::vm::VMExtern::from_raw_v8_unchecked(&a, raw.raw().cast()) }.unwrap();
        assert_eq!(
            Extern::try_from_vm_extern(&mut a, imported).unwrap(),
            legitimate
        );
        assert!(
            table
                .set(&mut b, 0, Value::FuncRef(Some(function.clone())))
                .is_err()
        );
        assert!(
            global
                .set(&mut b, Value::FuncRef(Some(function.clone())))
                .is_err()
        );
        assert!(memory.grow_at_least(&mut b, 0).is_err());
        assert!(memory.reset(&mut b).is_err());
        assert!(memory.copy(&b).is_err());
        assert!(memory.as_shared(&b).is_none());
        for value in [
            Extern::Function(function),
            Extern::Table(table),
            Extern::Global(global),
            Extern::Memory(memory),
            Extern::Tag(tag),
        ] {
            assert!(!value.is_from_store(&b));
            let module = Module::new(
                &b,
                wat::parse_str("(module (import \"host\" \"target\" (func)))").unwrap(),
            )
            .unwrap();
            // Import origin rejection precedes even kind/signature conversion.
            let imports = crate::imports! { "host" => { "target" => value } };
            assert!(Instance::new(&mut b, &module, &imports).is_err());
        }
    }
}

// Candidate-provider gates: these deliberately INVOKE nonnull references. The
// original archive is a preserved negative reference, not their run target.
#[cfg(all(test, feature = "experimental-async"))]
mod fidelity_tests {
    use super::*;
    use crate::{
        AsStoreAsync, AsStoreMut, AsStoreRef, Function, FunctionType, Global, Imports, Instance,
        Module, RuntimeError, Store, StoreAsync, Table, TableType, Type, Value,
    };
    use std::{
        rc::Rc,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    fn owned_child(name: &str) -> bool {
        const CHILD: &str = "WASMER_V8_NONNULL_FIDELITY_CHILD";
        if std::env::var_os(CHILD).is_some() {
            return true;
        }
        struct Owned(std::process::Child);
        impl Drop for Owned {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = Owned(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", name, "--nocapture"])
                .env(CHILD, "1")
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            match child.0.try_wait() {
                Ok(Some(status)) => {
                    assert!(status.success(), "nonnull fidelity child failed: {status}");
                    return false;
                }
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(10))
                }
                status => panic!("nonnull fidelity did not finish: {status:?}"),
            }
        }
    }
    fn roots(store: &StoreAsync) -> usize {
        let guard = futures::executor::block_on(store.read_lock());
        guard
            .as_store_ref()
            .inner
            .store
            .as_v8()
            .reference_roots
            .len()
    }
    fn call(function: &Function, store: &StoreAsync, args: Vec<Value>) -> Box<[Value]> {
        futures::executor::block_on(function.call_async(store, args)).unwrap()
    }
    fn function(value: &Value) -> Function {
        value
            .unwrap_funcref()
            .as_ref()
            .expect("nonnull callable reference")
            .clone()
    }
    fn fixture() -> (Store, Instance) {
        let mut store = Store::new(crate::v8::V8::new());
        let mut source = String::from(
            r#"(module
            (type $unary (func (param i32) (result i32)))
            (func $a (export "a") (type $unary) local.get 0 i32.const 7 i32.add)
            (func $b (export "b") (type $unary) local.get 0 i32.const 19 i32.add)
            (table $refs (export "table") 4 8 funcref)
            (elem (i32.const 0) $a $b)
            (global $selected (export "global") (mut funcref) (ref.func $a))
            (global $calls (export "calls") (mut i32) (i32.const 0))
            (func (export "get") (result funcref) ref.func $a)
            (func (export "none") (result funcref) ref.null func)
            (func (export "invoke_arg") (param funcref i32) (result i32)
                global.get $calls i32.const 1 i32.add global.set $calls
                i32.const 2 local.get 0 table.set $refs
                local.get 1 i32.const 2 call_indirect (type $unary))
            (func (export "invoke_table") (param i32 i32) (result i32)
                local.get 1 local.get 0 call_indirect (type $unary))
            (func (export "invoke_global") (param i32) (result i32)
                i32.const 3 global.get $selected table.set $refs
                local.get 0 i32.const 3 call_indirect (type $unary))
        "#,
        );
        // These functions have NOT been exported/externalized before multi's
        // first conversion. Each result therefore exercises allocating canonical
        // externalization while all later raw references must already be rooted.
        for index in 0..16 {
            source.push_str(&format!(
                "(func $hidden{index} (type $unary) local.get 0 i32.const {} i32.add)\n",
                50 + index
            ));
        }
        source.push_str("(elem declare func");
        for index in 0..16 {
            source.push_str(&format!(" $hidden{index}"));
        }
        source.push_str(") (func (export \"multi\") (result");
        for _ in 0..16 {
            source.push_str(" funcref");
        }
        source.push(')');
        for index in 0..16 {
            source.push_str(&format!(" ref.func $hidden{index}"));
        }
        source.push_str("))");
        let module = Module::new(&store, wat::parse_str(source).unwrap()).unwrap();
        let instance = Instance::new(&mut store, &module, &Imports::new()).unwrap();
        (store, instance)
    }

    #[test]
    fn nonnull_return_and_guest_argument_are_callable_in_sync_typed_and_async_entries() {
        if !owned_child(
            "backend::v8::references::fidelity_tests::nonnull_return_and_guest_argument_are_callable_in_sync_typed_and_async_entries",
        ) {
            return;
        }
        let (mut store, instance) = fixture();
        let get = instance.exports.get_function("get").unwrap().clone();
        let target = instance.exports.get_function("a").unwrap().clone();
        let invoke = instance.exports.get_function("invoke_arg").unwrap().clone();
        let typed_get = instance
            .exports
            .get_typed_function::<(), Option<Function>>(&store, "get")
            .unwrap();
        let typed_invoke = instance
            .exports
            .get_typed_function::<(Option<Function>, i32), i32>(&store, "invoke_arg")
            .unwrap();
        let returned = function(&get.call(&mut store, &[]).unwrap()[0]);
        assert_eq!(
            returned.call(&mut store, &[Value::I32(5)]).unwrap()[0].i32(),
            Some(12)
        );
        assert_eq!(
            invoke
                .call(&mut store, &[Value::FuncRef(Some(returned)), Value::I32(9)])
                .unwrap()[0]
                .i32(),
            Some(16)
        );
        let typed_returned = typed_get.call(&mut store).unwrap().unwrap();
        assert_eq!(
            typed_returned
                .typed::<i32, i32>(&store)
                .unwrap()
                .call(&mut store, 6)
                .unwrap(),
            13
        );
        assert_eq!(
            typed_invoke
                .call(&mut store, Some(typed_returned), 10)
                .unwrap(),
            17
        );
        let store = store.into_async();
        let baseline = roots(&store);
        for _ in 0..16 {
            let returned = function(&call(&get, &store, vec![])[0]);
            assert_eq!(returned, target);
            assert_eq!(
                call(&returned, &store, vec![Value::I32(11)])[0].i32(),
                Some(18)
            );
            assert_eq!(
                call(
                    &invoke,
                    &store,
                    vec![Value::FuncRef(Some(returned)), Value::I32(12)]
                )[0]
                .i32(),
                Some(19)
            );
            let typed_returned = futures::executor::block_on(typed_get.call_async(&store))
                .unwrap()
                .unwrap();
            assert_eq!(
                futures::executor::block_on(typed_invoke.call_async(
                    &store,
                    Some(typed_returned),
                    13
                ))
                .unwrap(),
                20
            );
            assert_eq!(
                roots(&store),
                baseline,
                "copies/returns do not grow root history"
            );
        }
        drop(instance);
    }

    #[test]
    fn nonnull_guest_callback_arguments_escape_and_callback_results_execute_in_the_guest() {
        if !owned_child(
            "backend::v8::references::fidelity_tests::nonnull_guest_callback_arguments_escape_and_callback_results_execute_in_the_guest",
        ) {
            return;
        }
        let mut store = Store::new(crate::v8::V8::new());
        let returned = Function::new_typed(&mut store, |value: i32| value + 19);
        let saved = Arc::new(std::sync::Mutex::new(Vec::<Function>::new()));
        let sync_saved = saved.clone();
        let sync_returned = returned.clone();
        let sync_calls = Arc::new(AtomicUsize::new(0));
        let sync_count = sync_calls.clone();
        let sync = Function::new(
            &mut store,
            FunctionType::new([Type::FuncRef], [Type::FuncRef]),
            move |values| {
                sync_count.fetch_add(1, Ordering::SeqCst);
                sync_saved.lock().unwrap().push(function(&values[0]));
                Ok(vec![Value::FuncRef(Some(sync_returned.clone()))])
            },
        );
        let async_saved = saved.clone();
        let async_returned = returned.clone();
        let async_calls = Rc::new(std::cell::Cell::new(0));
        let async_count = async_calls.clone();
        let asynchronous = Function::new_async(
            &mut store,
            FunctionType::new([Type::FuncRef], [Type::FuncRef]),
            move |values| {
                async_count.set(async_count.get() + 1);
                async_saved.lock().unwrap().push(function(&values[0]));
                let result = async_returned.clone();
                async move { Ok(vec![Value::FuncRef(Some(result))]) }
            },
        );
        let module = Module::new(
            &store,
            wat::parse_str(
                r#"(module
            (type $unary (func (param i32) (result i32)))
            (import "host" "sync" (func $sync (param funcref) (result funcref)))
            (import "host" "async" (func $async (param funcref) (result funcref)))
            (func $guest (type $unary) local.get 0 i32.const 7 i32.add)
            (elem declare func $guest) (table $refs 1 funcref)
            (func (export "sync") (result i32)
                i32.const 0 ref.func $guest call $sync table.set $refs
                i32.const 11 i32.const 0 call_indirect (type $unary))
            (func (export "async") (result i32)
                i32.const 0 ref.func $guest call $async table.set $refs
                i32.const 13 i32.const 0 call_indirect (type $unary)))"#,
            )
            .unwrap(),
        )
        .unwrap();
        let instance = Instance::new(
            &mut store,
            &module,
            &crate::imports! { "host" => { "sync" => sync, "async" => asynchronous } },
        )
        .unwrap();
        let sync = instance.exports.get_function("sync").unwrap().clone();
        let asynchronous = instance.exports.get_function("async").unwrap().clone();
        assert_eq!(sync.call(&mut store, &[]).unwrap()[0].i32(), Some(30));
        let escaped_sync = saved.lock().unwrap()[0].clone();
        assert_eq!(
            escaped_sync.call(&mut store, &[Value::I32(5)]).unwrap()[0].i32(),
            Some(12)
        );
        let store = store.into_async();
        let baseline = roots(&store);
        assert_eq!(call(&asynchronous, &store, vec![])[0].i32(), Some(32));
        let escaped_async = saved.lock().unwrap()[1].clone();
        assert_eq!(
            call(&escaped_async, &store, vec![Value::I32(9)])[0].i32(),
            Some(16)
        );
        assert_eq!(
            call(&returned, &store, vec![Value::I32(10)])[0].i32(),
            Some(29)
        );
        assert_eq!(sync_calls.load(Ordering::SeqCst), 1);
        assert_eq!(async_calls.get(), 1);
        assert_eq!(
            roots(&store),
            baseline,
            "callback vectors release copies; escaped roots are canonical"
        );
        drop(instance);
    }

    #[test]
    fn nonnull_global_table_and_multi_results_survive_conversion_and_independent_cleanup() {
        if !owned_child(
            "backend::v8::references::fidelity_tests::nonnull_global_table_and_multi_results_survive_conversion_and_independent_cleanup",
        ) {
            return;
        }
        let (mut store, instance) = fixture();
        let a = instance.exports.get_function("a").unwrap().clone();
        let b = instance.exports.get_function("b").unwrap().clone();
        let invoke_global = instance
            .exports
            .get_function("invoke_global")
            .unwrap()
            .clone();
        let invoke_table = instance
            .exports
            .get_function("invoke_table")
            .unwrap()
            .clone();
        let table = instance.exports.get_table("table").unwrap().clone();
        let global = instance.exports.get_global("global").unwrap().clone();
        global
            .set(&mut store, Value::FuncRef(Some(b.clone())))
            .unwrap();
        assert_eq!(
            invoke_global.call(&mut store, &[Value::I32(3)]).unwrap()[0].i32(),
            Some(22)
        );
        let external_global = Global::new_mut(&mut store, Value::FuncRef(Some(b.clone())));
        let from_global = function(&external_global.get(&mut store));
        assert_eq!(
            from_global.call(&mut store, &[Value::I32(4)]).unwrap()[0].i32(),
            Some(23)
        );
        external_global
            .set(&mut store, Value::FuncRef(Some(a.clone())))
            .unwrap();
        let from_global = function(&external_global.get(&mut store));
        assert_eq!(
            from_global.call(&mut store, &[Value::I32(4)]).unwrap()[0].i32(),
            Some(11)
        );
        table
            .set(&mut store, 0, Value::FuncRef(Some(b.clone())))
            .unwrap();
        assert_eq!(
            invoke_table
                .call(&mut store, &[Value::I32(0), Value::I32(5)])
                .unwrap()[0]
                .i32(),
            Some(24)
        );
        let constructed = Table::new(
            &mut store,
            TableType::new(Type::FuncRef, 2, Some(4)),
            Value::FuncRef(Some(a.clone())),
        )
        .unwrap();
        for index in 0..2 {
            let decoded = function(&constructed.get(&mut store, index).unwrap());
            assert_eq!(
                decoded.call(&mut store, &[Value::I32(6)]).unwrap()[0].i32(),
                Some(13)
            );
        }
        assert_eq!(
            constructed
                .grow(&mut store, 2, Value::FuncRef(Some(b.clone())))
                .unwrap(),
            2
        );
        for index in 2..4 {
            let decoded = function(&constructed.get(&mut store, index).unwrap());
            assert_eq!(
                decoded.call(&mut store, &[Value::I32(6)]).unwrap()[0].i32(),
                Some(25)
            );
        }
        assert!(
            constructed
                .grow(&mut store, 1, Value::FuncRef(Some(a.clone())))
                .is_err()
        );
        assert_eq!(constructed.size(&store), 4);
        assert!(constructed.set(&mut store, 0, Value::I32(1)).is_err());
        assert!(external_global.set(&mut store, Value::I32(1)).is_err());
        external_global
            .set(&mut store, Value::FuncRef(None))
            .unwrap();
        assert!(external_global.get(&mut store).unwrap_funcref().is_none());
        external_global
            .set(&mut store, Value::FuncRef(Some(a.clone())))
            .unwrap();
        let null_global = Global::new_mut(&mut store, Value::FuncRef(None));
        assert!(null_global.get(&mut store).unwrap_funcref().is_none());
        null_global
            .set(&mut store, Value::FuncRef(Some(b.clone())))
            .unwrap();
        let decoded = function(&null_global.get(&mut store));
        assert_eq!(
            decoded.call(&mut store, &[Value::I32(7)]).unwrap()[0].i32(),
            Some(26)
        );
        let null_table = Table::new(
            &mut store,
            TableType::new(Type::FuncRef, 1, Some(3)),
            Value::FuncRef(None),
        )
        .unwrap();
        assert!(
            null_table
                .get(&mut store, 0)
                .unwrap()
                .unwrap_funcref()
                .is_none()
        );
        assert_eq!(
            null_table
                .grow(&mut store, 2, Value::FuncRef(None))
                .unwrap(),
            1
        );
        for index in 1..3 {
            assert!(
                null_table
                    .get(&mut store, index)
                    .unwrap()
                    .unwrap_funcref()
                    .is_none()
            );
        }
        null_table
            .set(&mut store, 2, Value::FuncRef(Some(a.clone())))
            .unwrap();
        let decoded = function(&null_table.get(&mut store, 2).unwrap());
        assert_eq!(
            decoded.call(&mut store, &[Value::I32(7)]).unwrap()[0].i32(),
            Some(14)
        );
        let multi = instance.exports.get_function("multi").unwrap().clone();
        let store = store.into_async();
        let outputs = call(&multi, &store, vec![]);
        assert_eq!(outputs.len(), 16);
        let independent = outputs.iter().map(function).collect::<Vec<_>>();
        drop(outputs); // every temporary owning result is actually gone
        for (index, target) in independent.iter().enumerate() {
            assert_eq!(
                call(target, &store, vec![Value::I32(2)])[0].i32(),
                Some(52 + index as i32)
            );
        }
        let baseline = roots(&store);
        for _ in 0..16 {
            let outputs = call(&multi, &store, vec![]);
            assert_eq!(outputs.len(), 16);
            for (index, value) in outputs.iter().enumerate() {
                let target = function(value);
                assert_eq!(
                    call(&target, &store, vec![Value::I32(3)])[0].i32(),
                    Some(53 + index as i32)
                );
            }
            assert_eq!(roots(&store), baseline);
        }
        assert_eq!(call(&a, &store, vec![Value::I32(7)])[0].i32(), Some(14));
        drop(independent);
        drop(instance);
    }

    #[test]
    fn nonnull_replay_null_and_rejected_values_preserve_targets_and_guest_admission() {
        if !owned_child(
            "backend::v8::references::fidelity_tests::nonnull_replay_null_and_rejected_values_preserve_targets_and_guest_admission",
        ) {
            return;
        }
        let (mut store, instance) = fixture();
        let a = instance.exports.get_function("a").unwrap().clone();
        let invoke = instance.exports.get_function("invoke_arg").unwrap().clone();
        let none = instance.exports.get_function("none").unwrap().clone();
        let calls = instance.exports.get_global("calls").unwrap().clone();
        let wrong_signature = Function::new_typed(&mut store, || 17i32);
        let callbacks = Arc::new(AtomicUsize::new(0));
        let count = callbacks.clone();
        store.as_store_mut().on_called(move |mut current| {
            count.fetch_add(1, Ordering::SeqCst);
            current.on_called(move |_| {
                count.fetch_add(1, Ordering::SeqCst);
                Ok(wasmer_types::OnCalledAction::Finish)
            });
            Ok(wasmer_types::OnCalledAction::InvokeAgain)
        });
        let args = [Value::FuncRef(Some(a.clone())), Value::I32(5)];
        assert_eq!(invoke.call(&mut store, &args).unwrap()[0].i32(), Some(12));
        assert_eq!(calls.get(&mut store).i32(), Some(2));
        assert_eq!(callbacks.load(Ordering::SeqCst), 2);
        assert_eq!(
            a.call(&mut store, &[Value::I32(6)]).unwrap()[0].i32(),
            Some(13)
        );
        assert!(
            invoke
                .call(&mut store, &[Value::I32(1), Value::I32(5)])
                .is_err()
        );
        assert!(invoke.call(&mut store, &[]).is_err());
        assert_eq!(
            calls.get(&mut store).i32(),
            Some(2),
            "wrong type/count rejected before guest work"
        );
        let mut foreign = Store::new(crate::v8::V8::new());
        let foreign_target = Function::new_typed(&mut foreign, |value: i32| value + 90);
        assert!(
            invoke
                .call(
                    &mut store,
                    &[Value::FuncRef(Some(foreign_target.clone())), Value::I32(5)]
                )
                .is_err()
        );
        assert_eq!(
            calls.get(&mut store).i32(),
            Some(2),
            "wrong origin also precedes guest work"
        );
        assert_eq!(
            foreign_target.call(&mut foreign, &[Value::I32(1)]).unwrap()[0].i32(),
            Some(91)
        );
        drop(foreign_target);
        drop(foreign);

        assert!(
            invoke
                .call(&mut store, &[Value::FuncRef(None), Value::I32(5)])
                .is_err()
        );
        assert!(
            invoke
                .call(
                    &mut store,
                    &[Value::FuncRef(Some(wrong_signature.clone())), Value::I32(5)]
                )
                .is_err()
        );
        assert_eq!(
            wrong_signature.call(&mut store, &[]).unwrap()[0].i32(),
            Some(17)
        );
        assert!(
            none.call(&mut store, &[]).unwrap()[0]
                .unwrap_funcref()
                .is_none()
        );
        let store = store.into_async();
        assert_eq!(
            call(
                &invoke,
                &store,
                vec![Value::FuncRef(Some(a.clone())), Value::I32(9)]
            )[0]
            .i32(),
            Some(16)
        );
        assert!(call(&none, &store, vec![])[0].unwrap_funcref().is_none());
        assert_eq!(call(&a, &store, vec![Value::I32(11)])[0].i32(), Some(18));
        drop(instance);
    }
    fn raw_argument_error(
        store: &mut Store,
        target: &Function,
        values: &[Value],
        supplied_types: &[Type],
    ) -> RuntimeError {
        use crate::backend::v8::error::{TrapCallScope, TrapMode};
        assert!(target.is_from_store(store));
        let result_count = target.ty(store).results().len();
        let scope = TrapCallScope::enter(store, TrapMode::BorrowedSync, None).unwrap();
        let arguments = Values::from_values(values, supplied_types).unwrap();
        let mut results = Values::empty(result_count);
        let raw_target = target.as_v8().handle.raw();
        let raw_results = &mut results.0;
        // Deliberately bypass the Rust signature precheck, but retain its exact
        // proven C++ OWN argument-vector transfer; no guessed reference casts.
        let raw_arguments = arguments.into_call_arguments();
        let trap = unsafe { wasm_func_call(raw_target, &raw_arguments, raw_results) };
        assert!(!trap.is_null(), "provider must reject malformed arguments");
        let error: RuntimeError = scope.decode_owned(trap).into();
        scope.dispose(|| drop(results));
        scope.finish_outcome(error)
    }

    #[test]
    fn provider_rejects_wrong_reference_kind_and_count_before_guest_work() {
        if !owned_child(
            "backend::v8::references::fidelity_tests::provider_rejects_wrong_reference_kind_and_count_before_guest_work",
        ) {
            return;
        }
        let (mut store, instance) = fixture();
        let a = instance.exports.get_function("a").unwrap().clone();
        let invoke = instance.exports.get_function("invoke_arg").unwrap().clone();
        let calls = instance.exports.get_global("calls").unwrap().clone();
        let error = raw_argument_error(
            &mut store,
            &invoke,
            &[Value::I32(1), Value::I32(5)],
            &[Type::I32, Type::I32],
        );
        assert!(error.to_string().contains("C API call argument type mismatch"));
        assert_eq!(calls.get(&mut store).i32(), Some(0));
        let error = raw_argument_error(
            &mut store,
            &invoke,
            &[Value::FuncRef(Some(a.clone()))],
            &[Type::FuncRef],
        );
        assert!(error.to_string().contains("C API call arity mismatch"));
        assert_eq!(calls.get(&mut store).i32(), Some(0));
        assert_eq!(
            a.call(&mut store, &[Value::I32(8)]).unwrap()[0].i32(),
            Some(15)
        );
        assert_eq!(
            invoke
                .call(
                    &mut store,
                    &[Value::FuncRef(Some(a.clone())), Value::I32(9)]
                )
                .unwrap()[0]
                .i32(),
            Some(16)
        );
        assert_eq!(calls.get(&mut store).i32(), Some(1));
        drop(instance);
    }
}

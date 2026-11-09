mod env;
use super::origin::{Handle, Origin};
use std::sync::Arc;

pub use env::*;

use super::{
    bindings::*, entities::function::env::FunctionEnv, function::Function, global::Global,
    memory::Memory, table::Table,
};
use crate::{
    AsStoreMut, BackendFunction, BackendGlobal, BackendMemory, BackendTable, Extern,
    v8::store::Store,
};
use wasmer_types::{MemoryError, RawValue};

pub use super::error::Trap;

pub(crate) type VMExtern = Handle<wasm_extern_t>;

// No EH for now.
pub(crate) type VMException = ();
pub(crate) type VMTag = Handle<wasm_tag_t>;
pub(crate) type VMExternTag = Handle<wasm_tag_t>;

pub(crate) type VMFunction = Handle<wasm_func_t>;
pub(crate) type VMFunctionBody = ();
pub(crate) type VMFunctionCallback = *mut ::std::os::raw::c_void;
pub(crate) type VMTrampoline = *mut ::std::os::raw::c_void;
pub(crate) type VMExternFunction = Handle<wasm_func_t>;

pub(crate) type VMGlobal = Handle<wasm_global_t>;
pub(crate) type VMExternGlobal = Handle<wasm_global_t>;

pub(crate) type VMTable = Handle<wasm_table_t>;
pub(crate) type VMExternTable = Handle<wasm_table_t>;

pub(crate) type VMInstance = *mut wasm_instance_t;

pub(crate) type VMExternObj = ();
pub(crate) type VMConfig = ();

impl VMExtern {
    pub(crate) fn try_to_extern(
        self,
        store: &mut impl AsStoreMut,
    ) -> Result<Extern, crate::RuntimeError> {
        let mut actual = store.as_store_mut();
        let raw = self.checked_raw(&actual)?;
        let backend = actual.inner.store.as_v8();
        let retained = backend
            .reference_roots
            .copy(unsafe { wasm_extern_as_ref(raw) }, false);
        let raw = unsafe { wasm_ref_as_extern(retained) };
        // Provenance is checked BEFORE querying even the raw kind.
        let kind = unsafe { wasm_extern_kind(raw) };
        let origin = self.origin.clone();
        Ok(match kind as wasm_valkind_enum {
            wasm_externkind_enum_WASM_EXTERN_FUNC => {
                let raw = unsafe { wasm_extern_as_func(raw) };
                Extern::Function(crate::Function::from_vm_extern(
                    &mut actual,
                    crate::vm::VMExternFunction::V8(Handle::new(raw, origin)),
                ))
            }
            wasm_externkind_enum_WASM_EXTERN_GLOBAL => {
                let raw = unsafe { wasm_extern_as_global(raw) };
                Extern::Global(crate::Global::from_vm_extern(
                    &mut actual,
                    crate::vm::VMExternGlobal::V8(Handle::new(raw, origin)),
                ))
            }
            wasm_externkind_enum_WASM_EXTERN_TABLE => {
                let raw = unsafe { wasm_extern_as_table(raw) };
                Extern::Table(crate::Table::from_vm_extern(
                    &mut actual,
                    crate::vm::VMExternTable::V8(Handle::new(raw, origin)),
                ))
            }
            wasm_externkind_enum_WASM_EXTERN_MEMORY => {
                let raw = unsafe { wasm_extern_as_memory(raw) };
                Extern::Memory(crate::Memory::from_vm_extern(
                    &mut actual,
                    crate::vm::VMExternMemory::V8(Handle::new(raw, origin)),
                ))
            }
            wasm_externkind_enum_WASM_EXTERN_TAG => {
                let raw = unsafe { wasm_extern_as_tag(raw) };
                Extern::Tag(crate::Tag::from_vm_extern(
                    &mut actual,
                    crate::vm::VMExternTag::V8(Handle::new(raw, origin)),
                ))
            }
            _ => return Err(crate::RuntimeError::new("unknown V8 extern kind")),
        })
    }
}
impl crate::VMExternToExtern for VMExtern {
    fn to_extern(self, store: &mut impl AsStoreMut) -> Extern {
        self.try_to_extern(store)
            .expect("VMExtern must belong to the supplied live V8 Store")
    }
}

pub(crate) struct VMExternRef(*mut wasm_ref_t);
impl VMExternRef {
    /// Converts the `VMExternRef` into a `RawValue`.
    pub fn into_raw(self) -> RawValue {
        unimplemented!()
    }

    /// Extracts a `VMExternRef` from a `RawValue`.
    ///
    /// # Safety
    /// `raw` must be a valid `VMExternRef` instance.
    pub unsafe fn from_raw(_raw: RawValue) -> Option<Self> {
        unimplemented!();
    }
}

pub(crate) struct VMFuncRef(*mut wasm_ref_t);
impl VMFuncRef {
    /// Passively encodes a borrowed external C function reference; no owner moves.
    pub fn into_raw(self) -> RawValue {
        RawValue {
            funcref: self.0 as usize,
        }
    }

    /// Extracts a borrowed external C function reference, or null as None.
    ///
    /// # Safety
    /// Nonzero bits must name a live external C function reference. The caller
    /// keeps its actual owner live; conversion into a Function must use that
    /// SAME live Store on its original thread. These are not V8-internal tagged
    /// references. Packing/unpacking itself performs no C++ operation.
    pub unsafe fn from_raw(raw: RawValue) -> Option<Self> {
        let reference = unsafe { raw.funcref } as *mut wasm_ref_t;
        if reference.is_null() {
            None
        } else {
            Some(Self(reference))
        }
    }
}

pub struct VMExceptionRef(*mut wasm_ref_t);
impl VMExceptionRef {
    /// Converts the `VMExceptionRef` into a `RawValue`.
    pub fn into_raw(self) -> RawValue {
        unimplemented!()
    }

    /// Extracts a `VMExceptionRef` from a `RawValue`.
    ///
    /// # Safety
    /// `raw` must be a valid `VMExceptionRef` instance.
    pub unsafe fn from_raw(_raw: RawValue) -> Option<Self> {
        unimplemented!();
    }
}

#[derive(Debug, Clone)]
pub struct VMMemory(pub(super) Handle<wasm_memory_t>);
struct SharedMemoryPointer(*mut wasm_shared_memory_t);
pub struct VMSharedMemory(Arc<SharedMemoryPointer>);
pub(crate) type VMExternMemory = Handle<wasm_memory_t>;

impl SharedMemoryPointer {
    fn new(ptr: *mut wasm_shared_memory_t) -> Self {
        Self(ptr)
    }
}

impl Drop for SharedMemoryPointer {
    fn drop(&mut self) {
        unsafe {
            wasm_shared_memory_delete(self.0);
        }
    }
}

/// # SAFETY: The WASM V8 memory is attached to a Store and cannot be accessed
/// from a different thread (long-term goal would removal of the unsafe Send/Sync).
unsafe impl Send for VMMemory {}
unsafe impl Sync for VMMemory {}

/// # SAFETY: Shared memory handles are safe to send across thread boundaries.
unsafe impl Send for SharedMemoryPointer {}
unsafe impl Sync for SharedMemoryPointer {}

impl VMMemory {
    /// Created a shared detached memory that can be attached in another Store.
    pub(crate) fn as_shared(&self) -> Result<VMSharedMemory, MemoryError> {
        let memory_type = unsafe { wasm_memory_type(self.0.raw()) };
        let limits = unsafe { wasm_memorytype_limits(memory_type) };
        if !unsafe { (*limits).shared } {
            unsafe {
                wasm_memorytype_delete(memory_type);
            }
            return Err(MemoryError::MemoryNotShared);
        }
        unsafe {
            wasm_memorytype_delete(memory_type);
        }

        let shared = unsafe { wasm_memory_share(self.0.raw()) };
        if shared.is_null() {
            return Err(MemoryError::Generic(
                "Failed to clone the memory".to_string(),
            ));
        }

        Ok(VMSharedMemory(Arc::new(SharedMemoryPointer::new(shared))))
    }
}

impl VMSharedMemory {
    /// We can clone a shared memory handle as we want.
    pub(crate) fn clone(&self) -> Self {
        Self(self.0.clone())
    }

    /// Attach the shared memory handle to a new Store and convert it to VMMemory
    pub(crate) fn into_vm_memory(self, store: &mut Store) -> VMMemory {
        let memory = unsafe { wasm_memory_obtain(store.inner, self.0.0) };
        assert!(
            !memory.is_null(),
            "Failed to obtain V8 shared memory: wasm_memory_obtain returned null"
        );
        VMMemory(Handle::new(
            store.reference_roots.memory(memory),
            store.origin.capture(),
        ))
    }
}

#[cfg(all(test, feature = "experimental-async"))]
mod raw_funcref_tests {
    use super::VMFuncRef;
    use crate::{AsStoreRef, Function, Store, Type, Value};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use wasmer_types::RawValue;

    #[test]
    fn rooted_external_cref_raw_roundtrip_survives_temporary_owners_and_rejects_foreign_inputs() {
        const TEST: &str = "backend::v8::vm::raw_funcref_tests::rooted_external_cref_raw_roundtrip_survives_temporary_owners_and_rejects_foreign_inputs";
        const CHILD: &str = "WASMER_V8_EXTERNAL_CREF_RAW_CHILD";
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
                        assert!(status.success(), "external CRef child failed: {status}");
                        return;
                    }
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(10))
                    }
                    status => panic!("external CRef roundtrip did not finish: {status:?}"),
                }
            }
        }
        let mut store = Store::new(crate::v8::V8::new());
        let target = Function::new_typed(&mut store, |input: i32| input + 7);
        let factories = Arc::new(AtomicUsize::new(0));
        let observed = factories.clone();
        let echo = Function::new_typed(&mut store, move |value: Option<Function>| {
            observed.fetch_add(1, Ordering::SeqCst);
            value
        });
        let typed = echo
            .typed::<Option<Function>, Option<Function>>(&store)
            .unwrap();
        let baseline = store
            .as_store_ref()
            .inner
            .store
            .as_v8()
            .reference_roots
            .len();
        let original = vec![Value::FuncRef(Some(target.clone()))];
        let raw = original[0].as_raw(&store);
        assert_ne!(unsafe { raw.funcref }, 0);
        let vm = unsafe { VMFuncRef::from_raw(raw) }.unwrap();
        let packed = vm.into_raw();
        assert_eq!(unsafe { packed.funcref }, unsafe { raw.funcref });
        let reconstructed = unsafe { Value::from_raw(&mut store, Type::FuncRef, packed) };
        let reconstructed = reconstructed.unwrap_funcref().as_ref().unwrap().clone();
        drop(original);
        assert_eq!(reconstructed, target);
        assert_eq!(
            reconstructed.call(&mut store, &[Value::I32(5)]).unwrap()[0].i32(),
            Some(12)
        );

        // This is an ACTUAL independently owned C++ Val vector, not merely
        // inert Function metadata. The decoder must retain before its Drop.
        let temporary = crate::backend::v8::references::Values::from_values(
            &[Value::FuncRef(Some(target.clone()))],
            &[Type::FuncRef],
        )
        .unwrap();
        let temporary_ref = unsafe { (*temporary.0.data).of.ref_ };
        let vm = unsafe {
            VMFuncRef::from_raw(RawValue {
                funcref: temporary_ref as usize,
            })
        }
        .unwrap();
        let retained =
            unsafe { Function::from_vm_funcref(&mut store, crate::vm::VMFuncRef::V8(vm)) };
        drop(temporary);
        assert_eq!(retained, target);
        assert_eq!(
            retained.call(&mut store, &[Value::I32(6)]).unwrap()[0].i32(),
            Some(13)
        );
        for _ in 0..8 {
            let value = Value::FuncRef(Some(retained.clone()));
            let raw = value.as_raw(&store);
            let copied = unsafe { Value::from_raw(&mut store, Type::FuncRef, raw) };
            assert_eq!(copied.unwrap_funcref().as_ref().unwrap(), &target);
            drop(value);
            drop(copied);
            assert_eq!(
                store
                    .as_store_ref()
                    .inner
                    .store
                    .as_v8()
                    .reference_roots
                    .len(),
                baseline
            );
        }
        let zero = Value::FuncRef(None).as_raw(&store);
        assert_eq!(unsafe { zero.funcref }, 0);
        assert!(unsafe { VMFuncRef::from_raw(zero) }.is_none());
        assert!(
            unsafe { Value::from_raw(&mut store, Type::FuncRef, zero) }
                .unwrap_funcref()
                .is_none()
        );

        let mut foreign = Store::new(crate::v8::V8::new());
        let foreign_target = Function::new_typed(&mut foreign, |input: i32| input + 90);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Value::FuncRef(Some(target.clone())).as_raw(&foreign)
            }))
            .is_err()
        );
        assert!(
            typed
                .call(&mut store, Some(foreign_target.clone()))
                .is_err()
        );
        assert_eq!(factories.load(Ordering::SeqCst), 0);
        assert!(typed.call(&mut store, None).unwrap().is_none());
        assert_eq!(factories.load(Ordering::SeqCst), 1);
        let store = store.into_async();
        let refused = typed.call_async(&store, Some(foreign_target.clone()));
        assert_eq!(factories.load(Ordering::SeqCst), 1, "construction is inert");
        assert!(futures::executor::block_on(refused).is_err());
        assert_eq!(factories.load(Ordering::SeqCst), 1);
        assert!(
            futures::executor::block_on(typed.call_async(&store, None))
                .unwrap()
                .is_none()
        );
        assert_eq!(factories.load(Ordering::SeqCst), 2);
        drop(store);
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                Value::FuncRef(Some(retained.clone())).as_raw(&foreign)
            }))
            .is_err(),
            "stale safe conversion refuses before C++"
        );
        assert_eq!(
            foreign_target.call(&mut foreign, &[Value::I32(1)]).unwrap()[0].i32(),
            Some(91)
        );
        // No nonnull FuncRef ever entered a guest argument/result packer here.
        // Candidate-only typed guest fidelity remains a distinct held gate.
    }
}

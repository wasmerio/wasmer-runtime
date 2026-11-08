//! Data types, functions and traits for `v8` runtime's `Function` implementation.
use std::sync::Arc;

use crate::{
    AsStoreMut, AsStoreRef, Exports, Extern, Imports, InstantiationError, Module,
    backend::v8::bindings::*,
    v8::error::{TrapCallScope, TrapMode},
    vm::VMExtern,
};

use super::check_isolate;
use crate::backend::v8::{
    origin::{Handle, Origin},
    references::{Exports as OwnedExports, OwnedValue},
};

#[derive(PartialEq, Eq)]
pub(crate) struct InstanceHandle(pub(crate) *mut wasm_instance_t);

unsafe impl Send for InstanceHandle {}
unsafe impl Sync for InstanceHandle {}

impl InstanceHandle {
    #[allow(clippy::result_large_err, clippy::unnecessary_mut_passed)]
    fn new(
        store: *mut wasm_store_t,
        module: *mut wasm_shared_module_t,
        mut externs: Vec<VMExtern>,
        traps: &TrapCallScope,
    ) -> Result<Self, InstantiationError> {
        let mut trap: *mut wasm_trap_t = std::ptr::null_mut() as _;

        let mut externs: Vec<_> = externs.into_iter().map(|v| v.unwrap_v_8().raw()).collect();
        let obtained = unsafe { wasm_module_obtain(store, module) };
        let _module = OwnedValue::reference(unsafe { wasm_module_as_ref(obtained) }, false);
        let instance = Self::call_raw(store, obtained, &mut externs, &mut trap);

        if instance.is_null() {
            let trap = traps.decode_owned(trap);
            return Err(InstantiationError::Start(trap.into()));
        }

        Ok(Self(instance))
    }

    fn call_raw(
        store: *mut wasm_store_t,
        module: *mut wasm_module_t,
        externs: &mut [*mut wasm_extern_t],
        trap: &mut *mut wasm_trap_t,
    ) -> *mut wasm_instance_t {
        // instance_new borrows this vector and its already-rooted handles.
        // Do not manufacture an owning C vector from borrowed wrappers.
        let imports = wasm_extern_vec_t {
            size: externs.len(),
            data: externs.as_mut_ptr(),
        };
        unsafe { wasm_instance_new(store, module, &imports, trap) }
    }

    fn get_exports(&self, mut store: &mut impl AsStoreMut, module: &Module) -> Exports {
        check_isolate(store);
        let mut exports = unsafe {
            let mut vec = Default::default();
            wasm_instance_exports(self.0, &mut vec);
            vec
        };

        let mut exports = OwnedExports(exports);

        let exports_ty = module.exports().collect::<Vec<_>>();
        exports_ty
            .iter()
            .take(exports.0.size)
            .enumerate()
            .map(|(index, export_type)| {
                let name = export_type.name();
                let mut store = store.as_store_mut();
                let _extern_type = export_type.ty();
                // Annotation is here to prevent spurious IDE warnings.

                let origin = Origin::of(&store).expect("actual V8 instance Store");
                let retained = store
                    .inner
                    .store
                    .as_v8()
                    .reference_roots
                    .adopt(exports.take(index));
                let raw = unsafe { wasm_ref_as_extern(retained) };
                let extern_ =
                    Extern::from_vm_extern(&mut store, VMExtern::V8(Handle::new(raw, origin)));
                (name.to_string(), extern_)
            })
            .collect::<Exports>()
    }
}
impl Drop for InstanceHandle {
    fn drop(&mut self) {
        unsafe { wasm_instance_delete(self.0) }
    }
}

#[derive(Clone, PartialEq, Eq)]
/// A WebAssembly `instance` in the `v8` runtime.
pub struct Instance {
    pub(crate) handle: Arc<InstanceHandle>,
}

impl Instance {
    #[allow(clippy::result_large_err)]
    pub(crate) fn new(
        store: &mut impl AsStoreMut,
        module: &Module,
        imports: &Imports,
    ) -> Result<(Self, Exports), InstantiationError> {
        check_isolate(store);
        let mut store = store.as_store_mut();

        let externs = imports
            .imports_for_module(module)
            .map_err(InstantiationError::Link)?;

        Self::new_by_index(&mut store, module, &externs)
    }

    #[allow(clippy::result_large_err)]
    pub(crate) fn new_by_index(
        store: &mut impl AsStoreMut,
        module: &Module,
        externs: &[Extern],
    ) -> Result<(Self, Exports), InstantiationError> {
        check_isolate(store);
        let externs = Self::checked_imports(store, externs)?;
        let traps = TrapCallScope::enter(store, TrapMode::BorrowedSync, None)
            .map_err(InstantiationError::Start)?;
        let outcome = traps.catch(|| {
            let instance = InstanceHandle::new(
                store.as_store_ref().inner.store.as_v8().inner,
                module.as_v8().handle.v8_shared_module_handle,
                externs,
                &traps,
            )?;
            let exports = instance.get_exports(store, module);

            Ok((
                Self {
                    handle: Arc::new(instance),
                },
                exports,
            ))
        });
        traps.finish_outcome(outcome.unwrap_or_else(|| {
            Err(InstantiationError::Start(crate::RuntimeError::new(
                "V8 instance start panicked",
            )))
        }))
    }

    fn checked_imports(
        store: &impl AsStoreRef,
        externs: &[Extern],
    ) -> Result<Vec<VMExtern>, InstantiationError> {
        if externs.iter().any(|value| {
            // Sys handles may share a numeric StoreId; that is not V8 origin.
            let is_v8 = match value {
                Extern::Function(value) => matches!(&value.0, crate::BackendFunction::V8(_)),
                Extern::Memory(value) => matches!(&value.0, crate::BackendMemory::V8(_)),
                Extern::Table(value) => matches!(&value.0, crate::BackendTable::V8(_)),
                Extern::Global(value) => matches!(&value.0, crate::BackendGlobal::V8(_)),
                Extern::Tag(value) => matches!(&value.0, crate::BackendTag::V8(_)),
            };
            !is_v8 || !value.is_from_store(store)
        }) {
            return Err(InstantiationError::DifferentStores);
        }
        Ok(externs
            .iter()
            .map(|extern_| extern_.to_vm_extern())
            .collect())
    }
}

#[cfg(feature = "experimental-async")]
type OwnedInstanceFuture = std::pin::Pin<
    Box<dyn std::future::Future<Output = Result<crate::Instance, InstantiationError>>>,
>;

/// Instantiate on this thread without retaining a Rust Store guard during start.
/// Construction is inert; the checked first poll blocks through exact imports.
#[cfg(feature = "experimental-async")]
#[doc(hidden)]
pub fn instantiate_async(
    asynchronous: &impl crate::AsStoreAsync,
    module: Module,
    imports: Vec<Extern>,
) -> OwnedInstanceFuture {
    use crate::StoreAsyncWriteLock;
    use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};

    let store = asynchronous.store();
    Box::pin(async move {
        let mut locked = Some(StoreAsyncWriteLock::try_acquire(&store).ok_or_else(|| {
            InstantiationError::Start(crate::RuntimeError::new("V8 instance Store is busy"))
        })?);
        Origin::of(locked.as_ref().unwrap()).map_err(InstantiationError::Start)?;
        if !matches!(&module.0, crate::BackendModule::V8(_)) {
            return Err(InstantiationError::DifferentStores);
        }
        let binding = locked
            .as_ref()
            .unwrap()
            .as_store_ref()
            .inner
            .store
            .as_v8()
            .async_binding
            .clone();
        let owner = binding
            .selected_executor()
            .map_err(InstantiationError::Start)?;
        let mut frame = binding.enter().map_err(InstantiationError::Start)?;
        frame
            .attach_traps(locked.as_ref().unwrap())
            .map_err(InstantiationError::Start)?;
        let unwinding = std::thread::panicking();
        let mut obtained = None;
        let mut instance = None;
        let mut pending_trap = None;
        let mut outcome = match catch_unwind(AssertUnwindSafe(|| {
            if imports.len() != module.imports().len() {
                return Err(InstantiationError::Start(crate::RuntimeError::new(
                    "V8 instance import count changed",
                )));
            }
            let externs = Instance::checked_imports(locked.as_ref().unwrap(), &imports)?;
            let mut externs = externs
                .into_iter()
                .map(|value| value.unwrap_v_8().raw())
                .collect::<Vec<_>>();
            let raw_store = locked
                .as_ref()
                .unwrap()
                .as_store_ref()
                .inner
                .store
                .as_v8()
                .inner;
            let raw_module = unsafe {
                wasm_module_obtain(raw_store, module.as_v8().handle.v8_shared_module_handle)
            };
            if raw_module.is_null() {
                return Err(InstantiationError::Start(crate::RuntimeError::new(
                    "V8 could not obtain the instance module",
                )));
            }
            obtained = Some(OwnedValue::reference(
                unsafe { wasm_module_as_ref(raw_module) },
                false,
            ));
            drop(locked.take());
            if frame.has_panic() {
                return Err(InstantiationError::Start(crate::RuntimeError::new(
                    "V8 instance guard cleanup panicked",
                )));
            }
            let mut trap = std::ptr::null_mut();
            let raw = InstanceHandle::call_raw(raw_store, raw_module, &mut externs, &mut trap);
            if !trap.is_null() {
                pending_trap = Some(OwnedValue::reference(
                    unsafe { wasm_trap_as_ref(trap) },
                    false,
                ));
            }
            if !raw.is_null() {
                instance = Some(InstanceHandle(raw));
            }
            if frame.has_panic() {
                return Err(InstantiationError::Start(crate::RuntimeError::new(
                    "V8 instance start panicked",
                )));
            }
            if instance.is_none() {
                return Err(InstantiationError::Start(
                    frame
                        .decode_trap(take_instance_trap(&mut pending_trap))
                        .into(),
                ));
            }
            let mut guard = StoreAsyncWriteLock::try_acquire(&store).ok_or_else(|| {
                InstantiationError::Start(crate::RuntimeError::new(
                    "V8 instance returned with a borrowed Store",
                ))
            })?;
            let exports = instance.as_ref().unwrap().get_exports(&mut guard, &module);
            Ok(crate::Instance {
                _inner: crate::BackendInstance::V8(Instance {
                    handle: Arc::new(instance.take().unwrap()),
                }),
                module: module.clone(),
                exports,
            })
        })) {
            Ok(outcome) => Some(outcome),
            Err(panic) => {
                frame.record(panic, unwinding);
                Some(Err(InstantiationError::Start(crate::RuntimeError::new(
                    "V8 instance start panicked",
                ))))
            }
        };
        frame.dispose(|| drop(locked.take()));
        frame.dispose(|| drop(instance.take()));
        frame.dispose(|| {
            let trap = take_instance_trap(&mut pending_trap);
            if !trap.is_null() {
                drop(frame.decode_trap(trap));
            }
        });
        frame.dispose(|| drop(obtained.take()));
        frame.dispose(|| drop(imports));
        frame.dispose(|| drop(module));
        frame.dispose(|| drop(owner));
        frame.finish_traps();
        if frame.has_panic() {
            frame.dispose(|| drop(outcome.take()));
        }
        let panic = frame.take_panic();
        drop(frame);
        if let Some(panic) = panic {
            resume_unwind(panic);
        }
        drop(store);
        outcome.take().unwrap()
    })
}

#[cfg(feature = "experimental-async")]
fn take_instance_trap(owner: &mut Option<OwnedValue>) -> *mut wasm_trap_t {
    owner
        .take()
        .map(|mut owner| unsafe { wasm_ref_as_trap(owner.take().of.ref_) })
        .unwrap_or(std::ptr::null_mut())
}

impl crate::BackendInstance {
    /// Consume [`self`] into a [`crate::backend::v8::instance::Instance`].
    pub(crate) fn into_v8(self) -> crate::backend::v8::instance::Instance {
        match self {
            Self::V8(s) => s,
            _ => panic!("Not a `v8` instance"),
        }
    }
}

use super::engine::wasm_engine_t;
use std::cell::{Cell, UnsafeCell};
use std::ptr::NonNull;
use std::rc::{Rc, Weak};
use thiserror::Error;
use wasmer_api::{AsStoreMut, AsStoreRef, Store, StoreMut, StoreRef as BaseStoreRef};
use wasmer_types::StoreId;

/// The store is held by a caller that did not lend it, so it cannot be
/// borrowed again without aliasing that caller's borrow.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("the store is in use by a call that did not lend it")]
pub struct StoreBusy;

/// How the store is borrowed from where [`Access`] says borrows come from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Held {
    Free,
    /// By one or more [`StoreCell::with`]. They are not counted: each one puts
    /// back the access it found.
    Shared,
    Exclusive,
}

/// Where the next borrow of the store comes from, and whether it can be had.
#[derive(Clone, Copy)]
struct Access {
    /// The handle a running host function lent, which borrows are derived
    /// from instead of the cell: the cell is borrowed by the call that reached
    /// the host function, so a borrow of its own would alias that one.
    ///
    /// The lifetime is erased. [`StoreCell::lend`] keeps the handle borrowed
    /// for as long as it is here.
    lent: Option<NonNull<StoreMut<'static>>>,
    held: Held,
}

/// Puts back the access that a borrow or a lend replaced, also when the code
/// it ran unwinds.
///
/// Borrows and lends nest strictly, so each one restores what it found rather
/// than undoing its own step. An outer one therefore also repairs the state if
/// an inner one never got to restore it.
struct Restore<'a> {
    access: &'a Cell<Access>,
    previous: Access,
}

impl Drop for Restore<'_> {
    fn drop(&mut self) {
        self.access.set(self.previous);
    }
}

/// The store, for as long as the cell owns it.
#[cfg(not(feature = "extensions-async"))]
type Owned = Store;
/// The store, for as long as the cell owns it. Sharing the store moves it out,
/// which is why the cell drops it by hand.
#[cfg(feature = "extensions-async")]
type Owned = std::mem::ManuallyDrop<Store>;

/// A store, and the record of who is using it.
///
/// The C API has no borrow checker: a host function called by
/// `wasm_func_call` calls back into the C API on the very store that call is
/// using. This cell hands the store out through closures, so that it knows
/// when the store is borrowed, and lets a host function lend the borrow it
/// was given to the calls it makes.
struct StoreCell {
    store: UnsafeCell<Owned>,
    id: StoreId,
    access: Cell<Access>,
    #[cfg(feature = "extensions-async")]
    extension: extension::Extension,
}

impl StoreCell {
    fn new(store: Store) -> Self {
        #[cfg(feature = "extensions-async")]
        let store = Owned::new(store);
        Self {
            id: store.id(),
            store: UnsafeCell::new(store),
            access: Cell::new(Access {
                lent: None,
                held: Held::Free,
            }),
            #[cfg(feature = "extensions-async")]
            extension: Default::default(),
        }
    }

    /// Marks the store as held in the `wanted` way until the guard is dropped.
    fn hold(&self, wanted: Held) -> Result<Restore<'_>, StoreBusy> {
        let previous = self.access.get();
        match (previous.held, wanted) {
            (Held::Free, _) | (Held::Shared, Held::Shared) => {}
            _ => return Err(StoreBusy),
        }
        self.access.set(Access {
            held: wanted,
            ..previous
        });
        Ok(Restore {
            access: &self.access,
            previous,
        })
    }

    fn with<R>(&self, f: impl FnOnce(BaseStoreRef<'_>) -> R) -> Result<R, StoreBusy> {
        // A shared store keeps its own record of who holds it, and every
        // borrow of it is exclusive.
        #[cfg(feature = "extensions-async")]
        if let Some(shared) = self.extension.shared() {
            return Ok(shared.try_with(|store| f(store.as_store_ref()))?);
        }
        let _held = self.hold(Held::Shared)?;
        let store = match self.access.get().lent {
            // SAFETY: `lend` keeps the handle alive and unreachable to its
            // owner while it is here, and `hold` found no exclusive borrow of
            // it.
            Some(lent) => unsafe { lent.as_ref() }.as_store_ref(),
            // SAFETY: `hold` found no exclusive borrow of the cell, and a
            // store that is lent is never borrowed from here.
            None => unsafe { &*self.store.get() }.as_store_ref(),
        };
        Ok(f(store))
    }

    fn with_mut<R>(&self, f: impl FnOnce(StoreMut<'_>) -> R) -> Result<R, StoreBusy> {
        #[cfg(feature = "extensions-async")]
        if let Some(shared) = self.extension.shared() {
            return Ok(shared.try_with(f)?);
        }
        let _held = self.hold(Held::Exclusive)?;
        let store = match self.access.get().lent {
            // SAFETY: `lend` keeps the handle alive and unreachable to its
            // owner while it is here, and `hold` found no other borrow of it.
            // The new borrow is derived from the lent one, so the lender's
            // stays valid.
            Some(mut lent) => unsafe { lent.as_mut() }.as_store_mut(),
            // SAFETY: `hold` found no other borrow of the cell, and a store
            // that is lent is never borrowed from here.
            None => unsafe { &mut *self.store.get() }.as_store_mut(),
        };
        Ok(f(store))
    }

    fn lend<R>(&self, store: &mut StoreMut<'_>, f: impl FnOnce() -> R) -> R {
        // A handle of another store would be given out as this one.
        if store.objects_mut().id() != self.id {
            return f();
        }
        // The calls a host function makes are not offered to the store's
        // hooks.
        #[cfg(feature = "extensions-async")]
        let _entered = self.extension.enter();
        // A shared store is not borrowed from the cell, so the cell has
        // nothing to lend. Parking the borrow lets every handle of the shared
        // store use it: this cell's, and the ones its extension holds.
        #[cfg(feature = "extensions-async")]
        if self.extension.shared().is_some() {
            return store.parked(f);
        }
        // `store` stays mutably borrowed until this function returns, which is
        // what makes the pointer usable for as long as it is in `access`. The
        // new level starts free: whoever held the store is below this frame
        // and cannot use its borrow before `f` returns.
        let previous = self.access.replace(Access {
            lent: Some(NonNull::from(store).cast()),
            held: Held::Free,
        });
        let _restore = Restore {
            access: &self.access,
            previous,
        };
        f()
    }
}

/// A handle to a store of the C API. All access to the store goes through
/// [`Self::with`] and [`Self::with_mut`].
#[derive(Clone)]
pub struct StoreRef {
    inner: Rc<StoreCell>,
}

impl StoreRef {
    pub(crate) fn new(store: Store) -> Self {
        Self {
            inner: Rc::new(StoreCell::new(store)),
        }
    }

    /// Runs `f` with a shared borrow of the store.
    ///
    /// Fails with [`StoreBusy`] while the store is borrowed exclusively by a
    /// caller that did not lend it.
    pub fn with<R>(&self, f: impl FnOnce(BaseStoreRef<'_>) -> R) -> Result<R, StoreBusy> {
        // `self` may live in an object that `f` lets C code delete, such as
        // the function being called. This keeps the store alive regardless.
        let cell = self.inner.clone();
        cell.with(f)
    }

    /// Runs `f` with an exclusive borrow of the store.
    ///
    /// Inside a host function of this store, the borrow is derived from the
    /// one the host function lent with [`Self::lend`]. Fails with
    /// [`StoreBusy`] while the store is borrowed by a caller that did not lend
    /// it.
    pub fn with_mut<R>(&self, f: impl FnOnce(StoreMut<'_>) -> R) -> Result<R, StoreBusy> {
        // See `with`.
        let cell = self.inner.clone();
        cell.with_mut(f)
    }

    /// Lends `store` to the [`Self::with`] and [`Self::with_mut`] calls made
    /// while `f` runs.
    ///
    /// A host function is called while its store is borrowed by the call that
    /// entered the guest. It lends the handle that call gave it, so that C
    /// code it runs can use the store again. A handle of another store is not
    /// lent.
    pub fn lend<R>(&self, store: &mut StoreMut<'_>, f: impl FnOnce() -> R) -> R {
        // See `with`.
        let cell = self.inner.clone();
        cell.lend(store, f)
    }

    /// Create a non-owning handle to this store.
    ///
    /// Host-function callbacks must not capture a strong [`StoreRef`]: the
    /// function lives inside the store's arena, so a strong clone would form a
    /// store → function → store cycle and leak the whole store. Callbacks
    /// capture a [`WeakStoreRef`] instead and upgrade it per call.
    pub fn downgrade(&self) -> WeakStoreRef {
        WeakStoreRef {
            inner: Rc::downgrade(&self.inner),
        }
    }
}

/// A non-owning handle to a store, held by host-function callbacks.
#[derive(Clone)]
pub struct WeakStoreRef {
    inner: Weak<StoreCell>,
}

// SAFETY: wasm-c-api stores are single-threaded (a documented invariant of the
// C API). Host callbacks are `Send + Sync`-bound by `Function::new_with_env`,
// so this handle must be too; it is never actually shared across threads.
unsafe impl Send for WeakStoreRef {}
unsafe impl Sync for WeakStoreRef {}

impl WeakStoreRef {
    /// Upgrade to a strong [`StoreRef`], or `None` if the store has been
    /// dropped (which cannot happen while one of its callbacks is running).
    pub fn upgrade(&self) -> Option<StoreRef> {
        self.inner.upgrade().map(|inner| StoreRef { inner })
    }
}

/// Opaque type representing a WebAssembly store.
#[allow(non_camel_case_types)]
pub struct wasm_store_t {
    pub inner: StoreRef,
}

/// Creates a new WebAssembly store given a specific [engine][super::engine].
///
/// # Example
///
/// See the module's documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_store_new(
    engine: Option<&wasm_engine_t>,
) -> Option<Box<wasm_store_t>> {
    let engine = engine?;
    let store = Store::new(engine.inner.clone());

    Some(Box::new(wasm_store_t {
        inner: StoreRef::new(store),
    }))
}

/// Deletes a WebAssembly store.
///
/// # Example
///
/// See the module's documentation.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_store_delete(_store: Option<Box<wasm_store_t>>) {}

#[cfg(feature = "extensions-async")]
pub use extension::{CallOutcome, StoreHooks};

/// What `extensions-async` adds to a store: a second state, in which the store
/// is a [`SharedStore`](wasmer_api::SharedStore), and the hooks of the layer
/// that shares it.
#[cfg(feature = "extensions-async")]
mod extension {
    use super::{Held, StoreBusy, StoreCell, StoreRef};
    use std::cell::{Cell, OnceCell, RefCell};
    use std::mem::ManuallyDrop;
    use std::rc::Rc;
    use wasmer_api::{Function, RuntimeError, SharedStore, Value};

    /// How a function call ended.
    pub type CallOutcome = Result<Box<[Value]>, RuntimeError>;

    /// The hooks of a layer over the C API, such as a WASI implementation, on
    /// one store. Install them with [`StoreRef::set_hooks`].
    ///
    /// The store owns its hooks. Hooks that own the store back, through a
    /// [`StoreRef`], keep it alive for good: hold a
    /// [`WeakStoreRef`](super::WeakStoreRef) or a
    /// [`WeakSharedStore`](wasmer_api::WeakSharedStore) instead.
    pub trait StoreHooks {
        /// Offers the layer a `wasm_func_call` on `store`, before the store
        /// is borrowed. `Some` is the outcome of the call, which the layer
        /// ran itself. `None` leaves the call to `wasm_func_call`.
        ///
        /// This is where the layer sets up, runs `function` under its own
        /// entry, maps the way the guest left, and cleans up. To let the
        /// guest suspend, it drives a `Function::call_async` on
        /// [`StoreRef::shared`] to completion.
        ///
        /// Only an outermost call is offered. A call made while another
        /// `wasm_func_call` or a host function of the store is running is
        /// under the layer's entry already. It also runs inside a slice of
        /// the outer call, where a call on the shared store cannot be polled
        /// and a guest cannot suspend.
        ///
        /// A start function is not offered either. `wasm_instance_new` runs
        /// it synchronously, so there is nothing to take over.
        fn call(
            &self,
            _store: &StoreRef,
            _function: &Function,
            _params: &[Value],
        ) -> Option<CallOutcome> {
            None
        }

        /// Runs once, when the last handle to the store is gone, and before
        /// the store is destroyed: after `wasm_store_delete` and the deletion
        /// of every object of the store.
        ///
        /// The store is whole. Its instances can be used and called through
        /// `store`, and the finalizers of its host function environments
        /// have not run. No C API handle to it is left, so a host function
        /// made with `wasm_func_new` fails if a guest calls it. The store is
        /// destroyed when this returns, unless the layer keeps a handle.
        ///
        /// The store is free, unless the last handle went while the layer
        /// itself was running a guest on its own handle: the C API keeps a
        /// handle for as long as one of its calls runs.
        fn close(&self, _store: &SharedStore) {}
    }

    impl From<wasmer_api::StoreBusy> for StoreBusy {
        fn from(_: wasmer_api::StoreBusy) -> Self {
            Self
        }
    }

    /// The part of a [`StoreCell`] that only an extension uses.
    #[derive(Default)]
    pub(super) struct Extension {
        /// The store, once it is shared. From then on the cell's own store is
        /// moved out, and the record of its borrows is the shared store's.
        shared: OnceCell<SharedStore>,
        hooks: RefCell<Option<Rc<dyn StoreHooks>>>,
        /// How many `wasm_func_call`s and host functions of the store are
        /// running.
        entries: Cell<u32>,
    }

    /// Counts one entry into the store until it is dropped.
    pub(super) struct Entered<'a>(&'a Cell<u32>);

    impl Drop for Entered<'_> {
        fn drop(&mut self) {
            self.0.set(self.0.get() - 1);
        }
    }

    impl Extension {
        pub(super) fn shared(&self) -> Option<&SharedStore> {
            self.shared.get()
        }

        pub(super) fn enter(&self) -> Entered<'_> {
            self.entries.set(self.entries.get() + 1);
            Entered(&self.entries)
        }

        /// The hooks to offer a call that is about to enter the store.
        fn hooks_of_an_outermost_call(&self) -> Option<Rc<dyn StoreHooks>> {
            if self.entries.get() > 0 {
                return None;
            }
            self.hooks.borrow().clone()
        }
    }

    impl StoreCell {
        fn share(&self) -> Result<SharedStore, StoreBusy> {
            if let Some(shared) = self.extension.shared() {
                return Ok(shared.clone());
            }
            let access = self.access.get();
            if access.held != Held::Free || access.lent.is_some() {
                return Err(StoreBusy);
            }
            // SAFETY: nothing borrows the store. It is not used from the cell
            // again, nor dropped by it: `shared` is set before any other code
            // runs, and every use of the cell's store looks there first.
            let store = unsafe { ManuallyDrop::take(&mut *self.store.get()) };
            Ok(self
                .extension
                .shared
                .get_or_init(|| store.into_shared())
                .clone())
        }
    }

    impl Drop for StoreCell {
        fn drop(&mut self) {
            // No handle is left to borrow the store, so it can be shared.
            if let Some(hooks) = self.extension.hooks.take()
                && let Ok(shared) = self.share()
            {
                hooks.close(&shared);
            }
            if self.extension.shared().is_none() {
                // SAFETY: the store was not moved out, and this is its last
                // use.
                unsafe { ManuallyDrop::drop(self.store.get_mut()) };
            }
        }
    }

    impl StoreRef {
        /// Turns the store into a [`SharedStore`] in place, and returns a
        /// handle to it. The store stays shared, and sharing it again returns
        /// another handle to the same shared store.
        ///
        /// Every handle of the C API goes on working. [`Self::with`] and
        /// [`Self::with_mut`] then both take the shared store for one slice,
        /// and fail with [`StoreBusy`] while anything else holds it, another
        /// [`Self::with`] included. [`Self::lend`] parks the borrow it is
        /// given, as `FunctionEnvMut::release` does. A host function made
        /// with `wasm_func_new` does that for as long as its callback runs,
        /// so that C API calls and the extension's own
        /// `SharedStore::try_with` succeed inside it.
        ///
        /// Fails with [`StoreBusy`] while the store is borrowed, which
        /// includes any call made from one of its host functions.
        pub fn share(&self) -> Result<SharedStore, StoreBusy> {
            self.inner.share()
        }

        /// The store as [`Self::share`] left it, or `None` if it was never
        /// shared.
        pub fn shared(&self) -> Option<SharedStore> {
            self.inner.extension.shared().cloned()
        }

        /// Installs `hooks` on the store, or removes them with `None`, and
        /// returns the hooks they replace.
        ///
        /// A call that is running keeps the hooks it was offered to, and the
        /// calls nested in it are not offered to the new ones: these see the
        /// next outermost call.
        pub fn set_hooks(&self, hooks: Option<Rc<dyn StoreHooks>>) -> Option<Rc<dyn StoreHooks>> {
            self.inner.extension.hooks.replace(hooks)
        }

        /// Runs `call`, a `wasm_func_call` of `function` on this store,
        /// unless the store's hooks take the call over.
        pub(crate) fn call_hooked(
            &self,
            function: &Function,
            params: &[Value],
            call: impl FnOnce() -> CallOutcome,
        ) -> CallOutcome {
            let extension = &self.inner.extension;
            let hooks = extension.hooks_of_an_outermost_call();
            let _entered = extension.enter();
            hooks
                .and_then(|hooks| hooks.call(self, function, params))
                .unwrap_or_else(call)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmer_api::Engine;

    fn store() -> StoreRef {
        StoreRef::new(Store::new(Engine::default()))
    }

    /// Uses a borrow in a way that needs it to be exclusive.
    fn touch(store: &mut StoreMut<'_>) -> StoreId {
        store.objects_mut().id()
    }

    #[test]
    fn an_exclusive_borrow_that_is_not_lent_refuses_another_borrow() {
        let store = store();
        let nested = store.with_mut(|_| (store.with(|_| ()), store.with_mut(|_| ())));
        assert_eq!(nested, Ok((Err(StoreBusy), Err(StoreBusy))));
        assert_eq!(store.with_mut(|_| ()), Ok(()));
    }

    #[test]
    fn a_shared_borrow_allows_shared_borrows_only() {
        let store = store();
        let nested = store.with(|_| (store.with(|_| ()), store.with_mut(|_| ())));
        assert_eq!(nested, Ok((Ok(()), Err(StoreBusy))));
        // The inner shared borrow did not release the outer one.
        let after_inner = store.with(|_| {
            let _ = store.with(|_| ());
            store.with_mut(|_| ())
        });
        assert_eq!(after_inner, Ok(Err(StoreBusy)));
        assert_eq!(store.with_mut(|_| ()), Ok(()));
    }

    /// Replays a host function that re-enters its store, without running any
    /// Wasm, so that Miri can check the borrows:
    ///
    /// ```text
    /// wasm_func_call      with_mut            -> `outer`, from the cell
    ///   host function     lend(outer)
    ///     C callback      with / with_mut     -> `inner`, from `outer`
    ///       host function lend(inner)
    ///         C callback  with_mut            -> from `inner`
    ///     C callback      goes on using the store
    /// wasm_func_call      goes on using `outer`
    /// ```
    ///
    /// Run it with:
    ///
    /// ```text
    /// cargo +nightly miri test -p wasmer-c-api --lib wasm_c_api::store
    /// ```
    #[test]
    fn a_lent_store_is_borrowed_again_from_the_lender() {
        let store = store();
        let id = store.inner.id;

        let outcome = store.with_mut(|mut outer| {
            assert_eq!(touch(&mut outer), id);
            store.lend(&mut outer, || {
                assert_eq!(store.with(|_| store.with(|_| ())), Ok(Ok(())));
                let nested = store.with_mut(|mut inner| {
                    assert_eq!(touch(&mut inner), id);
                    // Borrowed again, the lent store is busy once more.
                    assert_eq!(store.with_mut(|_| ()), Err(StoreBusy));
                    store.lend(&mut inner, || {
                        store.with_mut(|mut innermost| touch(&mut innermost))
                    })
                });
                assert_eq!(nested, Ok(Ok(id)));
                assert_eq!(store.with_mut(|mut again| touch(&mut again)), Ok(id));
            });
            // The lend is over: the store is this borrow's alone again.
            assert_eq!(store.with(|_| ()), Err(StoreBusy));
            touch(&mut outer)
        });

        assert_eq!(outcome, Ok(id));
        assert_eq!(store.with_mut(|mut free| touch(&mut free)), Ok(id));
    }

    #[test]
    fn a_lend_that_unwinds_gives_the_store_back() {
        let store = store();

        let caught = store.with_mut(|mut outer| {
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                store.lend(&mut outer, || {
                    let _ = store.with_mut(|_| panic!("host function panicked"));
                })
            }));
            // Neither the lend nor the borrow made under it is left behind.
            assert_eq!(store.with(|_| ()), Err(StoreBusy));
            touch(&mut outer);
            caught.is_err()
        });

        assert_eq!(caught, Ok(true));
        assert_eq!(store.with_mut(|_| ()), Ok(()));
    }

    #[test]
    fn a_handle_of_another_store_is_not_lent() {
        let (store, other) = (store(), store());

        let nested = other.with_mut(|mut foreign| {
            store.with_mut(|_| store.lend(&mut foreign, || store.with_mut(|_| ())))
        });

        assert_eq!(nested, Ok(Ok(Err(StoreBusy))));
    }

    /// C code is only run with the store lent or released, so only Rust code
    /// that extends the C API can hold the store and call an entry point. It
    /// gets that entry point's failure signal.
    #[test]
    fn entry_points_refuse_a_store_that_is_held_and_not_lent() {
        use crate::error::take_last_error;
        use crate::wasm_c_api::engine::wasm_engine_new;
        use crate::wasm_c_api::externals::{
            wasm_func_call, wasm_func_new, wasm_global_get, wasm_global_new, wasm_global_set,
        };
        use crate::wasm_c_api::trap::wasm_trap_t;
        use crate::wasm_c_api::types::{wasm_functype_t, wasm_globaltype_t, wasm_valkind_enum};
        use crate::wasm_c_api::value::{wasm_val_inner, wasm_val_t, wasm_val_vec_t};
        use wasmer_api::{FunctionType, GlobalType, Mutability, Type};

        unsafe extern "C" fn host(
            _: &wasm_val_vec_t,
            _: &mut wasm_val_vec_t,
        ) -> Option<Box<wasm_trap_t>> {
            None
        }

        fn i32_val(value: i32) -> wasm_val_t {
            wasm_val_t {
                kind: wasm_valkind_enum::WASM_I32 as _,
                of: wasm_val_inner { int32_t: value },
            }
        }

        let busy = Some(StoreBusy.to_string());
        let engine = wasm_engine_new();
        let mut store = unsafe { wasm_store_new(Some(&engine)) }.unwrap();
        let handle = store.inner.clone();
        let global_type = wasm_globaltype_t::new(GlobalType::new(Type::I32, Mutability::Var));
        let func_type = wasm_functype_t::new(FunctionType::new(vec![], vec![]));
        let (one, two) = (i32_val(1), i32_val(2));
        let mut global =
            unsafe { wasm_global_new(Some(&mut store), Some(&global_type), Some(&one)) }.unwrap();
        let mut func =
            unsafe { wasm_func_new(Some(&mut store), Some(&func_type), Some(host)) }.unwrap();
        let no_args: wasm_val_vec_t = vec![].into();
        let mut no_results: wasm_val_vec_t = vec![].into();
        let mut out = i32_val(-1);

        handle
            .with_mut(|_| unsafe {
                let created = wasm_global_new(Some(&mut store), Some(&global_type), Some(&two));
                assert!(created.is_none());
                assert_eq!(take_last_error(), busy);

                wasm_global_set(Some(&mut global), Some(&two));
                assert_eq!(take_last_error(), busy);

                wasm_global_get(Some(&mut global), Some(&mut out));
                assert_eq!(take_last_error(), busy);
                assert_eq!(out.of.int32_t, -1);

                let trap = wasm_func_call(Some(&mut func), Some(&no_args), &mut no_results);
                assert_eq!(trap.map(|trap| trap.inner.message()), busy);
            })
            .unwrap();

        // Released, the store is usable again and was left as it was.
        unsafe { wasm_global_get(Some(&mut global), Some(&mut out)) };
        assert_eq!(unsafe { out.of.int32_t }, 1);
        let trap = unsafe { wasm_func_call(Some(&mut func), Some(&no_args), &mut no_results) };
        assert!(trap.is_none());
        assert_eq!(take_last_error(), None);
    }

    /// A store that is shared, and the hooks that close it. None of these
    /// run Wasm, so that Miri can check them, under Stacked Borrows and with
    /// `MIRIFLAGS=-Zmiri-tree-borrows`:
    ///
    /// ```text
    /// cargo +nightly miri test -p wasmer-c-api --lib --features extensions-async shared_store
    /// ```
    #[cfg(feature = "extensions-async")]
    mod shared_store {
        use super::*;
        use std::cell::RefCell;
        use std::panic::{AssertUnwindSafe, catch_unwind};
        use wasmer_api::{SharedStore, WeakSharedStore};

        #[test]
        fn a_store_is_shared_once_and_only_while_it_is_free() {
            let store = store();

            assert_eq!(store.with(|_| store.share().err()), Ok(Some(StoreBusy)));
            assert_eq!(store.with_mut(|_| store.share().err()), Ok(Some(StoreBusy)));
            let lent = store.with_mut(|mut outer| store.lend(&mut outer, || store.share().err()));
            assert_eq!(lent, Ok(Some(StoreBusy)));
            // Refused, the store is still the cell's own.
            assert!(store.shared().is_none());
            assert_eq!(store.with(|_| store.with(|_| ())), Ok(Ok(())));

            let shared = store.share().unwrap();
            assert!(shared.ptr_eq(&store.shared().unwrap()));
            // Shared again, also while it is held, it is the same store.
            let again = shared.try_with(|_| store.share()).unwrap();
            assert!(shared.ptr_eq(&again.unwrap()));
        }

        /// The replay of `a_lent_store_is_borrowed_again_from_the_lender` on
        /// a shared store, where a host function parks the borrow it was
        /// given, and a layer uses the store through a handle of its own:
        ///
        /// ```text
        /// wasm_func_call      with_mut            -> `outer`, a slice of the store
        ///   host function     lend(outer)         -> parks `outer`
        ///     C callback      with / with_mut     -> `inner`, from `outer`
        ///       host function lend(inner)         -> parks `inner`
        ///         the layer   try_with            -> from `inner`
        ///     C callback      goes on using the store, as does the layer
        /// wasm_func_call      goes on using `outer`
        /// ```
        #[test]
        fn a_shared_store_is_borrowed_again_from_the_host_function_that_parked_it() {
            let store = store();
            let id = store.inner.id;
            let layer = store.share().unwrap();

            let outcome = store.with_mut(|mut outer| {
                assert_eq!(touch(&mut outer), id);
                // Held and not parked, it is busy for the C API and the layer.
                assert_eq!(store.with(|_| ()), Err(StoreBusy));
                assert_eq!(store.with_mut(|_| ()), Err(StoreBusy));
                assert!(layer.try_with(|_| ()).is_err());
                store.lend(&mut outer, || {
                    assert_eq!(store.with(|_| ()), Ok(()));
                    let nested = store.with_mut(|mut inner| {
                        assert_eq!(touch(&mut inner), id);
                        // Borrowed again, the parked store is busy once more.
                        assert_eq!(store.with_mut(|_| ()), Err(StoreBusy));
                        assert!(layer.try_with(|_| ()).is_err());
                        store.lend(&mut inner, || {
                            layer.try_with(|mut innermost| touch(&mut innermost)).ok()
                        })
                    });
                    assert_eq!(nested, Ok(Some(id)));
                    assert_eq!(layer.try_with(|mut again| touch(&mut again)).ok(), Some(id));
                    assert_eq!(store.with_mut(|mut again| touch(&mut again)), Ok(id));
                });
                // No longer parked, the store is this borrow's alone again.
                assert_eq!(store.with(|_| ()), Err(StoreBusy));
                touch(&mut outer)
            });

            assert_eq!(outcome, Ok(id));
            assert_eq!(store.with_mut(|mut free| touch(&mut free)), Ok(id));
        }

        #[test]
        fn a_parked_store_that_unwinds_is_given_back() {
            let store = store();
            let layer = store.share().unwrap();

            let caught = store.with_mut(|mut outer| {
                let caught = catch_unwind(AssertUnwindSafe(|| {
                    store.lend(&mut outer, || {
                        let _ = store.with_mut(|_| panic!("host function panicked"));
                    })
                }));
                // Neither the parking nor the slice taken under it is left
                // behind.
                assert_eq!(store.with(|_| ()), Err(StoreBusy));
                assert!(layer.try_with(|_| ()).is_err());
                touch(&mut outer);
                caught.is_err()
            });

            assert_eq!(caught, Ok(true));
            assert_eq!(store.with_mut(|_| ()), Ok(()));
        }

        /// Hooks that record the stores they close.
        #[derive(Default)]
        struct Closing {
            closed: RefCell<Vec<WeakSharedStore>>,
        }

        impl StoreHooks for Closing {
            fn close(&self, store: &SharedStore) {
                assert!(store.try_with(|_| ()).is_ok());
                self.closed.borrow_mut().push(store.downgrade());
            }
        }

        #[test]
        fn hooks_close_a_store_with_its_last_handle_and_go_with_it() {
            for share in [false, true] {
                let store = store();
                if share {
                    store.share().unwrap();
                }
                let hooks = Rc::new(Closing::default());
                assert!(store.set_hooks(Some(hooks.clone())).is_none());

                let last = store.clone();
                drop(store);
                assert!(hooks.closed.borrow().is_empty());
                drop(last);

                let closed = hooks.closed.take();
                assert_eq!(closed.len(), 1);
                // The store was destroyed, and so was its handle to the hooks.
                assert!(closed[0].upgrade().is_none());
                assert_eq!(Rc::strong_count(&hooks), 1);
            }
        }

        #[test]
        fn hooks_that_were_removed_do_not_close_the_store() {
            let store = store();
            let hooks = Rc::new(Closing::default());
            store.set_hooks(Some(hooks.clone()));

            assert!(store.set_hooks(None).is_some());
            drop(store);

            assert!(hooks.closed.borrow().is_empty());
            assert_eq!(Rc::strong_count(&hooks), 1);
        }
    }

    /// A layer over a store made through the C API, as a WASI implementation
    /// would be one.
    #[cfg(all(feature = "extensions-async", feature = "sys", not(miri)))]
    mod layer {
        use super::*;
        use crate::error::take_last_error;
        use crate::wasm_c_api::engine::wasm_engine_new;
        use crate::wasm_c_api::externals::{
            wasm_extern_as_func, wasm_extern_as_global, wasm_extern_as_memory, wasm_extern_t,
            wasm_extern_vec_t, wasm_func_call, wasm_func_new_with_env, wasm_func_t,
            wasm_global_get, wasm_global_set, wasm_global_t, wasm_memory_data,
            wasm_memory_data_size, wasm_memory_grow, wasm_memory_size, wasm_memory_t,
        };
        use crate::wasm_c_api::instance::{
            wasm_instance_delete, wasm_instance_exports, wasm_instance_new, wasm_instance_t,
        };
        use crate::wasm_c_api::module::wasm_module_t;
        use crate::wasm_c_api::trap::wasm_trap_t;
        use crate::wasm_c_api::types::{wasm_functype_t, wasm_valkind_enum};
        use crate::wasm_c_api::value::{wasm_val_inner, wasm_val_t, wasm_val_vec_t};
        use std::cell::RefCell;
        use std::ffi::c_void;
        use std::future::Future;
        use std::mem::MaybeUninit;
        use std::pin::{Pin, pin};
        use std::task::{Context, Poll, Waker};
        use wasmer_api::{
            AsyncEnv, Function, FunctionEnv, FunctionType, Global, Module, RuntimeError,
            SharedStore, Type, Value, WeakSharedStore,
        };

        const GUEST: &str = r#"(module
            (import "env" "host" (func $host (param i32) (result i32)))
            (import "layer" "wait" (func $wait (param i32) (result i32)))
            (import "layer" "exit" (func $exit (param i32)))
            (memory $memory 1)
            (global $counter (mut i32) (i32.const 7))
            (func $twice (param i32) (result i32)
              (i32.mul (local.get 0) (i32.const 2)))
            (func $run (param i32) (result i32)
              (i32.store8 (i32.const 0) (local.get 0))
              (call $host (local.get 0)))
            (func $main (param i32) (result i32)
              (i32.add (call $wait (local.get 0)) (call $run (local.get 0))))
            (func $quit (param i32)
              (call $exit (local.get 0)))
            (export "memory" (memory $memory))
            (export "counter" (global $counter))
            (export "twice" (func $twice))
            (export "run" (func $run))
            (export "main" (func $main))
            (export "quit" (func $quit)))"#;

        /// What ran, in order.
        #[derive(Clone, Default)]
        struct Log(Rc<RefCell<Vec<String>>>);

        impl Log {
            fn push(&self, entry: impl Into<String>) {
                self.0.borrow_mut().push(entry.into());
            }

            fn take(&self) -> Vec<String> {
                self.0.take()
            }
        }

        /// The handles C code keeps to the exports of the guest.
        #[derive(Clone)]
        struct Exports {
            memory: wasm_memory_t,
            counter: wasm_global_t,
            twice: wasm_func_t,
            run: wasm_func_t,
            main: wasm_func_t,
            quit: wasm_func_t,
        }

        /// What the `host` import reaches through its environment, as a C
        /// callback would through statics.
        struct Host {
            exports: RefCell<Option<Exports>>,
            calls: Cell<i32>,
            /// The store as a layer holds it, if it is shared.
            layer_store: Option<WeakSharedStore>,
            log: Log,
        }

        fn i32_val(value: i32) -> wasm_val_t {
            wasm_val_t {
                kind: wasm_valkind_enum::WASM_I32 as _,
                of: wasm_val_inner { int32_t: value },
            }
        }

        /// The result `call` reports for a function that has none.
        const NO_RESULT: i32 = -1;

        /// Calls a function that takes an `i32` through the C API. The error
        /// is the message of the trap.
        fn call(function: &wasm_func_t, argument: i32) -> Result<i32, String> {
            let mut function = function.clone();
            let arguments: wasm_val_vec_t = vec![i32_val(argument)].into();
            let mut results: wasm_val_vec_t = vec![i32_val(NO_RESULT)].into();
            let trap =
                unsafe { wasm_func_call(Some(&mut function), Some(&arguments), &mut results) };
            match trap {
                Some(trap) => Err(trap.inner.message()),
                None => Ok(unsafe { results.as_slice()[0].of.int32_t }),
            }
        }

        fn global_value(global: &wasm_global_t) -> i32 {
            let mut global = global.clone();
            let mut value = i32_val(-1);
            unsafe {
                wasm_global_get(Some(&mut global), Some(&mut value));
                value.of.int32_t
            }
        }

        /// The `host` import. It does what the callback of
        /// `test_host_function_reenters_its_store` does to the store that the
        /// call which reached it is using, and uses the layer's handle too.
        unsafe extern "C" fn host_callback(
            env: *mut c_void,
            arguments: &wasm_val_vec_t,
            results: &mut wasm_val_vec_t,
        ) -> Option<Box<wasm_trap_t>> {
            let host = unsafe { &*(env as *const Host) };
            let depth = unsafe { arguments.as_slice()[0].of.int32_t };
            host.calls.set(host.calls.get() + 1);
            host.log.push(format!("host {depth}"));
            let exports = host.exports.borrow().clone().expect("the exports are set");
            let (mut memory, mut counter) = (exports.memory, exports.counter);

            unsafe {
                // The memory has what `run` stored before it called.
                assert_eq!(wasm_memory_size(Some(&memory)), 1);
                assert_eq!(wasm_memory_data_size(Some(&memory)), 65536);
                let data = wasm_memory_data(Some(&mut memory));
                assert!(!data.is_null());
                assert_eq!(i32::from(*data), depth);
                *data.add(1) += 1;
            }

            let next = global_value(&counter) + 1;
            unsafe { wasm_global_set(Some(&mut counter), Some(&i32_val(next))) };
            assert_eq!(global_value(&counter), next);
            // The layer reaches the same store through its own handle.
            if let Some(store) = host.layer_store.as_ref().and_then(|store| store.upgrade()) {
                let global = counter.extern_.global();
                let seen = store.try_with(|mut store| global.get(&mut store));
                assert_eq!(seen.ok().and_then(|value| value.i32()), Some(next));
            }

            // Enter the guest again: back through this import while there is
            // depth left, and into a plain function after that.
            let result = if depth > 0 {
                let result = call(&exports.run, depth - 1).unwrap() + 1;
                // The innermost call grew the memory, and its `run` was the
                // last one to store its depth.
                unsafe {
                    assert_eq!(wasm_memory_size(Some(&memory)), 2);
                    assert_eq!(*wasm_memory_data(Some(&mut memory)), 0);
                }
                result
            } else {
                let result = call(&exports.twice, 21).unwrap();
                unsafe {
                    assert!(wasm_memory_grow(Some(&mut memory), 1));
                    assert_eq!(wasm_memory_size(Some(&memory)), 2);
                }
                result
            };

            assert_eq!(take_last_error(), None);
            results.as_uninit_slice()[0] = MaybeUninit::new(i32_val(result));
            None
        }

        unsafe extern "C" fn host_finalizer(env: *mut c_void) {
            let host = unsafe { Box::from_raw(env as *mut Host) };
            host.log.push("finalizer");
        }

        /// How the `exit` import leaves the guest.
        #[derive(Debug, Error)]
        #[error("exit {0}")]
        struct Exit(i32);

        /// Pending the first time it is polled.
        #[derive(Default)]
        struct YieldOnce(bool);

        impl Future for YieldOnce {
            type Output = ();

            fn poll(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
                if std::mem::replace(&mut self.0, true) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
        }

        /// A minimal executor: polls `future` until it is done.
        fn drive<F: Future>(future: F, log: &Log) -> F::Output {
            let mut future = pin!(future);
            loop {
                match future
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                {
                    Poll::Ready(output) => return output,
                    Poll::Pending => log.push("suspended"),
                }
            }
        }

        /// Compiles the guest and makes the imports a layer provides: `wait`,
        /// which suspends the guest once, and `exit`, which leaves it.
        fn layer_imports(store: &mut StoreMut<'_>, log: &Log) -> (Module, Function, Function) {
            let module = Module::new(&*store, wat::parse_str(GUEST).unwrap()).unwrap();
            let env = FunctionEnv::new(store, ());
            let log = log.clone();
            let wait = Function::new_typed_async(
                store,
                &env,
                async move |cx: &mut AsyncEnv<()>, x: i32| -> i32 {
                    log.push("wait");
                    let store = cx.with(|env| env.shared()).expect("the store is shared");
                    YieldOnce::default().await;
                    // Between two slices, the layer's handle has the store.
                    assert!(store.try_with(|_| ()).is_ok());
                    log.push("resumed");
                    x * 10
                },
            );
            let exit =
                Function::new_typed(store, |code: i32| -> Result<(), Exit> { Err(Exit(code)) });
            (module, wait, exit)
        }

        fn exports_of(instance: &wasm_instance_t) -> Exports {
            let mut externs: wasm_extern_vec_t = vec![].into();
            unsafe { wasm_instance_exports(instance, &mut externs) };
            let export = |index: usize| externs.as_slice()[index].as_deref();
            let function = |index| wasm_extern_as_func(export(index)).unwrap().clone();
            Exports {
                memory: wasm_extern_as_memory(export(0)).unwrap().clone(),
                counter: wasm_extern_as_global(export(1)).unwrap().clone(),
                twice: function(2),
                run: function(3),
                main: function(4),
                quit: function(5),
            }
        }

        /// A store made through the C API, with the guest in it.
        struct Guest {
            store: Box<wasm_store_t>,
            instance: Box<wasm_instance_t>,
            exports: Exports,
            host: *const Host,
        }

        impl Guest {
            /// Makes the store, shares it if `share`, and instantiates the
            /// guest in it, as C code and a layer would between them.
            fn new(share: bool, log: &Log) -> Self {
                let engine = wasm_engine_new();
                let mut store = unsafe { wasm_store_new(Some(&engine)) }.unwrap();
                let layer_store = share.then(|| store.inner.share().unwrap().downgrade());
                let host = Box::into_raw(Box::new(Host {
                    exports: RefCell::new(None),
                    calls: Cell::new(0),
                    layer_store,
                    log: log.clone(),
                }));
                let host_type =
                    wasm_functype_t::new(FunctionType::new(vec![Type::I32], vec![Type::I32]));
                let host_function = unsafe {
                    wasm_func_new_with_env(
                        Some(&mut store),
                        Some(&host_type),
                        Some(host_callback),
                        host.cast(),
                        Some(host_finalizer),
                    )
                }
                .unwrap();
                let (module, wait, exit) = store
                    .inner
                    .with_mut(|mut raw| layer_imports(&mut raw, log))
                    .unwrap();
                let import = |function: Function| {
                    Some(Box::new(wasm_extern_t::new(
                        store.inner.clone(),
                        function.into(),
                    )))
                };
                let imports: wasm_extern_vec_t = vec![
                    Some(Box::new(host_function.extern_.clone())),
                    import(wait),
                    import(exit),
                ]
                .into();
                let module = wasm_module_t { inner: module };
                let instance = unsafe {
                    wasm_instance_new(Some(&mut store), Some(&module), Some(&imports), None)
                }
                .unwrap();
                let exports = exports_of(&instance);
                unsafe { &*host }.exports.replace(Some(exports.clone()));
                Self {
                    store,
                    instance,
                    exports,
                    host,
                }
            }

            fn host_calls(&self) -> i32 {
                // The environment lives until the store finalizes it.
                unsafe { &*self.host }.calls.get()
            }

            /// Deletes every handle to the store that is left, the store's
            /// own last.
            fn delete(self) {
                unsafe { &*self.host }.exports.take();
                drop(self.exports);
                unsafe {
                    wasm_instance_delete(Some(self.instance));
                    wasm_store_delete(Some(self.store));
                }
            }
        }

        /// The scenario of `test_host_function_reenters_its_store`, on a
        /// store that is the cell's own and on one that is shared.
        #[test]
        fn a_host_function_reenters_its_store_shared_or_not() {
            for share in [false, true] {
                let log = Log::default();
                let guest = Guest::new(share, &log);

                // The import runs at depths 2, 1 and 0. The innermost one
                // returns `twice(21)` and each of the others adds one.
                assert_eq!(call(&guest.exports.run, 2), Ok(44));
                assert_eq!(guest.host_calls(), 3);

                // What the callbacks did to the store is there afterwards.
                assert_eq!(global_value(&guest.exports.counter), 10);
                let mut memory = guest.exports.memory.clone();
                unsafe {
                    assert_eq!(wasm_memory_size(Some(&memory)), 2);
                    assert_eq!(*wasm_memory_data(Some(&mut memory)).add(1), 3);
                }
                assert_eq!(take_last_error(), None);

                drop(memory);
                guest.delete();
                assert_eq!(log.take(), ["host 2", "host 1", "host 0", "finalizer"]);
            }
        }

        #[test]
        fn entry_points_refuse_a_shared_store_that_is_held() {
            let guest = Guest::new(true, &Log::default());
            let busy = StoreBusy.to_string();
            let layer_store = guest.store.inner.shared().unwrap();
            let (mut memory, mut counter) =
                (guest.exports.memory.clone(), guest.exports.counter.clone());
            let mut out = i32_val(-1);

            layer_store
                .try_with(|_| unsafe {
                    wasm_global_get(Some(&mut counter), Some(&mut out));
                    assert_eq!(take_last_error().as_ref(), Some(&busy));
                    assert_eq!(out.of.int32_t, -1);

                    wasm_global_set(Some(&mut counter), Some(&i32_val(1)));
                    assert_eq!(take_last_error().as_ref(), Some(&busy));

                    assert!(wasm_memory_data(Some(&mut memory)).is_null());
                    assert_eq!(take_last_error().as_ref(), Some(&busy));

                    assert_eq!(call(&guest.exports.twice, 1).as_ref(), Err(&busy));
                })
                .unwrap();

            // Released, the store is usable again and was left as it was.
            assert_eq!(global_value(&counter), 7);
            assert_eq!(call(&guest.exports.twice, 1), Ok(2));
            assert_eq!(take_last_error(), None);

            drop((memory, counter));
            guest.delete();
        }

        /// The entry of a layer, as a WASI implementation would have it.
        struct Layer {
            log: Log,
            /// The function whose calls the layer leaves to `wasm_func_call`.
            twice: Function,
            counter: Global,
        }

        /// Maps the way the guest left: an exit with code 0 is not a trap.
        fn map_exit(outcome: CallOutcome) -> CallOutcome {
            match outcome.map_err(|error| error.downcast::<Exit>()) {
                Ok(results) => Ok(results),
                Err(Ok(Exit(0))) => Ok(Box::new([])),
                Err(Ok(Exit(code))) => Err(RuntimeError::new(format!("exited with code {code}"))),
                Err(Err(error)) => Err(error),
            }
        }

        impl StoreHooks for Layer {
            fn call(
                &self,
                store: &StoreRef,
                function: &Function,
                params: &[Value],
            ) -> Option<CallOutcome> {
                if *function == self.twice {
                    self.log.push("declined");
                    return None;
                }
                self.log.push("set-up");
                let mut store = store.shared().expect("the layer shared the store");
                let outcome = drive(function.call_async(&mut store, params), &self.log);
                self.log.push("returned");
                let outcome = map_exit(outcome);
                self.log.push("clean-up");
                Some(outcome)
            }

            fn close(&self, store: &SharedStore) {
                let counter = store.try_with(|mut store| self.counter.get(&mut store));
                let counter = counter.ok().and_then(|value| value.i32());
                self.log.push(format!("close, counter {counter:?}"));
            }
        }

        #[test]
        fn a_layer_runs_the_calls_of_a_shared_store_under_its_own_entry() {
            let log = Log::default();
            let guest = Guest::new(true, &log);
            let exports = guest.exports.clone();
            let layer = Layer {
                log: log.clone(),
                twice: exports.twice.extern_.function(),
                counter: exports.counter.extern_.global(),
            };
            guest.store.inner.set_hooks(Some(Rc::new(layer)));

            // `main` suspends in `wait`, then reaches `host` at depths 2, 1
            // and 0. The calls that `host` makes are not offered to the
            // layer: `twice` is not declined there.
            assert_eq!(call(&exports.main, 2), Ok(20 + 44));
            let entry = ["set-up", "wait", "suspended", "resumed"];
            let rest = ["host 2", "host 1", "host 0", "returned", "clean-up"];
            assert_eq!(log.take(), [entry.as_slice(), rest.as_slice()].concat());
            assert_eq!(guest.host_calls(), 3);

            // A call the layer declines takes the usual path.
            assert_eq!(call(&exports.twice, 4), Ok(8));
            assert_eq!(log.take(), ["declined"]);

            // The layer maps the way the guest left to a result or a trap.
            assert_eq!(call(&exports.quit, 0), Ok(NO_RESULT));
            assert_eq!(log.take(), ["set-up", "returned", "clean-up"]);
            let trapped = call(&exports.quit, 3);
            assert_eq!(trapped, Err("exited with code 3".to_string()));
            assert_eq!(log.take(), ["set-up", "returned", "clean-up"]);
            assert_eq!(take_last_error(), None);

            // The store outlives `wasm_store_delete` for as long as a handle
            // to it is left. It is closed with the last one, while it is
            // still whole, and its environments are finalized after that.
            let store = guest.store.inner.shared().unwrap().downgrade();
            let last = exports.twice.clone();
            drop(exports);
            guest.delete();
            assert!(log.take().is_empty());
            drop(last);
            assert_eq!(log.take(), ["close, counter Some(10)", "finalizer"]);
            assert!(store.upgrade().is_none());
        }

        #[test]
        fn a_call_made_from_a_host_function_is_not_offered_to_the_layer() {
            let log = Log::default();
            let guest = Guest::new(true, &log);
            let layer = Layer {
                log: log.clone(),
                twice: guest.exports.twice.extern_.function(),
                counter: guest.exports.counter.extern_.global(),
            };
            guest.store.inner.set_hooks(Some(Rc::new(layer)));

            // The layer enters the guest itself, and not from
            // `wasm_func_call`. Still, `host` calls `run` and then `twice`
            // from inside a slice, where the layer could not run them.
            let mut store = guest.store.inner.shared().unwrap();
            let run = guest.exports.run.extern_.function();
            let results = drive(run.call_async(&mut store, &[Value::I32(1)]), &log);

            assert_eq!(results.unwrap()[0].i32(), Some(43));
            assert_eq!(log.take(), ["host 1", "host 0"]);
            guest.delete();
        }

        /// Hooks that run the call they are offered through `wasm_func_call`.
        struct Again(Log);

        impl StoreHooks for Again {
            fn call(
                &self,
                store: &StoreRef,
                function: &Function,
                params: &[Value],
            ) -> Option<CallOutcome> {
                self.0.push("offered");
                let function = wasm_func_t {
                    extern_: wasm_extern_t::new(store.clone(), function.clone().into()),
                };
                let result = call(&function, params[0].unwrap_i32());
                Some(match result {
                    Ok(result) => Ok(Box::new([Value::I32(result)])),
                    Err(message) => Err(RuntimeError::new(message)),
                })
            }
        }

        #[test]
        fn a_call_made_by_the_hooks_is_not_offered_to_them_again() {
            let log = Log::default();
            let guest = Guest::new(false, &log);
            guest
                .store
                .inner
                .set_hooks(Some(Rc::new(Again(log.clone()))));

            assert_eq!(call(&guest.exports.twice, 3), Ok(6));

            assert_eq!(log.take(), ["offered"]);
            guest.delete();
        }

        /// Hooks that take a call over, and remove themselves as they do.
        struct Leaving(Log);

        impl StoreHooks for Leaving {
            fn call(&self, store: &StoreRef, _: &Function, _: &[Value]) -> Option<CallOutcome> {
                // The store gives up its handle to these hooks, which the
                // call that is running keeps.
                drop(store.set_hooks(None));
                self.0.push("left");
                Some(Ok(Box::new([Value::I32(5)])))
            }

            fn close(&self, _: &SharedStore) {
                self.0.push("close");
            }
        }

        #[test]
        fn hooks_removed_during_their_call_finish_it() {
            let log = Log::default();
            let guest = Guest::new(false, &log);
            let hooks = Rc::new(Leaving(log.clone()));
            guest.store.inner.set_hooks(Some(hooks));

            assert_eq!(call(&guest.exports.twice, 1), Ok(5));
            // They are gone: the next call is not theirs, nor is the store
            // theirs to close.
            assert_eq!(call(&guest.exports.twice, 1), Ok(2));
            guest.delete();

            assert_eq!(log.take(), ["left", "finalizer"]);
        }
    }
}

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

/// A store, and the record of who is using it.
///
/// The C API has no borrow checker: a host function called by
/// `wasm_func_call` calls back into the C API on the very store that call is
/// using. This cell hands the store out through closures, so that it knows
/// when the store is borrowed, and lets a host function lend the borrow it
/// was given to the calls it makes.
struct StoreCell {
    store: UnsafeCell<Store>,
    id: StoreId,
    access: Cell<Access>,
}

impl StoreCell {
    fn new(store: Store) -> Self {
        Self {
            id: store.id(),
            store: UnsafeCell::new(store),
            access: Cell::new(Access {
                lent: None,
                held: Held::Free,
            }),
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
}

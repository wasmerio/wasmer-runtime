//! Owner-retained runtime cells with a scoped route during late TLS destruction.

use std::{
    cell::{Cell, RefCell},
    marker::PhantomData,
    rc::Rc,
    thread::{AccessError, LocalKey},
};

pub(crate) struct TeardownTls<T: 'static> {
    primary: &'static LocalKey<Rc<RefCell<T>>>,
    active: &'static LocalKey<Cell<*const Rc<RefCell<T>>>>,
}

impl<T: 'static> TeardownTls<T> {
    pub(crate) const fn new(
        primary: &'static LocalKey<Rc<RefCell<T>>>,
        active: &'static LocalKey<Cell<*const Rc<RefCell<T>>>>,
    ) -> Self {
        Self { primary, active }
    }

    pub(crate) fn with<R>(&'static self, operation: impl FnOnce(&RefCell<T>) -> R) -> R {
        self.try_with(operation)
            .expect("native runtime TLS requires a live owner or teardown scope")
    }

    pub(crate) fn try_with<R>(
        &'static self,
        operation: impl FnOnce(&RefCell<T>) -> R,
    ) -> Result<R, AccessError> {
        self.active.with(|active| {
            let pointer = active.get();
            if pointer.is_null() {
                self.primary.try_with(|state| operation(state.as_ref()))
            } else {
                // Clone before arbitrary code: the callback may drop its own
                // route and reclaim the Box while still borrowing this cell.
                let state = Rc::clone(unsafe { &*pointer });
                Ok(operation(state.as_ref()))
            }
        })
    }

    pub(crate) fn capture(&'static self) -> Rc<RefCell<T>> {
        self.active.with(|active| {
            let pointer = active.get();
            if pointer.is_null() {
                self.primary.with(Rc::clone)
            } else {
                // The scoped guard owns this Rc; capture retains the same cell.
                unsafe { &*pointer }.clone()
            }
        })
    }

    pub(crate) fn enter(&'static self, state: Rc<RefCell<T>>) -> TeardownTlsGuard<T> {
        let previous = self.active.with(Cell::get);
        if !previous.is_null() {
            assert!(
                Rc::ptr_eq(unsafe { &*previous }, &state),
                "nested native teardown changed runtime-state identity"
            );
            return TeardownTlsGuard {
                key: self,
                owned: None,
                late: true,
                _local: PhantomData,
            };
        }
        if let Ok(same) = self.primary.try_with(|current| Rc::ptr_eq(current, &state)) {
            assert!(same, "native teardown belongs to another runtime thread");
            return TeardownTlsGuard {
                key: self,
                owned: None,
                late: false,
                _local: PhantomData,
            };
        }
        // The TLS destructor released only its Rc. Physical roots/cleanup
        // owners retain the original cell, including live paused-stack rows.
        let owned = Box::new(state);
        self.active.with(|active| active.set(&*owned));
        TeardownTlsGuard {
            key: self,
            owned: Some(owned),
            late: true,
            _local: PhantomData,
        }
    }
}

/// Used only by synchronous teardown frames, never stored in a pending future.
pub(crate) struct TeardownTlsGuard<T: 'static> {
    key: &'static TeardownTls<T>,
    owned: Option<Box<Rc<RefCell<T>>>>,
    late: bool,
    _local: PhantomData<Rc<()>>,
}
impl<T: 'static> TeardownTlsGuard<T> {
    pub(crate) fn is_late(&self) -> bool {
        self.late
    }
}
impl<T: 'static> Drop for TeardownTlsGuard<T> {
    fn drop(&mut self) {
        if let Some(owned) = self.owned.take() {
            self.key.active.with(|active| {
                assert_eq!(
                    active.get(),
                    &*owned as *const _,
                    "native teardown route restored out of order"
                );
                active.set(std::ptr::null());
            });
            drop(owned);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    thread_local! {
        static PRIMARY: Rc<RefCell<Vec<Probe>>> = Rc::new(RefCell::new(Vec::new()));
        static ACTIVE: Cell<*const Rc<RefCell<Vec<Probe>>>> = const { Cell::new(std::ptr::null()) };
        static OWNER: RefCell<Option<Owner>> = const { RefCell::new(None) };
        static DROPPING_OWNER: RefCell<Option<DroppingOwner>> = const { RefCell::new(None) };
    }
    static KEY: TeardownTls<Vec<Probe>> = TeardownTls::new(&PRIMARY, &ACTIVE);
    struct Probe(Arc<AtomicUsize>);
    impl Drop for Probe {
        fn drop(&mut self) {
            ACTIVE
                .with(|active| assert!(active.get().is_null(), "state dropped before route reset"));
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }
    struct Owner {
        state: Rc<RefCell<Vec<Probe>>>,
        late: bool,
        checks: Arc<AtomicUsize>,
    }
    impl Drop for Owner {
        fn drop(&mut self) {
            let before = Rc::strong_count(&self.state);
            {
                let route = KEY.enter(self.state.clone());
                assert_eq!(route.is_late(), self.late);
                let pointer = ACTIVE.with(Cell::get);
                KEY.with(|state| assert_eq!(state.borrow().len(), 1));
                let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let nested = KEY.enter(self.state.clone());
                    assert_eq!(nested.is_late(), self.late);
                    assert_eq!(ACTIVE.with(Cell::get), pointer);
                    assert!(Rc::ptr_eq(&KEY.capture(), &self.state));
                    panic!("nested teardown proof");
                }))
                .unwrap_err();
                assert_eq!(panic.downcast_ref::<&str>(), Some(&"nested teardown proof"));
                assert_eq!(ACTIVE.with(Cell::get), pointer);
                KEY.with(|state| assert_eq!(state.borrow().len(), 1));
            }
            assert!(ACTIVE.with(Cell::get).is_null());
            assert_eq!(
                Rc::strong_count(&self.state),
                before,
                "guard Box leaked its Rc"
            );
            // A second independent scope must use the exact cell after unwind.
            {
                let _route = KEY.enter(self.state.clone());
                assert!(Rc::ptr_eq(&KEY.capture(), &self.state));
            }
            assert!(ACTIVE.with(Cell::get).is_null());
            assert_eq!(Rc::strong_count(&self.state), before);
            self.checks.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct DroppingOwner {
        state: Option<Rc<RefCell<Vec<Probe>>>>,
        checks: Arc<AtomicUsize>,
    }
    impl Drop for DroppingOwner {
        fn drop(&mut self) {
            let state = self.state.take().unwrap();
            let weak = Rc::downgrade(&state);
            let mut route = Some(KEY.enter(state));
            assert!(route.as_ref().unwrap().is_late());
            let used = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                KEY.with(|cell| {
                    drop(route.take());
                    assert!(ACTIVE.with(Cell::get).is_null());
                    assert!(
                        weak.upgrade().is_some(),
                        "callback lost its exact backing cell"
                    );
                    assert_eq!(cell.borrow().len(), 1);
                    {
                        let _nested = KEY.enter(weak.upgrade().unwrap());
                        KEY.with(|same| assert!(std::ptr::eq(same, cell)));
                    }
                    assert!(ACTIVE.with(Cell::get).is_null());
                    assert_eq!(cell.borrow().len(), 1);
                });
            }));
            if used.is_ok() {
                assert!(weak.upgrade().is_none(), "callback leaked the backing cell");
                self.checks.fetch_add(1, Ordering::SeqCst);
            }
        }
    }

    #[test]
    fn late_callback_can_drop_its_own_route_without_releasing_the_borrowed_cell() {
        let dropped = Arc::new(AtomicUsize::new(0));
        let checked = Arc::new(AtomicUsize::new(0));
        let (dropped_thread, checked_thread) = (dropped.clone(), checked.clone());
        std::thread::spawn(move || {
            DROPPING_OWNER.with(|_| ());
            let state = KEY.capture();
            state.borrow_mut().push(Probe(dropped_thread));
            DROPPING_OWNER.with(|owner| {
                *owner.borrow_mut() = Some(DroppingOwner {
                    state: Some(state),
                    checks: checked_thread,
                })
            });
        })
        .join()
        .unwrap();
        assert_eq!(
            checked.load(Ordering::SeqCst),
            1,
            "late callback failed while dropping its route"
        );
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            1,
            "callback or route leaked its original cell"
        );
    }

    #[test]
    fn scoped_routes_restore_and_reclaim_after_nested_panic_in_both_tls_orders() {
        for external_first in [false, true] {
            let dropped = Arc::new(AtomicUsize::new(0));
            let checked = Arc::new(AtomicUsize::new(0));
            let (dropped_thread, checked_thread) = (dropped.clone(), checked.clone());
            std::thread::spawn(move || {
                if external_first {
                    OWNER.with(|_| ());
                }
                let state = KEY.capture();
                state.borrow_mut().push(Probe(dropped_thread));
                OWNER.with(|owner| {
                    *owner.borrow_mut() = Some(Owner {
                        state,
                        late: external_first,
                        checks: checked_thread,
                    })
                });
            })
            .join()
            .unwrap();
            assert_eq!(checked.load(Ordering::SeqCst), 1);
            assert_eq!(
                dropped.load(Ordering::SeqCst),
                1,
                "retained cell leaked after its last physical owner"
            );
        }
    }
}

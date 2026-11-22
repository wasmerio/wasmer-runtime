use std::{
    cell::{Cell, UnsafeCell},
    fmt,
    ops::{Deref, DerefMut},
    rc::Rc,
};

use wasmer_types::StoreId;

use super::{BackendStore, StoreInner, StoreObjects};

/// The allocation outlives an engine-owned guest stack even when its call is
/// forgotten and the public Store is dropped. Only the owning Store, a shared
/// store slice, or an authorized guest activation may dereference its value.
pub(crate) struct AllocationInner {
    pub(crate) value: UnsafeCell<StoreInner>,
    pub(crate) busy: Cell<bool>,
    pub(crate) lent: Cell<*mut StoreInner>,
    pub(crate) id: StoreId,
    pub(crate) shared: Cell<bool>,
    #[cfg(feature = "js")]
    pub(crate) guest: Rc<crate::backend::js::guest_guard::GuestGuard>,
}

/// A store's access capability. Cloning the allocation's owner does not grant
/// permission to access its value; shared calls and JSPI enforce that separately.
#[derive(Clone)]
pub(crate) struct StoreAllocation(pub(crate) Rc<AllocationInner>);

impl StoreAllocation {
    pub(crate) fn new(store: BackendStore) -> Self {
        let objects = StoreObjects::from_store_ref(&store);
        let id = objects.id();
        Self(Rc::new_cyclic(|allocation| AllocationInner {
            value: UnsafeCell::new(StoreInner {
                objects,
                store,
                allocation: allocation.clone(),
            }),
            busy: Cell::new(false),
            lent: Cell::new(std::ptr::null_mut()),
            id,
            shared: Cell::new(false),
            #[cfg(feature = "js")]
            guest: Rc::default(),
        }))
    }

    pub(crate) fn into_inner(self) -> StoreInner {
        #[cfg(feature = "js")]
        self.0.guest.assert_access();
        Rc::try_unwrap(self.0)
            .unwrap_or_else(|_| panic!("cannot move a store while a guest still owns it"))
            .value
            .into_inner()
    }
}

impl Deref for StoreAllocation {
    type Target = StoreInner;

    fn deref(&self) -> &StoreInner {
        #[cfg(feature = "js")]
        self.0.guest.assert_access();
        // SAFETY: the capability's owner excludes other access. JSPI also
        // checks this when a guest can run after its Rust borrow has ended.
        unsafe { &*self.0.value.get() }
    }
}

impl DerefMut for StoreAllocation {
    fn deref_mut(&mut self) -> &mut StoreInner {
        #[cfg(feature = "js")]
        self.0.guest.assert_access();
        // SAFETY: mutable access is available only through the unique Store
        // or an exclusive shared-store slice, never through an owner alone.
        unsafe { &mut *self.0.value.get() }
    }
}

impl fmt::Debug for StoreAllocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("StoreAllocation")
            .field("id", &self.0.id)
            .finish()
    }
}

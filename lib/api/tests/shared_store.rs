//! Store capability regressions that do not execute compiled WebAssembly and
//! can run under Miri with `sys,headless,experimental-async`.
#![cfg(all(
    feature = "experimental-async",
    feature = "sys",
    not(feature = "v8-default")
))]

use wasmer::{FunctionEnv, Store};

#[test]
fn a_shared_slice_cannot_reenter_through_the_released_current_store() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0usize);
    let shared = store.into_shared();
    shared
        .try_with(|mut store| {
            let mut outer = env.clone().into_mut(&mut store);
            outer.release(|| {
                shared
                    .try_with(|mut store| {
                        let state = env.as_mut(&mut store);
                        let reentered = Store::with_current(|current| {
                            *env.as_mut(current) += 10;
                        });
                        *state += 1;
                        assert!(
                            reentered.is_none(),
                            "the shared slice already owns this store"
                        );
                    })
                    .unwrap();
            });
            assert_eq!(*outer.data(), 1);
        })
        .unwrap();
}

#[test]
fn a_current_store_callback_cannot_reenter_through_a_shared_handle() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0usize);
    let shared = store.into_shared();
    shared
        .try_with(|mut store| {
            let mut outer = env.clone().into_mut(&mut store);
            outer.release(|| {
                Store::with_current(|current| {
                    let state = env.as_mut(current);
                    let reentered = shared.try_with(|mut store| {
                        *env.as_mut(&mut store) += 10;
                    });
                    *state += 1;
                    assert!(
                        reentered.is_err(),
                        "the current callback already owns this store"
                    );
                })
                .unwrap();
            });
            assert_eq!(*outer.data(), 1);
        })
        .unwrap();
}

#[test]
fn parked_current_callbacks_can_lend_again_without_aliasing_outer_slices() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0usize);
    let shared = store.into_shared();
    shared
        .try_with(|mut store| {
            let mut outer = env.clone().into_mut(&mut store);
            outer.release(|| {
                Store::with_current(|current| {
                    *env.as_mut(current) += 1;
                    current.parked(|| {
                        shared
                            .try_with(|mut store| *env.as_mut(&mut store) += 2)
                            .unwrap();
                        Store::with_current(|current| *env.as_mut(current) += 4).unwrap();
                    });
                    assert!(shared.try_with(|_| ()).is_err());
                    *env.as_mut(current) += 8;
                })
                .unwrap();
            });
            assert_eq!(*outer.data(), 15);
        })
        .unwrap();
}

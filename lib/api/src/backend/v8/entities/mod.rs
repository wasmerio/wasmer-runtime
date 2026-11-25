pub mod engine;
pub(crate) mod exception;
pub(crate) mod external;
pub(crate) mod function;
pub(crate) mod global;
pub(crate) mod instance;
pub(crate) mod memory;
pub(crate) mod module;
pub(crate) mod store;
pub(crate) mod table;
pub(crate) mod tag;

thread_local! {
    /// This thread, kept: asking the standard library takes longer than a
    /// guest entry should.
    static THREAD: std::thread::ThreadId = std::thread::current().id();
}

/// Panics unless `store` is used on the thread of its isolate. V8 does not
/// check it, and takes the limit of the stack from the thread the store was
/// created on.
fn check_isolate(store: &impl crate::AsStoreRef) {
    let store = store.as_store_ref();
    let v8_store = store.inner.store.as_v8();

    if THREAD.with(|thread| *thread) != v8_store.thread_id {
        panic!(
            "Fatal error (v8): current thread is different from the thread the store was created in!"
        );
    }
}

/// Panics unless an object of the store `owner` is used with that store, on
/// the thread of its isolate. V8 checks neither: an object of another store
/// is one of another isolate.
pub(crate) fn check_owner(store: &impl crate::AsStoreRef, owner: wasmer_types::StoreId) {
    assert_eq!(
        owner,
        store.as_store_ref().objects().id(),
        "object used with the wrong context"
    );
    check_isolate(store);
}

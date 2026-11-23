//! Keeps a thread's call state consistent across JavaScript exceptions.
//!
//! A JavaScript exception unwinds through Rust frames without running their
//! destructors. Host code can raise one by calling any binding without
//! `catch`. So the code that runs host futures and lends the store runs them
//! through [`catching`], which restores what the skipped frames would have.
//!
//! Synchronous host functions do not pass through here, so their fast path has
//! no JavaScript frame: the call that lent them the store restores its lends
//! when its guest returns (see `lent`).

use wasm_bindgen::{JsValue, closure::ScopedClosure, prelude::wasm_bindgen};

use crate::{
    PollStore, StoreContext,
    entities::store::{published_store, publish_store},
    js::{function, lent},
};

#[wasm_bindgen(inline_js = "export function jspiCatching(f) { f(); }")]
extern "C" {
    #[wasm_bindgen(js_name = jspiCatching, catch)]
    fn shim_catching(f: &JsValue) -> Result<(), JsValue>;
}

/// What the frames inside a boundary may leave changed.
struct Saved {
    lends: (usize, bool),
    context: (usize, u32),
    published: Option<PollStore>,
    busy: Option<bool>,
    entered: bool,
}

impl Saved {
    fn capture() -> Self {
        let published = published_store();
        Self {
            lends: lent::checkpoint(),
            context: StoreContext::checkpoint(),
            published,
            busy: published.and_then(PollStore::busy),
            entered: function::entered_synchronously(),
        }
    }

    fn restore(self) {
        lent::restore(self.lends);
        StoreContext::restore_checkpoint(self.context);
        publish_store(self.published);
        if let (Some(store), Some(busy)) = (self.published, self.busy) {
            store.set_busy(busy);
        }
        function::mark_entered(self.entered);
    }
}

/// Runs `f`, and returns what JavaScript threw through it, with the thread's
/// call state as it was before `f` ran. A future `f` was polling must then be
/// forgotten: it was left part way through, and can be neither resumed nor
/// dropped.
pub(crate) fn catching<R>(f: impl FnOnce() -> R) -> Result<R, JsValue> {
    let saved = Saved::capture();
    let mut f = Some(f);
    let mut result = None;
    let mut call = || result = f.take().map(|f| f());
    let closure = ScopedClosure::borrow_mut_assert_unwind_safe(&mut call);
    let called = shim_catching(closure.as_ref());
    drop(closure);
    match called {
        Ok(()) => Ok(result.expect("the boundary calls its function synchronously")),
        Err(error) => {
            saved.restore();
            Err(error)
        }
    }
}

//! Same-thread, synchronous V8 host-future execution. No V8 stack is suspended.

use std::{
    any::Any,
    cell::RefCell,
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind, resume_unwind},
    pin::Pin,
    rc::{Rc, Weak},
    sync::{Arc, Condvar, Mutex},
    task::{Context, Poll, Wake, Waker},
    thread::ThreadId,
};

use crate::{AsStoreMut, RuntimeError, StoreAsync, WeakStoreAsync};

type Panic = Box<dyn Any + Send>;

/// The owning embedding's synchronous driver for its existing local tasks.
///
/// This runs on the Store's original thread and must support lexical nesting.
/// It must not return success before the supplied future completes.
#[doc(hidden)]
pub trait BlockingExecutor {
    /// Drive this exact borrowed future, including the owner's admitted tasks.
    fn block_on(&self, future: Pin<&mut dyn Future<Output = ()>>) -> Result<(), RuntimeError>;
}

/// Bind the actual Store's existing owner; no runtime or executor is created.
/// An expired bound owner is an error, rather than a fallback to another driver.
#[doc(hidden)]
pub fn set_blocking_executor(
    store: &mut impl AsStoreMut,
    executor: Weak<dyn BlockingExecutor>,
) -> Result<Option<Weak<dyn BlockingExecutor>>, RuntimeError> {
    let view = store.as_store_mut();
    if !view.inner.store.is_v8() {
        return Err(RuntimeError::new("V8 executor requires a V8 Store"));
    }
    let binding = &view.inner.store.as_v8().async_binding;
    binding.check_thread()?;
    let previous = binding.executor.borrow_mut().replace(executor);
    Ok(previous)
}

pub(crate) struct AsyncBinding {
    owner: ThreadId,
    backing: RefCell<Option<WeakStoreAsync>>,
    executor: RefCell<Option<Weak<dyn BlockingExecutor>>>,
    active: RefCell<Weak<CallFrame>>,
    panic: RefCell<Option<Panic>>,
}

impl std::fmt::Debug for AsyncBinding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncBinding")
            .field("owner", &self.owner)
            .finish_non_exhaustive()
    }
}

impl AsyncBinding {
    pub(crate) fn new() -> Rc<Self> {
        Rc::new(Self {
            owner: std::thread::current().id(),
            backing: RefCell::new(None),
            executor: RefCell::new(None),
            active: RefCell::new(Weak::new()),
            panic: RefCell::new(None),
        })
    }

    pub(crate) fn bind(&self, store: &StoreAsync) {
        // Conversion owns the exclusive Box; no old lock can remain usable.
        let previous = self.backing.borrow_mut().replace(store.downgrade());
        drop(previous);
    }

    pub(crate) fn check_thread(&self) -> Result<(), RuntimeError> {
        if self.owner != std::thread::current().id() {
            return Err(RuntimeError::new("V8 Store is on another thread"));
        }
        Ok(())
    }

    pub(crate) fn store(&self) -> Result<StoreAsync, RuntimeError> {
        self.check_thread()?;
        let backing = self.backing.borrow().clone();
        backing
            .and_then(|store| store.upgrade())
            .ok_or_else(|| RuntimeError::new("V8 async callback has no live asynchronous Store"))
    }

    pub(crate) fn check_async_entry(&self) -> Result<(), RuntimeError> {
        self.check_thread()?;
        let active = self.active.borrow().upgrade();
        if active.is_some_and(|frame| frame.sync_borrows.get() != 0) {
            return Err(RuntimeError::new(
                "V8 async entry cannot borrow a synchronous host callback's Store",
            ));
        }
        // Check before user factory/guest execution, not after admission.
        if let Some(executor) = self.executor.borrow().as_ref()
            && executor.strong_count() == 0
        {
            return Err(RuntimeError::new("V8 Store's bound executor has expired"));
        }
        Ok(())
    }

    pub(crate) fn has_active_call(&self) -> bool {
        self.active.borrow().upgrade().is_some()
    }

    pub(crate) fn enter(self: &Rc<Self>) -> Result<CallGuard, RuntimeError> {
        self.check_async_entry()?;
        let frame = Rc::new(CallFrame {
            sync_borrows: Default::default(),
        });
        let previous = self.active.replace(Rc::downgrade(&frame));
        Ok(CallGuard {
            binding: self.clone(),
            frame,
            previous,
        })
    }

    pub(crate) fn sync_borrow(&self) -> Option<SyncBorrow> {
        let frame = self.active.borrow().upgrade()?;
        frame.sync_borrows.set(frame.sync_borrows.get() + 1);
        Some(SyncBorrow(frame))
    }

    pub(crate) fn record(&self, panic: Panic, entry_unwinding: bool) {
        if entry_unwinding {
            discard_panic(panic);
            return;
        }
        let secondary = {
            let mut first = self.panic.borrow_mut();
            if first.is_none() {
                *first = Some(panic);
                None
            } else {
                Some(panic)
            }
        };
        if let Some(secondary) = secondary {
            discard_panic(secondary);
        }
    }

    pub(crate) fn run_host_future<T>(
        &self,
        future: impl Future<Output = Result<T, RuntimeError>>,
    ) -> Result<T, RuntimeError> {
        let mut future = Box::pin(future);
        let entry_unwinding = std::thread::panicking();
        let result = catch_unwind(AssertUnwindSafe(|| {
            self.block_on(std::future::poll_fn(|cx| {
                let unwinding = std::thread::panicking();
                match catch_unwind(AssertUnwindSafe(|| future.as_mut().poll(cx))) {
                    Ok(poll) => poll,
                    Err(panic) => {
                        self.record(panic, unwinding);
                        Poll::Ready(Err(RuntimeError::new("V8 host future panicked")))
                    }
                }
            }))
        }));
        let result = match result {
            Ok(result) => result,
            Err(panic) => {
                self.record(panic, entry_unwinding);
                Err(RuntimeError::new("V8 executor panicked"))
            }
        };
        let unwinding = std::thread::panicking();
        if let Err(panic) = catch_unwind(AssertUnwindSafe(|| drop(future))) {
            self.record(panic, unwinding);
        }
        result?
    }

    pub(crate) fn take_panic(&self) -> Option<Panic> {
        self.panic.borrow_mut().take()
    }

    pub(crate) fn block_on<T>(&self, future: impl Future<Output = T>) -> Result<T, RuntimeError> {
        self.check_async_entry()?;
        let selected = self.executor.borrow().clone();
        let executor = match selected {
            Some(executor) => Some(
                executor
                    .upgrade()
                    .ok_or_else(|| RuntimeError::new("V8 Store's bound executor has expired"))?,
            ),
            None => None,
        };
        let mut output = None;
        {
            let task = async { output = Some(future.await) };
            let mut task = std::pin::pin!(task);
            match executor {
                Some(executor) => executor.block_on(task.as_mut())?,
                None => park_on_thread(task.as_mut()),
            }
        }
        output.ok_or_else(|| RuntimeError::new("V8 executor returned before completion"))
    }
}

struct CallFrame {
    sync_borrows: std::cell::Cell<usize>,
}

pub(crate) struct CallGuard {
    binding: Rc<AsyncBinding>,
    frame: Rc<CallFrame>,
    previous: Weak<CallFrame>,
}

impl CallGuard {
    pub(crate) fn is_outermost(&self) -> bool {
        self.previous.upgrade().is_none()
    }
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        let mut current = self.binding.active.borrow_mut();
        debug_assert!(current.ptr_eq(&Rc::downgrade(&self.frame)));
        *current = self.previous.clone();
    }
}

pub(crate) struct SyncBorrow(Rc<CallFrame>);
impl Drop for SyncBorrow {
    fn drop(&mut self) {
        self.0.sync_borrows.set(self.0.sync_borrows.get() - 1);
    }
}

/// Actual callback allocations, not registrations. V8 borrows their stable Box
/// addresses until the owning Store is destroyed, just as JS owns its Closures.
#[derive(Default)]
pub(crate) struct HostCallbacks(Vec<CallbackAllocation>);

struct CallbackAllocation {
    value: Option<Box<dyn Any>>,
    binding: Rc<AsyncBinding>,
}

impl std::fmt::Debug for HostCallbacks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("HostCallbacks").field(&self.0.len()).finish()
    }
}

impl HostCallbacks {
    pub(crate) fn insert<T: 'static>(
        &mut self,
        value: Box<T>,
        binding: Rc<AsyncBinding>,
    ) -> *mut T {
        let pointer = &*value as *const T as *mut T;
        self.0.push(CallbackAllocation {
            value: Some(value),
            binding,
        });
        pointer
    }
}

impl Drop for CallbackAllocation {
    fn drop(&mut self) {
        let unwinding = std::thread::panicking();
        let value = self.value.take();
        if let Err(panic) = catch_unwind(AssertUnwindSafe(|| drop(value))) {
            self.binding.record(panic, unwinding);
        }
    }
}

impl Drop for HostCallbacks {
    fn drop(&mut self) {
        let unwinding = std::thread::panicking();
        let mut first = None;
        // Drain every actual allocation before transporting the exact first
        // value at THIS Rust arena boundary; no trailing StoreInner receiver.
        for allocation in std::mem::take(&mut self.0) {
            let binding = allocation.binding.clone();
            drop(allocation);
            if let Some(panic) = binding.take_panic() {
                if first.is_none() && !unwinding {
                    first = Some(panic);
                } else {
                    discard_panic(panic);
                }
            }
        }
        if let Some(first) = first {
            resume_unwind(first);
        }
    }
}

pub(crate) fn discard_panic(mut panic: Panic) {
    loop {
        match catch_unwind(AssertUnwindSafe(|| drop(panic))) {
            Ok(()) => return,
            Err(next) => panic = next,
        }
    }
}

#[derive(Default)]
struct Parker {
    notified: Mutex<bool>,
    ready: Condvar,
}
impl Wake for Parker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    fn wake_by_ref(self: &Rc<Self>) {
        *self.notified.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.ready.notify_one();
    }
}
fn park_on_thread(mut future: Pin<&mut dyn Future<Output = ()>>) {
    let parker = Arc::new(Parker::default());
    let waker = Waker::from(parker.clone());
    let mut context = Context::from_waker(&waker);
    loop {
        if let Poll::Ready(()) = future.as_mut().poll(&mut context) {
            return;
        }
        let mut notified = parker.notified.lock().unwrap_or_else(|e| e.into_inner());
        while !*notified {
            notified = parker
                .ready
                .wait(notified)
                .unwrap_or_else(|e| e.into_inner());
        }
        *notified = false;
    }
}

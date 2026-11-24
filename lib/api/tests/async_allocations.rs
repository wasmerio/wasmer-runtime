#![cfg(all(
    feature = "experimental-async",
    feature = "sys",
    not(feature = "v8-default"),
    not(target_arch = "wasm32")
))]
//! What an async call keeps on the heap.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
    pin::pin,
    task::{Context, Poll, Waker},
};

use wasmer::{
    AsStoreMut, AsyncEnv, Function, FunctionEnv, FunctionEnvMut, Instance, Module, Store,
    TypedFunction, imports,
};

/// Counts the allocations of each thread.
struct Counting;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // A thread still allocates while its locals are destroyed.
        let _ = ALLOCATIONS.try_with(|count| count.set(count.get() + 1));
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// How many times `f` allocates.
fn allocations<R>(f: impl FnOnce() -> R) -> (usize, R) {
    let before = ALLOCATIONS.get();
    let result = f();
    (ALLOCATIONS.get() - before, result)
}

const GUEST: &str = r#"(module
  (import "h" "double" (func $double (param i32) (result i32)))
  (func (export "run") (param i32 i32) (result i32)
    (i32.add (call $double (local.get 0)) (local.get 1)))
  (func (export "add") (param i32 i32) (result i32)
    (i32.add (local.get 0) (local.get 1))))"#;

/// Runs a call that does not wait.
fn ready<R>(call: impl Future<Output = R>) -> R {
    match pin!(call).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(result) => result,
        Poll::Pending => panic!("the call waits"),
    }
}

/// A typed call that does not wait keeps nothing on the heap: not its
/// future, its arguments or its results, and the stack of its guest is
/// reused. With host interrupts, entering a store that is not running
/// allocates the flag an interrupt of it sets, as a synchronous call does.
#[test]
fn a_typed_call_that_does_not_wait_allocates_as_a_synchronous_one() {
    let mut store = Store::default();
    let module = Module::new(&store, GUEST).unwrap();
    let double = Function::new_typed(&mut store, |x: i32| x * 2);
    let imports = imports! { "h" => { "double" => double } };
    let instance = Instance::new(&mut store, &module, &imports).unwrap();
    let run: TypedFunction<(i32, i32), i32> =
        instance.exports.get_typed_function(&store, "run").unwrap();

    // The first calls take a stack for their guest, which later ones reuse.
    assert_eq!(run.call(&mut store, 20, 2).unwrap(), 42);
    assert_eq!(ready(run.call_async(&mut store, 20, 2)).unwrap(), 42);

    let (sync, result) = allocations(|| run.call(&mut store, 20, 2));
    assert_eq!(result.unwrap(), 42);
    let (asynchronous, result) = allocations(|| ready(run.call_async(&mut store, 20, 2)));
    assert_eq!(result.unwrap(), 42);
    assert_eq!(asynchronous, sync);
    #[cfg(not(feature = "experimental-host-interrupt"))]
    assert_eq!(asynchronous, 0);
}

/// A typed async import whose future is ready at once allocates as a sync one
/// does, from its second call on: its future is made in the block the last
/// one was dropped from.
#[test]
fn a_typed_async_import_that_does_not_wait_allocates_as_a_sync_one() {
    let mut store = Store::default();
    let module = Module::new(&store, GUEST).unwrap();
    let env = FunctionEnv::new(&mut store, ());
    let sync = Function::new_typed(&mut store, |x: i32| x * 2);
    let asynchronous = Function::new_typed_async(
        &mut store,
        &env,
        async |_: &mut AsyncEnv<()>, x: i32| -> i32 { x * 2 },
    );
    let mut run = |import: Function| -> TypedFunction<(i32, i32), i32> {
        let imports = imports! { "h" => { "double" => import } };
        let instance = Instance::new(&mut store, &module, &imports).unwrap();
        instance.exports.get_typed_function(&store, "run").unwrap()
    };
    let (run_sync, run_async) = (run(sync), run(asynchronous));

    let mut count = |run: &TypedFunction<(i32, i32), i32>| {
        // The first calls take a stack for their guest, and the import's
        // block, which later ones reuse.
        assert_eq!(run.call(&mut store, 20, 2).unwrap(), 42);
        assert_eq!(ready(run.call_async(&mut store, 20, 2)).unwrap(), 42);

        let (call, result) = allocations(|| run.call(&mut store, 20, 2));
        assert_eq!(result.unwrap(), 42);
        let (call_async, result) = allocations(|| ready(run.call_async(&mut store, 20, 2)));
        assert_eq!(result.unwrap(), 42);
        (call, call_async)
    };
    assert_eq!(count(&run_async), count(&run_sync));
    #[cfg(not(feature = "experimental-host-interrupt"))]
    assert_eq!(count(&run_async), (0, 0));
}

/// Nor does a typed call that a host function makes of a guest of its own
/// store, as a callback is called: with or without host interrupts, the store
/// is running already.
#[test]
fn a_typed_call_made_by_a_host_function_allocates_nothing() {
    struct Env {
        add: Option<TypedFunction<(i32, i32), i32>>,
        allocations: usize,
    }
    let mut store = Store::default();
    let module = Module::new(&store, GUEST).unwrap();
    let env = FunctionEnv::new(
        &mut store,
        Env {
            add: None,
            allocations: usize::MAX,
        },
    );
    let double = Function::new_typed_with_env(
        &mut store,
        &env,
        |mut env: FunctionEnvMut<Env>, x: i32| -> i32 {
            let add = env.data().add.clone().unwrap();
            let (count, result) =
                allocations(|| ready(add.call_async(&mut env.as_store_mut(), x, x)));
            env.data_mut().allocations = count;
            result.unwrap()
        },
    );
    let imports = imports! { "h" => { "double" => double } };
    let instance = Instance::new(&mut store, &module, &imports).unwrap();
    let exports = &instance.exports;
    env.as_mut(&mut store).add = Some(exports.get_typed_function(&store, "add").unwrap());
    let run: TypedFunction<(i32, i32), i32> = exports.get_typed_function(&store, "run").unwrap();

    // The first call takes a stack for each guest, which later ones reuse.
    assert_eq!(ready(run.call_async(&mut store, 20, 2)).unwrap(), 42);
    assert_eq!(ready(run.call_async(&mut store, 20, 2)).unwrap(), 42);
    assert_eq!(env.as_ref(&store).allocations, 0);
}

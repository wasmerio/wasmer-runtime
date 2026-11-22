//! Compares a guest's calls of a sync typed import with its calls of an async
//! typed import that is ready at once.
//!
//! The guest calls the import `IMPORT_CALLS` times in a loop and drops each
//! result, so an iteration costs little more than the import calls. How an
//! async import is driven depends on how the guest was entered, so each import
//! is measured under both entries:
//!
//! * `call`: the import's future is polled in place.
//! * `call_async`: the guest runs on a coroutine, which suspends for the
//!   caller to poll the import's future, and is then resumed.
//!
//! Run it with at least one compiler:
//!
//! ```sh
//! cargo bench --locked -p wasmer-workspace --features cranelift,experimental-async --bench async_import
//! ```

use std::{
    future::Future,
    pin::pin,
    task::{Context, Poll, Waker},
};

use criterion::{
    BenchmarkGroup, Criterion, Throughput, criterion_group, criterion_main, measurement::WallTime,
};
use wasmer::{
    AsyncEnv, Function, FunctionEnv, FunctionEnvMut, Instance, Module, Store, TypedFunction,
    imports,
};

const IMPORT_CALLS: u32 = 10_000;

const WAT: &str = r#"(module
    (import "host" "identity" (func $identity (param i32) (result i32)))
    (func (export "run") (param $remaining i32)
        (loop $repeat
            (drop (call $identity (local.get $remaining)))
            (local.set $remaining (i32.sub (local.get $remaining) (i32.const 1)))
            (br_if $repeat (i32.gt_s (local.get $remaining) (i32.const 0))))))"#;

type NewStore = fn() -> Store;

/// The compilers enabled with Cargo features.
const COMPILERS: &[(&str, NewStore)] = &[
    #[cfg(feature = "cranelift")]
    ("cranelift", || {
        Store::new(wasmer_compiler_cranelift::Cranelift::new())
    }),
    #[cfg(feature = "llvm")]
    ("llvm", || Store::new(wasmer_compiler_llvm::LLVM::new())),
    #[cfg(feature = "singlepass")]
    ("singlepass", || {
        Store::new(wasmer_compiler_singlepass::Singlepass::new())
    }),
];

#[derive(Clone, Copy)]
enum Import {
    Sync,
    Async,
}

/// Instantiates the guest with its import, and returns its `run` export.
fn guest(store: &mut Store, import: Import) -> TypedFunction<u32, ()> {
    let module = Module::new(store, WAT).unwrap();
    let env = FunctionEnv::new(store, ());
    let identity = match import {
        Import::Sync => Function::new_typed_with_env(
            store,
            &env,
            |_env: FunctionEnvMut<'_, ()>, value: i32| -> i32 { value },
        ),
        Import::Async => Function::new_typed_async(
            store,
            &env,
            async |_cx: &mut AsyncEnv<()>, value: i32| -> i32 { value },
        ),
    };
    let imports = imports! { "host" => { "identity" => identity } };
    let instance = Instance::new(store, &module, &imports).unwrap();
    instance.exports.get_typed_function(store, "run").unwrap()
}

/// Drives a call that never waits: its imports are ready at once, so it
/// finishes in its first poll.
fn poll_once<F: Future>(call: F) -> F::Output {
    match pin!(call).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("a call whose imports are ready waited"),
    }
}

fn bench_import(
    mut store: Store,
    import: Import,
    name: &str,
    group: &mut BenchmarkGroup<'_, WallTime>,
) {
    let run = guest(&mut store, import);

    group.bench_function(format!("{name}/call"), |b| {
        b.iter(|| run.call(&mut store, IMPORT_CALLS).unwrap());
    });
    group.bench_function(format!("{name}/call_async"), |b| {
        b.iter(|| poll_once(run.call_async(&mut store, IMPORT_CALLS)).unwrap());
    });
}

fn run_benchmarks(c: &mut Criterion) {
    for (compiler, new_store) in COMPILERS {
        let mut group = c.benchmark_group(format!("async_import/{compiler}"));
        group.throughput(Throughput::Elements(u64::from(IMPORT_CALLS)));
        bench_import(new_store(), Import::Sync, "sync_import", &mut group);
        bench_import(new_store(), Import::Async, "async_import", &mut group);
        group.finish();
    }
}

criterion_group!(benches, run_benchmarks);
criterion_main!(benches);

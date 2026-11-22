#![cfg(all(feature = "experimental-async", not(target_arch = "wasm32")))]

use std::{cell::RefCell, sync::OnceLock};

use anyhow::Result;
use futures::future;
use wasmer::{
    AsyncEnv, Function, FunctionEnv, FunctionEnvMut, FunctionType, Instance, Module, Store, Type,
    TypedFunction, Value, imports,
};
use wasmer_types::TrapCode;

#[derive(Default)]
struct DeltaState {
    deltas: Vec<f64>,
    index: usize,
}

impl DeltaState {
    fn next(&mut self) -> f64 {
        let value = self.deltas.get(self.index).copied().unwrap_or(0.0);
        self.index += 1;
        value
    }
}

fn jspi_module() -> &'static [u8] {
    static BYTES: OnceLock<Vec<u8>> = OnceLock::new();
    const JSPI_WAT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/examples/jspi.wat");
    BYTES.get_or_init(|| wat::parse_file(JSPI_WAT).expect("valid example module"))
}

#[test]
#[cfg_attr(
    feature = "v8-default",
    ignore = "these guests suspend, which the default v8 backend cannot"
)]
fn async_state_updates_follow_jspi_example() -> Result<()> {
    let wasm = jspi_module();
    let mut store = Store::default();
    let module = Module::new(&store, wasm)?;

    let unit_env = FunctionEnv::new(&mut store, ());
    let init_state = Function::new_async(
        &mut store,
        &unit_env,
        FunctionType::new(vec![], vec![Type::F64]),
        async |_: &mut AsyncEnv<()>, _: &[Value]| {
            // Note: future::ready doesn't actually suspend. It's important
            // to note that, while we're in an async import here, it's
            // impossible to suspend during module instantiation, which is
            // where this import is called.
            // To see this in action, uncomment the following line:
            // tokio::task::yield_now().await;
            future::ready(()).await;
            Ok(vec![Value::F64(1.0)])
        },
    );

    let delta_env = FunctionEnv::new(
        &mut store,
        DeltaState {
            deltas: vec![0.5, -1.0, 2.5],
            index: 0,
        },
    );
    let compute_delta = Function::new_async(
        &mut store,
        &delta_env,
        FunctionType::new(vec![], vec![Type::F64]),
        async |cx: &mut AsyncEnv<DeltaState>, _: &[Value]| {
            // The store is reachable only in this synchronous slice, so no
            // borrow of it can be held across the await below.
            let delta = cx.with(|mut env| env.data_mut().next());
            // We can, however, actually suspend whenever
            // `Function::call_async` is used to call WASM functions.
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            Ok(vec![Value::F64(delta)])
        },
    );

    let import_object = imports! {
        "js" => {
            "init_state" => init_state,
            "compute_delta" => compute_delta,
        }
    };

    let instance = Instance::new(&mut store, &module, &import_object)?;
    let get_state = instance.exports.get_function("get_state")?;
    let update_state = instance.exports.get_function("update_state")?;

    fn as_f64(values: &[Value]) -> f64 {
        match &values[0] {
            Value::F64(v) => *v,
            other => panic!("expected f64 value, got {other:?}"),
        }
    }

    assert_eq!(as_f64(&get_state.call(&mut store, &[])?), 1.0);

    let step = |store: &mut Store, func: &wasmer::Function| -> Result<f64> {
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(func.call_async(store, &[]))?;
        Ok(as_f64(&result))
    };

    assert_eq!(step(&mut store, update_state)?, 1.5);
    assert_eq!(step(&mut store, update_state)?, 0.5);
    assert_eq!(step(&mut store, update_state)?, 3.0);

    Ok(())
}

#[test]
#[cfg_attr(
    feature = "v8-default",
    ignore = "these guests suspend, which the default v8 backend cannot"
)]
fn typed_async_host_and_calls_work() -> Result<()> {
    let wasm = wat::parse_str(
        r#"
        (module
          (import "host" "async_add" (func $async_add (param i32 i32) (result i32)))
          (import "host" "async_double" (func $async_double (param i32) (result i32)))
          (func (export "compute") (param i32) (result i32)
            local.get 0
            i32.const 10
            call $async_add
            local.get 0
            call $async_double
            i32.add))
        "#,
    )?;

    #[derive(Clone, Copy)]
    struct AddBias {
        bias: i32,
    }

    let mut store = Store::default();
    let module = Module::new(&store, wasm)?;

    let add_env = FunctionEnv::new(&mut store, AddBias { bias: 5 });
    let async_add = Function::new_typed_async(
        &mut store,
        &add_env,
        async move |cx: &mut AsyncEnv<AddBias>, a: i32, b: i32| {
            let bias = cx.with(|env| env.data().bias);
            tokio::task::yield_now().await;
            a + b + bias
        },
    );
    let unit_env = FunctionEnv::new(&mut store, ());
    let async_double = Function::new_typed_async(
        &mut store,
        &unit_env,
        async move |_: &mut AsyncEnv<()>, value: i32| {
            tokio::task::yield_now().await;
            value * 2
        },
    );

    let import_object = imports! {
        "host" => {
            "async_add" => async_add,
            "async_double" => async_double,
        }
    };

    let instance = Instance::new(&mut store, &module, &import_object)?;
    let compute: TypedFunction<i32, i32> =
        instance.exports.get_typed_function(&store, "compute")?;

    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(compute.call_async(&mut store, 4))?;
    assert_eq!(result, 27);

    Ok(())
}

#[test]
#[cfg_attr(
    feature = "v8-default",
    ignore = "these guests suspend, which the default v8 backend cannot"
)]
fn cannot_yield_when_not_in_async_context() -> Result<()> {
    const WAT: &str = r#"
    (module
        (import "env" "yield_now" (func $yield_now))
        (func (export "yield_outside")
            call $yield_now
        )
    )
    "#;
    let wasm = wat::parse_str(WAT).expect("valid WAT module");

    let mut store = Store::default();
    let module = Module::new(&store, wasm)?;

    let env = FunctionEnv::new(&mut store, ());
    let yield_now = Function::new_async(
        &mut store,
        &env,
        FunctionType::new(vec![], vec![]),
        async |_: &mut AsyncEnv<()>, _: &[Value]| {
            // Attempting to yield when not in an async context should trap.
            tokio::task::yield_now().await;
            Ok(vec![])
        },
    );

    let import_object = imports! {
        "env" => {
            "yield_now" => yield_now,
        }
    };
    let instance = Instance::new(&mut store, &module, &import_object)?;
    let yield_outside = instance.exports.get_function("yield_outside")?;

    let trap = yield_outside
        .call(&mut store, &[])
        .expect_err("expected trap calling yield outside async context");

    // TODO: wasm trace generation appears to be broken?
    // assert!(!trap.trace().is_empty(), "should have a stack trace");
    let trap_code = trap.to_trap().expect("expected trap code");
    assert_eq!(
        trap_code,
        TrapCode::YieldOutsideAsyncContext,
        "expected YieldOutsideAsyncContext trap code"
    );

    Ok(())
}

/// A synchronous host function of a `call_async` calls back into its guest,
/// holding its borrow of the store across that call, so the guest it calls
/// cannot be suspended: whoever used the store while it waited would alias
/// that borrow. An async import that is ready at once goes through; one that
/// has to wait fails with `YieldOutsideAsyncContext`, as on the JavaScript
/// backend, and the host function goes on with its store.
#[test]
#[cfg_attr(
    feature = "v8-default",
    ignore = "these guests suspend, which the default v8 backend cannot"
)]
fn nested_async_in_sync() -> Result<()> {
    const WAT: &str = r#"
    (module
        (import "env" "sync" (func $sync (param i32) (result i32)))
        (import "env" "ready" (func $ready (result i32)))
        (import "env" "async" (func $async (result i32)))
        (func (export "entry") (param i32) (result i32)
            (call $sync (local.get 0))
        )
        (func (export "inner_ready") (result i32)
            call $ready
        )
        (func (export "inner_async") (result i32)
            call $async
        )
    )
    "#;
    let wasm = wat::parse_str(WAT).expect("valid WAT module");

    let mut store = Store::default();
    let module = Module::new(&store, wasm)?;

    struct Env {
        inner: RefCell<Vec<wasmer::TypedFunction<(), i32>>>,
    }
    let env = FunctionEnv::new(
        &mut store,
        Env {
            inner: RefCell::new(Vec::new()),
        },
    );

    let sync = Function::new_typed_with_env(
        &mut store,
        &env,
        |mut env: FunctionEnvMut<Env>, which: i32| -> i32 {
            let inner = env.data().inner.borrow()[which as usize].clone();
            match inner.call(&mut env) {
                Ok(value) => value,
                Err(error) => {
                    assert_eq!(error.to_trap(), Some(TrapCode::YieldOutsideAsyncContext));
                    // The host function goes on with its store.
                    -(env.data().inner.borrow().len() as i32)
                }
            }
        },
    );

    let unit_env = FunctionEnv::new(&mut store, ());
    let ready = Function::new_typed_async(&mut store, &unit_env, async |_: &mut AsyncEnv<()>| 7);
    let async_ = Function::new_typed_async(&mut store, &unit_env, async |_: &mut AsyncEnv<()>| {
        tokio::task::yield_now().await;
        42
    });

    let imports = imports! {
        "env" => {
            "sync" => sync,
            "ready" => ready,
            "async" => async_,
        }
    };

    let instance = Instance::new(&mut store, &module, &imports)?;

    for name in ["inner_ready", "inner_async"] {
        let inner = instance
            .exports
            .get_typed_function::<(), i32>(&store, name)
            .unwrap();
        env.as_mut(&mut store).inner.borrow_mut().push(inner);
    }

    let entry = instance
        .exports
        .get_typed_function::<i32, i32>(&store, "entry")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    assert_eq!(runtime.block_on(entry.call_async(&mut store, 0))?, 7);
    assert_eq!(runtime.block_on(entry.call_async(&mut store, 1))?, -2);

    Ok(())
}

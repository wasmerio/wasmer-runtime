#![cfg(all(
    feature = "v8",
    any(feature = "v8-default", not(feature = "sys")),
    not(target_arch = "wasm32")
))]
//! V8 knows nothing of stores: an object of another store is one of another
//! isolate, which V8 would take for one of its own, and it does not check
//! which thread it is used on. Every handle records its store, and every
//! entry checks it.

use std::panic::{AssertUnwindSafe, catch_unwind};

use wasmer::{
    Function, FunctionType, Global, Instance, InstantiationError, Memory, MemoryType, Module,
    RuntimeError, Store, Table, TableType, Type, Value, imports,
};

const GUEST: &str = r#"(module
  (func $one (result i32) (i32.const 1))
  (table (export "table") 1 funcref)
  (elem (i32.const 0) $one)
  (memory (export "memory") 1)
  (global (export "global") (mut i32) (i32.const 7))
  (func (export "one") (result i32) (call $one))
  (func (export "pass") (param funcref) (result funcref) (local.get 0)))"#;

fn guest(store: &mut Store) -> Instance {
    let module = Module::new(store, GUEST).unwrap();
    Instance::new(store, &module, &imports! {}).unwrap()
}

/// The message of the panic of `f`.
fn panic_of<R>(f: impl FnOnce() -> R) -> String {
    let Err(panic) = catch_unwind(AssertUnwindSafe(f)) else {
        panic!("it did not panic");
    };
    match panic.downcast::<String>() {
        Ok(message) => *message,
        Err(panic) => panic.downcast_ref::<&str>().expect("a message").to_string(),
    }
}

fn assert_cross_store<T: std::fmt::Debug>(what: &str, result: Result<T, RuntimeError>) {
    let message = result.unwrap_err().message();
    assert!(message.contains("cross-`Store`"), "{what}: {message}");
}

/// A handle belongs to the store it was made with, or exported from.
#[test]
fn a_handle_belongs_to_its_store() {
    let mut first = Store::default();
    let second = Store::default();
    let instance = guest(&mut first);
    let exports = &instance.exports;

    let host = Function::new_typed(&mut first, || 1i32);
    let export = exports.get_function("one").unwrap();
    for function in [&host, export] {
        assert!(function.is_from_store(&first));
        assert!(!function.is_from_store(&second));
    }
    for global in [
        &Global::new(&mut first, Value::I32(1)).unwrap(),
        exports.get_global("global").unwrap(),
    ] {
        assert!(global.is_from_store(&first));
        assert!(!global.is_from_store(&second));
    }
    let table_type = TableType::new(Type::FuncRef, 1, None);
    for table in [
        &Table::new(&mut first, table_type, Value::FuncRef(None)).unwrap(),
        exports.get_table("table").unwrap(),
    ] {
        assert!(table.is_from_store(&first));
        assert!(!table.is_from_store(&second));
    }
    for memory in [
        &Memory::new(&mut first, MemoryType::new(1, None, false)).unwrap(),
        exports.get_memory("memory").unwrap(),
    ] {
        assert!(memory.is_from_store(&first));
        assert!(!memory.is_from_store(&second));
    }
}

/// A function that comes out of a guest's table belongs to the guest's
/// store, and can be called with it.
#[test]
fn a_function_out_of_a_guest_belongs_to_its_store() {
    let mut store = Store::default();
    let other = Store::default();
    let instance = guest(&mut store);
    let table = instance.exports.get_table("table").unwrap();

    let Some(Value::FuncRef(Some(function))) = table.get(&mut store, 0) else {
        panic!("the table has no function");
    };
    assert!(function.is_from_store(&store));
    assert!(!function.is_from_store(&other));
    assert_eq!(function.call(&mut store, &[]).unwrap()[0], Value::I32(1));
}

/// An instance takes no import of another store.
#[test]
fn an_import_of_another_store_is_refused() {
    let mut first = Store::default();
    let mut second = Store::default();
    let function = Function::new_typed(&mut first, || {});
    let global = Global::new(&mut first, Value::I32(1)).unwrap();
    let memory = Memory::new(&mut first, MemoryType::new(1, None, false)).unwrap();
    let cases = [
        (
            r#"(import "h" "x" (func))"#,
            imports! { "h" => { "x" => function } },
        ),
        (
            r#"(import "h" "x" (global i32))"#,
            imports! { "h" => { "x" => global } },
        ),
        (
            r#"(import "h" "x" (memory 1))"#,
            imports! { "h" => { "x" => memory } },
        ),
    ];
    for (import, imports) in cases {
        let module = Module::new(&second, format!("(module {import})")).unwrap();
        let error = Instance::new(&mut second, &module, &imports).unwrap_err();
        assert!(
            matches!(error, InstantiationError::DifferentStores),
            "{import}: {error}"
        );
        // With its own store, it is taken.
        Instance::new(&mut first, &module, &imports).unwrap();
    }
}

/// A handle used with another store than its own panics, before V8 is given
/// it.
#[test]
fn a_handle_used_with_another_store_panics() {
    let mut first = Store::default();
    let mut second = Store::default();
    let instance = guest(&mut first);
    let one = instance.exports.get_function("one").unwrap().clone();
    let typed = one.typed::<(), i32>(&first).unwrap();
    let global = instance.exports.get_global("global").unwrap().clone();
    let table = instance.exports.get_table("table").unwrap().clone();
    let memory = instance.exports.get_memory("memory").unwrap().clone();

    let panics = [
        panic_of(|| one.call(&mut second, &[])),
        panic_of(|| typed.call(&mut second)),
        panic_of(|| one.ty(&second)),
        panic_of(|| global.get(&mut second)),
        panic_of(|| global.set(&mut second, Value::I32(1))),
        panic_of(|| table.size(&second)),
        panic_of(|| table.get(&mut second, 0)),
        panic_of(|| memory.view(&second).data_size()),
        panic_of(|| memory.grow(&mut second, 1)),
    ];
    for (index, message) in panics.iter().enumerate() {
        assert!(message.contains("wrong context"), "{index}: {message}");
    }
    // With their own store, they work.
    assert_eq!(typed.call(&mut first).unwrap(), 1);
    assert_eq!(global.get(&mut first), Value::I32(7));
}

/// A function of another store is not given to a guest: not as an argument,
/// not through a global or a table, and not as the result of a host function.
#[test]
fn a_function_of_another_store_is_not_given_to_a_guest() {
    let mut first = Store::default();
    let mut second = Store::default();
    let foreign = Function::new_typed(&mut first, || {});
    let foreign = || Value::FuncRef(Some(foreign.clone()));
    let instance = guest(&mut second);
    let pass = instance.exports.get_function("pass").unwrap();
    let table = instance.exports.get_table("table").unwrap();

    assert_cross_store("an argument", pass.call(&mut second, &[foreign()]));
    assert_cross_store("a new global", Global::new(&mut second, foreign()));
    assert_cross_store(
        "a new mutable global",
        Global::new_mut(&mut second, foreign()),
    );
    let table_type = TableType::new(Type::FuncRef, 1, None);
    assert_cross_store(
        "a new table",
        Table::new(&mut second, table_type, foreign()),
    );
    assert_cross_store("a table entry", table.set(&mut second, 0, foreign()));
    assert_cross_store("a grown table", table.grow(&mut second, 1, foreign()));

    let returned = foreign();
    let returns = Function::new(
        &mut second,
        FunctionType::new([], [Type::FuncRef]),
        move |_| Ok(vec![returned.clone()]),
    );
    assert_cross_store("a host result", returns.call(&mut second, &[]));
}

/// V8 has no global of an `externref` or of an exception reference: making
/// one is an error.
#[test]
fn a_global_v8_cannot_hold_is_refused() {
    let mut store = Store::default();
    for value in [Value::ExternRef(None), Value::ExceptionRef(None)] {
        let results = [
            Global::new(&mut store, value.clone()),
            Global::new_mut(&mut store, value.clone()),
        ];
        for result in results {
            let message = result.unwrap_err().message();
            assert!(message.contains("not supported in V8"), "{message}");
        }
    }
}

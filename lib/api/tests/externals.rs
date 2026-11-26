use macro_wasmer_engine_test::engine_test;
#[cfg(feature = "js")]
use wasm_bindgen_test::*;

use wasmer::*;

#[engine_test]
fn global_new() -> Result<(), String> {
    let mut store = Store::default();
    let global = Global::new(&mut store, Value::I32(10)).map_err(|e| format!("{e:?}"))?;
    assert_eq!(
        global.ty(&store),
        GlobalType {
            ty: Type::I32,
            mutability: Mutability::Const
        }
    );
    assert_eq!(global.get(&mut store), Value::I32(10));

    let global_mut = Global::new_mut(&mut store, Value::F32(10.0)).map_err(|e| format!("{e:?}"))?;
    assert_eq!(
        global_mut.ty(&store),
        GlobalType {
            ty: Type::F32,
            mutability: Mutability::Var
        }
    );
    assert_eq!(global_mut.get(&mut store), Value::F32(10.0));

    Ok(())
}

#[engine_test]
fn global_get() -> Result<(), String> {
    let mut store = Store::default();

    let global_i32 = Global::new(&mut store, Value::I32(10)).map_err(|e| format!("{e:?}"))?;
    assert_eq!(global_i32.get(&mut store), Value::I32(10));

    // 64-bit values are not yet fully supported in some versions of Node
    #[cfg(feature = "sys")]
    {
        let global_i64 = Global::new(&mut store, Value::I64(20)).map_err(|e| format!("{e:?}"))?;
        assert_eq!(global_i64.get(&mut store), Value::I64(20));
    }

    let global_f32 = Global::new(&mut store, Value::F32(10.0)).map_err(|e| format!("{e:?}"))?;
    assert_eq!(global_f32.get(&mut store), Value::F32(10.0));

    // 64-bit values are not yet fully supported in some versions of Node
    #[cfg(feature = "sys")]
    {
        let global_f64 = Global::new(&mut store, Value::F64(20.0)).map_err(|e| format!("{e:?}"))?;
        assert_eq!(global_f64.get(&mut store), Value::F64(20.0));
    }

    Ok(())
}

#[engine_test]
fn global_set() -> Result<(), String> {
    let mut store = Store::default();
    let global_i32 = Global::new(&mut store, Value::I32(10)).map_err(|e| format!("{e:?}"))?;
    // Set on a constant should error
    assert!(global_i32.set(&mut store, Value::I32(20)).is_err());

    let global_i32_mut =
        Global::new_mut(&mut store, Value::I32(10)).map_err(|e| format!("{e:?}"))?;
    // Set on different type should error
    assert!(global_i32_mut.set(&mut store, Value::I64(20)).is_err());

    // Set on same type should succeed
    global_i32_mut
        .set(&mut store, Value::I32(20))
        .map_err(|e| format!("{e:?}"))?;
    assert_eq!(global_i32_mut.get(&mut store), Value::I32(20));

    Ok(())
}

/// Asserts that `result` is the error a value of another store is refused
/// with.
fn assert_cross_store(result: Result<Global, RuntimeError>) {
    let message = result.unwrap_err().message();
    // `js` prefixes the message of an error with its kind.
    let expected = "cross-`Store` values are not supported";
    assert!(message.ends_with(expected), "{message}");
}

#[engine_test]
fn global_new_funcref() -> Result<(), String> {
    let mut store = Store::default();

    // Without `async`, `js` cannot tell the store of a function.
    #[cfg(any(not(feature = "js"), feature = "async"))]
    {
        let mut other_store = Store::default();
        let foreign = Function::new_typed(&mut other_store, || {});
        let foreign = || Value::FuncRef(Some(foreign.clone()));
        assert_cross_store(Global::new(&mut store, foreign()));
        assert_cross_store(Global::new_mut(&mut store, foreign()));
    }

    let f = Function::new_typed(&mut store, || 7i32);
    let global = Global::new(&mut store, Value::FuncRef(Some(f.clone())));
    let global_mut = Global::new_mut(&mut store, Value::FuncRef(Some(f)));

    // `js` has no global of a reference yet: that is an error too.
    #[cfg(feature = "js")]
    assert!(global.is_err() && global_mut.is_err());

    #[cfg(not(feature = "js"))]
    for (global, mutability) in [(global, Mutability::Const), (global_mut, Mutability::Var)] {
        let global = global.map_err(|e| format!("{e:?}"))?;
        let ty = Type::FuncRef;
        assert_eq!(global.ty(&store), GlobalType { ty, mutability });

        // The default v8 backend cannot read a function back from a global.
        #[cfg(not(feature = "v8-default"))]
        {
            let Value::FuncRef(Some(f)) = global.get(&mut store) else {
                return Err("global.get does not match `FuncRef(Some(..))`!".into());
            };
            let results = f.call(&mut store, &[]).map_err(|e| format!("{e:?}"))?;
            assert_eq!(*results, [Value::I32(7)]);
        }
    }

    Ok(())
}

#[engine_test]
#[cfg_attr(
    feature = "v8-default",
    ignore = "extern refs are not supported by the default v8 backend"
)]
fn global_new_externref() -> Result<(), String> {
    let mut store = Store::default();
    let mut other_store = Store::default();

    let foreign = ExternRef::new(&mut other_store, 0u32);
    let foreign = || Value::ExternRef(Some(foreign.clone()));
    assert_cross_store(Global::new(&mut store, foreign()));
    assert_cross_store(Global::new_mut(&mut store, foreign()));

    // Only `sys` can create an exception.
    #[cfg(feature = "sys")]
    {
        let tag = Tag::new(&mut other_store, []).unwrap();
        let foreign = Exception::new(&mut other_store, &tag, &[]).unwrap();
        let foreign = Value::ExceptionRef(Some(foreign));
        assert_cross_store(Global::new(&mut store, foreign));
    }

    let extern_ref = ExternRef::new(&mut store, 1u32);
    let own = || Value::ExternRef(Some(extern_ref.clone()));
    let global = Global::new(&mut store, own());
    let global_mut = Global::new_mut(&mut store, own());

    // `js` has no global of a reference yet: that is an error too.
    #[cfg(feature = "js")]
    assert!(global.is_err() && global_mut.is_err());

    #[cfg(not(feature = "js"))]
    for (global, mutability) in [(global, Mutability::Const), (global_mut, Mutability::Var)] {
        let global = global.map_err(|e| format!("{e:?}"))?;
        let ty = Type::ExternRef;
        assert_eq!(global.ty(&store), GlobalType { ty, mutability });
        let Value::ExternRef(Some(value)) = global.get(&mut store) else {
            return Err("global.get does not match `ExternRef(Some(..))`!".into());
        };
        assert_eq!(value.downcast::<u32>(&store), Some(&1u32));
    }

    Ok(())
}

/// `js` has no global of a reference or of a `v128` yet, and the engine
/// refuses the number it passes for an `i64`: each is an error, not a panic.
#[cfg(feature = "js")]
#[wasm_bindgen_test]
fn global_new_refuses_what_js_cannot_create() {
    let mut store = Store::default();
    for value in [Value::FuncRef(None), Value::V128(7), Value::I64(20)] {
        assert!(Global::new(&mut store, value.clone()).is_err());
        assert!(Global::new_mut(&mut store, value).is_err());
    }
}

#[engine_test]
fn table_new() -> Result<(), String> {
    let mut store = Store::default();
    let table_type = TableType {
        ty: Type::FuncRef,
        minimum: 0,
        maximum: None,
        readonly: false,
    };
    let f = Function::new_typed(&mut store, || {});
    let table = Table::new(&mut store, table_type, Value::FuncRef(Some(f)))
        .map_err(|e| format!("{e:?}"))?;
    assert_eq!(table.ty(&store), table_type);

    // Anyrefs not yet supported
    // let table_type = TableType {
    //     ty: Type::ExternRef,
    //     minimum: 0,
    //     maximum: None,
    // };
    // let table = Table::new(&store, table_type, Value::ExternRef(ExternRef::Null)).map_err(|e| format!("{e:?}"))?;
    // assert_eq!(*table.ty(), table_type);

    Ok(())
}

#[engine_test]
fn table_get() -> Result<(), String> {
    // Tables are not yet fully supported in Wasm
    // This test was marked as #[ignore] on -sys, which is why it is commented out.

    //    let mut store = Store::default();
    //    let table_type = TableType {
    //        ty: Type::FuncRef,
    //        minimum: 0,
    //        maximum: Some(1),
    //    };
    //    let f = Function::new_typed(&mut store, |num: i32| num + 1);
    //    let table = Table::new(&mut store, table_type, Value::FuncRef(Some(f)))
    //        .map_err(|e| format!("{e:?}"))?;
    //    assert_eq!(table.ty(&mut store), table_type);
    //    let _elem = table.get(&mut store, 0).unwrap();
    //    assert_eq!(elem.funcref().unwrap(), f);

    Ok(())
}

/// The message of the error `result` is.
fn error_message<T>(result: Result<T, RuntimeError>) -> String {
    result.err().expect("an error").message()
}

#[engine_test]
fn table_get_typed_function() -> Result<(), String> {
    let mut store = Store::default();
    let wat = r#"(module
  (table (export "table") 3 funcref)
  (func $nullary)
  (func $double (param i32) (result i32) (i32.mul (local.get 0) (i32.const 2)))
  (elem (i32.const 0) $nullary $double))"#;
    let module = Module::new(&store, wat).map_err(|e| format!("{e:?}"))?;
    let instance =
        Instance::new(&mut store, &module, &imports! {}).map_err(|e| format!("{e:?}"))?;
    let table = instance
        .exports
        .get_table("table")
        .map_err(|e| format!("{e:?}"))?;

    // A function that takes and returns nothing.
    let nullary: TypedFunction<(), ()> = table
        .get_typed_function(&mut store, 0)
        .map_err(|e| format!("{e:?}"))?;
    nullary.call(&mut store).map_err(|e| format!("{e:?}"))?;

    // A function of another signature.
    let double: TypedFunction<i32, i32> = table
        .get_typed_function(&mut store, 1)
        .map_err(|e| format!("{e:?}"))?;
    assert_eq!(double.call(&mut store, 21).unwrap(), 42);

    // Neither is given with a signature that is not its own.
    let mismatch = "don't match the actual types";
    let arguments = error_message(table.get_typed_function::<i32, i32>(&mut store, 0));
    assert!(arguments.contains(mismatch), "{arguments}");
    let arguments = error_message(table.get_typed_function::<(), i32>(&mut store, 1));
    assert!(arguments.contains(mismatch), "{arguments}");
    let results = error_message(table.get_typed_function::<i32, ()>(&mut store, 1));
    assert!(results.contains(mismatch), "{results}");
    let both = error_message(table.get_typed_function::<(), ()>(&mut store, 1));
    assert!(both.contains(mismatch), "{both}");

    // A null element, and indices past the end of the table.
    let null = error_message(table.get_typed_function::<(), ()>(&mut store, 2));
    assert!(null.contains("is null"), "{null}");
    for index in [3, u32::MAX] {
        let outside = error_message(table.get_typed_function::<(), ()>(&mut store, index));
        assert!(outside.contains("out of bounds"), "{outside}");
    }

    Ok(())
}

#[engine_test]
#[cfg_attr(
    feature = "v8-default",
    ignore = "extern refs are not supported by the default v8 backend"
)]
fn table_get_typed_function_needs_a_table_of_functions() -> Result<(), String> {
    let mut store = Store::default();
    let ty = TableType::new(Type::ExternRef, 1, None);
    let table = Table::new(&mut store, ty, Value::ExternRef(None)).map_err(|e| format!("{e:?}"))?;

    let message = error_message(table.get_typed_function::<(), ()>(&mut store, 0));
    assert!(message.contains("does not hold functions"), "{message}");

    Ok(())
}

/// JavaScript does not tell the signature of a WebAssembly function. Wasmer
/// knows the ones of a module's element segments, and not the ones of what a
/// guest stores in a table as it runs.
#[cfg(feature = "js")]
#[wasm_bindgen_test]
fn js_table_get_typed_function_does_not_guess_a_signature() {
    let mut store = Store::default();
    let wat = r#"(module
  (table (export "table") 4 funcref)
  (func $nullary)
  (func $double (param i32) (result i32) (i32.mul (local.get 0) (i32.const 2)))
  (func $late_nullary)
  (func $late_double (param i32) (result i32) (i32.mul (local.get 0) (i32.const 2)))
  (elem (i32.const 0) $nullary $double)
  (elem declare func $late_nullary $late_double)
  (func (export "store_late")
    (table.set 0 (i32.const 2) (ref.func $late_nullary))
    (table.set 0 (i32.const 3) (ref.func $late_double))))"#;
    let module = Module::new(&store, wat).unwrap();
    let instance = Instance::new(&mut store, &module, &imports! {}).unwrap();
    let table = instance.exports.get_table("table").unwrap();
    let store_late = instance.exports.get_function("store_late").unwrap();
    store_late.call(&mut store, &[]).unwrap();

    let unknown = "is unknown";
    for index in [2, 3] {
        // Not even with the signature the function has.
        let message = error_message(table.get_typed_function::<(), ()>(&mut store, index));
        assert!(message.contains(unknown), "{message}");
        let message = error_message(table.get_typed_function::<i32, i32>(&mut store, index));
        assert!(message.contains(unknown), "{message}");

        // `get` gives the function as a `() -> ()` one, whatever it is, which
        // `typed` then takes its word for...
        let Some(Value::FuncRef(Some(guessed))) = table.get(&mut store, index) else {
            panic!("the table has no function at {index}");
        };
        assert_eq!(guessed.ty(&store), FunctionType::new([], []));
        assert!(guessed.typed::<(), ()>(&store).is_ok());
        // ... and that guess does not become the signature of the function.
        let message = error_message(table.get_typed_function::<(), ()>(&mut store, index));
        assert!(message.contains(unknown), "{message}");
    }

    // A function known to be a `() -> ()` one is told from those.
    let nullary: TypedFunction<(), ()> = table.get_typed_function(&mut store, 0).unwrap();
    nullary.call(&mut store).unwrap();
    let double: TypedFunction<i32, i32> = table.get_typed_function(&mut store, 1).unwrap();
    assert_eq!(double.call(&mut store, 21).unwrap(), 42);
}

/// A table of functions never holds a plain JavaScript function: the engine
/// refuses one. In a table of references it is a value, not a function.
#[cfg(feature = "js")]
#[wasm_bindgen_test]
fn js_table_get_typed_function_gives_no_plain_javascript_function() {
    use wasm_bindgen::JsCast;
    use wasmer::js::AsJs;

    let mut store = Store::default();
    let plain: js_sys::Function = js_sys::Object::new().constructor();

    let ty = TableType::new(Type::FuncRef, 1, None);
    let functions = Table::new(&mut store, ty, Value::FuncRef(None)).unwrap();
    let js_functions: js_sys::WebAssembly::Table = functions.as_jsvalue(&store).unchecked_into();
    assert!(js_functions.set(0, &plain).is_err());
    let message = error_message(functions.get_typed_function::<(), ()>(&mut store, 0));
    assert!(message.contains("is null"), "{message}");

    let ty = TableType::new(Type::ExternRef, 1, None);
    let references = Table::new(&mut store, ty, Value::ExternRef(None)).unwrap();
    let js_references: js_sys::WebAssembly::Table = references.as_jsvalue(&store).unchecked_into();
    js_references.set_raw(0, &plain).unwrap();
    let message = error_message(references.get_typed_function::<(), ()>(&mut store, 0));
    assert!(message.contains("does not hold functions"), "{message}");
}

#[engine_test]
#[cfg_attr(
    feature = "v8-default",
    ignore = "extern refs are not supported by the default v8 backend"
)]
fn table_set() -> Result<(), String> {
    // Table set not yet tested
    #[cfg(feature = "sys")]
    {
        let mut store = Store::default();

        let table_type = TableType {
            ty: Type::ExternRef,
            minimum: 1,
            maximum: None,
            readonly: false,
        };
        let extern_ref = ExternRef::new(&mut store, 0u32);
        let table = Table::new(
            &mut store,
            table_type,
            Value::ExternRef(Some(extern_ref.clone())),
        )
        .map_err(|e| format!("{e:?}"))?;
        assert_eq!(table.ty(&store), table_type);

        let v = table.get(&mut store, 0);
        assert!(v.is_some());

        let v = v.unwrap();

        let v = if let Value::ExternRef(Some(ext)) = v {
            ext.downcast::<u32>(&store)
        } else {
            return Err("table.get does not match `ExternRef(Some(..))`!".into());
        };

        let v = v.unwrap();
        assert_eq!(*v, 0u32);

        let extern_ref = ExternRef::new(&mut store, 1u32);
        table
            .set(&mut store, 0, Value::ExternRef(Some(extern_ref)))
            .map_err(|e| e.to_string())?;

        let v = table.get(&mut store, 0);
        assert!(v.is_some());
        let v = v.unwrap();

        let v = if let Value::ExternRef(Some(ext)) = v {
            ext.downcast::<u32>(&store)
        } else {
            return Err("table.get does not match `ExternRef(Some(..))`!".into());
        };

        assert!(v.is_some());
        let v = v.unwrap();
        assert_eq!(*v, 1u32);

        let extern_ref = ExternRef::new(&mut store, 2u32);
        table
            .set(&mut store, 0, Value::ExternRef(Some(extern_ref)))
            .map_err(|e| e.to_string())?;

        let v = table.get(&mut store, 0);
        assert!(v.is_some());
        let v = v.unwrap();

        let v = if let Value::ExternRef(Some(ext)) = v {
            ext.downcast::<u32>(&store)
        } else {
            return Err("table.get does not match `ExternRef(Some(..))`!".into());
        };

        assert!(v.is_some());
        let v = v.unwrap();
        assert_eq!(*v, 2u32);
    }

    Ok(())
}

#[engine_test]
#[cfg_attr(
    feature = "v8-default",
    ignore = "function refs are not supported by the default v8 backend"
)]
fn table_grow() -> Result<(), String> {
    // Tables are not yet fully supported in Wasm
    #[cfg(feature = "sys")]
    {
        let mut store = Store::default();
        let table_type = TableType {
            ty: Type::FuncRef,
            minimum: 0,
            maximum: Some(10),
            readonly: false,
        };
        let f = Function::new_typed(&mut store, |num: i32| num + 1);
        let table = Table::new(&mut store, table_type, Value::FuncRef(Some(f.clone())))
            .map_err(|e| format!("{e:?}"))?;

        let old_len = table.grow(&mut store, 1, Value::FuncRef(Some(f.clone())));
        assert_eq!(0, old_len.unwrap());
        let old_len = table.grow(&mut store, 1, Value::FuncRef(Some(f.clone())));
        assert_eq!(1, old_len.unwrap());

        // Growing to a bigger maximum should return None
        let old_len = table.grow(&mut store, 12, Value::FuncRef(Some(f.clone())));
        assert!(old_len.is_err());

        let old_len = table
            .grow(&mut store, 5, Value::FuncRef(Some(f)))
            .map_err(|e| format!("{e:?}"))?;
        assert_eq!(old_len, 2);
    }

    Ok(())
}

#[engine_test]
fn table_copy() -> Result<(), String> {
    // `Table::copy` is unimplemented on js, and returns an error on v8: this
    // needs the default store to be a `sys` one.
    #[cfg(all(feature = "sys", not(feature = "v8-default")))]
    {
        let mut store = Store::default();
        let table_type = TableType {
            ty: Type::ExternRef,
            minimum: 1,
            maximum: Some(1),
            readonly: false,
        };
        // Three tables, so that a copy between two of them can reach the one
        // next to either, and one of them is the last in the store.
        let tables = (1..=3u32)
            .map(|n| {
                let value = Value::ExternRef(Some(ExternRef::new(&mut store, n)));
                Table::new(&mut store, table_type, value).map_err(|e| format!("{e:?}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let read = |store: &mut Store| -> Vec<u32> {
            tables
                .iter()
                .map(|table| match table.get(store, 0) {
                    Some(Value::ExternRef(Some(ext))) => *ext.downcast::<u32>(store).unwrap(),
                    other => panic!("unexpected table element: {other:?}"),
                })
                .collect()
        };
        let copy = |store: &mut Store, dst: usize, src: usize| {
            Table::copy(store, &tables[dst], 0, &tables[src], 0, 1).map_err(|e| format!("{e:?}"))
        };

        // From an earlier table to a later one.
        copy(&mut store, 1, 0)?;
        assert_eq!(read(&mut store), [1, 1, 3]);
        // From a later table to an earlier one.
        copy(&mut store, 0, 2)?;
        assert_eq!(read(&mut store), [3, 1, 3]);
        // Into the last table in the store.
        copy(&mut store, 2, 1)?;
        assert_eq!(read(&mut store), [3, 1, 1]);
    }

    Ok(())
}

#[engine_test]
fn memory_new() -> Result<(), String> {
    let mut store = Store::default();
    let memory_type = MemoryType {
        shared: false,
        minimum: Pages(0),
        maximum: Some(Pages(10)),
    };
    let memory = Memory::new(&mut store, memory_type).map_err(|e| format!("{e:?}"))?;
    assert_eq!(memory.view(&store).size(), Pages(0));
    assert_eq!(memory.ty(&store), memory_type);
    Ok(())
}

#[test]
#[cfg(feature = "v8")]
fn memory_ty_round_trips_in_v8() -> Result<(), String> {
    let engine: Engine = wasmer::v8::V8::new().into();
    let mut store = Store::new(engine);

    for memory_type in [
        MemoryType::new(Pages(1), None, false),
        MemoryType::new(Pages(1), Some(Pages(2)), false),
        MemoryType::new(Pages(1), Some(Pages(2)), true),
    ] {
        let memory = Memory::new(&mut store, memory_type).map_err(|e| format!("{e:?}"))?;
        assert_eq!(memory.ty(&store), memory_type);
    }

    Ok(())
}

#[engine_test]
fn memory_grow() -> Result<(), String> {
    let mut store = Store::default();
    let desc = MemoryType::new(Pages(10), Some(Pages(16)), false);
    let memory = Memory::new(&mut store, desc).map_err(|e| format!("{e:?}"))?;
    assert_eq!(memory.view(&store).size(), Pages(10));

    let result = memory.grow(&mut store, Pages(2)).unwrap();
    assert_eq!(result, Pages(10));
    assert_eq!(memory.view(&store).size(), Pages(12));

    let result = memory.grow(&mut store, Pages(10));
    assert_eq!(
        result,
        Err(MemoryError::CouldNotGrow {
            current: 12.into(),
            attempted_delta: 10.into()
        })
    );

    // JS will never give BadMemory unless V8 is broken somehow
    #[cfg(feature = "sys")]
    {
        let bad_desc = MemoryType::new(Pages(15), Some(Pages(10)), false);
        let bad_result = Memory::new(&mut store, bad_desc);
        assert!(matches!(bad_result, Err(MemoryError::InvalidMemory { .. })));
    }

    Ok(())
}

#[engine_test]
fn function_new() -> Result<(), String> {
    let mut store = Store::default();
    let function = Function::new_typed(&mut store, || {});
    assert_eq!(function.ty(&store), FunctionType::new(vec![], vec![]));
    let function = Function::new_typed(&mut store, |_a: i32| {});
    assert_eq!(
        function.ty(&store),
        FunctionType::new(vec![Type::I32], vec![])
    );
    let function = Function::new_typed(&mut store, |_a: i32, _b: i64, _c: f32, _d: f64| {});
    assert_eq!(
        function.ty(&store),
        FunctionType::new(vec![Type::I32, Type::I64, Type::F32, Type::F64], vec![])
    );
    let function = Function::new_typed(&mut store, || -> i32 { 1 });
    assert_eq!(
        function.ty(&store),
        FunctionType::new(vec![], vec![Type::I32])
    );
    let function = Function::new_typed(&mut store, || -> (i32, i64, f32, f64) { (1, 2, 3.0, 4.0) });
    assert_eq!(
        function.ty(&store),
        FunctionType::new(vec![], vec![Type::I32, Type::I64, Type::F32, Type::F64])
    );
    Ok(())
}

#[engine_test]
fn function_new_env() -> Result<(), String> {
    let mut store = Store::default();
    #[derive(Clone)]
    struct MyEnv {}

    let my_env = MyEnv {};
    let env = FunctionEnv::new(&mut store, my_env);
    let function = Function::new_typed_with_env(&mut store, &env, |_env: FunctionEnvMut<MyEnv>| {});
    assert_eq!(function.ty(&store), FunctionType::new(vec![], vec![]));
    let function =
        Function::new_typed_with_env(&mut store, &env, |_env: FunctionEnvMut<MyEnv>, _a: i32| {});
    assert_eq!(
        function.ty(&store),
        FunctionType::new(vec![Type::I32], vec![])
    );
    let function = Function::new_typed_with_env(
        &mut store,
        &env,
        |_env: FunctionEnvMut<MyEnv>, _a: i32, _b: i64, _c: f32, _d: f64| {},
    );
    assert_eq!(
        function.ty(&store),
        FunctionType::new(vec![Type::I32, Type::I64, Type::F32, Type::F64], vec![])
    );
    let function =
        Function::new_typed_with_env(&mut store, &env, |_env: FunctionEnvMut<MyEnv>| -> i32 { 1 });
    assert_eq!(
        function.ty(&store),
        FunctionType::new(vec![], vec![Type::I32])
    );
    let function = Function::new_typed_with_env(
        &mut store,
        &env,
        |_env: FunctionEnvMut<MyEnv>| -> (i32, i64, f32, f64) { (1, 2, 3.0, 4.0) },
    );
    assert_eq!(
        function.ty(&store),
        FunctionType::new(vec![], vec![Type::I32, Type::I64, Type::F32, Type::F64])
    );
    Ok(())
}

#[engine_test]
fn function_new_dynamic() -> Result<(), String> {
    let mut store = Store::default();

    // Using &FunctionType signature
    let function_type = FunctionType::new(vec![], vec![]);
    let function = Function::new(
        &mut store,
        &function_type,
        |_values: &[Value]| unimplemented!(),
    );
    assert_eq!(function.ty(&store), function_type);
    let function_type = FunctionType::new(vec![Type::I32], vec![]);
    let function = Function::new(
        &mut store,
        &function_type,
        |_values: &[Value]| unimplemented!(),
    );
    assert_eq!(function.ty(&store), function_type);
    let function_type = FunctionType::new(vec![Type::I32, Type::I64, Type::F32, Type::F64], vec![]);
    let function = Function::new(
        &mut store,
        &function_type,
        |_values: &[Value]| unimplemented!(),
    );
    assert_eq!(function.ty(&store), function_type);
    let function_type = FunctionType::new(vec![], vec![Type::I32]);
    let function = Function::new(
        &mut store,
        &function_type,
        |_values: &[Value]| unimplemented!(),
    );
    assert_eq!(function.ty(&store), function_type);
    let function_type = FunctionType::new(vec![], vec![Type::I32, Type::I64, Type::F32, Type::F64]);
    let function = Function::new(
        &mut store,
        &function_type,
        |_values: &[Value]| unimplemented!(),
    );
    assert_eq!(function.ty(&store), function_type);

    // V8 does not support V128 through its wasm_c_api bindings.
    #[cfg(not(feature = "v8"))]
    {
        // Using array signature
        let function_type = ([Type::V128], [Type::I32, Type::F32, Type::F64]);
        let function = Function::new(
            &mut store,
            function_type,
            |_values: &[Value]| unimplemented!(),
        );
        assert_eq!(function.ty(&store).params(), [Type::V128]);
        assert_eq!(
            function.ty(&store).results(),
            [Type::I32, Type::F32, Type::F64]
        );
    }

    Ok(())
}

#[engine_test]
fn function_new_dynamic_env() -> Result<(), String> {
    let mut store = Store::default();
    #[derive(Clone)]
    struct MyEnv {}
    let my_env = MyEnv {};
    let env = FunctionEnv::new(&mut store, my_env);

    // Using &FunctionType signature
    let function_type = FunctionType::new(vec![], vec![]);
    let function = Function::new_with_env(
        &mut store,
        &env,
        &function_type,
        |_env: FunctionEnvMut<MyEnv>, _values: &[Value]| unimplemented!(),
    );
    assert_eq!(function.ty(&store), function_type);
    let function_type = FunctionType::new(vec![Type::I32], vec![]);
    let function = Function::new_with_env(
        &mut store,
        &env,
        &function_type,
        |_env: FunctionEnvMut<MyEnv>, _values: &[Value]| unimplemented!(),
    );
    assert_eq!(function.ty(&store), function_type);
    let function_type = FunctionType::new(vec![Type::I32, Type::I64, Type::F32, Type::F64], vec![]);
    let function = Function::new_with_env(
        &mut store,
        &env,
        &function_type,
        |_env: FunctionEnvMut<MyEnv>, _values: &[Value]| unimplemented!(),
    );
    assert_eq!(function.ty(&store), function_type);
    let function_type = FunctionType::new(vec![], vec![Type::I32]);
    let function = Function::new_with_env(
        &mut store,
        &env,
        &function_type,
        |_env: FunctionEnvMut<MyEnv>, _values: &[Value]| unimplemented!(),
    );
    assert_eq!(function.ty(&store), function_type);
    let function_type = FunctionType::new(vec![], vec![Type::I32, Type::I64, Type::F32, Type::F64]);
    let function = Function::new_with_env(
        &mut store,
        &env,
        &function_type,
        |_env: FunctionEnvMut<MyEnv>, _values: &[Value]| unimplemented!(),
    );
    assert_eq!(function.ty(&store), function_type);

    // V8 does not support V128 through its wasm_c_api bindings.
    #[cfg(not(feature = "v8"))]
    {
        // Using array signature
        let function_type = ([Type::V128], [Type::I32, Type::F32, Type::F64]);
        let function = Function::new_with_env(
            &mut store,
            &env,
            function_type,
            |_env: FunctionEnvMut<MyEnv>, _values: &[Value]| unimplemented!(),
        );
        assert_eq!(function.ty(&store).params(), [Type::V128]);
        assert_eq!(
            function.ty(&store).results(),
            [Type::I32, Type::F32, Type::F64]
        );
    }
    Ok(())
}

#[engine_test]
fn exception_new_checks_its_arguments() -> Result<(), String> {
    let mut store = Store::default();
    let tag = Tag::new(&mut store, [Type::I32]).map_err(|e| format!("{e:?}"))?;

    // The default store is a `sys` one.
    #[cfg(all(feature = "sys", not(feature = "v8-default")))]
    {
        let exception = Exception::new(&mut store, &tag, &[Value::I32(1)]);
        assert_eq!(exception.unwrap().payload(&mut store), vec![Value::I32(1)]);

        // The payload must have one value of each parameter's type.
        assert!(Exception::new(&mut store, &tag, &[]).is_err());
        assert!(Exception::new(&mut store, &tag, &[Value::I32(1), Value::I32(2)]).is_err());
        assert!(Exception::new(&mut store, &tag, &[Value::I64(1)]).is_err());

        // The tag and the references of the payload must be of the store.
        let mut other = Store::default();
        let foreign_tag = Tag::new(&mut other, [Type::I32]).unwrap();
        assert!(Exception::new(&mut store, &foreign_tag, &[Value::I32(1)]).is_err());
        let ref_tag = Tag::new(&mut store, [Type::ExternRef]).unwrap();
        let foreign = Value::ExternRef(Some(ExternRef::new(&mut other, 0u32)));
        assert!(Exception::new(&mut store, &ref_tag, &[foreign]).is_err());
    }

    // Only `sys` has exceptions: other default stores refuse them.
    #[cfg(not(all(feature = "sys", not(feature = "v8-default"))))]
    assert!(Exception::new(&mut store, &tag, &[Value::I32(1)]).is_err());

    // `js` has tags of numbers only.
    #[cfg(feature = "js")]
    assert!(Tag::new(&mut store, [Type::ExternRef]).is_err());

    Ok(())
}

#[engine_test]
fn dynamic_functions_are_refused_as_funcrefs() -> anyhow::Result<()> {
    // A dynamic host function of `sys` has no code that a funcref can point
    // to: everything that would give one to WebAssembly refuses it. The
    // default store is a `sys` one.
    #[cfg(all(feature = "sys", not(feature = "v8-default")))]
    {
        let mut store = Store::default();
        let dynamic = Function::new(&mut store, FunctionType::new([], []), |_| Ok(vec![]));
        let typed = Function::new_typed(&mut store, || {});
        let funcref = |f: &Function| Value::FuncRef(Some(f.clone()));

        let table_type = TableType::new(Type::FuncRef, 1, None);
        assert!(Table::new(&mut store, table_type, funcref(&dynamic)).is_err());
        let table = Table::new(&mut store, table_type, funcref(&typed))?;
        assert!(table.set(&mut store, 0, funcref(&dynamic)).is_err());
        assert!(table.grow(&mut store, 1, funcref(&dynamic)).is_err());

        assert!(Global::new(&mut store, funcref(&dynamic)).is_err());
        let global = Global::new_mut(&mut store, funcref(&typed))?;
        assert!(global.set(&mut store, funcref(&dynamic)).is_err());

        let tag = Tag::new(&mut store, [Type::FuncRef])?;
        assert!(Exception::new(&mut store, &tag, &[funcref(&dynamic)]).is_err());

        let module = Module::new(
            &store,
            r#"(module
                (import "env" "get" (func $get (result funcref)))
                (func (export "take") (param funcref))
                (func (export "get") (result funcref) call $get))"#,
        )?;
        let given = dynamic.clone();
        let get = Function::new(
            &mut store,
            FunctionType::new([], [Type::FuncRef]),
            move |_| Ok(vec![Value::FuncRef(Some(given.clone()))]),
        );
        let instance = Instance::new(&mut store, &module, &imports! { "env" => { "get" => get } })?;

        let take = instance.exports.get_function("take")?;
        assert!(take.call(&mut store, &[funcref(&dynamic)]).is_err());
        take.call(&mut store, &[funcref(&typed)])?;
        let take: TypedFunction<Option<Function>, ()> = take.typed(&store)?;
        assert!(take.call(&mut store, Some(dynamic.clone())).is_err());

        // A host function that returns one traps.
        let get = instance.exports.get_function("get")?;
        assert!(get.call(&mut store, &[]).is_err());
    }

    Ok(())
}

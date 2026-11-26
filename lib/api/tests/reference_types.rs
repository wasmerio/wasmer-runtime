#[cfg(feature = "sys")]
pub mod reference_types {

    use anyhow::Result;
    use macro_wasmer_engine_test::engine_test;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    #[cfg(feature = "js")]
    use wasm_bindgen_test::*;
    use wasmer::*;

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "function refs are not supported by the default v8 backend"
    )]
    fn func_ref_passed_and_returned() -> Result<()> {
        let mut store = Store::default();
        let wat = r#"(module
(import "env" "func_ref_identity" (func (param funcref) (result funcref)))
(type $ret_i32_ty (func (result i32)))
(table $table (export "table") 2 2 funcref)

(func (export "run") (param) (result funcref)
      (call 0 (ref.null func)))
(func (export "call_set_value") (param $fr funcref) (result i32)
      (table.set $table (i32.const 0) (local.get $fr))
      (call_indirect $table (type $ret_i32_ty) (i32.const 0)))
)"#;
        let module = Module::new(&store, wat)?;
        #[derive(Clone, Debug)]
        pub struct Env(Arc<AtomicBool>);
        let env = Env(Arc::new(AtomicBool::new(false)));
        let env = FunctionEnv::new(&mut store, env);
        let imports = imports! {
            "env" => {
                "func_ref_identity" => Function::new_with_env(&mut store, &env, FunctionType::new([Type::FuncRef], [Type::FuncRef]), |_env: FunctionEnvMut<Env>, values: &[Value]| -> Result<Vec<_>, _> {
                    Ok(vec![values[0].clone()])
                })
            },
        };

        let instance = Instance::new(&mut store, &module, &imports)?;

        let f: &Function = instance.exports.get_function("run")?;
        let results = f.call(&mut store, &[]).unwrap();
        if let Value::FuncRef(fr) = &results[0] {
            assert!(fr.is_none());
        } else {
            panic!("funcref not found!");
        }

        let func_to_call =
            Function::new_typed_with_env(&mut store, &env, |env: FunctionEnvMut<Env>| -> i32 {
                env.data().0.store(true, Ordering::SeqCst);
                343
            });
        let call_set_value: &Function = instance.exports.get_function("call_set_value")?;
        let results: Box<[Value]> =
            call_set_value.call(&mut store, &[Value::FuncRef(Some(func_to_call))])?;
        assert!(env.as_ref(&store).0.load(Ordering::SeqCst));
        assert_eq!(&*results, &[Value::I32(343)]);

        Ok(())
    }

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "function refs are not supported by the default v8 backend"
    )]
    fn func_ref_passed_and_called() -> Result<()> {
        let mut store = Store::default();
        let wat = r#"(module
(func $func_ref_call (import "env" "func_ref_call") (param funcref) (result i32))
(type $ret_i32_ty (func (result i32)))
(table $table (export "table") 2 2 funcref)

(func $product (param $x i32) (param $y i32) (result i32)
      (i32.mul (local.get $x) (local.get $y)))
;; TODO: figure out exactly why this statement is needed
(elem declare func $product)
(func (export "call_set_value") (param $fr funcref) (result i32)
      (table.set $table (i32.const 0) (local.get $fr))
      (call_indirect $table (type $ret_i32_ty) (i32.const 0)))
(func (export "call_func") (param $fr funcref) (result i32)
      (call $func_ref_call (local.get $fr)))
(func (export "call_host_func_with_wasm_func") (result i32)
      (call $func_ref_call (ref.func $product)))
)"#;
        let module = Module::new(&store, wat)?;
        let env = FunctionEnv::new(&mut store, ());
        fn func_ref_call(
            mut env: FunctionEnvMut<()>,
            values: &[Value],
        ) -> Result<Vec<Value>, RuntimeError> {
            // TODO: look into `Box<[Value]>` being returned breakage
            let f = values[0].unwrap_funcref().as_ref().unwrap();
            let f: TypedFunction<(i32, i32), i32> = f.typed(&env)?;
            Ok(vec![Value::I32(f.call(&mut env, 7, 9)?)])
        }

        let imports = imports! {
            "env" => {
                "func_ref_call" => Function::new_with_env(
                    &mut store,
                    &env,
                    FunctionType::new([Type::FuncRef], [Type::I32]),
                    func_ref_call
                ),
                // "func_ref_call_native" => Function::new_native(&mut store, |f: Function| -> Result<i32, RuntimeError> {
                //     let f: TypedFunction::<(i32, i32), i32> = f.typed(&mut store)?;
                //     f.call(&mut store, 7, 9)
                // })
            },
        };

        let instance = Instance::new(&mut store, &module, &imports)?;
        {
            fn sum(a: i32, b: i32) -> i32 {
                a + b
            }
            let sum_func = Function::new_typed(&mut store, sum);

            let call_func: &Function = instance.exports.get_function("call_func")?;
            let result = call_func.call(&mut store, &[Value::FuncRef(Some(sum_func))])?;
            assert_eq!(result[0].unwrap_i32(), 16);
        }

        {
            let f: TypedFunction<(), i32> = instance
                .exports
                .get_typed_function(&store, "call_host_func_with_wasm_func")?;
            let result = f.call(&mut store)?;
            assert_eq!(result, 63);
        }

        Ok(())
    }

    #[macro_wasmer_engine_test::engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "extern refs are not supported by the default v8 backend"
    )]
    fn extern_ref_passed_and_returned() -> Result<()> {
        use std::collections::HashMap;
        let mut store = Store::default();
        let wat = r#"(module
    (func $extern_ref_identity (import "env" "extern_ref_identity") (param externref) (result externref))
    (func $extern_ref_identity_native (import "env" "extern_ref_identity_native") (param externref) (result externref))
    (func $get_new_extern_ref (import "env" "get_new_extern_ref") (result externref))
    (func $get_new_extern_ref_native (import "env" "get_new_extern_ref_native") (result externref))

    (func (export "run") (param) (result externref)
          (call $extern_ref_identity (ref.null extern)))
    (func (export "run_native") (param) (result externref)
          (call $extern_ref_identity_native (ref.null extern)))
    (func (export "get_hashmap") (param) (result externref)
          (call $get_new_extern_ref))
    (func (export "get_hashmap_native") (param) (result externref)
          (call $get_new_extern_ref_native))
)"#;
        let module = Module::new(&store, wat)?;
        let env = FunctionEnv::new(&mut store, ());
        let imports = imports! {
            "env" => {
                "extern_ref_identity" => Function::new_with_env(&mut store, &env, FunctionType::new([Type::ExternRef], [Type::ExternRef]), |_env, values| -> Result<Vec<_>, _> {
                    Ok(vec![values[0].clone()])
                }),
                "extern_ref_identity_native" => Function::new_typed(&mut store, |er: Option<ExternRef>| -> Option<ExternRef> {
                    er
                }),
                "get_new_extern_ref" => Function::new_with_env(&mut store, &env, FunctionType::new([], [Type::ExternRef]), |mut env, _| -> Result<Vec<_>, _> {
                    let inner =
                        [("hello".to_string(), "world".to_string()),
                         ("color".to_string(), "orange".to_string())]
                        .iter()
                        .cloned()
                        .collect::<HashMap<String, String>>();
                    let new_extern_ref = ExternRef::new(&mut env, inner);
                    Ok(vec![Value::ExternRef(Some(new_extern_ref))])
                }),
                "get_new_extern_ref_native" => Function::new_typed_with_env(&mut store, &env, |mut env: FunctionEnvMut<()>| -> Option<ExternRef> {
                    let inner =
                        [("hello".to_string(), "world".to_string()),
                         ("color".to_string(), "orange".to_string())]
                        .iter()
                        .cloned()
                        .collect::<HashMap<String, String>>();
                    Some(ExternRef::new(&mut env.as_store_mut(), inner))
                })
            },
        };

        let instance = Instance::new(&mut store, &module, &imports)?;
        for run in &["run", "run_native"] {
            let f: &Function = instance.exports.get_function(run)?;
            let results = f.call(&mut store, &[]).unwrap();
            if let Value::ExternRef(er) = &results[0] {
                assert!(er.is_none());
            } else {
                panic!("result is not an extern ref!");
            }

            let f: TypedFunction<(), Option<ExternRef>> =
                instance.exports.get_typed_function(&store, run)?;
            let result: Option<ExternRef> = f.call(&mut store)?;
            assert!(result.is_none());
        }

        for get_hashmap in &["get_hashmap", "get_hashmap_native"] {
            let f: &Function = instance.exports.get_function(get_hashmap)?;
            let results = f.call(&mut store, &[]).unwrap();
            if let Value::ExternRef(er) = &results[0] {
                let inner: &HashMap<String, String> =
                    er.as_ref().unwrap().downcast(&store).unwrap();
                assert_eq!(inner["hello"], "world");
                assert_eq!(inner["color"], "orange");
            } else {
                panic!("result is not an extern ref!");
            }

            let f: TypedFunction<(), Option<ExternRef>> =
                instance.exports.get_typed_function(&store, get_hashmap)?;

            let result: Option<ExternRef> = f.call(&mut store)?;
            let inner: &HashMap<String, String> = result.unwrap().downcast(&store).unwrap();
            assert_eq!(inner["hello"], "world");
            assert_eq!(inner["color"], "orange");
        }

        Ok(())
    }

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "extern refs are not supported by the default v8 backend"
    )]
    fn extern_ref_ref_counting_basic() -> Result<()> {
        let mut store = Store::default();
        let wat = r#"(module
    (func (export "drop") (param $er externref) (result)
          (drop (local.get $er)))
)"#;
        let module = Module::new(&store, wat)?;
        let instance = Instance::new(&mut store, &module, &imports! {})?;
        let f: TypedFunction<Option<ExternRef>, ()> =
            instance.exports.get_typed_function(&store, "drop")?;

        let er = ExternRef::new(&mut store, 3u32);
        f.call(&mut store, Some(er.clone()))?;

        let tmp: Option<&u32> = er.downcast(&store);
        assert_eq!(tmp.unwrap(), &3);

        Ok(())
    }

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "reference types are not supported by the default v8 backend"
    )]
    fn refs_in_globals() -> Result<()> {
        let mut store = Store::default();
        let wat = r#"(module
    (global $er_global (export "er_global") (mut externref) (ref.null extern))
    (global $fr_global (export "fr_global") (mut funcref) (ref.null func))
    (global $fr_immutable_global (export "fr_immutable_global") funcref (ref.func $hello))
    (func $hello (param) (result i32)
          (i32.const 73))
)"#;
        let module = Module::new(&store, wat)?;
        let instance = Instance::new(&mut store, &module, &imports! {})?;
        {
            let er_global: &Global = instance.exports.get_global("er_global")?;

            if let Value::ExternRef(er) = er_global.get(&mut store) {
                assert!(er.is_none());
            } else {
                panic!("Did not find extern ref in the global");
            }
            let extref = Some(ExternRef::new(&mut store, 3u32));
            er_global.set(&mut store, Value::ExternRef(extref))?;

            if let Value::ExternRef(er) = er_global.get(&mut store) {
                let tmp: Option<&u32> = er.unwrap().downcast(&store);
                assert_eq!(tmp.unwrap(), &3);
            } else {
                panic!("Did not find extern ref in the global");
            }
        }

        {
            let fr_global: &Global = instance.exports.get_global("fr_immutable_global")?;

            if let Value::FuncRef(Some(f)) = fr_global.get(&mut store) {
                let native_func: TypedFunction<(), u32> = f.typed(&store)?;
                assert_eq!(native_func.call(&mut store)?, 73);
            } else {
                panic!("Did not find non-null func ref in the global");
            }
        }

        {
            let fr_global: &Global = instance.exports.get_global("fr_global")?;

            if let Value::FuncRef(None) = fr_global.get(&mut store) {
            } else {
                panic!("Did not find a null func ref in the global");
            }

            let f = Function::new_typed(&mut store, |arg1: i32, arg2: i32| -> i32 { arg1 + arg2 });

            fr_global.set(&mut store, Value::FuncRef(Some(f)))?;

            if let Value::FuncRef(Some(f)) = fr_global.get(&mut store) {
                let native: TypedFunction<(i32, i32), i32> = f.typed(&store)?;
                assert_eq!(native.call(&mut store, 5, 7)?, 12);
            } else {
                panic!("Did not find extern ref in the global");
            }
        }

        Ok(())
    }

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "extern refs are not supported by the default v8 backend"
    )]
    fn extern_ref_ref_counting_table_basic() -> Result<()> {
        let mut store = Store::default();
        let wat = r#"(module
    (global $global (export "global") (mut externref) (ref.null extern))
    (table $table (export "table") 4 4 externref)
    (func $insert (param $er externref) (param $idx i32)
           (table.set $table (local.get $idx) (local.get $er)))
    (func $intermediate (param $er externref) (param $idx i32)
          (call $insert (local.get $er) (local.get $idx)))
    (func $insert_into_table (export "insert_into_table") (param $er externref) (param $idx i32) (result externref)
          (call $intermediate (local.get $er) (local.get $idx))
          (local.get $er))
)"#;
        let module = Module::new(&store, wat)?;
        let instance = Instance::new(&mut store, &module, &imports! {})?;

        let f: TypedFunction<(Option<ExternRef>, i32), Option<ExternRef>> = instance
            .exports
            .get_typed_function(&store, "insert_into_table")?;

        let er = ExternRef::new(&mut store, 3usize);

        let er = f.call(&mut store, Some(er), 1)?;
        assert!(er.is_some());

        let table: &Table = instance.exports.get_table("table")?;

        {
            let er2 = table.get(&mut store, 1).unwrap();
            let er2 = er2.externref().unwrap();
            assert!(er2.is_some());
        }

        assert!(er.is_some());
        table.set(&mut store, 1, Value::ExternRef(None))?;

        assert!(er.is_some());

        Ok(())
    }

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "extern refs are not supported by the default v8 backend"
    )]
    fn extern_ref_ref_counting_global_basic() -> Result<()> {
        let mut store = Store::default();
        let wat = r#"(module
    (global $global (export "global") (mut externref) (ref.null extern))
    (func $get_from_global (export "get_from_global") (result externref)
          (drop (global.get $global))
          (global.get $global))
)"#;
        let module = Module::new(&store, wat)?;
        let instance = Instance::new(&mut store, &module, &imports! {})?;

        let global: &Global = instance.exports.get_global("global")?;
        {
            let er = ExternRef::new(&mut store, 3usize);
            global.set(&mut store, Value::ExternRef(Some(er)))?;
        }
        let get_from_global: TypedFunction<(), Option<ExternRef>> = instance
            .exports
            .get_typed_function(&store, "get_from_global")?;

        let er = get_from_global.call(&mut store)?;
        assert!(er.is_some());
        global.set(&mut store, Value::ExternRef(None))?;
        assert!(er.is_some());

        Ok(())
    }

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "extern refs are not supported by the default v8 backend"
    )]
    fn extern_ref_ref_counting_traps() -> Result<()> {
        let mut store = Store::default();
        let wat = r#"(module
    (func $pass_er (export "pass_extern_ref") (param externref)
          (local.get 0)
          (unreachable))
)"#;
        let module = Module::new(&store, wat)?;
        let instance = Instance::new(&mut store, &module, &imports! {})?;

        let pass_extern_ref: TypedFunction<Option<ExternRef>, ()> = instance
            .exports
            .get_typed_function(&store, "pass_extern_ref")?;

        let er = ExternRef::new(&mut store, 3usize);

        let result = pass_extern_ref.call(&mut store, Some(er));
        assert!(result.is_err());

        Ok(())
    }

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "extern refs are not supported by the default v8 backend"
    )]
    fn extern_ref_ref_counting_table_instructions() -> Result<()> {
        let mut store = Store::default();
        let wat = r#"(module
    (table $table1 (export "table1") 2 12 externref)
    (table $table2 (export "table2") 6 12 externref)
    (func $grow_table_with_ref (export "grow_table_with_ref") (param $er externref) (param $size i32) (result i32)
          (table.grow $table1 (local.get $er) (local.get $size)))
    (func $fill_table_with_ref (export "fill_table_with_ref") (param $er externref) (param $start i32) (param $end i32)
          (table.fill $table1 (local.get $start) (local.get $er) (local.get $end)))
    (func $copy_into_table2 (export "copy_into_table2")
          (table.copy $table2 $table1 (i32.const 0) (i32.const 0) (i32.const 4)))
)"#;
        let module = Module::new(&store, wat)?;
        let instance = Instance::new(&mut store, &module, &imports! {})?;

        let grow_table_with_ref: TypedFunction<(Option<ExternRef>, i32), i32> = instance
            .exports
            .get_typed_function(&store, "grow_table_with_ref")?;
        let fill_table_with_ref: TypedFunction<(Option<ExternRef>, i32, i32), ()> = instance
            .exports
            .get_typed_function(&store, "fill_table_with_ref")?;
        let copy_into_table2: TypedFunction<(), ()> = instance
            .exports
            .get_typed_function(&store, "copy_into_table2")?;
        let table1: &Table = instance.exports.get_table("table1")?;
        let table2: &Table = instance.exports.get_table("table2")?;

        let er1 = ExternRef::new(&mut store, 3usize);
        let er2 = ExternRef::new(&mut store, 5usize);
        let er3 = ExternRef::new(&mut store, 7usize);
        {
            let result = grow_table_with_ref.call(&mut store, Some(er1.clone()), 0)?;
            assert_eq!(result, 2);

            let result = grow_table_with_ref.call(&mut store, Some(er1.clone()), 10_000)?;
            assert_eq!(result, -1);

            let result = grow_table_with_ref.call(&mut store, Some(er1), 8)?;
            assert_eq!(result, 2);

            for i in 2..10 {
                let v = table1.get(&mut store, i);
                let e = v.as_ref().unwrap().unwrap_externref();
                let e_val: Option<&usize> = e.as_ref().unwrap().downcast(&store);
                assert_eq!(*e_val.unwrap(), 3);
            }
        }

        {
            fill_table_with_ref.call(&mut store, Some(er2), 0, 2)?;
        }

        {
            table2.set(&mut store, 0, Value::ExternRef(Some(er3.clone())))?;
            table2.set(&mut store, 1, Value::ExternRef(Some(er3.clone())))?;
            table2.set(&mut store, 2, Value::ExternRef(Some(er3.clone())))?;
            table2.set(&mut store, 3, Value::ExternRef(Some(er3.clone())))?;
            table2.set(&mut store, 4, Value::ExternRef(Some(er3)))?;
        }

        {
            copy_into_table2.call(&mut store)?;
            for i in 1..5 {
                let v = table2.get(&mut store, i);
                let e = v.as_ref().unwrap().unwrap_externref();
                let value: &usize = e.as_ref().unwrap().downcast(&store).unwrap();
                match i {
                    0 | 1 => assert_eq!(*value, 5),
                    4 => assert_eq!(*value, 7),
                    _ => assert_eq!(*value, 3),
                }
            }
        }

        {
            for i in 0..table1.size(&store) {
                table1.set(&mut store, i, Value::ExternRef(None))?;
            }
            for i in 0..table2.size(&store) {
                table2.set(&mut store, i, Value::ExternRef(None))?;
            }
        }

        Ok(())
    }

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "extern refs are not supported by the default v8 backend"
    )]
    fn extern_ref_ref_counting_table_instructions_in_module() -> Result<()> {
        let mut store = Store::default();
        let wat = r#"(module
    (table $table1 (export "table1") 2 12 externref)
    (table $table2 (export "table2") 6 12 externref)
    (func $grow_table_with_ref (export "grow_table_with_ref") (param $er externref) (param $size i32) (result i32)
          (table.grow $table1 (local.get $er) (local.get $size)))
    (func $fill_table_with_ref (export "fill_table_with_ref") (param $er externref) (param $start i32) (param $end i32)
          (table.fill $table1 (local.get $start) (local.get $er) (local.get $end)))
    (func $copy_into_table2 (export "copy_into_table2")
          (table.copy $table2 $table1 (i32.const 0) (i32.const 0) (i32.const 4)))
    (func (export "call_set_value") (param $er externref) (param $idx i32)
          (table.set $table2 (local.get $idx) (local.get $er)))
)"#;
        let module = Module::new(&store, wat)?;
        let instance = Instance::new(&mut store, &module, &imports! {})?;

        let grow_table_with_ref: TypedFunction<(Option<ExternRef>, i32), i32> = instance
            .exports
            .get_typed_function(&store, "grow_table_with_ref")?;
        let fill_table_with_ref: TypedFunction<(Option<ExternRef>, i32, i32), ()> = instance
            .exports
            .get_typed_function(&store, "fill_table_with_ref")?;
        let copy_into_table2: TypedFunction<(), ()> = instance
            .exports
            .get_typed_function(&store, "copy_into_table2")?;
        let call_set_value: TypedFunction<(Option<ExternRef>, i32), ()> = instance
            .exports
            .get_typed_function(&store, "call_set_value")?;
        let table1: &Table = instance.exports.get_table("table1")?;
        let table2: &Table = instance.exports.get_table("table2")?;

        let er1 = ExternRef::new(&mut store, 3usize);
        let er2 = ExternRef::new(&mut store, 5usize);
        let er3 = ExternRef::new(&mut store, 7usize);
        {
            let result = grow_table_with_ref.call(&mut store, Some(er1.clone()), 0)?;
            assert_eq!(result, 2);

            let result = grow_table_with_ref.call(&mut store, Some(er1.clone()), 10_000)?;
            assert_eq!(result, -1);

            let result = grow_table_with_ref.call(&mut store, Some(er1), 8)?;
            assert_eq!(result, 2);

            for i in 2..10 {
                let v = table1.get(&mut store, i);
                let e = v.as_ref().unwrap().unwrap_externref();
                let e_val: Option<&usize> = e.as_ref().unwrap().downcast(&store);
                assert_eq!(*e_val.unwrap(), 3);
            }
        }

        {
            fill_table_with_ref.call(&mut store, Some(er2), 0, 2)?;
        }

        {
            call_set_value.call(&mut store, Some(er3.clone()), 0)?;
            call_set_value.call(&mut store, Some(er3.clone()), 1)?;
            call_set_value.call(&mut store, Some(er3.clone()), 2)?;
            call_set_value.call(&mut store, Some(er3.clone()), 3)?;
            call_set_value.call(&mut store, Some(er3), 4)?;
        }

        {
            copy_into_table2.call(&mut store)?;
            for i in 1..5 {
                let v = table2.get(&mut store, i);
                let e = v.as_ref().unwrap().unwrap_externref();
                let value: &usize = e.as_ref().unwrap().downcast(&store).unwrap();
                match i {
                    0 | 1 => assert_eq!(*value, 5),
                    4 => assert_eq!(*value, 7),
                    _ => assert_eq!(*value, 3),
                }
            }
        }

        {
            for i in 0..table1.size(&store) {
                table1.set(&mut store, i, Value::ExternRef(None))?;
            }
            for i in 0..table2.size(&store) {
                table2.set(&mut store, i, Value::ExternRef(None))?;
            }
        }

        Ok(())
    }

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "extern refs are not supported by the default v8 backend"
    )]
    fn extern_ref_table_host_guest() -> Result<()> {
        let mut store = Store::default();
        let wat = r#"(module
    (table $table (export "table") 1 12 externref)

    (func (export "call_set_value") (param $er externref) (param $idx i32)
          (table.set $table (local.get $idx) (local.get $er)))

    (func (export "call_get_value") (param $idx i32) (result externref)
          (table.get $table (local.get $idx)))

)"#;
        let module = Module::new(&store, wat)?;
        let instance = Instance::new(&mut store, &module, &imports! {})?;
        let call_set_value: TypedFunction<(Option<ExternRef>, i32), ()> = instance
            .exports
            .get_typed_function(&store, "call_set_value")?;

        let call_get_value: TypedFunction<i32, Option<ExternRef>> = instance
            .exports
            .get_typed_function(&store, "call_get_value")?;

        let table: &Table = instance.exports.get_table("table")?;

        let er_even = ExternRef::new(&mut store, 4usize);
        let er_odd = ExternRef::new(&mut store, 9usize);

        for i in 0..table.size(&store) {
            if i % 2 == 0 {
                call_set_value.call(&mut store, Some(er_even.clone()), i as i32)?;
            } else {
                call_set_value.call(&mut store, Some(er_odd.clone()), i as i32)?;
            }
        }

        for i in 0..table.size(&store) {
            let v = table.get(&mut store, i);
            let e = v.as_ref().unwrap().unwrap_externref();
            let value: &usize = e.as_ref().unwrap().downcast(&store).unwrap();

            if i % 2 == 0 {
                assert_eq!(*value, 4)
            } else {
                assert_eq!(*value, 9)
            }
        }

        let er_even = ExternRef::new(&mut store, 6usize);
        let er_odd = ExternRef::new(&mut store, 11usize);

        for i in 0..table.size(&store) {
            if i % 2 == 0 {
                table.set(&mut store, i, Value::ExternRef(Some(er_even.clone())))?;
            } else {
                table.set(&mut store, i, Value::ExternRef(Some(er_odd.clone())))?;
            }
        }

        for i in 0..table.size(&store) {
            let v = call_get_value.call(&mut store, i as i32)?;
            let e = v.as_ref().unwrap();
            let value: &usize = e.downcast(&store).unwrap();

            if i % 2 == 0 {
                assert_eq!(*value, 6)
            } else {
                assert_eq!(*value, 11)
            }
        }

        Ok(())
    }

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "function and extern refs are not supported by the default v8 backend"
    )]
    fn function_call_with_reference_arguments_and_results() -> Result<()> {
        let mut store = Store::default();
        let wat = r#"(module
    (type $ret_i32_ty (func (result i32)))
    (table $table 1 funcref)
    (func (export "swap") (param $f funcref) (param $e externref) (param $x i32)
          (result externref funcref i32)
          (local.get $e)
          (local.get $f)
          (local.get $x))
    (func (export "call_ref") (param $f funcref) (result i32)
          (table.set $table (i32.const 0) (local.get $f))
          (call_indirect $table (type $ret_i32_ty) (i32.const 0)))
)"#;
        let module = Module::new(&store, wat)?;
        let instance = Instance::new(&mut store, &module, &imports! {})?;
        let swap: &Function = instance.exports.get_function("swap")?;
        let call_ref: &Function = instance.exports.get_function("call_ref")?;

        let callee = Function::new_typed(&mut store, || -> i32 { 77 });
        let extern_ref = ExternRef::new(&mut store, 5usize);
        let results = swap.call(
            &mut store,
            &[
                Value::FuncRef(Some(callee)),
                Value::ExternRef(Some(extern_ref)),
                Value::I32(3),
            ],
        )?;
        assert_eq!(results.len(), 3);
        let returned_extern = results[0].unwrap_externref().as_ref().unwrap();
        assert_eq!(returned_extern.downcast::<usize>(&store), Some(&5));
        let returned_func = results[1].unwrap_funcref().clone();
        assert!(returned_func.is_some());
        assert_eq!(results[2], Value::I32(3));
        let called = call_ref.call(&mut store, &[Value::FuncRef(returned_func)])?;
        assert_eq!(&*called, &[Value::I32(77)]);

        let results = swap.call(
            &mut store,
            &[Value::FuncRef(None), Value::ExternRef(None), Value::I32(4)],
        )?;
        assert!(results[0].unwrap_externref().is_none());
        assert!(results[1].unwrap_funcref().is_none());
        assert_eq!(results[2], Value::I32(4));

        // A reference argument of the wrong type is still refused.
        let error = swap
            .call(
                &mut store,
                &[
                    Value::ExternRef(None),
                    Value::ExternRef(None),
                    Value::I32(4),
                ],
            )
            .unwrap_err();
        assert!(
            error.message().contains("did not match signature"),
            "{error}"
        );

        Ok(())
    }

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "function refs are not supported by the default v8 backend"
    )]
    fn function_call_survives_an_import_creating_functions() -> Result<()> {
        // The import adds functions to the store while the call runs, which
        // grows (and moves) the store's function list: the call must not keep
        // a borrow of the callee's entry in it.
        let mut store = Store::default();
        let wat = r#"(module
    (import "env" "grow" (func $grow (param funcref i32) (result funcref)))
    (type $ret_i32_ty (func (result i32)))
    (table $table 1 funcref)
    (func (export "run") (param $f funcref) (param $n i32) (result funcref i32 externref)
          (call $grow (local.get $f) (local.get $n))
          (local.get $n)
          (ref.null extern))
    (func (export "call_ref") (param $f funcref) (result i32)
          (table.set $table (i32.const 0) (local.get $f))
          (call_indirect $table (type $ret_i32_ty) (i32.const 0)))
)"#;
        let module = Module::new(&store, wat)?;
        let env = FunctionEnv::new(&mut store, ());
        let grow = Function::new_with_env(
            &mut store,
            &env,
            FunctionType::new([Type::FuncRef, Type::I32], [Type::FuncRef]),
            |mut env: FunctionEnvMut<()>, values: &[Value]| -> Result<Vec<Value>, RuntimeError> {
                let n = values[1].unwrap_i32();
                for i in 0..n {
                    Function::new_typed(&mut env, move || -> i32 { i });
                }
                Ok(vec![values[0].clone()])
            },
        );
        let instance = Instance::new(
            &mut store,
            &module,
            &imports! { "env" => { "grow" => grow } },
        )?;
        let run: &Function = instance.exports.get_function("run")?;
        let call_ref: &Function = instance.exports.get_function("call_ref")?;

        let callee = Function::new_typed(&mut store, || -> i32 { 91 });
        for n in [1, 1000, 5000] {
            let results = run.call(
                &mut store,
                &[Value::FuncRef(Some(callee.clone())), Value::I32(n)],
            )?;
            assert_eq!(results.len(), 3);
            assert_eq!(results[1], Value::I32(n));
            assert!(results[2].unwrap_externref().is_none());
            let returned = results[0].unwrap_funcref().clone();
            assert!(returned.is_some());
            let called = call_ref.call(&mut store, &[Value::FuncRef(returned)])?;
            assert_eq!(&*called, &[Value::I32(91)]);
        }

        Ok(())
    }

    // The tests below return a reference from a host function: one of the
    // store the function is in, which the guest and the caller get, and one of
    // another store, which ends the call as passing it in does.

    /// The error of a call that a value of another store is used in.
    const CROSS_STORE: &str = "cross-`Store` values are not supported";

    /// A function and an extern object of `store`. The function returns
    /// `number` and the object is `name`, which tells them from the ones
    /// another store has at the same place.
    fn references(store: &mut Store, name: &'static str, number: i32) -> (Function, ExternRef) {
        let function = Function::new_typed(store, move || number);
        (function, ExternRef::new(store, name))
    }

    /// What `value` is when it is used with `store`.
    fn resolve(store: &mut Store, value: &Value) -> String {
        match value {
            Value::FuncRef(Some(function)) => {
                format!("a function returning {:?}", function.call(store, &[]))
            }
            Value::ExternRef(Some(object)) => {
                format!("the object {:?}", object.downcast::<&str>(store))
            }
            other => format!("{other:?}"),
        }
    }

    /// Asserts that a call ended with the error of a value of another store.
    /// If it returned instead, the panic says what the caller got.
    fn assert_refused(store: &mut Store, what: &str, result: Result<Box<[Value]>, RuntimeError>) {
        match result {
            Err(error) => assert_eq!(error.message(), CROSS_STORE, "{what}"),
            Ok(values) => {
                let values: Vec<String> = values.iter().map(|v| resolve(store, v)).collect();
                panic!("{what}: the call returned {}", values.join(" and "));
            }
        }
    }

    /// A guest function that calls `host` and returns what it returned.
    fn guest_caller(store: &mut Store, host: &Function) -> Result<Function> {
        let results: Vec<&str> = host
            .ty(store)
            .results()
            .iter()
            .map(|ty| match ty {
                Type::I32 => "i32",
                Type::FuncRef => "funcref",
                Type::ExternRef => "externref",
                other => unimplemented!("a result of type {other}"),
            })
            .collect();
        let results = match results.as_slice() {
            [] => String::new(),
            results => format!("(result {})", results.join(" ")),
        };
        let wat = format!(
            r#"(module
    (import "env" "host" (func $host {results}))
    (func (export "call") {results} (call $host)))"#
        );
        let module = Module::new(store, wat)?;
        let imports = imports! { "env" => { "host" => host.clone() } };
        let instance = Instance::new(store, &module, &imports)?;
        Ok(instance.exports.get_function("call")?.clone())
    }

    /// The results of `host`, called by the embedder and by a guest.
    fn call_both_ways(
        store: &mut Store,
        host: &Function,
    ) -> Result<[Result<Box<[Value]>, RuntimeError>; 2]> {
        let guest = guest_caller(store, host)?;
        Ok([host.call(store, &[]), guest.call(store, &[])])
    }

    /// Asserts that `host` is refused its result, however it is called.
    fn assert_result_refused(store: &mut Store, what: &str, host: &Function) -> Result<()> {
        for result in call_both_ways(store, host)? {
            assert_refused(store, what, result);
        }
        Ok(())
    }

    /// Asserts that `host` returns the function and the object that
    /// [`references`] made for `(name, number)`, or no reference for `None`.
    fn assert_result_returned(
        store: &mut Store,
        results: impl IntoIterator<Item = Result<Box<[Value]>, RuntimeError>>,
        expected: Option<(&'static str, i32)>,
    ) -> Result<()> {
        for result in results {
            for value in result?.iter() {
                match (value, expected) {
                    (Value::FuncRef(None) | Value::ExternRef(None), None) => {}
                    (Value::FuncRef(Some(function)), Some((_, number))) => {
                        assert_eq!(&*function.call(store, &[])?, &[Value::I32(number)]);
                    }
                    (Value::ExternRef(Some(object)), Some((name, _))) => {
                        assert_eq!(object.downcast::<&str>(store), Some(&name));
                    }
                    (Value::I32(_), _) => {}
                    (other, _) => panic!("{other:?} where {expected:?} is expected"),
                }
            }
        }
        Ok(())
    }

    /// A dynamic host function of `store` that returns `value`.
    fn dynamic(store: &mut Store, with_env: bool, value: Value) -> Function {
        let ty = FunctionType::new([], [value.ty()]);
        if with_env {
            let env = FunctionEnv::new(store, ());
            let body = move |_: FunctionEnvMut<()>, _: &[Value]| Ok(vec![value.clone()]);
            Function::new_with_env(store, &env, ty, body)
        } else {
            Function::new(store, ty, move |_: &[Value]| Ok(vec![value.clone()]))
        }
    }

    /// The typed host functions of `store` that return `function`, with and
    /// without an environment.
    fn typed_returning_function(store: &mut Store, function: Option<Function>) -> [Function; 2] {
        let env = FunctionEnv::new(store, ());
        let (plain, with_env) = (function.clone(), function);
        [
            Function::new_typed(store, move || -> Option<Function> { plain.clone() }),
            Function::new_typed_with_env(
                store,
                &env,
                move |_: FunctionEnvMut<()>| -> Option<Function> { with_env.clone() },
            ),
        ]
    }

    /// The typed host functions of `store` that return `object`: with and
    /// without an environment, and after another result.
    fn typed_returning_object(store: &mut Store, object: Option<ExternRef>) -> [Function; 3] {
        let env = FunctionEnv::new(store, ());
        let (plain, with_env, second) = (object.clone(), object.clone(), object);
        [
            Function::new_typed(store, move || -> Option<ExternRef> { plain.clone() }),
            Function::new_typed_with_env(
                store,
                &env,
                move |_: FunctionEnvMut<()>| -> Option<ExternRef> { with_env.clone() },
            ),
            Function::new_typed(store, move || -> (i32, Option<ExternRef>) {
                (7, second.clone())
            }),
        ]
    }

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "function refs are not supported by the default v8 backend"
    )]
    fn func_ref_of_another_store_returned_by_dynamic_host_function() -> Result<()> {
        let (mut store, mut other) = (Store::default(), Store::default());
        let _mine = references(&mut store, "mine", 1);
        let (theirs, _) = references(&mut other, "theirs", 2);

        for with_env in [false, true] {
            let host = dynamic(&mut store, with_env, Value::FuncRef(Some(theirs.clone())));
            assert_result_refused(&mut store, "a function of another store", &host)?;
        }

        Ok(())
    }

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "extern refs are not supported by the default v8 backend"
    )]
    fn extern_ref_of_another_store_returned_by_dynamic_host_function() -> Result<()> {
        let (mut store, mut other) = (Store::default(), Store::default());
        // Unchecked, the object of the other store is handed out as the one
        // this store has at the same place.
        let _mine = references(&mut store, "mine", 1);
        let (_, theirs) = references(&mut other, "theirs", 2);

        for with_env in [false, true] {
            let host = dynamic(&mut store, with_env, Value::ExternRef(Some(theirs.clone())));
            assert_result_refused(&mut store, "an object of another store", &host)?;
        }

        // The object is still usable where it belongs.
        let host = dynamic(&mut other, false, Value::ExternRef(Some(theirs)));
        let results = call_both_ways(&mut other, &host)?;
        assert_result_returned(&mut other, results, Some(("theirs", 2)))
    }

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "extern refs are not supported by the default v8 backend"
    )]
    fn refs_of_own_store_returned_by_dynamic_host_function() -> Result<()> {
        let mut store = Store::default();
        let (function, object) = references(&mut store, "mine", 1);

        for with_env in [false, true] {
            for (value, expected) in [
                (Value::FuncRef(Some(function.clone())), Some(("mine", 1))),
                (Value::ExternRef(Some(object.clone())), Some(("mine", 1))),
                (Value::FuncRef(None), None),
                (Value::ExternRef(None), None),
            ] {
                let host = dynamic(&mut store, with_env, value);
                let results = call_both_ways(&mut store, &host)?;
                assert_result_returned(&mut store, results, expected)?;
            }
        }

        Ok(())
    }

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "function refs are not supported by the default v8 backend"
    )]
    fn func_ref_of_another_store_returned_by_typed_host_function() -> Result<()> {
        let (mut store, mut other) = (Store::default(), Store::default());
        let _mine = references(&mut store, "mine", 1);
        let (theirs, _) = references(&mut other, "theirs", 2);

        for host in typed_returning_function(&mut store, Some(theirs)) {
            assert_result_refused(&mut store, "a function of another store", &host)?;

            let typed: TypedFunction<(), Option<Function>> = host.typed(&store)?;
            let error = typed.call(&mut store).unwrap_err();
            assert_eq!(error.message(), CROSS_STORE);
        }

        Ok(())
    }

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "extern refs are not supported by the default v8 backend"
    )]
    fn extern_ref_of_another_store_returned_by_typed_host_function() -> Result<()> {
        let (mut store, mut other) = (Store::default(), Store::default());
        let _mine = references(&mut store, "mine", 1);
        let (_, theirs) = references(&mut other, "theirs", 2);

        for host in typed_returning_object(&mut store, Some(theirs)) {
            assert_result_refused(&mut store, "an object of another store", &host)?;
        }

        Ok(())
    }

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "extern refs are not supported by the default v8 backend"
    )]
    fn refs_of_own_store_returned_by_typed_host_function() -> Result<()> {
        let mut store = Store::default();
        let (function, object) = references(&mut store, "mine", 1);

        for (function, object, expected) in [
            (Some(function), Some(object), Some(("mine", 1))),
            (None, None, None),
        ] {
            let hosts = typed_returning_function(&mut store, function)
                .into_iter()
                .chain(typed_returning_object(&mut store, object));
            for host in hosts.collect::<Vec<_>>() {
                let results = call_both_ways(&mut store, &host)?;
                assert_result_returned(&mut store, results, expected)?;
            }
        }

        Ok(())
    }

    /// The async host functions of `store` that return `value`: a dynamic one
    /// and a typed one.
    #[cfg(feature = "experimental-async")]
    fn async_returning(store: &mut Store, value: Value) -> [Function; 2] {
        let env = FunctionEnv::new(store, ());
        let ty = FunctionType::new([], [value.ty()]);
        let dynamic = value.clone();
        let typed = match value {
            Value::FuncRef(function) => Function::new_typed_async(
                store,
                &env,
                async move |_: &mut AsyncEnv<()>| -> Option<Function> { function.clone() },
            ),
            Value::ExternRef(object) => Function::new_typed_async(
                store,
                &env,
                async move |_: &mut AsyncEnv<()>| -> Option<ExternRef> { object.clone() },
            ),
            other => unimplemented!("an async host function returning {other:?}"),
        };
        [
            Function::new_async(
                store,
                &env,
                ty,
                async move |_: &mut AsyncEnv<()>, _: &[Value]| Ok(vec![dynamic.clone()]),
            ),
            typed,
        ]
    }

    /// The results of `host`, called by the embedder and by a guest: in place,
    /// and on a coroutine.
    #[cfg(feature = "experimental-async")]
    fn call_every_way(
        store: &mut Store,
        host: &Function,
    ) -> Result<Vec<Result<Box<[Value]>, RuntimeError>>> {
        let guest = guest_caller(store, host)?;
        let mut results = vec![host.call(store, &[]), guest.call(store, &[])];
        for function in [host, &guest] {
            results.push(futures::executor::block_on(function.call_async(store, &[])));
        }
        Ok(results)
    }

    #[cfg(feature = "experimental-async")]
    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "function refs are not supported by the default v8 backend"
    )]
    fn func_ref_of_another_store_returned_by_async_host_function() -> Result<()> {
        let (mut store, mut other) = (Store::default(), Store::default());
        let _mine = references(&mut store, "mine", 1);
        let (theirs, _) = references(&mut other, "theirs", 2);

        for host in async_returning(&mut store, Value::FuncRef(Some(theirs))) {
            for result in call_every_way(&mut store, &host)? {
                assert_refused(&mut store, "a function of another store", result);
            }
        }

        Ok(())
    }

    #[cfg(feature = "experimental-async")]
    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "extern refs are not supported by the default v8 backend"
    )]
    fn extern_ref_of_another_store_returned_by_async_host_function() -> Result<()> {
        let (mut store, mut other) = (Store::default(), Store::default());
        let _mine = references(&mut store, "mine", 1);
        let (_, theirs) = references(&mut other, "theirs", 2);

        for host in async_returning(&mut store, Value::ExternRef(Some(theirs))) {
            for result in call_every_way(&mut store, &host)? {
                assert_refused(&mut store, "an object of another store", result);
            }
        }

        Ok(())
    }

    #[cfg(feature = "experimental-async")]
    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "extern refs are not supported by the default v8 backend"
    )]
    fn refs_of_own_store_returned_by_async_host_function() -> Result<()> {
        let mut store = Store::default();
        let (function, object) = references(&mut store, "mine", 1);

        for (value, expected) in [
            (Value::FuncRef(Some(function)), Some(("mine", 1))),
            (Value::ExternRef(Some(object)), Some(("mine", 1))),
            (Value::FuncRef(None), None),
            (Value::ExternRef(None), None),
        ] {
            for host in async_returning(&mut store, value) {
                let results = call_every_way(&mut store, &host)?;
                assert_result_returned(&mut store, results, expected)?;
            }
        }

        Ok(())
    }

    /// A host function of `store` that ends with `error`.
    fn failing(store: &mut Store, error: RuntimeError) -> Function {
        Function::new(store, FunctionType::new([], []), move |_: &[Value]| {
            Err(error.clone())
        })
    }

    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "exceptions are not supported by the default v8 backend"
    )]
    fn exception_of_another_store_thrown_by_host_function() -> Result<()> {
        let (mut store, mut other) = (Store::default(), Store::default());
        let no_payload = Vec::<Type>::new();
        let mine = Tag::new(&mut store, no_payload.clone())?;
        let mine = Exception::new(&mut store, &mine, &[])?;
        let theirs = Tag::new(&mut other, no_payload)?;
        let theirs = Exception::new(&mut other, &theirs, &[])?;

        // A guest that catches what `host` throws, and returns whether it
        // threw.
        let wat = r#"(module
    (import "env" "host" (func $host))
    (func (export "catch") (result i32)
          (block $caught
            (try_table (catch_all $caught)
              (call $host)
              (return (i32.const 0))))
          (i32.const 1)))"#;
        let module = Module::new(&store, wat)?;
        let catch = |store: &mut Store, error: RuntimeError| -> Result<_> {
            let host = failing(store, error);
            let imports = imports! { "env" => { "host" => host } };
            let instance = Instance::new(store, &module, &imports)?;
            Ok(instance.exports.get_function("catch")?.call(store, &[]))
        };

        let error = RuntimeError::exception(&store, mine);
        assert_eq!(&*catch(&mut store, error)??, &[Value::I32(1)]);

        // Thrown into this store, the exception of the other one would be the
        // one this store has at the same place. The guest does not get to
        // catch it.
        let error = RuntimeError::exception(&other, theirs);
        let caught = catch(&mut store, error)?;
        assert_refused(&mut store, "an exception of another store", caught);

        Ok(())
    }

    // The async host functions below take the paths of this backend that the
    // tests above do not: a future the guest waits for gives its results raw,
    // and a function that returns `Deferred` has a trampoline of its own.

    /// Pending once, then ready: the guest of an import that awaits it waits.
    #[cfg(feature = "experimental-async")]
    struct Once(bool);

    #[cfg(feature = "experimental-async")]
    impl std::future::Future for Once {
        type Output = ();

        fn poll(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<()> {
            if std::mem::replace(&mut self.0, true) {
                return std::task::Poll::Ready(());
            }
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
    }

    /// A kind of async host function, whether its guest waits for it, and
    /// one that returns a given value.
    #[cfg(feature = "experimental-async")]
    type AsyncKind = (&'static str, bool, Function);

    /// The typed async host functions of a store that return `$value`, of the
    /// type `$ty`: an `async` one that waits, and those that return each
    /// form of `Deferred`.
    #[cfg(feature = "experimental-async")]
    macro_rules! typed_async_kinds {
        ($store:expr, $env:expr, $value:expr, $ty:ty) => {{
            let value: $ty = $value;
            let (waits, ready, polled, pending) =
                (value.clone(), value.clone(), value.clone(), value);
            let kinds: Vec<AsyncKind> = vec![
                (
                    "an async function that waits",
                    true,
                    Function::new_typed_async(
                        $store,
                        $env,
                        async move |_: &mut AsyncEnv<()>| -> $ty {
                            Once(false).await;
                            waits.clone()
                        },
                    ),
                ),
                (
                    "a function that returns `Deferred::ready`",
                    false,
                    Function::new_typed_async(
                        $store,
                        $env,
                        move |_: &mut AsyncEnv<()>| -> Deferred<$ty> {
                            Deferred::ready(ready.clone())
                        },
                    ),
                ),
                (
                    "a function that returns a `Deferred` that is ready when polled",
                    false,
                    Function::new_typed_async(
                        $store,
                        $env,
                        move |_: &mut AsyncEnv<()>| -> Deferred<$ty> {
                            let polled = polled.clone();
                            Deferred::pending(async move { polled })
                        },
                    ),
                ),
                (
                    "a function that returns a `Deferred` that waits",
                    true,
                    Function::new_typed_async(
                        $store,
                        $env,
                        move |_: &mut AsyncEnv<()>| -> Deferred<$ty> {
                            let pending = pending.clone();
                            Deferred::pending(async move {
                                Once(false).await;
                                pending
                            })
                        },
                    ),
                ),
            ];
            kinds
        }};
    }

    /// The dynamic async host functions of `store` that return `value`: an
    /// `async` one that waits, and one that returns `Deferred`.
    #[cfg(feature = "experimental-async")]
    fn dynamic_async_kinds(
        store: &mut Store,
        env: &FunctionEnv<()>,
        value: Value,
    ) -> Vec<AsyncKind> {
        let ty = FunctionType::new([], [value.ty()]);
        let (waits, ready) = (value.clone(), value);
        vec![
            (
                "a dynamic async function that waits",
                true,
                Function::new_async(
                    store,
                    env,
                    ty.clone(),
                    async move |_: &mut AsyncEnv<()>, _: &[Value]| {
                        Once(false).await;
                        Ok(vec![waits.clone()])
                    },
                ),
            ),
            (
                "a dynamic function that returns `Deferred::ready`",
                false,
                Function::new_async(store, env, ty, move |_: &mut AsyncEnv<()>, _: &[Value]| {
                    Deferred::ready(Ok(vec![ready.clone()]))
                }),
            ),
        ]
    }

    /// Every kind of async host function of `store` that returns `function`,
    /// and every kind that returns `object`.
    #[cfg(feature = "experimental-async")]
    fn async_kinds(
        store: &mut Store,
        function: Option<Function>,
        object: Option<ExternRef>,
    ) -> Vec<AsyncKind> {
        let env = FunctionEnv::new(store, ());
        let mut kinds = typed_async_kinds!(&mut *store, &env, function.clone(), Option<Function>);
        kinds.extend(typed_async_kinds!(
            &mut *store,
            &env,
            object.clone(),
            Option<ExternRef>
        ));
        kinds.extend(dynamic_async_kinds(store, &env, Value::FuncRef(function)));
        kinds.extend(dynamic_async_kinds(store, &env, Value::ExternRef(object)));
        kinds
    }

    /// The results of `host`, called by the embedder and by a guest: on a
    /// coroutine, and in place too unless its guest has to wait, which it
    /// cannot do there.
    #[cfg(feature = "experimental-async")]
    fn call_where_it_can_wait(
        store: &mut Store,
        host: &Function,
        waits: bool,
    ) -> Result<Vec<Result<Box<[Value]>, RuntimeError>>> {
        let guest = guest_caller(store, host)?;
        let mut results = Vec::new();
        for function in [host, &guest] {
            results.push(futures::executor::block_on(function.call_async(store, &[])));
            if !waits {
                results.push(function.call(store, &[]));
            }
        }
        Ok(results)
    }

    #[cfg(feature = "experimental-async")]
    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "function and extern refs are not supported by the default v8 backend"
    )]
    fn refs_of_another_store_returned_by_every_kind_of_async_host_function() -> Result<()> {
        let (mut store, mut other) = (Store::default(), Store::default());
        let _mine = references(&mut store, "mine", 1);
        let (function, object) = references(&mut other, "theirs", 2);

        for (kind, waits, host) in async_kinds(&mut store, Some(function), Some(object)) {
            for result in call_where_it_can_wait(&mut store, &host, waits)? {
                assert_refused(&mut store, kind, result);
            }
        }

        Ok(())
    }

    #[cfg(feature = "experimental-async")]
    #[engine_test]
    #[cfg_attr(
        feature = "v8-default",
        ignore = "function and extern refs are not supported by the default v8 backend"
    )]
    fn refs_of_own_store_returned_by_every_kind_of_async_host_function() -> Result<()> {
        let mut store = Store::default();
        let (function, object) = references(&mut store, "mine", 1);

        for (function, object, expected) in [
            (Some(function), Some(object), Some(("mine", 1))),
            (None, None, None),
        ] {
            for (_kind, waits, host) in async_kinds(&mut store, function, object) {
                let results = call_where_it_can_wait(&mut store, &host, waits)?;
                assert_result_returned(&mut store, results, expected)?;
            }
        }

        Ok(())
    }
}

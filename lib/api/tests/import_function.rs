use macro_wasmer_engine_test::engine_test;
#[cfg(feature = "js")]
use wasm_bindgen_test::*;

use anyhow::Result;
use wasmer::*;

mod common;
use common::Exit;

#[engine_test]
#[cfg_attr(
    all(target_os = "windows", feature = "v8"),
    ignore = "flaky test on windows when using v8"
)]
fn pass_i64_between_host_and_plugin() -> Result<(), String> {
    let mut store = Store::default();

    let wat = r#"(module
        (func $add_one_i64 (import "host" "add_one_i64") (param i64) (result i64))
        (func (export "add_three_i64") (param i64) (result i64)
            (i64.add (call $add_one_i64 (i64.add (local.get 0) (i64.const 1))) (i64.const 1))
        )
    )"#;
    let module = Module::new(&store, wat).map_err(|e| format!("{e:?}"))?;

    let imports = {
        imports! {
            "host" => {
                "add_one_i64" => Function::new_typed(&mut store, |value: i64| value.wrapping_add(1)),
            },
        }
    };

    let instance = Instance::new(&mut store, &module, &imports).map_err(|e| format!("{e:?}"))?;
    let add_three_i64 = instance
        .exports
        .get_typed_function::<i64, i64>(&store, "add_three_i64")
        .map_err(|e| format!("{e:?}"))?;

    let mut numbers = Vec::<i64>::new();
    numbers.extend(-4..=4);
    numbers.extend((i64::MAX - 4)..=i64::MAX);
    numbers.extend((i64::MIN)..=(i64::MIN + 4));

    for number in numbers {
        let wasm_result = add_three_i64
            .call(&mut store, number)
            .map_err(|e| format!("{e:?}"))?;
        let compare_result = number.wrapping_add(3);

        assert_eq!(wasm_result, compare_result)
    }
    Ok(())
}

#[engine_test]
#[cfg_attr(
    all(target_os = "windows", feature = "v8"),
    ignore = "flaky test on windows when using v8"
)]
fn pass_u64_between_host_and_plugin() -> Result<(), String> {
    let mut store = Store::default();

    let wat = r#"(module
        (func $add_one_u64 (import "host" "add_one_u64") (param i64) (result i64))
        (func (export "add_three_u64") (param i64) (result i64)
            (i64.add (call $add_one_u64 (i64.add (local.get 0) (i64.const 1))) (i64.const 1))
        )
    )"#;
    let module = Module::new(&store, wat).map_err(|e| format!("{e:?}"))?;

    let imports = {
        imports! {
            "host" => {
                "add_one_u64" => Function::new_typed(&mut store, |value: u64| value.wrapping_add(1)),
            },
        }
    };

    let instance = Instance::new(&mut store, &module, &imports).map_err(|e| format!("{e:?}"))?;
    let add_three_u64 = instance
        .exports
        .get_typed_function::<u64, u64>(&store, "add_three_u64")
        .map_err(|e| format!("{e:?}"))?;

    let mut numbers = Vec::<u64>::new();
    numbers.extend(0..=4);
    numbers.extend((u64::MAX / 2 - 4)..=(u64::MAX / 2 + 4));
    numbers.extend((u64::MAX - 4)..=u64::MAX);

    for number in numbers {
        let wasm_result = add_three_u64
            .call(&mut store, number)
            .map_err(|e| format!("{e:?}"))?;
        let compare_result = number.wrapping_add(3);

        assert_eq!(wasm_result, compare_result)
    }
    Ok(())
}

#[engine_test]
fn calling_function_exports() -> Result<()> {
    let mut store = Store::default();
    let wat = r#"(module
    (func (export "add") (param $lhs i32) (param $rhs i32) (result i32)
        local.get $lhs
        local.get $rhs
        i32.add)
)"#;
    let module = Module::new(&store, wat)?;
    let imports = imports! {
        // "host" => {
        //     "host_func1" => Function::new_typed(&mut store, |p: u64| {
        //         println!("host_func1: Found number {}", p);
        //         // assert_eq!(p, u64::max_value());
        //     }),
        // }
    };
    let instance = Instance::new(&mut store, &module, &imports)?;

    let add: TypedFunction<(i32, i32), i32> = instance.exports.get_typed_function(&store, "add")?;

    let result = add.call(&mut store, 10, 20)?;
    assert_eq!(result, 30);

    Ok(())
}

#[engine_test]
fn back_and_forth_with_imports() -> Result<()> {
    let mut store = Store::default();
    // We can use the WAT syntax as well!
    let module = Module::new(
        &store,
        br#"(module
            (func $sum (import "env" "sum") (param i32 i32) (result i32))
            (func (export "add_one") (param i32) (result i32)
                (call $sum (local.get 0) (i32.const 1))
            )
        )"#,
    )?;

    fn sum(a: i32, b: i32) -> i32 {
        println!("Summing: {a}+{b}");
        a + b
    }

    let import_object = imports! {
        "env" => {
            "sum" => Function::new_typed(&mut store, sum),
        }
    };
    let instance = Instance::new(&mut store, &module, &import_object)?;

    let add_one: TypedFunction<i32, i32> =
        instance.exports.get_typed_function(&store, "add_one")?;
    add_one.call(&mut store, 1)?;

    Ok(())
}

// A typed host function, and dynamic ones with no, one and two results (each
// its own path in the JS backend), each failing with `Exit`: the caller gets
// that error back, and can downcast it.
#[engine_test]
fn host_functions_fail_with_their_own_error() -> Result<()> {
    let mut store = Store::default();
    let module = Module::new(
        &store,
        br#"(module
            (import "host" "typed" (func $typed (param i32)))
            (import "host" "none" (func $none (param i32)))
            (import "host" "one" (func $one (param i32) (result i32)))
            (import "host" "two" (func $two (param i32) (result i32 i32)))
            (func (export "typed") (param i32) (call $typed (local.get 0)))
            (func (export "none") (param i32) (call $none (local.get 0)))
            (func (export "one") (param i32) (result i32) (call $one (local.get 0)))
            (func (export "two") (param i32) (result i32 i32) (call $two (local.get 0)))
        )"#,
    )?;

    let exit = |args: &[Value]| -> Result<Vec<Value>, RuntimeError> {
        Err(RuntimeError::user(Box::new(Exit(args[0].unwrap_i32()))))
    };
    let imports = imports! {
        "host" => {
            "typed" => Function::new_typed(&mut store, |code: i32| Err::<(), _>(Exit(code))),
            "none" => Function::new(&mut store, FunctionType::new([Type::I32], []), exit),
            "one" => Function::new(&mut store, FunctionType::new([Type::I32], [Type::I32]), exit),
            "two" => Function::new(
                &mut store,
                FunctionType::new([Type::I32], [Type::I32, Type::I32]),
                exit,
            ),
        }
    };
    let instance = Instance::new(&mut store, &module, &imports)?;

    for (code, name) in (1..).zip(["typed", "none", "one", "two"]) {
        let error = instance
            .exports
            .get_function(name)?
            .call(&mut store, &[Value::I32(code)])
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<Exit>().map(|exit| exit.0),
            Some(code),
            "{name}: {error}"
        );
    }
    Ok(())
}

/// On the `js` backend the error of a host function reaches its caller as a
/// JavaScript exception, thrown through the guest. It is the exception of a
/// trap, which no handler of a guest catches, as none catches a host
/// function's error on the other backends: the guest is ended, and the
/// caller gets the error itself back. Each guest here calls its imports
/// inside one kind of handler that catches all a guest can catch, and says
/// so with `caught`.
#[cfg(feature = "js")]
mod guest_handlers {
    use wasm_bindgen::{JsValue, prelude::wasm_bindgen};
    use wasmer::js::AsJs;

    use super::*;

    #[wasm_bindgen(inline_js = r#"
    export function throwingAnError() {
      return () => { throw new Error("thrown by JavaScript"); };
    }
    "#)]
    extern "C" {
        /// A JavaScript function that throws an `Error`.
        #[wasm_bindgen(js_name = throwingAnError)]
        fn throwing_an_error() -> JsValue;
    }

    /// Each kind of handler, around `{call}`.
    const HANDLERS: [(&str, &str); 3] = [
        (
            "try_table catch_all",
            "(block $h (try_table (catch_all $h) {call} (return))) (call $caught)",
        ),
        (
            "try_table catch_all_ref",
            "(block $h (result exnref) (try_table (catch_all_ref $h) {call}) (return))
             (drop) (call $caught)",
        ),
        (
            "legacy catch_all",
            "try {call} catch_all (call $caught) end",
        ),
    ];

    /// The imports that fail with `Exit` of the code they are given, their
    /// types, and how a guest calls each with its own parameter: every path
    /// a host function's error takes out of the JS backend, and `reenter`,
    /// which fails with the error of a guest it called itself.
    const FAILING: [(&str, &str, &str); 8] = [
        ("typed", "(param i32)", "(call $typed (local.get 0))"),
        (
            "typed_env",
            "(param i32)",
            "(call $typed_env (local.get 0))",
        ),
        (
            "numbers",
            "(param i32) (result i32)",
            "(drop (call $numbers (local.get 0)))",
        ),
        (
            "none",
            "(param i64)",
            "(call $none (i64.extend_i32_s (local.get 0)))",
        ),
        (
            "one",
            "(param i64) (result i64)",
            "(drop (call $one (i64.extend_i32_s (local.get 0))))",
        ),
        (
            "two",
            "(param i32) (result i32 i32)",
            "(call $two (local.get 0)) (drop) (drop)",
        ),
        (
            "dynamic_env",
            "(param i32)",
            "(call $dynamic_env (local.get 0))",
        ),
        ("reenter", "(param i32)", "(call $reenter (local.get 0))"),
    ];

    /// The other exports, and what each does inside its handler.
    const OTHERS: [(&str, &str); 5] = [
        ("message", "(call $message (local.get 0))"),
        ("recover", "(call $recover (local.get 0))"),
        ("recurse", "(call $recurse (local.get 0))"),
        ("javascript", "(call $javascript)"),
        ("own", "(throw $own)"),
    ];

    /// A guest whose exports each take an `i32`, and do what they are listed
    /// with above inside `handler`.
    fn wat(handler: &str) -> String {
        let failing = FAILING.iter();
        let imports: String = failing
            .map(|(name, ty, _)| format!(r#"(import "host" "{name}" (func ${name} {ty}))"#))
            .collect();
        let calls = FAILING.iter().map(|(name, _, call)| (name, call));
        let exports: String = calls
            .chain(OTHERS.iter().map(|(name, call)| (name, call)))
            .map(|(name, call)| {
                let body = handler.replace("{call}", call);
                format!(r#"(func (export "{name}") (param i32) {body})"#)
            })
            .collect();
        format!(
            r#"(module
              {imports}
              (import "host" "message" (func $message (param i32)))
              (import "host" "recover" (func $recover (param i32)))
              (import "host" "recurse" (func $recurse (param i32)))
              (import "host" "javascript" (func $javascript))
              (import "host" "caught" (func $caught))
              (tag $own)
              {exports})"#
        )
    }

    /// What the host functions of a guest keep.
    #[derive(Default)]
    struct Host {
        /// How often a handler of the guest ran.
        caught: u32,
        /// The guest, for the imports that call back into it.
        guest: Option<Instance>,
        /// The code of the `Exit` that `recover` got from the guest.
        recovered: Option<i32>,
    }

    // The tests are single-threaded; a function environment has to be `Send`.
    unsafe impl Send for Host {}

    struct Guest {
        store: Store,
        instance: Instance,
        env: FunctionEnv<Host>,
    }

    fn exit_code(error: &RuntimeError) -> Option<i32> {
        error.downcast_ref::<Exit>().map(|exit| exit.0)
    }

    /// A dynamic host function that fails with `Exit` of its argument.
    fn exit(arguments: &[Value]) -> Result<Vec<Value>, RuntimeError> {
        let code = match arguments[0] {
            Value::I64(code) => code as i32,
            ref code => code.unwrap_i32(),
        };
        Err(RuntimeError::user(Box::new(Exit(code))))
    }

    /// Calls `export` of the guest from inside a host function.
    fn call_guest(
        env: &mut FunctionEnvMut<'_, Host>,
        export: &str,
        code: i32,
    ) -> Result<(), RuntimeError> {
        let guest = env.data().guest.clone().expect("the guest is set");
        let export = guest.exports.get_function(export).unwrap();
        let results = export.call(&mut env.as_store_mut(), &[Value::I32(code)]);
        results.map(drop)
    }

    /// Fails with what the export `typed` fails with.
    fn reenter(mut env: FunctionEnvMut<'_, Host>, code: i32) -> Result<(), RuntimeError> {
        call_guest(&mut env, "typed", code)
    }

    /// Goes on after the export `typed` failed, and keeps what it failed
    /// with.
    fn recover(mut env: FunctionEnvMut<'_, Host>, code: i32) {
        let error = call_guest(&mut env, "typed", code).unwrap_err();
        env.data_mut().recovered = exit_code(&error);
    }

    /// Calls the export `recurse`, which calls this again, and so on until
    /// the backend refuses to nest calls any deeper: fails with that error.
    fn recurse(mut env: FunctionEnvMut<'_, Host>, code: i32) -> Result<(), RuntimeError> {
        call_guest(&mut env, "recurse", code)
    }

    /// The guest with `handler`, if the engine compiles that handler.
    fn guest(handler: &str) -> Option<Guest> {
        let mut store = Store::default();
        let module = Module::new(&store, wat(handler)).ok()?;
        let env = FunctionEnv::new(&mut store, Host::default());
        let dynamic = |param, results: &[Type]| FunctionType::new([param], results.to_vec());
        let imports = imports! {
            "host" => {
                "typed" => Function::new_typed(&mut store, |code: i32| Err::<(), _>(Exit(code))),
                "typed_env" => Function::new_typed_with_env(
                    &mut store,
                    &env,
                    |_: FunctionEnvMut<'_, Host>, code: i32| Err::<(), _>(Exit(code)),
                ),
                "numbers" => Function::new(&mut store, dynamic(Type::I32, &[Type::I32]), exit),
                "none" => Function::new(&mut store, dynamic(Type::I64, &[]), exit),
                "one" => Function::new(&mut store, dynamic(Type::I64, &[Type::I64]), exit),
                "two" => Function::new(&mut store, dynamic(Type::I32, &[Type::I32; 2]), exit),
                "dynamic_env" => Function::new_with_env(
                    &mut store,
                    &env,
                    dynamic(Type::I32, &[]),
                    |_: FunctionEnvMut<'_, Host>, arguments: &[Value]| exit(arguments),
                ),
                "reenter" => Function::new_typed_with_env(&mut store, &env, reenter),
                "message" => Function::new_typed(&mut store, |_: i32| {
                    Err::<(), _>(RuntimeError::new("out of fuel"))
                }),
                "recover" => Function::new_typed_with_env(&mut store, &env, recover),
                "recurse" => Function::new_typed_with_env(&mut store, &env, recurse),
                "javascript" => Function::from_jsvalue(
                    &mut store,
                    &FunctionType::new([], []),
                    &throwing_an_error(),
                )
                .unwrap(),
                "caught" => Function::new_typed_with_env(
                    &mut store,
                    &env,
                    |mut env: FunctionEnvMut<'_, Host>| env.data_mut().caught += 1,
                ),
            }
        };
        let instance = Instance::new(&mut store, &module, &imports).unwrap();
        env.as_mut(&mut store).guest = Some(instance.clone());
        Some(Guest {
            store,
            instance,
            env,
        })
    }

    /// A guest for each kind of handler the engine compiles.
    fn guests() -> Vec<(&'static str, Guest)> {
        let handlers = HANDLERS.iter();
        let guests: Vec<_> = handlers
            .filter_map(|(name, handler)| Some((*name, guest(handler)?)))
            .collect();
        assert!(
            !guests.is_empty(),
            "no handler compiles here: nothing is tested"
        );
        guests
    }

    impl Guest {
        /// Calls `export` with `code`: as a typed function, or with
        /// `Function::call`.
        fn call(&mut self, export: &str, code: i32, typed: bool) -> Result<(), RuntimeError> {
            let function = self.instance.exports.get_function(export).unwrap().clone();
            if typed {
                let function: TypedFunction<i32, ()> = function.typed(&self.store).unwrap();
                return function.call(&mut self.store, code);
            }
            let results = function.call(&mut self.store, &[Value::I32(code)]);
            results.map(drop)
        }

        fn caught(&mut self) -> u32 {
            self.env.as_mut(&mut self.store).caught
        }
    }

    /// The handlers do run: they catch what the guest throws itself and what
    /// JavaScript throws, also once a host function has failed.
    #[wasm_bindgen_test]
    fn the_handlers_catch_what_a_guest_can_catch() {
        for (handler, mut guest) in guests() {
            for (caught, typed) in [(2, false), (4, true)] {
                guest.call("own", 0, typed).unwrap();
                guest.call("javascript", 0, typed).unwrap();
                assert_eq!(guest.caught(), caught, "{handler}");
                guest.call("typed", 1, typed).unwrap_err();
            }
        }
    }

    /// No handler catches the error of a host function, whichever kind of
    /// host function fails and however the guest was called, also when a
    /// guest that a host function called is in between.
    #[wasm_bindgen_test]
    fn no_handler_catches_the_error_of_a_host_function() {
        for (handler, mut guest) in guests() {
            for (code, (name, ..)) in (1..).zip(FAILING) {
                for typed in [false, true] {
                    let case = format!("{handler}, {name}, typed: {typed}");
                    let result = guest.call(name, code, typed);
                    assert_eq!(guest.caught(), 0, "{case}: the guest caught the error");
                    let error = result.unwrap_err();
                    assert_eq!(exit_code(&error), Some(code), "{case}: {error}");
                }
            }
        }
    }

    /// An error that is a `RuntimeError` already comes back as it is.
    #[wasm_bindgen_test]
    fn a_host_error_keeps_its_message_through_a_handler() {
        let message = RuntimeError::new("out of fuel").message();
        for (handler, mut guest) in guests() {
            let error = guest.call("message", 0, false).unwrap_err();
            assert_eq!(error.message(), message, "{handler}");
            assert_eq!(guest.caught(), 0, "{handler}");
        }
    }

    /// A trap code comes back as well, through every handler and host
    /// function of a recursion as deep as the backend lets calls nest.
    #[wasm_bindgen_test]
    fn a_host_error_keeps_its_trap_code_through_nested_handlers() {
        for (handler, mut guest) in guests() {
            let error = guest.call("recurse", 0, false).unwrap_err();
            assert_eq!(
                error.clone().to_trap(),
                Some(wasmer_types::TrapCode::StackOverflow),
                "{handler}: {error}"
            );
            assert_eq!(guest.caught(), 0, "{handler}");
        }
    }

    /// A host function gets the error itself from a guest it called, whose
    /// handlers did not see it, and can go on: its own guest is not ended.
    #[wasm_bindgen_test]
    fn a_host_function_gets_the_error_of_the_guest_it_called() {
        for (handler, mut guest) in guests() {
            guest.call("recover", 7, false).unwrap();
            let recovered = guest.env.as_mut(&mut guest.store).recovered;
            assert_eq!(recovered, Some(7), "{handler}");
            assert_eq!(guest.caught(), 0, "{handler}");
        }
    }
}

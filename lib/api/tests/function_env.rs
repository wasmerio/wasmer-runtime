use macro_wasmer_engine_test::engine_test;
#[cfg(feature = "js")]
use wasm_bindgen_test::wasm_bindgen_test;

use wasmer::*;

#[engine_test]
fn data_and_store() -> Result<(), String> {
    let mut store = Store::default();
    let global_mut = Global::new_mut(&mut store, Value::I32(10));
    struct Env {
        value: i32,
        global: Global,
    }
    let env = FunctionEnv::new(
        &mut store,
        Env {
            value: 0i32,
            global: global_mut,
        },
    );
    let mut envmut = env.into_mut(&mut store);

    let global = envmut.data().global.clone();
    {
        let (data, store) = envmut.data_and_store();
        assert_eq!(
            data.global.ty(&store),
            GlobalType {
                ty: Type::I32,
                mutability: Mutability::Var
            }
        );
        data.value = 10;
    }
    assert_eq!(global.get(&mut envmut), Value::I32(10));
    let value = global.get(&mut envmut).unwrap_i32() + envmut.data().value;
    envmut.data_mut().value = value;

    global.set(&mut envmut, Value::I32(value)).unwrap();

    assert_eq!(global.get(&mut envmut), Value::I32(value));

    Ok(())
}

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

    let (data, store) = envmut.data_and_store();

    assert_eq!(
        data.global.ty(&store),
        GlobalType {
            ty: Type::I32,
            mutability: Mutability::Var
        }
    );
    data.value = 20;
    let global = data.global.clone();
    assert_eq!(global.get(&mut envmut), Value::I32(10));
    let value = envmut.data().value;
    global.set(&mut envmut, Value::I32(value)).unwrap();
    assert_eq!(global.get(&mut envmut), Value::I32(value));

    Ok(())
}

#[engine_test]
fn data_and_store_memory_view() -> Result<(), String> {
    let mut store = Store::default();
    let memory = Memory::new(&mut store, MemoryType::new(1, None, false))
        .map_err(|error| error.to_string())?;
    let env = FunctionEnv::new(&mut store, (memory, 0u8));
    let mut env = env.into_mut(&mut store);
    let (data, store) = env.data_and_store();
    let view = data.0.view(&store);
    view.write_u8(0, 42).map_err(|error| error.to_string())?;
    data.1 = view.read_u8(0).map_err(|error| error.to_string())?;
    assert_eq!(data.1, 42);
    Ok(())
}

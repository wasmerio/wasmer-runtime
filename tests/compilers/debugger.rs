//! An engine with a debugger registers every module it loads, whether the
//! module is compiled, deserialized from bytes or deserialized from a file.

use std::{
    fs,
    path::PathBuf,
    sync::atomic::{AtomicU32, Ordering},
};

use anyhow::Result;
use wasmer::{
    Engine, Module,
    sys::{EngineBuilder, NativeEngineExt},
};
use wasmer_compiler::Debugger;

const DEBUGGERS: [Debugger; 2] = [Debugger::Gdb, Debugger::Lldb];

fn debugger_engine(config: &crate::Config, debugger: Debugger) -> Engine {
    let mut compiler_config = config.compiler_config(false, false);
    compiler_config.enable_debugger(debugger);
    EngineBuilder::new(compiler_config).engine().into()
}

/// A module that nothing else in this process compiles.
///
/// All the engines of a process append to one command file per debugger, so
/// a test can only tell its own registrations by the image they point at.
fn unique_wat() -> String {
    static NEXT_RESULT: AtomicU32 = AtomicU32::new(0x0054_0000);
    let result = NEXT_RESULT.fetch_add(1, Ordering::Relaxed);
    format!(r#"(module (func (export "unique") (result i32) i32.const {result}))"#)
}

/// The image files that the command file of `debugger` loads symbols from.
fn registered_images(debugger: Debugger) -> Vec<PathBuf> {
    let (command_file, load_command) = match debugger {
        Debugger::Gdb => ("wasmer-gdb", "add-symbol-file \""),
        Debugger::Lldb => ("wasmer-lldb", "target modules load --file \""),
    };
    let command_file = format!("/tmp/{command_file}.{}", std::process::id());
    fs::read_to_string(command_file)
        .unwrap_or_default()
        .lines()
        .filter_map(|line| line.strip_prefix(load_command))
        .filter_map(|rest| rest.split_once('"'))
        .map(|(path, _)| PathBuf::from(path))
        .collect()
}

/// The registered image files that hold exactly `image`.
fn registrations_of(debugger: Debugger, image: &[u8]) -> Vec<PathBuf> {
    registered_images(debugger)
        .into_iter()
        // Another test may already have removed the image it registered.
        .filter(|path| fs::read(path).is_ok_and(|bytes| bytes == image))
        .collect()
}

/// Checks that `image` was registered once, through a file that the engine
/// saved for the debugger and that loads as a module.
fn assert_registered_from_memory(debugger: Debugger, image: &[u8]) -> Result<()> {
    let registrations = registrations_of(debugger, image);
    assert_eq!(
        registrations.len(),
        1,
        "{debugger} registrations of the module: {registrations:?}"
    );
    let headless = Engine::headless();
    let module = unsafe { Module::deserialize_from_file(&headless, &registrations[0])? };
    assert_eq!(module.exports().count(), 1);
    fs::remove_file(&registrations[0])?;
    Ok(())
}

#[compiler_test(debugger)]
fn registers_a_compiled_module(config: crate::Config) -> Result<()> {
    if config.compiler == crate::Compiler::V8 {
        return Ok(());
    }
    for debugger in DEBUGGERS {
        let engine = debugger_engine(&config, debugger);
        let module = Module::new(&engine, unique_wat())?;
        assert_registered_from_memory(debugger, &module.serialize()?)?;
    }
    Ok(())
}

#[compiler_test(debugger)]
fn registers_a_module_deserialized_from_bytes(config: crate::Config) -> Result<()> {
    if config.compiler == crate::Compiler::V8 {
        return Ok(());
    }
    for debugger in DEBUGGERS {
        // Compile without a debugger, so that only the deserialization can
        // register the module.
        let image = Module::new(&config.engine(), unique_wat())?.serialize()?;
        let engine = debugger_engine(&config, debugger);
        let _module = unsafe { Module::deserialize(&engine, image.clone())? };
        assert_registered_from_memory(debugger, &image)?;
    }
    Ok(())
}

#[compiler_test(debugger)]
fn registers_a_module_deserialized_from_a_file_once(config: crate::Config) -> Result<()> {
    if config.compiler == crate::Compiler::V8 {
        return Ok(());
    }
    for debugger in DEBUGGERS {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("module.bin");
        let module = Module::new(&config.engine(), unique_wat())?;
        module.serialize_to_file(&path)?;

        let engine = debugger_engine(&config, debugger);
        let _module = unsafe { Module::deserialize_from_file(&engine, &path)? };

        // The debugger loads the file itself: the engine does not save a copy.
        assert_eq!(
            registrations_of(debugger, &module.serialize()?),
            [path.canonicalize()?]
        );
    }
    Ok(())
}

#[compiler_test(debugger)]
fn registers_a_module_deserialized_unchecked_from_a_file(config: crate::Config) -> Result<()> {
    if config.compiler == crate::Compiler::V8 {
        return Ok(());
    }
    for debugger in DEBUGGERS {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("module.bin");
        let module = Module::new(&config.engine(), unique_wat())?;
        module.serialize_to_file(&path)?;

        // This reads the file into memory, where the engine loses its path.
        let engine = debugger_engine(&config, debugger);
        let _module = unsafe { Module::deserialize_from_file_unchecked(&engine, &path)? };
        assert_registered_from_memory(debugger, &module.serialize()?)?;
    }
    Ok(())
}

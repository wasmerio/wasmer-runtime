//! Reading and writing the memory of a guest from host code that cannot reach
//! its store: [`HostMemoryAccess`].

#![cfg(any(feature = "cranelift", feature = "llvm", feature = "singlepass"))]

use std::{cell::RefCell, ptr::NonNull, sync::mpsc, thread};

use wasmer::{
    Engine, Function, FunctionEnv, FunctionEnvMut, Instance, Memory, MemoryAccessError,
    MemoryError, MemoryStyle, MemoryType, Module, Store, TableStyle, TableType, TypedFunction,
    WASM_PAGE_SIZE, imports,
    sys::{
        BaseTunables, HostMemoryAccess, NativeEngineExt, NativeMemoryExt, NativeSharedMemoryExt,
        Tunables,
        vm::{VMMemory, VMMemoryDefinition, VMTable, VMTableDefinition},
    },
};

const PAGE: u64 = WASM_PAGE_SIZE as u64;

/// Gives every memory the style that has no room to grow in place, so that
/// each `memory.grow` moves it.
struct DynamicMemoryTunables(BaseTunables);

impl Tunables for DynamicMemoryTunables {
    fn memory_style(&self, _memory: &MemoryType) -> MemoryStyle {
        MemoryStyle::Dynamic {
            offset_guard_size: PAGE,
        }
    }

    fn table_style(&self, table: &TableType) -> TableStyle {
        self.0.table_style(table)
    }

    fn create_host_memory(
        &self,
        ty: &MemoryType,
        style: &MemoryStyle,
    ) -> Result<VMMemory, MemoryError> {
        self.0.create_host_memory(ty, style)
    }

    unsafe fn create_vm_memory(
        &self,
        ty: &MemoryType,
        style: &MemoryStyle,
        vm_definition_location: NonNull<VMMemoryDefinition>,
    ) -> Result<VMMemory, MemoryError> {
        unsafe { self.0.create_vm_memory(ty, style, vm_definition_location) }
    }

    fn create_host_table(&self, ty: &TableType, style: &TableStyle) -> Result<VMTable, String> {
        self.0.create_host_table(ty, style)
    }

    unsafe fn create_vm_table(
        &self,
        ty: &TableType,
        style: &TableStyle,
        vm_definition_location: NonNull<VMTableDefinition>,
    ) -> Result<VMTable, String> {
        unsafe { self.0.create_vm_table(ty, style, vm_definition_location) }
    }
}

/// A store whose memories are static, as by default, or dynamic.
fn store(dynamic: bool) -> Store {
    let mut engine = Engine::default();
    if dynamic {
        engine.set_tunables(DynamicMemoryTunables(BaseTunables::new()));
    }
    Store::new(engine)
}

/// The handle of a memory, taken the way its kind allows: a shared memory
/// has one without a store.
fn host_access(store: &Store, memory: &Memory) -> HostMemoryAccess {
    match memory.as_shared(store) {
        Some(shared) => shared.host_memory_access().unwrap(),
        None => memory.host_memory_access(store).unwrap(),
    }
}

/// The address of the word the guests below pass around.
const WORD: u64 = 16;

fn peek_word(access: &HostMemoryAccess, address: u64) -> Result<i32, MemoryAccessError> {
    let mut word = [0; 4];
    access.read(address, &mut word)?;
    Ok(i32::from_le_bytes(word))
}

fn poke_word(access: &HostMemoryAccess, address: u64, word: i32) -> Result<(), MemoryAccessError> {
    access.write(address, &word.to_le_bytes())
}

/// The guest whose memory is looked into. `run` stores 1 in the word, stops
/// in the host, and returns what the word holds once the host lets it go on.
fn tracee(store: &mut Store, stopped: Function) -> (Instance, Memory) {
    let wat = r#"(module
        (import "host" "stopped" (func $stopped))
        (memory (export "memory") 1)
        (func (export "run") (result i32)
            (i32.store (i32.const 16) (i32.const 1))
            (call $stopped)
            (i32.load (i32.const 16))))"#;
    let module = Module::new(&*store, wat).unwrap();
    let imports = imports! { "host" => { "stopped" => stopped } };
    let instance = Instance::new(store, &module, &imports).unwrap();
    let memory = instance.exports.get_memory("memory").unwrap().clone();
    (instance, memory)
}

/// The guest that runs while the host looks into the tracee. Its `bump` adds
/// one to a word of the tracee, with the peek and poke the host gives it.
fn tracer(store: &mut Store, tracee: HostMemoryAccess) -> TypedFunction<i64, ()> {
    let wat = r#"(module
        (import "host" "peek" (func $peek (param i64) (result i32)))
        (import "host" "poke" (func $poke (param i64 i32)))
        (func (export "bump") (param i64)
            (call $poke
                (local.get 0)
                (i32.add (call $peek (local.get 0)) (i32.const 1)))))"#;
    let module = Module::new(&*store, wat).unwrap();
    let env = FunctionEnv::new(store, tracee);
    let peek = |env: FunctionEnvMut<HostMemoryAccess>, address: i64| {
        peek_word(env.data(), address as u64).unwrap()
    };
    let poke = |env: FunctionEnvMut<HostMemoryAccess>, address: i64, word: i32| {
        poke_word(env.data(), address as u64, word).unwrap()
    };
    let imports = imports! {
        "host" => {
            "peek" => Function::new_typed_with_env(store, &env, peek),
            "poke" => Function::new_typed_with_env(store, &env, poke),
        }
    };
    let instance = Instance::new(store, &module, &imports).unwrap();
    instance.exports.get_typed_function(store, "bump").unwrap()
}

fn never_stops(store: &mut Store) -> Function {
    Function::new_typed(store, || {})
}

#[test]
fn import_of_one_guest_reaches_the_memory_of_another_in_its_store() {
    let mut store = Store::default();
    let stopped = never_stops(&mut store);
    let (_tracee, memory) = tracee(&mut store, stopped);
    let access = memory.host_memory_access(&store).unwrap();
    let bump = tracer(&mut store, access);
    poke_word(&host_access(&store, &memory), WORD, 41).unwrap();

    bump.call(&mut store, WORD as i64).unwrap();

    let mut word = [0; 4];
    memory.view(&store).read(WORD, &mut word).unwrap();
    assert_eq!(i32::from_le_bytes(word), 42);
}

#[test]
fn import_of_one_guest_reaches_the_memory_of_a_guest_in_another_store() {
    let mut tracee_store = Store::default();
    let stopped = never_stops(&mut tracee_store);
    let (_tracee, memory) = tracee(&mut tracee_store, stopped);
    let access = memory.host_memory_access(&tracee_store).unwrap();
    memory.view(&tracee_store).write(WORD, &[41]).unwrap();

    let mut tracer_store = Store::default();
    let bump = tracer(&mut tracer_store, access);
    bump.call(&mut tracer_store, WORD as i64).unwrap();

    assert_eq!(memory.view(&tracee_store).read_u8(WORD).unwrap(), 42);
}

/// A guest with the store it runs in.
struct Tracer {
    store: Store,
    bump: TypedFunction<i64, ()>,
}

thread_local! {
    /// The tracer of this thread: a store cannot go into the environment of
    /// a function of another store.
    static TRACER: RefCell<Option<Tracer>> = const { RefCell::new(None) };
}

/// Lets the tracer of this thread bump the word of the tracee.
fn run_tracer() {
    TRACER.with_borrow_mut(|tracer| {
        let tracer = tracer.as_mut().unwrap();
        tracer.bump.call(&mut tracer.store, WORD as i64).unwrap();
    });
}

/// The `ptrace` case: the tracee is stopped inside a host call, so its store
/// is borrowed for as long as it stays there, and no view of its memory can
/// be made. The tracer runs meanwhile, and the tracee goes on with the word
/// the tracer left.
#[test]
fn import_of_one_guest_reaches_the_memory_of_a_guest_stopped_in_a_call() {
    let mut store = Store::default();
    let stopped = Function::new_typed(&mut store, run_tracer);
    let (tracee, memory) = tracee(&mut store, stopped);

    let access = memory.host_memory_access(&store).unwrap();
    let mut tracer_store = Store::default();
    let bump = tracer(&mut tracer_store, access);
    TRACER.set(Some(Tracer {
        store: tracer_store,
        bump,
    }));

    let run: TypedFunction<(), i32> = tracee.exports.get_typed_function(&store, "run").unwrap();
    assert_eq!(run.call(&mut store).unwrap(), 2);
    TRACER.take();
}

/// A guest that writes to its memory until the host tells it to stop.
///
/// Each round stores `0x11` and then `0x22` in the 16 bytes at 256. It ends
/// when the byte at 128 is set, or after `limit` rounds, and returns the
/// rounds it made.
fn spinner(store: &mut Store, shared: bool) -> (TypedFunction<i32, i32>, Memory) {
    let limits = if shared { "1 1 shared" } else { "1 1" };
    let wat = format!(
        r#"(module
        (memory (export "memory") {limits})
        (func (export "spin") (param $limit i32) (result i32)
            (local $rounds i32)
            (loop $again
                (i64.store (i32.const 256) (i64.const 0x1111111111111111))
                (i64.store (i32.const 264) (i64.const 0x1111111111111111))
                (i64.store (i32.const 256) (i64.const 0x2222222222222222))
                (i64.store (i32.const 264) (i64.const 0x2222222222222222))
                (local.set $rounds (i32.add (local.get $rounds) (i32.const 1)))
                (br_if $again
                    (i32.and
                        (i32.eqz (i32.load8_u (i32.const 128)))
                        (i32.lt_u (local.get $rounds) (local.get $limit)))))
            (local.get $rounds)))"#
    );
    let module = Module::new(&*store, wat).unwrap();
    let instance = Instance::new(store, &module, &imports! {}).unwrap();
    let memory = instance.exports.get_memory("memory").unwrap().clone();
    let spin = instance.exports.get_typed_function(store, "spin").unwrap();
    (spin, memory)
}

/// The guest runs in its own store on another thread, and the host reads and
/// writes its memory meanwhile. A read sees, for each byte, a value the guest
/// stored there. What the guest leaves alone reads back as the host wrote it.
/// And the guest sees the byte that tells it to stop.
fn host_reads_and_writes_while_the_guest_runs(shared: bool) {
    const LIMIT: i32 = i32::MAX;

    let (sender, receiver) = mpsc::channel();
    let guest = thread::spawn(move || {
        let mut store = Store::default();
        let (spin, memory) = spinner(&mut store, shared);
        sender.send(host_access(&store, &memory)).unwrap();
        spin.call(&mut store, LIMIT).unwrap()
    });
    let access = receiver.recv().unwrap();

    // The guest is in its loop once it has stored the last of the 16 bytes.
    let mut racing = [0; 16];
    while racing[15] == 0 {
        access.read(256, &mut racing).unwrap();
    }
    for round in 0..100_000_u64 {
        access.read(256, &mut racing).unwrap();
        assert!(racing.iter().all(|byte| [0x11, 0x22].contains(byte)));

        let written = round.to_le_bytes();
        let mut read = [0; 8];
        access.write(1024, &written).unwrap();
        access.read(1024, &mut read).unwrap();
        assert_eq!(read, written);
    }

    access.write(128, &[1]).unwrap();
    let rounds = guest.join().unwrap();
    assert!((1..LIMIT).contains(&rounds));
    assert!(matches!(
        access.data_size(),
        Err(MemoryAccessError::MemoryDropped)
    ));
}

#[test]
fn host_reads_and_writes_a_non_shared_memory_while_its_guest_runs() {
    host_reads_and_writes_while_the_guest_runs(false);
}

#[test]
fn host_reads_and_writes_a_shared_memory_while_its_guest_runs() {
    host_reads_and_writes_while_the_guest_runs(true);
}

/// A memory the host made grows in its own thread, and the handle follows it:
/// to another address if the memory is dynamic.
fn access_follows_a_grow(dynamic: bool, shared: bool) {
    let mut store = store(dynamic);
    let memory = Memory::new(&mut store, MemoryType::new(1, Some(4), shared)).unwrap();
    let access = host_access(&store, &memory);
    access.write(100, b"kept").unwrap();
    let base = memory.view(&store).data_ptr();
    assert!(matches!(
        access.read(PAGE, &mut [0]),
        Err(MemoryAccessError::HeapOutOfBounds)
    ));

    memory.grow(&mut store, 2).unwrap();

    assert_eq!(memory.view(&store).data_ptr() != base, dynamic);
    assert_eq!(access.data_size().unwrap(), 3 * PAGE);
    let mut kept = [0; 4];
    access.read(100, &mut kept).unwrap();
    assert_eq!(&kept, b"kept");

    access.write(3 * PAGE - 4, b"tail").unwrap();
    let mut tail = [0; 4];
    memory.view(&store).read(3 * PAGE - 4, &mut tail).unwrap();
    assert_eq!(&tail, b"tail");
}

#[test]
fn access_follows_a_static_memory_that_grows_in_place() {
    access_follows_a_grow(false, false);
    access_follows_a_grow(false, true);
}

#[test]
fn access_follows_a_dynamic_memory_that_grow_moves() {
    access_follows_a_grow(true, false);
    access_follows_a_grow(true, true);
}

/// A guest that grows its memory a page at a time, as far as it can.
///
/// `grow` returns what `memory.grow` does, and marks the last byte of the
/// memory with `0x5a`.
fn grower(store: &mut Store, shared: bool) -> (TypedFunction<(), i32>, Memory) {
    let limits = if shared { "1 24 shared" } else { "1 24" };
    let wat = format!(
        r#"(module
        (memory (export "memory") {limits})
        (data (i32.const 0) "grower")
        (func (export "grow") (result i32)
            (local $pages i32)
            (local.set $pages (memory.grow (i32.const 1)))
            (i32.store8
                (i32.sub (i32.mul (memory.size) (i32.const 65536)) (i32.const 1))
                (i32.const 0x5a))
            (local.get $pages)))"#
    );
    let module = Module::new(&*store, wat).unwrap();
    let instance = Instance::new(store, &module, &imports! {}).unwrap();
    let memory = instance.exports.get_memory("memory").unwrap().clone();
    let grow = instance.exports.get_typed_function(store, "grow").unwrap();
    (grow, memory)
}

/// What the host keeps between its accesses to the memory of a [`grower`].
#[derive(Default)]
struct Dumper {
    /// The last dump of the whole memory.
    dump: Vec<u8>,
    /// The byte this thread left at 64. No one else writes there, so it reads
    /// back unless a grow lost the write.
    written: u8,
}

impl Dumper {
    /// Dumps the memory, as it is at this moment, and writes to it. Returns
    /// its size.
    fn access(&mut self, access: &HostMemoryAccess) -> Result<u64, MemoryAccessError> {
        let size = access.data_size()?;
        self.dump.resize(size as usize, 0);
        access.read(0, &mut self.dump)?;
        assert_eq!(&self.dump[..6], b"grower");
        assert!([0, 0x5a].contains(self.dump.last().unwrap()));
        assert_eq!(self.dump[64], self.written);

        self.written = self.written.wrapping_add(1);
        access.write(64, &[self.written])?;
        Ok(size)
    }
}

/// The guest grows its memory on another thread, and then drops it with its
/// store, while the host goes on reading and writing. Every access either
/// works on the memory as it is at that moment, or says the memory is gone.
fn accesses_follow_a_guest_that_grows_on_another_thread(dynamic: bool, shared: bool) {
    for _ in 0..4 {
        let (sender, receiver) = mpsc::channel();
        let (starter, start) = mpsc::channel();
        let guest = thread::spawn(move || {
            let mut store = store(dynamic);
            let (grow, memory) = grower(&mut store, shared);
            sender.send(host_access(&store, &memory)).unwrap();
            start.recv().unwrap();
            while grow.call(&mut store).unwrap() != -1 {}
        });
        let access = receiver.recv().unwrap();

        let mut dumper = Dumper::default();
        let mut size = dumper.access(&access).unwrap();
        assert_eq!(size, PAGE);
        starter.send(()).unwrap();
        loop {
            match dumper.access(&access) {
                Ok(now) => {
                    assert!(now >= size);
                    size = now;
                }
                Err(MemoryAccessError::MemoryDropped) => break,
                Err(error) => panic!("unexpected error: {error}"),
            }
        }
        assert!(size <= 24 * PAGE);
        guest.join().unwrap();
    }
}

#[test]
fn accesses_follow_a_static_memory_that_grows_on_another_thread() {
    accesses_follow_a_guest_that_grows_on_another_thread(false, false);
    accesses_follow_a_guest_that_grows_on_another_thread(false, true);
}

#[test]
fn accesses_follow_a_dynamic_memory_that_moves_on_another_thread() {
    accesses_follow_a_guest_that_grows_on_another_thread(true, false);
    accesses_follow_a_guest_that_grows_on_another_thread(true, true);
}

#[test]
fn access_outside_the_memory_is_rejected() {
    let mut store = Store::default();
    let memory = Memory::new(&mut store, MemoryType::new(1, Some(1), false)).unwrap();
    let access = memory.host_memory_access(&store).unwrap();
    assert_eq!(access.data_size().unwrap(), PAGE);

    access.write(PAGE - 2, &[1, 2]).unwrap();
    access.read(PAGE - 2, &mut [0; 2]).unwrap();
    access.read(PAGE, &mut []).unwrap();

    assert!(matches!(
        access.read(PAGE - 1, &mut [0; 2]),
        Err(MemoryAccessError::HeapOutOfBounds)
    ));
    assert!(matches!(
        access.write(PAGE - 1, &[7; 2]),
        Err(MemoryAccessError::HeapOutOfBounds)
    ));
    // A write that does not fit writes nothing.
    assert_eq!(memory.view(&store).read_u8(PAGE - 1).unwrap(), 2);

    assert!(matches!(
        access.read(PAGE + 1, &mut []),
        Err(MemoryAccessError::HeapOutOfBounds)
    ));
    assert!(matches!(
        access.read(u64::MAX, &mut [0; 2]),
        Err(MemoryAccessError::Overflow)
    ));
    assert!(matches!(
        access.write(u64::MAX, &[0; 2]),
        Err(MemoryAccessError::Overflow)
    ));
}

#[test]
fn access_sees_a_reset_memory_as_empty() {
    let mut store = Store::default();
    let memory = Memory::new(&mut store, MemoryType::new(1, Some(1), false)).unwrap();
    let access = memory.host_memory_access(&store).unwrap();

    memory.reset(&mut store).unwrap();

    assert_eq!(access.data_size().unwrap(), 0);
    assert!(matches!(
        access.read(0, &mut [0]),
        Err(MemoryAccessError::HeapOutOfBounds)
    ));
}

fn assert_dropped(access: &HostMemoryAccess) {
    assert!(matches!(
        access.data_size(),
        Err(MemoryAccessError::MemoryDropped)
    ));
    assert!(matches!(
        access.read(0, &mut [0]),
        Err(MemoryAccessError::MemoryDropped)
    ));
    assert!(matches!(
        access.write(0, &[0]),
        Err(MemoryAccessError::MemoryDropped)
    ));
}

#[test]
fn access_ends_when_the_store_of_a_non_shared_memory_is_dropped() {
    let mut store = Store::default();
    let memory = Memory::new(&mut store, MemoryType::new(1, None, false)).unwrap();
    let access = memory.host_memory_access(&store).unwrap();
    let clone = access.clone();
    access.write(0, &[1]).unwrap();

    drop(store);

    assert_dropped(&access);
    assert_dropped(&clone);
}

#[test]
fn access_ends_when_the_last_handle_of_a_shared_memory_is_dropped() {
    let mut store = Store::default();
    let memory = Memory::new(&mut store, MemoryType::new(1, Some(2), true)).unwrap();
    let shared = memory.as_shared(&store).unwrap();
    let access = memory.host_memory_access(&store).unwrap();
    shared.host_memory_access().unwrap().write(0, &[1]).unwrap();

    // The detached handle keeps the memory alive, and the access does not.
    drop(store);
    let mut byte = [0];
    access.read(0, &mut byte).unwrap();
    assert_eq!(byte, [1]);

    drop(shared);
    assert_dropped(&access);
}

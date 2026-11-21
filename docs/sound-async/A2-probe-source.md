# Appendix A2: probe source

The runner and the safe-Rust probes against the API at PR 16. See [the probe
notes](./03-soundness-probes.md).

The [rebuild command](./README.md#rebuild-the-trials) extracts these files.

### `run-trials.sh`

<!-- trial-file: run-trials.sh -->
````bash
#!/usr/bin/env bash
# Rebuilds the sound-async trials. Needs a checkout with PR 16's async API.
set -uo pipefail

TRIALS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(git -C "$TRIALS" rev-parse --show-toplevel)"
TARGET="${TRIALS_TARGET:-$TRIALS/target}"
failures=0

report() {
    if [ "$2" -eq 0 ]; then
        echo "PASS  $1"
    else
        echo "FAIL  $1"
        failures=$((failures + 1))
    fi
}

# Runs a command and requires its output to contain the expected text.
expect() {
    local name="$1" expected="$2" output rc
    shift 2
    output="$("$@" 2>&1)"
    grep -Fq -- "$expected" <<<"$output"
    rc=$?
    report "$name" "$rc"
    if [ "$rc" -ne 0 ]; then
        tail -5 <<<"$output"
    fi
}

echo "== type model"
mkdir -p "$TRIALS/model/out"
(cd "$TRIALS/model" && CARGO_TARGET_DIR=target cargo build --quiet)
report "model builds" $?
expect "model contract" "20 accept, 23 reject, 0 failures" \
    python3 -I "$TRIALS/model/gen-ui.py" "$TRIALS/model"
(cd "$TRIALS/model" && rustc --edition 2024 --crate-type lib --emit=metadata \
    --extern m=target/debug/libasync_api_model.rlib -L target/debug/deps \
    --out-dir out review_signatures.rs)
report "review scenarios still type-check" $?

echo "== probes against the current API"
cp "$ROOT/Cargo.lock" "$TRIALS/probes/Cargo.lock"
PROBES="$TRIALS/probes/Cargo.toml"
CARGO_TARGET_DIR="$TARGET" cargo build --quiet --release --manifest-path "$PROBES" \
    --bin p2_alias --bin p3_interleave --bin p4_self_deadlock --bin p5_escape --bin p7_start --bin r_flip
report "probes build" $?
BIN="$TARGET/release"
expect "p2 aliased host state" "both &mut live, same address = true" "$BIN/p2_alias"
expect "p3 interleaved guest" "first call = false, second call = false" "$BIN/p3_interleave"
expect "p4 self-deadlock" "still waiting for a lock its own task holds" "$BIN/p4_self_deadlock"
expect "p5 escaped handle" "store can be taken back = false" "$BIN/p5_escape"
start="$("$BIN/p7_start" 2>&1)"
for line in "typed ready, direct Function::call = Err" \
    "typed ready import: Instance::new failed" \
    "dynamic ready import: instantiated, start ran = I32(1)" \
    "pending import: Instance::new failed"; do
    grep -Fq -- "$line" <<<"$start"
    report "p7 $line" $?
done

send="$(CARGO_TARGET_DIR="$TARGET" cargo check --manifest-path "$PROBES" --bin p6_send --message-format=short 2>&1)"
lines="$(grep -oE 'p6_send\.rs:[0-9]+:[0-9]+: error' <<<"$send" | cut -d: -f2 | sort -nu | tr '\n' ' ')"
[ "$lines" = "5 6 7 8 " ]
report "p6 store, handles and call are !Send; a !Send host future is accepted" $?

weak="$(CARGO_TARGET_DIR="$TARGET" cargo check --manifest-path "$PROBES" --bin r_env_weak --message-format=short 2>&1)"
grep -Eq 'r_env_weak\.rs:.*error\[E0277\]' <<<"$weak"
report "FunctionEnv rejects an Rc-based weak handle" $?

echo "== Miri"
cp "$ROOT/Cargo.lock" "$TRIALS/miri-alias/Cargo.lock"
expect "Miri reports the aliasing" "Undefined Behavior" \
    env MIRIFLAGS="-Zmiri-disable-isolation -Zmiri-ignore-leaks" CARGO_TARGET_DIR="$TARGET/miri" \
    cargo +nightly miri run --manifest-path "$TRIALS/miri-alias/Cargo.toml"

echo
echo "$failures failures"
exit "$((failures > 0))"
````

### `probes/Cargo.toml`

<!-- trial-file: probes/Cargo.toml -->
````toml
[package]
name = "async-soundness-probes"
version = "0.0.0"
edition = "2024"
publish = false

[workspace]

[dependencies]
wasmer = { path = "../../../lib/api", default-features = false, features = ["sys", "cranelift", "wat", "experimental-async"] }
futures = "0.3"

[[bin]]
name = "p2_alias"
path = "src/p2_alias.rs"

[[bin]]
name = "p3_interleave"
path = "src/p3_interleave.rs"

[[bin]]
name = "p4_self_deadlock"
path = "src/p4_self_deadlock.rs"

[[bin]]
name = "p5_escape"
path = "src/p5_escape.rs"

[[bin]]
name = "p6_send"
path = "src/p6_send.rs"

[[bin]]
name = "p7_start"
path = "src/p7_start.rs"

[[bin]]
name = "r_flip"
path = "src/r_flip.rs"

[[bin]]
name = "r_env_weak"
path = "src/r_env_weak.rs"
````

### `probes/src/p2_alias.rs`

<!-- trial-file: probes/src/p2_alias.rs -->
````rust
//! Safe code: two live `&mut` references to the same function environment.
#![forbid(unsafe_code)]
use wasmer::{FunctionEnv, Store};

fn main() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0u64);
    let handle = env.clone();
    let mut env_mut = env.into_mut(&mut store);
    let (first, mut store_mut) = env_mut.data_and_store_mut();
    let second: &mut u64 = handle.as_mut(&mut store_mut);
    let same = std::ptr::eq(&raw const *first, &raw const *second);
    *second = 1;
    *first = 2;
    println!("both &mut live, same address = {same}, value = {}", *second);
}
````

### `probes/src/p3_interleave.rs`

<!-- trial-file: probes/src/p3_interleave.rs -->
````rust
//! Safe code: two overlapping calls interleave on one instance. The guest keeps
//! a shadow stack pointer in a global, as C and Rust guests do.
#![forbid(unsafe_code)]
use futures::{executor::block_on, future::join};
use std::{
    future::Future,
    pin::Pin,
    task::{Context, Poll},
};
use wasmer::{Function, Instance, Module, Store, Value, imports};

struct YieldOnce(bool);
impl Future for YieldOnce {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            return Poll::Ready(());
        }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

const GUEST: &str = r#"(module
  (import "host" "wait" (func $wait))
  (global $sp (mut i32) (i32.const 1000))
  (func (export "run") (result i32) (local $frame i32)
    global.get $sp i32.const 16 i32.sub local.tee $frame global.set $sp
    call $wait
    global.get $sp local.get $frame i32.eq
    local.get $frame i32.const 16 i32.add global.set $sp))"#;

fn main() {
    let mut store = Store::default();
    let module = Module::new(&store, GUEST).unwrap();
    let wait = Function::new_typed_async(&mut store, || YieldOnce(false));
    let imports = imports! { "host" => { "wait" => wait } };
    let instance = Instance::new(&mut store, &module, &imports).unwrap();
    let run = instance.exports.get_function("run").unwrap().clone();
    let store = store.into_async();
    let (first, second) = block_on(join(
        run.call_async(&store, vec![]),
        run.call_async(&store, vec![]),
    ));
    let intact = |r: Result<Box<[Value]>, _>| r.unwrap()[0] == Value::I32(1);
    println!(
        "shadow stack intact: first call = {}, second call = {}",
        intact(first),
        intact(second)
    );
}
````

### `probes/src/p4_self_deadlock.rs`

<!-- trial-file: probes/src/p4_self_deadlock.rs -->
````rust
//! Safe code: a call awaited while the same task holds a store lock can never
//! finish.
#![forbid(unsafe_code)]
use futures::{FutureExt, executor::block_on};
use wasmer::{AsStoreAsync, Function, Store};

fn main() {
    let mut store = Store::default();
    let function = Function::new_typed(&mut store, || {});
    let store = store.into_async();
    block_on(async {
        let held = store.read_lock().await;
        let mut call = Box::pin(function.call_async(&store, vec![]));
        for _ in 0..1000 {
            assert!((&mut call).now_or_never().is_none());
        }
        println!("after 1000 polls the call is still waiting for a lock its own task holds");
        drop(held);
    });
}
````

### `probes/src/p5_escape.rs`

<!-- trial-file: probes/src/p5_escape.rs -->
````rust
//! Safe code: a host function keeps its store handle after the call returns.
#![forbid(unsafe_code)]
use futures::executor::block_on;
use std::cell::RefCell;
use wasmer::{AsyncFunctionEnvMut, Function, FunctionEnv, Store};

thread_local! {
    static KEPT: RefCell<Option<AsyncFunctionEnvMut<u32>>> = const { RefCell::new(None) };
}

fn main() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0u32);
    let function = Function::new_typed_with_env_async(
        &mut store,
        &env,
        |env: AsyncFunctionEnvMut<u32>| {
            KEPT.with(|kept| *kept.borrow_mut() = Some(env));
            async {}
        },
    );
    let store = store.into_async();
    block_on(function.call_async(&store, vec![])).unwrap();
    println!(
        "call finished; store can be taken back = {}",
        store.into_store().is_ok()
    );
}
````

### `probes/src/p6_send.rs`

<!-- trial-file: probes/src/p6_send.rs -->
````rust
#![forbid(unsafe_code)]
use std::rc::Rc;
use wasmer::{AsyncFunctionEnvMut, Function, Store, StoreAsync};
fn assert_send<T: Send>(_: &T) {}
pub fn store(s: &Store) { assert_send(s); }
pub fn store_async(s: &StoreAsync) { assert_send(s); }
pub fn async_env(e: &AsyncFunctionEnvMut<u32>) { assert_send(e); }
pub fn call_future(f: &Function, s: &StoreAsync) { let call = f.call_async(s, vec![]); assert_send(&call); }
// Must compile: async host functions accept a !Send future today.
pub fn non_send_host_future(s: &mut Store) {
    let _ = Function::new_typed_async(s, || { let local = Rc::new(1); async move { std::future::ready(()).await; drop(local); } });
}
fn main() {}
````

### `probes/src/p7_start.rs`

<!-- trial-file: probes/src/p7_start.rs -->
````rust
#![forbid(unsafe_code)]
use std::{future::Future, pin::Pin, task::{Context, Poll}};
use wasmer::{Function, Instance, Module, Store, imports};

struct YieldOnce(bool);
impl Future for YieldOnce {
    type Output = ();
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.0 { return Poll::Ready(()); }
        self.0 = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

const GUEST: &str = r#"(module
  (import "host" "f" (func $f))
  (global $done (export "done") (mut i32) (i32.const 0))
  (func $start call $f i32.const 1 global.set $done)
  (start $start))"#;

fn main() {
    let mut store = Store::default();
    let module = Module::new(&store, GUEST).unwrap();
    let ready = Function::new_typed_async(&mut store, || std::future::ready(()));
    println!("typed ready, direct Function::call = {:?}", ready.call(&mut store, &[]).map(|r| r.len()).map_err(|e| e.to_string()));
    let imports_ready = imports! { "host" => { "f" => ready } };
    match Instance::new(&mut store, &module, &imports_ready) {
        Ok(instance) => println!("typed ready import: instantiated, start ran = {:?}", instance.exports.get_global("done").unwrap().get(&mut store)),
        Err(error) => println!("typed ready import: Instance::new failed: {error}"),
    }

    let mut store = Store::default();
    let module = Module::new(&store, GUEST).unwrap();
    let dynamic = Function::new_async(&mut store, wasmer::FunctionType::new(vec![], vec![]), |_| async { Ok(vec![]) });
    let imports_dynamic = imports! { "host" => { "f" => dynamic } };
    match Instance::new(&mut store, &module, &imports_dynamic) {
        Ok(instance) => println!("dynamic ready import: instantiated, start ran = {:?}", instance.exports.get_global("done").unwrap().get(&mut store)),
        Err(error) => println!("dynamic ready import: Instance::new failed: {error}"),
    }

    let mut store = Store::default();
    let module = Module::new(&store, GUEST).unwrap();
    let pending = Function::new_typed_async(&mut store, || YieldOnce(false));
    let imports_pending = imports! { "host" => { "f" => pending } };
    match Instance::new(&mut store, &module, &imports_pending) {
        Ok(_) => println!("pending import: instantiated"),
        Err(error) => println!("pending import: Instance::new failed: {error}"),
    }
}
````

### `probes/src/r_env_weak.rs`

<!-- trial-file: probes/src/r_env_weak.rs -->
````rust
use wasmer::{FunctionEnv, Store};
struct E(std::rc::Weak<()>);
fn main() { let mut s = Store::default(); let _ = FunctionEnv::new(&mut s, E(std::rc::Weak::new())); }
````

### `probes/src/r_flip.rs`

<!-- trial-file: probes/src/r_flip.rs -->
````rust
#![forbid(unsafe_code)]
use wasmer::{AsStoreMut, AsStoreRef, Store, StoreMut, StoreObjects, StoreRef};
struct Flip { store: Store, polls: u32 }
impl AsStoreRef for Flip { fn as_store_ref(&self) -> StoreRef<'_> { self.store.as_store_ref() } }
impl AsStoreMut for Flip {
    fn as_store_mut(&mut self) -> StoreMut<'_> {
        self.polls += 1;
        if self.polls == 2 { self.store = Store::default(); } // drops the old store
        self.store.as_store_mut()
    }
    fn objects_mut(&mut self) -> &mut StoreObjects { self.store.objects_mut() }
}
fn main() { let mut f = Flip { store: Store::default(), polls: 0 }; let _ = f.as_store_mut(); }
````

### `miri-alias/Cargo.toml`

<!-- trial-file: miri-alias/Cargo.toml -->
````toml
[package]
name = "miri-alias"
version = "0.0.0"
edition = "2024"
publish = false
[workspace]
[dependencies]
wasmer = { path = "../../../lib/api", default-features = false, features = ["sys", "headless"] }
````

### `miri-alias/src/main.rs`

<!-- trial-file: miri-alias/src/main.rs -->
````rust
//! Safe code: two live `&mut` references to the same function environment.
#![forbid(unsafe_code)]
use wasmer::{FunctionEnv, Store};

fn main() {
    let mut store = Store::default();
    let env = FunctionEnv::new(&mut store, 0u64);
    let handle = env.clone();
    let mut env_mut = env.into_mut(&mut store);
    let (first, mut store_mut) = env_mut.data_and_store_mut();
    let second: &mut u64 = handle.as_mut(&mut store_mut);
    let same = std::ptr::eq(&raw const *first, &raw const *second);
    *second = 1;
    *first = 2;
    println!("both &mut live, same address = {same}, value = {}", *second);
}
````

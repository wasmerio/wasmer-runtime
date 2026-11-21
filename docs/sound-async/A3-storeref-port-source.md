# Appendix A3: `StoreRef` port source

The runner, the source edits for `next`, and the aliasing checks. See [the
`StoreRef` trial](./06-storeref-trial.md).

The [rebuild command](./README.md#rebuild-the-trials) extracts these files.

### `run-storeref-trial.sh`

<!-- trial-file: run-storeref-trial.sh -->
````bash
#!/usr/bin/env bash
# Applies the StoreRef port to `next` in a throwaway worktree and type-checks it.
# Set NAPI_REPO to a local NAPI clone to avoid downloading the submodule.
set -euo pipefail

NEXT_SHA=fa7ec59be53260497ceb111d586967c6dbd37aef
TRIALS="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(git -C "$TRIALS" rev-parse --show-toplevel)"
WORKTREE="$TRIALS/next-storeref"
export CARGO_TARGET_DIR="${STOREREF_TARGET:-$TRIALS/next-target}"

git -C "$ROOT" cat-file -e "$NEXT_SHA^{commit}" 2>/dev/null || git -C "$ROOT" fetch origin next
git -C "$ROOT" worktree add -q --detach "$WORKTREE" "$NEXT_SHA"
trap 'git -C "$ROOT" worktree remove --force "$WORKTREE"' EXIT

pin="$(git -C "$WORKTREE" rev-parse HEAD:lib/napi)"
if [ -n "${NAPI_REPO:-}" ]; then
    mkdir -p "$WORKTREE/lib/napi"
    git -C "$NAPI_REPO" archive "$pin" | tar -x -C "$WORKTREE/lib/napi"
else
    git -C "$WORKTREE" submodule update --init lib/napi
fi

python3 -I "$TRIALS/storeref/port.py" "$WORKTREE"
cd "$WORKTREE"
git diff --shortstat -- lib/api lib/wasix lib/c-api-imports

cargo check --quiet --locked -p wasmer-wasix --features wasmer/cranelift
echo "PASS  WASIX type-checks on StoreRef"
cargo check --quiet --locked -p wasmer-c-api-imports
echo "PASS  C API imports type-check on StoreRef"

cp "$TRIALS/storeref/zz_alias.rs" lib/api/tests/zz_alias.rs
if output="$(cargo check --locked -p wasmer --features cranelift --test zz_alias --message-format=short 2>&1)"; then
    echo "FAIL  the aliasing still compiles"
    exit 1
fi
rm lib/api/tests/zz_alias.rs
count="$(grep -c 'zz_alias\.rs:.*error\[E0277\]' <<<"$output")"
if [ "$count" -ne 3 ]; then
    echo "FAIL  expected 3 E0277 errors, found $count"
    exit 1
fi
echo "PASS  re-reading T, re-writing T and calling a guest are compile errors"
````

### `storeref/port.py`

<!-- trial-file: storeref/port.py -->
````python
"""Moves the API, WASIX and C API imports on `next` onto StoreRef."""
import pathlib
import sys

ROOT = pathlib.Path(sys.argv[1])


def edit(path, *pairs):
    file = ROOT / path
    text = file.read_text()
    for old, new in pairs:
        if text.count(old) != 1:
            sys.exit(f"{path}: expected one match, found {text.count(old)}: {old[:70]!r}")
        text = text.replace(old, new)
    file.write_text(text)


edit("lib/api/src/entities/function/env/mod.rs", (
    """    pub fn data_and_store_mut(&mut self) -> (&mut T, StoreMut<'_>) {
        self.0.data_and_store_mut()
    }""",
    """    pub fn data_and_store_mut(&mut self) -> (&mut T, StoreRef<'_>) {
        let (data, store) = self.0.data_and_store_mut();
        let StoreMut { inner } = store;
        (data, StoreRef { inner: &*inner })
    }""",
), (
    "pub fn as_ref<'a>(&self, store: &'a impl AsStoreRef) -> &'a T",
    "pub fn as_ref<'a>(&self, store: &'a impl AsStoreMut) -> &'a T",
))

edit("lib/wasix/src/state/func_env.rs", (
    "pub fn data<'a>(&'a self, store: &'a impl AsStoreRef) -> &'a WasiEnv {",
    "pub fn data<'a>(&'a self, store: &'a impl wasmer::AsStoreMut) -> &'a WasiEnv {",
))

edit("lib/wasix/src/state/handles/mod.rs", (
    "use crate::{WasiEnv, state::LinkError};\n",
    """use crate::{WasiEnv, state::LinkError};

impl WasiEnv {
    /// Takes the table handle out of the environment before using the store.
    pub(crate) fn lookup_indirect_function(
        ctx: &mut wasmer::FunctionEnvMut<'_, WasiEnv>,
        index: u32,
    ) -> Result<Function, FunctionLookupError> {
        let table = ctx
            .data()
            .inner()
            .main_module_instance_handles()
            .indirect_function_table
            .clone()
            .ok_or(FunctionLookupError::NoIndirectFunctionTable)?;
        let value = table
            .get(ctx, index)
            .ok_or(FunctionLookupError::OutOfBounds(index))?;
        let Value::FuncRef(funcref) = value else {
            return Err(FunctionLookupError::NotAFunctionTable);
        };
        funcref.ok_or(FunctionLookupError::Empty(index))
    }
}
""",
))

edit("lib/wasix/src/syscalls/wasix/call_dynamic.rs", (
    """    let (env, mut store) = ctx.data_and_store_mut();

    let strict = matches!(strict, Bool::True);

    let function = wasi_try_ok!(
        env.inner()
            .indirect_function_table_lookup(&mut store, function_id)
            .map_err(Errno::from)
    );
""",
    """    let strict = matches!(strict, Bool::True);

    let function = wasi_try_ok!(
        WasiEnv::lookup_indirect_function(&mut ctx, function_id).map_err(Errno::from)
    );
    let (env, store) = ctx.data_and_store_mut();
""",
), (
    """        .call_asyncify(&mut store, &func_env, values_buffer.as_slice())
        .map_err(crate::flatten_runtime_error)?;

    let memory""",
    """        .call_asyncify(&mut ctx, &func_env, values_buffer.as_slice())
        .map_err(crate::flatten_runtime_error)?;

    let (env, store) = ctx.data_and_store_mut();
    let memory""",
))

edit("lib/wasix/src/syscalls/wasix/reflect_signature.rs", (
    """    let (env, mut store) = ctx.data_and_store_mut();

    let function_lookup_result = env
        .inner()
        .indirect_function_table_lookup(&mut store, function_id);
""",
    """    let function_lookup_result = WasiEnv::lookup_indirect_function(&mut ctx, function_id);
    let (env, store) = ctx.data_and_store_mut();
""",
))

edit("lib/wasix/src/syscalls/wasix/context_create.rs", (
    """    data: &WasiEnv,
    mut store: &mut StoreMut<'_>,
    entrypoint_id: u32,""",
    """    ctx: &mut FunctionEnvMut<'_, WasiEnv>,
    entrypoint_id: u32,""",
), (
    """    let entrypoint = match data
        .inner()
        .indirect_function_table_lookup(&mut store, entrypoint_id)
    {""",
    "    let entrypoint = match WasiEnv::lookup_indirect_function(ctx, entrypoint_id) {",
), (
    "let entrypoint_type = entrypoint.ty(&store);",
    "let entrypoint_type = entrypoint.ty(&*ctx);",
), (
    """    let (data, mut store) = ctx.data_and_store_mut();

    // Get the context-switching environment
    let Some(environment) = &data.context_switching_environment else {
        tracing::warn!(
            "The WASIX context-switching API is only available in a context-switching environment"
        );
        return Ok(Errno::Notsup);
    };

    // Lookup and check the entrypoint function
    let entrypoint = match lookup_typechecked_entrypoint(data, &mut store, entrypoint) {
        Ok(func) => func,
        Err(err) => {
            return Ok(err);
        }
    };
""",
    """    if ctx.data().context_switching_environment.is_none() {
        tracing::warn!(
            "The WASIX context-switching API is only available in a context-switching environment"
        );
        return Ok(Errno::Notsup);
    }

    // Lookup and check the entrypoint function
    let entrypoint = match lookup_typechecked_entrypoint(&mut ctx, entrypoint) {
        Ok(func) => func,
        Err(err) => {
            return Ok(err);
        }
    };

    let (data, store) = ctx.data_and_store_mut();
    let environment = data
        .context_switching_environment
        .as_ref()
        .expect("checked above");
""",
))

edit("lib/wasix/src/journal/effector/memory_and_snapshot.rs", (
    """        let (env, mut store) = ctx.data_and_store_mut();

        let (uncompressed_size, compressed_data) = block::uncompressed_size(compressed_data)
            .map_err(|err| anyhow::anyhow!("failed to decompress - {err}"))?;

        let memory = unsafe { env.memory() };
        memory.grow_at_least(&mut store, region.end + uncompressed_size as u64)?;
""",
    """        let (uncompressed_size, compressed_data) = block::uncompressed_size(compressed_data)
            .map_err(|err| anyhow::anyhow!("failed to decompress - {err}"))?;

        let memory = unsafe { ctx.data().memory() }.clone();
        memory.grow_at_least(ctx, region.end + uncompressed_size as u64)?;
        let (env, store) = ctx.data_and_store_mut();
""",
))

edit("lib/wasix/src/syscalls/mod.rs", (
    """pub(crate) fn set_memory_stack_offset(
    env: &WasiEnv,
    store: &mut impl AsStoreMut,
    offset: u64,
) -> Result<(), String> {
    // Sets the stack pointer
    let stack_upper = get_stack_upper(env);
    let stack_pointer = stack_upper - offset;
    if let Some(stack_pointer_ptr) = env
        .inner()
        .main_module_instance_handles()
        .stack_pointer
        .clone()
    {
        match stack_pointer_ptr.get(store) {
            Value::I32(_) => {
                stack_pointer_ptr.set(store, Value::I32(stack_pointer as i32));
            }
            Value::I64(_) => {
                stack_pointer_ptr.set(store, Value::I64(stack_pointer as i64));
            }""",
    """pub(crate) fn set_memory_stack_offset(
    ctx: &mut FunctionEnvMut<'_, WasiEnv>,
    offset: u64,
) -> Result<(), String> {
    // Sets the stack pointer
    let stack_upper = get_stack_upper(ctx.data());
    let stack_pointer = stack_upper - offset;
    if let Some(stack_pointer_ptr) = ctx
        .data()
        .inner()
        .main_module_instance_handles()
        .stack_pointer
        .clone()
    {
        match stack_pointer_ptr.get(ctx) {
            Value::I32(_) => {
                stack_pointer_ptr.set(ctx, Value::I32(stack_pointer as i32));
            }
            Value::I64(_) => {
                stack_pointer_ptr.set(ctx, Value::I64(stack_pointer as i64));
            }""",
), (
    """pub(crate) fn get_memory_stack<M: MemorySize>(
    env: &WasiEnv,
    store: &mut impl AsStoreMut,
) -> Result<BytesMut, String> {
    // Get the current value of the stack pointer (which we will use
    // to save all of the stack)
    let stack_base = get_stack_upper(env);
    let stack_pointer = if let Some(stack_pointer) = env
        .inner()
        .main_module_instance_handles()
        .stack_pointer
        .clone()
    {
        match stack_pointer.get(store) {
            Value::I32(a) => a as u64,
            Value::I64(a) => a as u64,
            _ => stack_base,
        }
    } else {
        return Err("failed to save stack: no __stack_pointer global".to_string());
    };
    let memory = env
        .try_memory_view(store)
""",
    """pub(crate) fn get_memory_stack<M: MemorySize>(
    ctx: &mut FunctionEnvMut<'_, WasiEnv>,
) -> Result<BytesMut, String> {
    // Get the current value of the stack pointer (which we will use
    // to save all of the stack)
    let stack_pointer = unsafe { get_memory_stack_pointer(ctx) }?;
    let (env, store) = ctx.data_and_store_mut();
    let memory = env
        .try_memory_view(&store)
""",
), (
    """pub(crate) fn set_memory_stack<M: MemorySize>(
    env: &WasiEnv,
    store: &mut impl AsStoreMut,
    stack: Bytes,
) -> Result<(), String> {
    // First we restore the memory stack
    let stack_upper = get_stack_upper(env);""",
    """pub(crate) fn set_memory_stack<M: MemorySize>(
    ctx: &mut FunctionEnvMut<'_, WasiEnv>,
    stack: Bytes,
) -> Result<(), String> {
    // First we restore the memory stack
    let (env, store) = ctx.data_and_store_mut();
    let stack_upper = get_stack_upper(env);""",
), (
    """    let memory = env
        .try_memory_view(store)
        .ok_or_else(|| "unable to set the stack pointer of the instance".to_string())?;""",
    """    let memory = env
        .try_memory_view(&store)
        .ok_or_else(|| "unable to set the stack pointer of the instance".to_string())?;""",
), (
    "    set_memory_stack_offset(env, store, stack_offset)?;",
    "    set_memory_stack_offset(ctx, stack_offset)?;",
), (
    """    let (env, mut store) = ctx.data_and_store_mut();
    let memory_stack = match get_memory_stack::<M>(env, &mut store) {""",
    "    let memory_stack = match get_memory_stack::<M>(&mut ctx) {",
), (
    """        let (env, mut store) = ctx.data_and_store_mut();
        if let Some(memory_stack) = memory_stack {
            set_memory_stack::<M>(env, &mut store, memory_stack);
        }""",
    """        if let Some(memory_stack) = memory_stack {
            set_memory_stack::<M>(ctx, memory_stack);
        }""",
))

edit("lib/c-api-imports/src/lib.rs", (
    """        let (_, mut store_ref) = env.data_and_store_mut();
        malloc_fn.call(&mut store_ref, len).ok()?""",
    "        malloc_fn.call(env, len).ok()?",
), (
    """    let (_, mut store_ref) = env.data_and_store_mut();
    let _ = free_fn.call(&mut store_ref, guest_ptr);""",
    "    let _ = free_fn.call(env, guest_ptr);",
), (
    """        let (_, mut store) = env.data_and_store_mut();
        ExternRef::new(
            &mut store,
            GuestForeign {""",
    """        ExternRef::new(
            &mut env,
            GuestForeign {""",
))
print("patched")
````

### `storeref/zz_alias.rs`

<!-- trial-file: storeref/zz_alias.rs -->
````rust
//! Must not compile once `data_and_store_mut` returns a `StoreRef`.
#![forbid(unsafe_code)]
use wasmer::{Function, FunctionEnv, FunctionEnvMut};

pub fn read_again(mut env: FunctionEnvMut<'_, u64>, handle: FunctionEnv<u64>) {
    let (first, store) = env.data_and_store_mut();
    let second: &u64 = handle.as_ref(&store);
    *first = *second + 1;
}

pub fn write_again(mut env: FunctionEnvMut<'_, u64>, handle: FunctionEnv<u64>) {
    let (first, mut store) = env.data_and_store_mut();
    let second: &mut u64 = handle.as_mut(&mut store);
    *first = 1;
    *second = 2;
}

pub fn call_guest(mut env: FunctionEnvMut<'_, u64>, function: Function) {
    let (first, mut store) = env.data_and_store_mut();
    let _ = function.call(&mut store, &[]);
    *first = 1;
}
````

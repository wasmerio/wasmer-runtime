use crate::error::update_last_error;

use super::super::store::wasm_store_t;
use super::super::types::{wasm_ref_t, wasm_table_size_t, wasm_tabletype_t};
use super::wasm_extern_t;
use wasmer_api::{Extern, RuntimeError, StoreMut, Table, Type, Value};

#[allow(non_camel_case_types)]
#[repr(C)]
#[derive(Clone)]
pub struct wasm_table_t {
    pub(crate) extern_: wasm_extern_t,
}

impl wasm_table_t {
    pub(crate) fn try_from(e: &wasm_extern_t) -> Option<&wasm_table_t> {
        match &e.inner {
            Extern::Table(_) => Some(unsafe { &*(e as *const _ as *const _) }),
            _ => None,
        }
    }
}

/// Builds the [`Value`] to store into a table slot from a caller-provided
/// reference and the table's element type. A null `init` becomes the null
/// reference of the appropriate kind.
fn init_value(init: *const wasm_ref_t, element_ty: Type) -> Value {
    if init.is_null() {
        match element_ty {
            Type::FuncRef => Value::FuncRef(None),
            _ => Value::ExternRef(None),
        }
    } else {
        // The boxed `wasm_ref_t` carries the authoritative reference value.
        unsafe { &*init }.inner.clone()
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_table_new(
    store: Option<&mut wasm_store_t>,
    table_type: Option<&wasm_tabletype_t>,
    init: *const wasm_ref_t,
) -> Option<Box<wasm_table_t>> {
    let store = store?;
    let table_type = table_type?;
    let table_type = table_type.inner()._table_type;
    let init_val = init_value(init, table_type.ty);

    let created = c_try!(store.inner.with_mut(|mut store_mut| Table::new(
        &mut store_mut,
        table_type,
        init_val
    )));
    let table = c_try!(created);

    Some(Box::new(wasm_table_t {
        extern_: wasm_extern_t::new(store.inner.clone(), table.into()),
    }))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_table_delete(_table: Option<Box<wasm_table_t>>) {}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_table_copy(
    table: Option<&wasm_table_t>,
) -> Option<Box<wasm_table_t>> {
    table.cloned().map(Box::new)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_table_size(table: Option<&wasm_table_t>) -> usize {
    let Some(table) = table else {
        update_last_error("table pointer is null");
        return 0;
    };
    let table_obj = table.extern_.table();
    c_try!(table.extern_.store.with(|store| table_obj.size(&store)); otherwise 0) as _
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_table_type(
    table: Option<&wasm_table_t>,
) -> Option<Box<wasm_tabletype_t>> {
    let Some(table) = table else {
        update_last_error("table pointer is null");
        return None;
    };
    let table_obj = table.extern_.table();
    let ty = c_try!(table.extern_.store.with(|store| table_obj.ty(&store)));
    Some(Box::new(wasm_tabletype_t::new(ty)))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_table_same(
    wasm_table1: &wasm_table_t,
    wasm_table2: &wasm_table_t,
) -> bool {
    wasm_table1.extern_.table() == wasm_table2.extern_.table()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_table_get(
    table: Option<&wasm_table_t>,
    index: wasm_table_size_t,
) -> Option<Box<wasm_ref_t>> {
    let Some(table) = table else {
        update_last_error("table pointer is null");
        return None;
    };
    let table_obj = table.extern_.table();
    let store = &table.extern_.store;
    let element = c_try!(store.with_mut(|mut store_mut| table_obj.get(&mut store_mut, index)));
    // `Table::get` returns `None` only for an out-of-bounds index; an in-bounds
    // null element is `Some(ExternRef(None))`, which boxes to a null
    // `wasm_ref_t*` below without registering an error.
    let value = c_try!(element, "table index out of bounds");
    wasm_ref_t::new(store.clone(), value)
}

/// Stores `r` at `index` of `table`, with a null `r` standing for the null
/// reference of the table's element type.
fn set_element(
    table: &Table,
    store: &mut StoreMut<'_>,
    index: wasm_table_size_t,
    r: *const wasm_ref_t,
) -> Result<(), RuntimeError> {
    let value = init_value(r, table.ty(store).ty);
    table.set(store, index, value)
}

/// Grows `table` by `delta` elements that are `init`, with a null `init`
/// standing for the null reference of the table's element type.
fn grow(
    table: &Table,
    store: &mut StoreMut<'_>,
    delta: wasm_table_size_t,
    init: *const wasm_ref_t,
) -> Result<u32, RuntimeError> {
    let init_val = init_value(init, table.ty(store).ty);
    table.grow(store, delta, init_val)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_table_set(
    table: Option<&mut wasm_table_t>,
    index: wasm_table_size_t,
    r: *mut wasm_ref_t,
) -> bool {
    let Some(table) = table else {
        update_last_error("table pointer is null");
        return false;
    };
    let table_obj = table.extern_.table();
    let store = &table.extern_.store;
    let set = c_try!(
        store.with_mut(|mut store_mut| set_element(&table_obj, &mut store_mut, index, r));
        otherwise false
    );
    c_try!(set; otherwise false);
    true
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn wasm_table_grow(
    table: &mut wasm_table_t,
    delta: wasm_table_size_t,
    init: *mut wasm_ref_t,
) -> bool {
    let table_obj = table.extern_.table();
    let store = &table.extern_.store;
    let grown = c_try!(
        store.with_mut(|mut store_mut| grow(&table_obj, &mut store_mut, delta, init));
        otherwise false
    );
    c_try!(grown; otherwise false);
    true
}

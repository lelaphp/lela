//! `ZT_G(global_weakmap)`, the request's table php's `zend_weakrefs_hash_*`
//! functions are tested on: a value kept under an object for as long as
//! the object lives. The engine's weak table is `WeakMap`'s, so the
//! request's is one `WeakMap`, made on first use as `RINIT` makes php's
//! and given up at `RSHUTDOWN` as php destroys it.

use php_ext::call::{call_method, iterate, lookup_class, new_object};
use php_ext::raising::{Thrown, fatal};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpObject, Value, ValueRef};

/// The request's map, its one owner, until [`release`] gives it up.
#[derive(Debug, Default)]
pub(crate) struct Map(Option<Owner<PhpObject>>);

/// The map, made on first use.
fn map(req: &mut Request) -> Result<PhpObject, Thrown> {
    if let Some(map) = &req.state::<crate::State>().weakmap.0 {
        return Ok(**map);
    }
    let Some(class) = lookup_class(req, b"WeakMap")? else {
        return Err(fatal(req, "InternalError: Core declares no WeakMap"));
    };
    let made = new_object(req, class)?;
    let map = *made;
    req.state::<crate::State>().weakmap.0 = Some(made);
    Ok(map)
}

/// `zend_weakrefs_hash_add`: the value kept under the object, where the
/// object has none yet.
pub(crate) fn attach(req: &mut Request, object: PhpObject, value: Value) -> Result<bool, Thrown> {
    let map = map(req)?;
    if held(req, map, object)? {
        return Ok(false);
    }
    call_method(req, &map, b"offsetSet", &[object.as_value(), value])?.give_up(req);
    Ok(true)
}

/// `zend_weakrefs_hash_del`: the object's value let go, where it has one.
pub(crate) fn remove(req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
    let map = map(req)?;
    if !held(req, map, object)? {
        return Ok(false);
    }
    call_method(req, &map, b"offsetUnset", &[object.as_value()])?.give_up(req);
    Ok(true)
}

fn held(req: &mut Request, map: PhpObject, object: PhpObject) -> Result<bool, Thrown> {
    let answer = call_method(req, &map, b"offsetExists", &[object.as_value()])?;
    let held = matches!(answer.as_ref(), ValueRef::Bool(true));
    answer.give_up(req);
    Ok(held)
}

/// `zend_array_dup` of the table: each value under its object's number,
/// where php's key is the object's address.
pub(crate) fn dump(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let map = map(req)?;
    let mut copy = PhpArray::new(req)?;
    let walk = iterate(req, &map)?;
    let copied = copy_into(req, *walk, &mut copy);
    walk.give_up(req);
    match copied {
        Ok(()) => Ok(copy),
        Err(thrown) => {
            copy.give_up(req);
            Err(thrown)
        }
    }
}

fn copy_into(
    req: &mut Request,
    walk: php_ext::call::Iter,
    copy: &mut Owner<PhpArray>,
) -> Result<(), Thrown> {
    walk.rewind(req)?;
    while walk.valid(req)? {
        let key = walk.key(req)?;
        let index = match key.as_ref() {
            ValueRef::Object(object) => i64::from(object.index()),
            _ => 0,
        };
        key.give_up(req);
        let value = walk.value(req)?;
        copy.set(req, Owner::index(index), value)?;
        walk.next(req)?;
    }
    Ok(())
}

/// `RSHUTDOWN`'s `zend_weakrefs_hash_destroy`: the map given up.
pub(crate) fn release(req: &mut Request) {
    if let Some(map) = req.state::<crate::State>().weakmap.0.take() {
        map.give_up(req);
    }
}

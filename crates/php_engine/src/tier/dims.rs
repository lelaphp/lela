//! What compiled code reaches an array's element by, where php's code
//! calls the table's own lookups (`zend_jit_fetch_dimension_address_inner`,
//! `Z/ext/opcache/jit/zend_jit_ir.c:11917-12511`): the key the handler's
//! inline path reads out of a dimension, and the element a write lays
//! down where the key is not there.

use php_heap::{Arr, Key, Zval, ZvalRef};

use crate::api::request::Request;

/// The key a dimension `key` names, as the handler's inline path reads
/// it ([`crate::ops::inline::dim_key`]): a long as itself, a string as
/// `ZEND_HANDLE_NUMERIC_STR` reads it unless it is a `constant`, which the
/// compiler wrote the integer in place of where it was one; none for
/// every other kind.
#[must_use]
pub fn dim_key(key: Zval, constant: bool) -> Option<Key> {
    crate::ops::inline::dim_key(key, constant)
}

/// The element `key` names in `array`, which compiled code holds the one
/// owner of, for a write: found, or laid down holding `null` where the
/// key is not there, as php's `zend_hash_lookup` and
/// `zend_hash_index_lookup` lay one down and the handler's inline path
/// lays one down for `BP_VAR_W`; none where the memory limit refused it,
/// whose fatal the request ends in.
pub fn lookup_element(req: &mut Request, array: Arr, key: Key) -> Option<ZvalRef> {
    (array.value_ref(key)).or_else(|| req.array_lookup(array, key).ok())
}

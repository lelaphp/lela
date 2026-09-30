//! `debug_zval_dump()`, php's `php_debug_zval_dump`
//! (`Z/ext/standard/var.c:301`): `var_dump`'s walk with each counted
//! value's refcount beside it, a string the program wrote `interned`,
//! and a list
//! `packed`.

use php_ext::abi::{PropertyPurpose, Variadic};
use php_ext::attr::php_function;
use php_ext::coerce::{Exponent, float_format};
use php_ext::raising::{Thrown, call_stack_end};
use php_ext::request::Request;
use php_ext::values::{HeldRow, Key, Laziness, RecursionGuard, Value, ValueRef};

use super::class_name;

/// `debug_zval_dump(mixed $value, mixed ...$values): void`, php's
/// `PHP_FUNCTION(debug_zval_dump)` (`Z/ext/standard/var.c:434`): each
/// argument dumped at the top level. The refcounts are the engine's, and
/// read as php's where the engine holds a value as php does. A string or
/// an array is `interned` where it is not refcounted (`Z_REFCOUNTED_P`),
/// an interned string or an immutable array, and a temporary of one
/// owner is `refcount(1)`.
///
/// ```php
/// debug_zval_dump(1, 1.5, "lit", null, true, false);
/// debug_zval_dump(array_fill(0, 2, 0), range(1, 2), str_repeat("ab", 2));
/// $s = str_repeat("x", 3); debug_zval_dump($s);
/// ```
/// ```expect
/// int(1)
/// float(1.5)
/// string(3) "lit" interned
/// NULL
/// bool(true)
/// bool(false)
/// array(2) packed refcount(1){
///   [0]=>
///   int(0)
///   [1]=>
///   int(0)
/// }
/// array(2) packed refcount(1){
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
/// }
/// string(4) "abab" refcount(1)
/// string(3) "xxx" refcount(2)
/// ```
///
/// The empty array written `[]` is php's one immutable `zend_empty_array`,
/// never counted, and an enum case's name is its constant's interned
/// name.
///
/// ```php
/// enum E { case Bar; }
/// debug_zval_dump([], E::Bar);
/// ```
/// ```expect
/// array(0) interned {
/// }
/// object(E)#1 (1) refcount(2){
///   ["name"]=>
///   string(3) "Bar" interned
/// }
/// ```
///
/// Any other array written in the source is immutable under opcache's model, so it
/// is `interned` where `php -n` counts its owners.
///
/// ```php
/// debug_zval_dump([1, 2]);
/// ```
/// ```expect regime=immutable-literals
/// array(2) packed interned {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
/// }
/// ```
///
/// `packed` is `HT_IS_PACKED`, how the table is laid out, not whether
/// its keys count from zero: a hash whose one key is 0 is not packed, and a
/// packed list with its first slot removed still is.
///
/// ```php
/// $h = ["a" => 1]; unset($h["a"]); $h[] = 2; $p = [1, 2, 3]; unset($p[0]);
/// debug_zval_dump($h, $p);
/// ```
/// ```expect
/// array(1) refcount(2){
///   [0]=>
///   int(2)
/// }
/// array(2) packed refcount(2){
///   [1]=>
///   int(2)
///   [2]=>
///   int(3)
/// }
/// ```
///
/// A class that builds its table for the reader holds one owner of each
/// row while the dump reads it, as `zend_get_properties_for` hands php's
/// dump a temporary table until `zend_release_properties`, so an
/// `SplFixedArray` that holds itself counts that row, and a string
/// element one owner more.
///
/// ```php
/// $a = new SplFixedArray(1); $a[0] = $a; debug_zval_dump($a);
/// $b = new SplFixedArray(2); $b[0] = "x" . rand(1, 1); debug_zval_dump($b);
/// ```
/// ```expect
/// object(SplFixedArray)#1 (1) refcount(4){
///   [0]=>
///   *RECURSION*
/// }
/// object(SplFixedArray)#2 (2) refcount(2){
///   [0]=>
///   string(2) "x1" refcount(2)
///   [1]=>
///   NULL
/// }
/// ```
///
/// A typed property nothing has written is a row of its own,
/// `uninitialized(T)`, left out of the count, as
/// `zval_object_property_dump` prints the slot
/// `zend_get_typed_property_info_for_slot` finds a type for; and an
/// initialised proxy shows its instance in a table of its own, which
/// holds one more owner of it while it prints
/// (`zend_lazy_object_debug_info`).
///
/// ```php
/// class T { public int $a; protected ?float $b; private string $c = "x"; }
/// $t = new T; debug_zval_dump($t);
/// $r = new ReflectionClass(T::class);
/// $p = $r->newLazyProxy(fn () => new T);
/// debug_zval_dump($p); $r->initializeLazyObject($p); debug_zval_dump($p);
/// ```
/// ```expect
/// object(T)#1 (1) refcount(2){
///   ["a"]=>
///   uninitialized(int)
///   ["b":protected]=>
///   uninitialized(?float)
///   ["c":"T":private]=>
///   string(1) "x" interned
/// }
/// lazy proxy object(T)#4 (0) refcount(2){
///   ["a"]=>
///   uninitialized(int)
///   ["b":protected]=>
///   uninitialized(?float)
///   ["c":"T":private]=>
///   uninitialized(string)
/// }
/// lazy proxy object(T)#4 (1) refcount(2){
///   ["instance"]=>
///   object(T)#5 (1) refcount(2){
///     ["a"]=>
///     uninitialized(int)
///     ["b":protected]=>
///     uninitialized(?float)
///     ["c":"T":private]=>
///     string(1) "x" interned
///   }
/// }
/// ```
#[php_function]
fn debug_zval_dump(req: &mut Request, value: Value, values: Variadic<Value>) -> Result<(), Thrown> {
    let values = std::iter::once(value).chain(values.0);
    let mut out = Vec::new();
    for value in values {
        dump(req, &mut out, &value, 1)?;
    }
    req.write(&out);
    Ok(())
}

/// `zval_object_property_dump`'s value: the row's value dumped, or for
/// a typed property nothing has written `uninitialized(T)` at the
/// indent the value would take.
fn dump_row(
    req: &mut Request,
    out: &mut Vec<u8>,
    row: &HeldRow,
    level: usize,
) -> Result<(), Thrown> {
    match row {
        HeldRow::Value(value) => dump(req, out, value, level),
        HeldRow::Uninitialized(ty) => {
            pad(out, level);
            out.extend_from_slice(format!("uninitialized({ty})\n").as_bytes());
            Ok(())
        }
    }
}

fn pad(out: &mut Vec<u8>, level: usize) {
    if level > 1 {
        out.extend(std::iter::repeat_n(b' ', level - 1));
    }
}

// One arm per type, as php's switch has them.
#[allow(clippy::too_many_lines)]
fn dump(req: &mut Request, out: &mut Vec<u8>, value: &Value, level: usize) -> Result<(), Thrown> {
    // Unguarded in php, held to the thread's floor here.
    call_stack_end(req)?;
    pad(out, level);
    match value.as_ref() {
        ValueRef::Bool(false) => out.extend_from_slice(b"bool(false)\n"),
        ValueRef::Bool(true) => out.extend_from_slice(b"bool(true)\n"),
        ValueRef::Null | ValueRef::Undef => out.extend_from_slice(b"NULL\n"),
        ValueRef::Int(n) => out.extend_from_slice(format!("int({n})\n").as_bytes()),
        ValueRef::Float(f) => {
            let text = float_format(f, req.serialize_precision(), Exponent::Upper);
            out.extend_from_slice(format!("float({text})\n").as_bytes());
        }
        ValueRef::Str(s) => {
            out.extend_from_slice(format!("string({}) \"", s.len()).as_bytes());
            out.extend_from_slice(s.as_bytes());
            if value.is_refcounted() {
                let count = value.refcount();
                out.extend_from_slice(format!("\" refcount({count})\n").as_bytes());
            } else {
                out.extend_from_slice(b"\" interned\n");
            }
        }
        ValueRef::Array(array) => {
            if array.is_recursive() {
                out.extend_from_slice(b"*RECURSION*\n");
                return Ok(());
            }
            array.protect();
            let packed = if array.is_packed() { "packed " } else { "" };
            if value.is_refcounted() {
                let count = value.refcount();
                out.extend_from_slice(
                    format!("array({}) {packed}refcount({count}){{\n", array.len()).as_bytes(),
                );
            } else {
                out.extend_from_slice(
                    format!("array({}) {packed}interned {{\n", array.len()).as_bytes(),
                );
            }
            let mut dumped = Ok(());
            for (key, element) in array.snapshot_refs() {
                out.extend(std::iter::repeat_n(b' ', level + 1));
                match key {
                    Key::Int(n) => out.extend_from_slice(format!("[{n}]=>\n").as_bytes()),
                    Key::Str(s) => {
                        out.extend_from_slice(b"[\"");
                        out.extend_from_slice(s.as_bytes());
                        out.extend_from_slice(b"\"]=>\n");
                    }
                }
                dumped = dump(req, out, &element, level + 2);
                if dumped.is_err() {
                    break;
                }
            }
            array.unprotect();
            dumped?;
            pad(out, level);
            out.extend_from_slice(b"}\n");
        }
        ValueRef::Object(object) => {
            if object.is_recursive_in(req, RecursionGuard::Debug) {
                out.extend_from_slice(b"*RECURSION*\n");
                return Ok(());
            }
            object.protect_in(req, RecursionGuard::Debug);
            // Held from here to the closing brace, as php holds the
            // table from `zend_get_properties_for` to
            // `zend_release_properties`, so the refcount printed counts
            // the owners a class's own table takes.
            let held = match object.props_held(req, PropertyPurpose::Debug) {
                Ok(held) => held,
                Err(thrown) => {
                    object.unprotect_in(req, RecursionGuard::Debug);
                    return Err(thrown);
                }
            };
            let props = held.rows().to_vec();
            // `ZSTR_VAL(class_name)` under `%s`: up to the NUL of an
            // anonymous class's name.
            let full = class_name(req, *object);
            let name = String::from_utf8_lossy(full.split(|b| *b == 0).next().unwrap_or_default())
                .into_owned();
            // `php_var_dump_object_prefix`: what a lazy object is.
            let prefix = match object.laziness() {
                Laziness::Eager => "",
                Laziness::Ghost => "lazy ghost ",
                Laziness::Proxy | Laziness::Forwarding => "lazy proxy ",
            };
            out.extend_from_slice(
                format!(
                    "{prefix}object({name})#{} ({}) refcount({}){{\n",
                    object.index(),
                    held.count(),
                    value.refcount()
                )
                .as_bytes(),
            );
            let mut dumped = Ok(());
            for (key, row) in props {
                out.extend(std::iter::repeat_n(b' ', level + 1));
                // An element a class shows among its properties is an
                // integer key, `[%ld]=>`.
                let prop = match key {
                    Key::Int(n) => {
                        out.extend_from_slice(format!("[{n}]=>\n").as_bytes());
                        dumped = dump_row(req, out, &row, level + 2);
                        if dumped.is_err() {
                            break;
                        }
                        continue;
                    }
                    Key::Str(prop) => prop,
                };
                let bytes = prop.as_bytes();
                out.push(b'[');
                match super::unmangle(bytes) {
                    (Some(b"*"), name) => {
                        out.push(b'"');
                        out.extend_from_slice(name);
                        out.extend_from_slice(b"\":protected");
                    }
                    (Some(class), name) => {
                        out.push(b'"');
                        out.extend_from_slice(name);
                        out.extend_from_slice(b"\":\"");
                        out.extend_from_slice(class.split(|b| *b == 0).next().unwrap_or_default());
                        out.extend_from_slice(b"\":private");
                    }
                    (None, _) => {
                        out.push(b'"');
                        out.extend_from_slice(bytes);
                        out.push(b'"');
                    }
                }
                out.extend_from_slice(b"]=>\n");
                dumped = dump_row(req, out, &row, level + 2);
                if dumped.is_err() {
                    break;
                }
            }
            held.release(req);
            object.unprotect_in(req, RecursionGuard::Debug);
            dumped?;
            pad(out, level);
            out.extend_from_slice(b"}\n");
        }
        ValueRef::Resource(resource) => {
            let kind = resource.type_name(req).map_or_else(
                || String::from("Unknown"),
                |k| String::from_utf8_lossy(k).into_owned(),
            );
            out.extend_from_slice(
                format!(
                    "resource({}) of type ({kind}) refcount({})\n",
                    resource.id(),
                    value.refcount()
                )
                .as_bytes(),
            );
        }
        ValueRef::Reference(cell) => {
            out.extend_from_slice(
                format!("reference refcount({}) {{\n", value.refcount()).as_bytes(),
            );
            dump(req, out, &cell.get(), level + 2)?;
            pad(out, level);
            out.extend_from_slice(b"}\n");
        }
    }
    Ok(())
}

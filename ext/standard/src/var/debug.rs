//! `debug_zval_dump()`, php's `php_debug_zval_dump`
//! (`Z/ext/standard/var.c:301`): `var_dump`'s walk with each counted
//! value's refcount beside it, a string the program wrote `interned`,
//! and a list
//! `packed`.

use php_ext::abi::{PropertyPurpose, Variadic};
use php_ext::attr::php_function;
use php_ext::coerce::{Exponent, float_format};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Key, Laziness, Value, ValueRef};

use super::class_name;

/// `debug_zval_dump(mixed $value, mixed ...$values): void`, php's
/// `PHP_FUNCTION(debug_zval_dump)` (`Z/ext/standard/var.c:434`): each
/// argument dumped at the top level. The refcounts are the engine's, and
/// read as php's where the engine holds a value as php does. The face
/// reads an interned string and an immutable array as one owner, as
/// `zend_string_refcount` does, so a value of one owner is printed as
/// php prints a string the program wrote, `interned`.
///
/// ```php
/// debug_zval_dump(1, 1.5, "lit", null, true, false);
/// ```
/// ```expect
/// int(1)
/// float(1.5)
/// string(3) "lit" interned
/// NULL
/// bool(true)
/// bool(false)
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

fn pad(out: &mut Vec<u8>, level: usize) {
    if level > 1 {
        out.extend(std::iter::repeat_n(b' ', level - 1));
    }
}

// One arm per type, as php's switch has them.
#[allow(clippy::too_many_lines)]
fn dump(req: &mut Request, out: &mut Vec<u8>, value: &Value, level: usize) -> Result<(), Thrown> {
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
            // The face reads an interned string as one owner, php's
            // `zend_string_refcount`, so one owner is taken for interned.
            let count = value.refcount();
            if count <= 1 {
                out.extend_from_slice(b"\" interned\n");
            } else {
                out.extend_from_slice(format!("\" refcount({count})\n").as_bytes());
            }
        }
        ValueRef::Array(array) => {
            if array.is_recursive() {
                out.extend_from_slice(b"*RECURSION*\n");
                return Ok(());
            }
            array.protect();
            let packed = if array.is_list() { "packed " } else { "" };
            let count = value.refcount();
            if count <= 1 {
                out.extend_from_slice(
                    format!("array({}) {packed}interned {{\n", array.len()).as_bytes(),
                );
            } else {
                out.extend_from_slice(
                    format!("array({}) {packed}refcount({count}){{\n", array.len()).as_bytes(),
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
            if object.is_recursive() {
                out.extend_from_slice(b"*RECURSION*\n");
                return Ok(());
            }
            object.protect();
            let props = object.props_for(req, PropertyPurpose::Debug)?;
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
                    props.len(),
                    value.refcount()
                )
                .as_bytes(),
            );
            let mut dumped = Ok(());
            for (key, element) in props {
                out.extend(std::iter::repeat_n(b' ', level + 1));
                // An element a class shows among its properties is an
                // integer key, `[%ld]=>`.
                let prop = match key {
                    Key::Int(n) => {
                        out.extend_from_slice(format!("[{n}]=>\n").as_bytes());
                        dumped = dump(req, out, &element, level + 2);
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
                dumped = dump(req, out, &element, level + 2);
                if dumped.is_err() {
                    break;
                }
            }
            object.unprotect();
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

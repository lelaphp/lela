//! The two natives of `array.c` that reach the caller's variables:
//! `extract`, which writes an array's elements into them, and
//! `compact`, which reads them into an array, each over the calling
//! frame's symbol table as `zend_rebuild_symbol_table` lays it down,
//! through the face's `Request::caller_var` and `set_caller_var`. A
//! variable declared and not yet written reads as one the frame has not.

use php_ext::coerce::value_name;
use php_ext::decl::{ConstExpr, FunctionDecl, Name, Parameter, ParameterDefault, Type};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpRef, PhpString, Value, ValueRef};

use super::key_copy;
use crate::output::Entry;
use crate::{
    argument_count_between, constant, declared, long_argument, parameter, string_argument,
};

/// `PHP_EXTR_*` (`Z/ext/standard/php_array.h`).
const EXTR_OVERWRITE: i64 = 0;
const EXTR_SKIP: i64 = 1;
const EXTR_PREFIX_SAME: i64 = 2;
const EXTR_PREFIX_ALL: i64 = 3;
const EXTR_PREFIX_INVALID: i64 = 4;
const EXTR_PREFIX_IF_EXISTS: i64 = 5;
const EXTR_IF_EXISTS: i64 = 6;
const EXTR_REFS: i64 = 0x100;

/// The `EXTR_*` constants, in the stub's order.
///
/// ```php
/// var_dump(EXTR_OVERWRITE, EXTR_SKIP, EXTR_PREFIX_SAME, EXTR_PREFIX_ALL, EXTR_PREFIX_INVALID, EXTR_PREFIX_IF_EXISTS, EXTR_IF_EXISTS, EXTR_REFS);
/// ```
/// ```expect
/// int(0)
/// int(1)
/// int(2)
/// int(3)
/// int(4)
/// int(5)
/// int(6)
/// int(256)
/// ```
pub(super) fn constants() -> Vec<php_ext::decl::ConstDecl> {
    vec![
        constant(b"EXTR_OVERWRITE", EXTR_OVERWRITE),
        constant(b"EXTR_SKIP", EXTR_SKIP),
        constant(b"EXTR_PREFIX_SAME", EXTR_PREFIX_SAME),
        constant(b"EXTR_PREFIX_ALL", EXTR_PREFIX_ALL),
        constant(b"EXTR_PREFIX_INVALID", EXTR_PREFIX_INVALID),
        constant(b"EXTR_PREFIX_IF_EXISTS", EXTR_PREFIX_IF_EXISTS),
        constant(b"EXTR_IF_EXISTS", EXTR_IF_EXISTS),
        constant(b"EXTR_REFS", EXTR_REFS),
    ]
}

/// The declarations, as `basic_functions.stub.php` has them.
///
/// ```php
/// echo new ReflectionFunction('extract'), new ReflectionFunction('compact');
/// ```
/// ```expect
/// Function [ <internal:standard> function extract ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> array &$array ]
///     Parameter #1 [ <optional> int $flags = EXTR_OVERWRITE ]
///     Parameter #2 [ <optional> string $prefix = "" ]
///   }
///   - Return [ int ]
/// }
/// Function [ <internal:standard> function compact ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> $var_name ]
///     Parameter #1 [ <optional> ...$var_names ]
///   }
///   - Return [ array ]
/// }
/// ```
pub(super) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            declared(
                b"extract",
                &[
                    Parameter {
                        by_ref: true,
                        prefer_ref: true,
                        promoted: false,
                        ..parameter(b"array", Type::Array)
                    },
                    Parameter {
                        default: ParameterDefault::Expr(ConstExpr::Constant(Name(
                            b"EXTR_OVERWRITE".as_slice().into(),
                        ))),
                        ..parameter(b"flags", Type::Int)
                    },
                    Parameter {
                        default: ParameterDefault::Expr(ConstExpr::Str(b"".as_slice().into())),
                        ..parameter(b"prefix", Type::String)
                    },
                ],
                Type::Int,
            ),
            extract as Entry,
        ),
        (
            declared(
                b"compact",
                &[
                    Parameter {
                        ty: None,
                        ..parameter(b"var_name", Type::Mixed)
                    },
                    Parameter {
                        ty: None,
                        variadic: true,
                        ..parameter(b"var_names", Type::Mixed)
                    },
                ],
                Type::Array,
            ),
            compact,
        ),
    ]
}

/// `php_valid_var_name`: `[a-zA-Z_\x7f-\xff][a-zA-Z0-9_\x7f-\xff]*`.
fn valid_var_name(name: &[u8]) -> bool {
    let Some((first, rest)) = name.split_first() else {
        return false;
    };
    let head = |c: u8| c.is_ascii_alphabetic() || c == b'_' || c >= 0x7f;
    head(*first) && rest.iter().all(|c| head(*c) || c.is_ascii_digit())
}

/// `php_prefix_varname` with the underscore.
fn prefixed(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    let mut out = prefix.to_vec();
    out.push(b'_');
    out.extend_from_slice(name);
    out
}

/// `ZEND_TRY_ASSIGN_COPY_EX` into the variable, or a new one: through a
/// reference the variable holds, held to the types it is typed by, else
/// in its place.
fn assign(req: &mut Request, name: &[u8], value: Value) -> Result<(), Thrown> {
    let value = value.deref();
    req.addref(&value);
    if let Some(cell) = req
        .caller_var(name)
        .and_then(|held| held.as_php_ref().copied())
    {
        return cell.set(req, value);
    }
    req.set_caller_var(name, value);
    Ok(())
}

/// The element made a reference in place (`ZVAL_MAKE_REF_EX`), and the
/// variable made that reference.
fn assign_ref(
    req: &mut Request,
    array: &mut PhpArray,
    key: &Key,
    value: Value,
    name: &[u8],
) -> Result<(), Thrown> {
    let cell = if let Some(cell) = value.as_php_ref() {
        *cell
    } else {
        req.addref(&value);
        let cell = PhpRef::new(req, value)?;
        let written = key_copy(req, key);
        array.set(req, written, cell.as_value())?;
        cell
    };
    req.addref(&cell.as_value());
    req.set_caller_var(name, cell.as_value());
    Ok(())
}

fn reassign_this(req: &mut Request) -> Thrown {
    throw(req, "Error", "Cannot re-assign $this")
}

/// `extract(array &$array, int $flags = EXTR_OVERWRITE, string $prefix = ""): int`,
/// php's `PHP_FUNCTION(extract)` (`Z/ext/standard/array.c:2576`): each
/// string-keyed element written into the caller's variable of that name,
/// as `$flags` says what to do where the variable is there already or
/// the name is no valid variable name, `$prefix` and an underscore put
/// before the name where `$flags` asks, and the count written. Under
/// `EXTR_REFS` each variable is made a reference to its element.
///
/// ```php
/// function f() { $size = "large"; $arr = ["color" => "blue", "size" => "medium", "shape" => "sphere", "1x" => 1, 5 => 2];
///   var_dump(extract($arr, EXTR_PREFIX_SAME, "wddx"), $color, $size, $shape, $wddx_size);
///   var_dump(extract($arr, EXTR_PREFIX_ALL, "p"), $p_color, $p_5);
///   var_dump(extract($arr, EXTR_SKIP), extract($arr, EXTR_PREFIX_INVALID, "q"), $q_1x, $q_5); }
/// f();
/// ```
/// ```expect
/// int(3)
/// string(4) "blue"
/// string(5) "large"
/// string(6) "sphere"
/// string(6) "medium"
/// int(5)
/// string(4) "blue"
/// int(2)
/// int(0)
/// int(5)
/// int(1)
/// int(2)
/// ```
///
/// ```php
/// function h() { $x = 1; $arr = ["x" => 5]; extract($arr, EXTR_REFS); $x = 9; var_dump($arr); var_dump(extract(["y" => 1], EXTR_IF_EXISTS), extract(["x" => 2], EXTR_IF_EXISTS), $x); }
/// h();
/// $top = 1; var_dump(extract(["top" => 2, "new" => 3]), $top, $new);
/// try { extract([], 99); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { extract([], EXTR_PREFIX_ALL); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { extract([], EXTR_PREFIX_ALL, "1"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// function t() { try { extract(["this" => 1]); } catch (Error $e) { echo $e->getMessage(), "\n"; } }
/// t();
/// ```
/// ```expect
/// array(1) {
///   ["x"]=>
///   &int(9)
/// }
/// int(0)
/// int(1)
/// int(2)
/// int(2)
/// int(2)
/// int(3)
/// extract(): Argument #2 ($flags) must be a valid extract type
/// extract(): Argument #3 ($prefix) is required when using this extract type
/// extract(): Argument #3 ($prefix) must be a valid identifier
/// Cannot re-assign $this
/// ```
// One arm per extract type, php's fourteen helpers folded into one walk.
#[allow(clippy::too_many_lines)]
fn extract(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 3)?;
    let given = req.argument(1).unwrap_or_default();
    let coerced = php_ext::coerce::coerce_param(req, &given, &Type::Array, 1)?;
    let flags = if req.argument_count() >= 2 {
        long_argument(req, 2)?
    } else {
        EXTR_OVERWRITE
    };
    let prefix = if req.argument_count() == 3 {
        Some(string_argument(req, 3)?.as_bytes().to_vec())
    } else {
        None
    };
    let refs = flags & EXTR_REFS != 0;
    let kind = flags & 0xff;
    if !(EXTR_OVERWRITE..=EXTR_IF_EXISTS).contains(&kind) {
        return Err(arg_value_error(req, 2, "be a valid extract type"));
    }
    if kind > EXTR_SKIP && kind <= EXTR_PREFIX_IF_EXISTS && req.argument_count() < 3 {
        return Err(throw(
            req,
            "ValueError",
            "extract(): Argument #3 ($prefix) is required when using this extract type",
        ));
    }
    if let Some(prefix) = &prefix
        && !prefix.is_empty()
        && !valid_var_name(prefix)
    {
        return Err(arg_value_error(req, 3, "be a valid identifier"));
    }
    let prefix = prefix.unwrap_or_default();
    let mut array = match (refs, given.as_ref()) {
        (true, ValueRef::Reference(cell)) => match cell.separate_array(req)? {
            Some(array) => array,
            None => return Ok(Value::int(0)),
        },
        _ => match coerced.deref().as_array() {
            Some(array) => *array,
            None => return Ok(Value::int(0)),
        },
    };
    let mut count = 0i64;
    for (key, entry) in array.snapshot_refs() {
        let text = match &key {
            Key::Str(s) => s.as_bytes().to_vec(),
            Key::Int(n) => n.to_string().into_bytes(),
        };
        let numeric = matches!(key, Key::Int(_));
        let target = match kind {
            EXTR_OVERWRITE
            | EXTR_IF_EXISTS
            | EXTR_SKIP
            | EXTR_PREFIX_SAME
            | EXTR_PREFIX_IF_EXISTS
                if numeric =>
            {
                continue;
            }
            EXTR_OVERWRITE => {
                if !valid_var_name(&text) {
                    continue;
                }
                if text == b"this" {
                    return Err(reassign_this(req));
                }
                if text == b"GLOBALS" && req.caller_var(&text).is_some() {
                    continue;
                }
                text
            }
            EXTR_IF_EXISTS => {
                if req.caller_var(&text).is_none() || !valid_var_name(&text) || text == b"GLOBALS" {
                    continue;
                }
                if text == b"this" {
                    return Err(reassign_this(req));
                }
                text
            }
            EXTR_SKIP => {
                if !valid_var_name(&text) || text == b"this" || req.caller_var(&text).is_some() {
                    continue;
                }
                text
            }
            EXTR_PREFIX_SAME => {
                if text.is_empty() {
                    continue;
                }
                let exists = req.caller_var(&text).is_some();
                if exists || text == b"this" {
                    if !exists && !valid_var_name(&text) {
                        continue;
                    }
                    let name = prefixed(&prefix, &text);
                    if !valid_var_name(&name) {
                        continue;
                    }
                    if name == b"this" {
                        return Err(reassign_this(req));
                    }
                    name
                } else {
                    if !valid_var_name(&text) {
                        continue;
                    }
                    text
                }
            }
            EXTR_PREFIX_IF_EXISTS => {
                if req.caller_var(&text).is_none() {
                    continue;
                }
                let name = prefixed(&prefix, &text);
                if !valid_var_name(&name) {
                    continue;
                }
                if name == b"this" {
                    return Err(reassign_this(req));
                }
                name
            }
            EXTR_PREFIX_ALL => {
                if text.is_empty() {
                    continue;
                }
                let name = prefixed(&prefix, &text);
                if !valid_var_name(&name) {
                    continue;
                }
                if name == b"this" {
                    return Err(reassign_this(req));
                }
                name
            }
            _ => {
                // EXTR_PREFIX_INVALID.
                let name = if numeric || !valid_var_name(&text) || text == b"this" {
                    let name = prefixed(&prefix, &text);
                    if !valid_var_name(&name) {
                        continue;
                    }
                    name
                } else {
                    text
                };
                if name == b"this" {
                    return Err(reassign_this(req));
                }
                name
            }
        };
        if refs {
            assign_ref(req, &mut array, &key, entry, &target)?;
        } else {
            assign(req, &target, entry)?;
        }
        count += 1;
    }
    Ok(Value::int(count))
}

/// `compact($var_name, ...$var_names): array`, php's
/// `PHP_FUNCTION(compact)` (`Z/ext/standard/array.c:2721`): each name,
/// or each name in an array of them, as a key of the caller's variable
/// of that name, a reference followed; php's warning for a name the
/// caller has no variable for and for an argument that is neither a
/// string nor an array.
///
/// ```php
/// function g() { $a = 1; $b = [1]; $r = 2; $ref = &$r; var_dump(compact('a', ['b', ['nope']], 'ref')); var_dump(compact(5)); }
/// g();
/// $top = "t"; var_dump(compact("top"));
/// ```
/// ```expect
/// Warning: compact(): Undefined variable $nope in %s on line 2
/// array(3) {
///   ["a"]=>
///   int(1)
///   ["b"]=>
///   array(1) {
///     [0]=>
///     int(1)
///   }
///   ["ref"]=>
///   int(2)
/// }
///
/// Warning: compact(): Argument #1 must be string or array of strings, int given in %s on line 2
/// array(0) {
/// }
/// array(1) {
///   ["top"]=>
///   string(1) "t"
/// }
/// ```
fn compact(req: &mut Request) -> Result<Value, Thrown> {
    if req.argument_count() < 1 {
        return Err(php_ext::raising::too_few_arguments(req, 1, 0));
    }
    let arguments = req.variadic_arguments(1)?;
    let mut out = PhpArray::new(req)?;
    for (at, argument) in arguments.iter().enumerate() {
        let position = at + 1;
        compact_var(req, &mut out, argument, position)?;
    }
    Ok(Value::array(out))
}

/// `php_compact_var`.
fn compact_var(
    req: &mut Request,
    out: &mut PhpArray,
    entry: &Value,
    position: usize,
) -> Result<(), Thrown> {
    let entry = entry.deref();
    match entry.as_ref() {
        ValueRef::Str(name) => {
            let bytes = name.as_bytes().to_vec();
            if let Some(value) = req.caller_var(&bytes) {
                let value = value.deref();
                req.addref(&value);
                let key = Key::from_string(PhpString::new(req, &bytes)?);
                out.set(req, key, value)?;
            } else if bytes != b"this" {
                let shown = String::from_utf8_lossy(&bytes).into_owned();
                docref(req, Level::Warning, &format!("Undefined variable ${shown}"));
            }
        }
        ValueRef::Array(array) => {
            if array.is_recursive() {
                return Err(throw(req, "Error", "Recursion detected"));
            }
            array.protect();
            let mut done = Ok(());
            for (_, element) in array.snapshot_refs() {
                done = compact_var(req, out, &element, position);
                if done.is_err() {
                    break;
                }
            }
            array.unprotect();
            done?;
        }
        _ => {
            let name = value_name(req, &entry);
            docref(
                req,
                Level::Warning,
                &format!("Argument #{position} must be string or array of strings, {name} given"),
            );
        }
    }
    Ok(())
}

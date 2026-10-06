//! The two natives of `array.c` that reach the caller's variables:
//! `extract`, which writes an array's elements into them, and
//! `compact`, which reads them into an array, each over the calling
//! frame's symbol table as `zend_rebuild_symbol_table` lays it down,
//! through the face's `Request::caller_var` and `set_caller_var`. A
//! variable declared and not yet written reads as one the frame has not.

use std::ops::ControlFlow;

use php_ext::abi::{Passed, Ref, Untyped, Variadic};
use php_ext::attr::php_function;
use php_ext::coerce::value_name;
use php_ext::raising::{
    Level, Thrown, arg_value_error, argument_error, call_stack_end, docref, throw_error,
};
use php_ext::request::Request;
use php_ext::values::walk::{self, Reached};
use php_ext::values::{Key, PhpArray, PhpRef, PhpString, Value, ValueRef};

use super::constants::{
    EXTR_IF_EXISTS, EXTR_OVERWRITE, EXTR_PREFIX_ALL, EXTR_PREFIX_IF_EXISTS, EXTR_PREFIX_SAME,
    EXTR_REFS, EXTR_SKIP,
};
use super::key_copy;

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
    throw_error(req, "Error", "Cannot re-assign $this")
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
///
/// The walk holds an owner of the array, as php's `ZVAL_COPY` of it
/// does, so an element written over the variable that held the array
/// leaves the elements after it standing. Under `EXTR_REFS` the
/// reference the call passed holds it, and an element made a reference
/// over that variable replaces the variable, not the reference's value.
///
/// ```php
/// function f($flags) { $k = 'kkkkk'; $v = 'vvvvv'; $z = 'ZZZZZ';
///   $arr = ['arr' => 1, $k . $k => $v . $v];
///   echo extract($arr, $flags), ','; $x = $z . $z; echo $arr, ',', $kkkkkkkkkk, "\n"; }
/// f(EXTR_OVERWRITE); f(EXTR_REFS);
/// ```
/// ```expect
/// 2,1,vvvvvvvvvv
/// 2,1,vvvvvvvvvv
/// ```
///
/// ```php
/// echo new ReflectionFunction('extract');
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
/// ```
#[php_function]
fn extract(
    req: &mut Request,
    #[php(prefer_ref)] array: Ref<PhpArray>,
    #[php(default = EXTR_OVERWRITE)] flags: i64,
    #[php(default = "")] prefix: Passed<PhpString>,
) -> Result<i64, Thrown> {
    let given = array;
    let prefix_given = matches!(prefix, Passed::Given(_));
    let prefix = match prefix {
        Passed::Given(prefix) => Some(prefix.as_bytes().to_vec()),
        Passed::Omitted => None,
    };
    let refs = flags & EXTR_REFS != 0;
    let kind = flags & 0xff;
    if !(EXTR_OVERWRITE..=EXTR_IF_EXISTS).contains(&kind) {
        return Err(arg_value_error(req, 2, "be a valid extract type"));
    }
    if kind > EXTR_SKIP && kind <= EXTR_PREFIX_IF_EXISTS && !prefix_given {
        return Err(argument_error(
            req,
            "ValueError",
            3,
            "is required when using this extract type",
        ));
    }
    if let Some(prefix) = &prefix
        && !prefix.is_empty()
        && !valid_var_name(prefix)
    {
        return Err(arg_value_error(req, 3, "be a valid identifier"));
    }
    req.forbid_dynamic_call()?;
    let prefix = prefix.unwrap_or_default();
    // `EXTR_REFS` writes references into the caller's array, separated
    // in place where the call passed it by reference.
    let separated = if refs { given.separated(req)? } else { None };
    let Some(mut array) = separated.or_else(|| given.get().copied()) else {
        return Ok(0);
    };
    if refs {
        // The reference the call passed holds the array: a variable an
        // element is made a reference over gives up the caller's
        // variable, never the reference's own value.
        let mut count = 0i64;
        for (key, entry) in array.snapshot_refs() {
            if let Some(target) = target(req, kind, &prefix, &key)? {
                assign_ref(req, &mut array, &key, entry, &target)?;
                count += 1;
            }
        }
        return Ok(count);
    }
    // `ZVAL_COPY(&array_copy, var_array_param)`: "The array might be
    // stored in a local variable that will be overwritten". The call
    // passed that variable by reference, so an element written through
    // it would give up the owner the variable held; the held walk holds
    // the array it reaches through that reference, and what writes the
    // array separates it. A temporary nobody else can reach is held by
    // the call, and no code tells the owner php takes of it apart.
    let reached = given
        .reference()
        .map_or(Reached::Held(array), |cell| Reached::Through(cell, array));
    let mut count = 0i64;
    walk::held(req, reached, |req, key, entry| {
        if let Some(target) = target(req, kind, &prefix, key)? {
            assign(req, &target, *entry)?;
            count += 1;
        }
        Ok(ControlFlow::<()>::Continue(()))
    })?;
    Ok(count)
}

/// The variable `extract` writes the element under `key` into, one arm
/// per extract type, php's fourteen `php_extract_*` helpers folded into
/// one: `None` where the type skips the element, and php's `Error` where
/// the name it makes is `this`.
fn target(
    req: &mut Request,
    kind: i64,
    prefix: &[u8],
    key: &Key,
) -> Result<Option<Vec<u8>>, Thrown> {
    let text = match key {
        Key::Str(s) => s.as_bytes().to_vec(),
        Key::Int(n) => n.to_string().into_bytes(),
    };
    let numeric = matches!(key, Key::Int(_));
    let name = match kind {
        EXTR_OVERWRITE | EXTR_IF_EXISTS | EXTR_SKIP | EXTR_PREFIX_SAME | EXTR_PREFIX_IF_EXISTS
            if numeric =>
        {
            None
        }
        EXTR_OVERWRITE => {
            if !valid_var_name(&text) || (text == b"GLOBALS" && req.caller_var(&text).is_some()) {
                None
            } else {
                Some(text)
            }
        }
        EXTR_IF_EXISTS => {
            if req.caller_var(&text).is_none() || !valid_var_name(&text) || text == b"GLOBALS" {
                None
            } else {
                Some(text)
            }
        }
        EXTR_SKIP => {
            if !valid_var_name(&text) || text == b"this" || req.caller_var(&text).is_some() {
                None
            } else {
                Some(text)
            }
        }
        EXTR_PREFIX_SAME => {
            if text.is_empty() {
                None
            } else {
                let exists = req.caller_var(&text).is_some();
                if exists || text == b"this" {
                    if !exists && !valid_var_name(&text) {
                        None
                    } else {
                        Some(prefixed(prefix, &text)).filter(|name| valid_var_name(name))
                    }
                } else {
                    Some(text).filter(|text| valid_var_name(text))
                }
            }
        }
        EXTR_PREFIX_IF_EXISTS => {
            if req.caller_var(&text).is_none() {
                None
            } else {
                Some(prefixed(prefix, &text)).filter(|name| valid_var_name(name))
            }
        }
        EXTR_PREFIX_ALL => {
            if text.is_empty() {
                None
            } else {
                Some(prefixed(prefix, &text)).filter(|name| valid_var_name(name))
            }
        }
        _ => {
            // EXTR_PREFIX_INVALID.
            if numeric || !valid_var_name(&text) || text == b"this" {
                Some(prefixed(prefix, &text)).filter(|name| valid_var_name(name))
            } else {
                Some(text)
            }
        }
    };
    match name {
        Some(name) if name == b"this" => Err(reassign_this(req)),
        name => Ok(name),
    }
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
///
/// ```php
/// echo new ReflectionFunction('compact');
/// ```
/// ```expect
/// Function [ <internal:standard> function compact ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> $var_name ]
///     Parameter #1 [ <optional> ...$var_names ]
///   }
///   - Return [ array ]
/// }
/// ```
#[php_function]
fn compact(
    req: &mut Request,
    var_name: Untyped<Value>,
    var_names: Variadic<Untyped<Value>>,
) -> Result<PhpArray, Thrown> {
    req.forbid_dynamic_call()?;
    let arguments: Vec<Value> = std::iter::once(var_name.0)
        .chain(var_names.0.into_iter().map(|name| name.0))
        .collect();
    let mut out = PhpArray::new(req)?;
    for (at, argument) in arguments.iter().enumerate() {
        let position = at + 1;
        compact_var(req, &mut out, argument, position)?;
    }
    Ok(out)
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
            } else if bytes == b"this" {
                // `zend_get_this_object`: the object the caller runs on,
                // which no symbol table holds.
                if let Some(object) = req.this_object() {
                    let value = object.as_value();
                    req.addref(&value);
                    let key = Key::from_string(PhpString::new(req, &bytes)?);
                    out.set(req, key, value)?;
                }
            } else {
                let shown = String::from_utf8_lossy(&bytes).into_owned();
                docref(req, Level::Warning, &format!("Undefined variable ${shown}"));
            }
        }
        ValueRef::Array(array) => {
            if array.is_recursive() {
                return Err(throw_error(req, "Error", "Recursion detected"));
            }
            call_stack_end(req)?;
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

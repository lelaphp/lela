//! The two natives of `array.c` that reach the caller's variables:
//! `extract`, which writes an array's elements into them, and
//! `compact`, which reads them into an array, each over the calling
//! frame's symbol table as `zend_rebuild_symbol_table` lays it down,
//! through the face's `Frames::caller_var` and `set_caller_var`. A
//! variable declared and not yet written reads as one the frame has not.

use std::borrow::Cow;
use std::ops::ControlFlow;

use php_ext::abi::{Passed, Ref, Untyped, Variadic};
use php_ext::attr::php_function;
use php_ext::coerce::value_name;
use php_ext::raising::{
    Level, Thrown, arg_value_error, argument_error, call_stack_end, docref, throw_error,
};
use php_ext::request::{Overwrite, Request};
use php_ext::values::walk::{self, GiveUp, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value, ValueRef};

use super::constants::{
    EXTR_IF_EXISTS, EXTR_OVERWRITE, EXTR_PREFIX_ALL, EXTR_PREFIX_IF_EXISTS, EXTR_PREFIX_SAME,
    EXTR_REFS, EXTR_SKIP,
};
use crate::string::KeyBytes;

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
/// in its place, the variable `null` while what it held is released
/// (`zval_ptr_safe_dtor`).
///
/// ```php
/// class D { function __destruct() { echo "dtor sees: "; var_dump($GLOBALS['x']); } }
/// $x = new D; extract(['x' => 5]);
/// var_dump($x);
/// ```
/// ```expect
/// dtor sees: NULL
/// int(5)
/// ```
fn assign(req: &mut Request, name: PhpString, value: Value) -> Result<(), Thrown> {
    let value = Owner::take(req, value.deref());
    if let Some(cell) = req
        .frames_mut()
        .caller_var(name.as_bytes())
        .and_then(|held| held.as_php_ref().copied())
    {
        return cell.set(req, value);
    }
    req.frames_mut()
        .set_caller_var(name, value, Overwrite::Nulled);
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
/// leaves the elements after it standing, and a destructor that runs as
/// a variable is overwritten counts that owner:
///
/// ```php
/// class D { function __destruct() { debug_zval_dump($GLOBALS['arr']); } }
/// $o = new D; $arr = ['o' => 1]; $arr['k'] = 'v';
/// echo extract($arr), "\n";
/// ```
/// ```expect
/// array(2) refcount(3){
///   ["o"]=>
///   int(1)
///   ["k"]=>
///   string(1) "v" interned
/// }
/// 2
/// ```
///
/// Under `EXTR_REFS` php separates the array first, before it checks
/// the flags, so a shared array is separated in the caller's variable
/// even where the flags are refused:
///
/// ```php
/// $a = ['x' => 1]; $a['y'] = 2; $b = $a;
/// try { extract($a, EXTR_REFS | 99); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// debug_zval_dump($b);
/// echo extract(array_merge(['p' => 1], ['q' => 2]), EXTR_REFS), ',', $p, $q, "\n";
/// ```
/// ```expect
/// extract(): Argument #2 ($flags) must be a valid extract type
/// array(2) refcount(2){
///   ["x"]=>
///   int(1)
///   ["y"]=>
///   int(2)
/// }
/// 2,12
/// ```
///
/// Under `EXTR_REFS` the walk holds nothing, as php's `php_extract_ref_*`
/// functions hold nothing: the reference the call passed holds the array, and an
/// element made a reference over that variable replaces the variable,
/// not the reference's value.
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
    // `SEPARATE_ARRAY(var_array_param)` (`Z/ext/standard/array.c:2594`),
    // before php checks the flags.
    let separated = if flags & EXTR_REFS != 0 {
        match Separated::of(req, &given)? {
            Some(separated) => Some(separated),
            None => return Ok(0),
        }
    } else {
        None
    };
    let kind = flags & 0xff;
    let checked = checked(req, kind, prefix_given, prefix);
    let prefix = match checked {
        Ok(prefix) => prefix,
        Err(thrown) => {
            if let Some(separated) = separated {
                separated.give_up(req);
            }
            return Err(thrown);
        }
    };
    if let Some(separated) = separated {
        return extracted_refs(req, separated, kind, &prefix);
    }
    let Some(array) = given.get().copied() else {
        return Ok(0);
    };
    let mut count = 0i64;
    // `ZVAL_COPY(&array_copy, var_array_param)`
    // (`Z/ext/standard/array.c:2649`): "The array might be stored in a
    // local variable that will be overwritten". The walk holds that
    // owner, so an element written over the variable leaves the rows
    // after it standing, given up as `zval_ptr_dtor(&array_copy)` (`:2673`).
    walk::foreach(req, array, Hold::Owner(GiveUp::Dtor), |req, row| {
        if let Some(target) = target(req, kind, &prefix, row.key())? {
            let assigned = assign(req, *target, *row.value());
            target.give_up(req);
            assigned?;
            count += 1;
        }
        Ok(ControlFlow::<()>::Continue(()))
    })?;
    Ok(count)
}

/// `extract` under `EXTR_REFS`, php's `php_extract_ref_*` helpers: the
/// array the call's reference holds walked by its buckets as their
/// `ZEND_HASH_MAP_FOREACH` walks them ([`walk::foreach`]), each row a
/// variable takes made a reference in place (`ZVAL_MAKE_REF_EX`) and the
/// variable bound to it. Overwriting a variable can serve a destructor
/// that writes the array: a row it unsets ahead is not visited, one it
/// rewrites is read as it now stands, one it appends is past the end,
/// and an array it puts in the variable is not the one walked. A
/// destructor that throws leaves the rows after it bound, as php's loop
/// tests no `EG(exception)`. A temporary passed where a reference was
/// asked for is walked in a copy of its own, which nobody reads after.
///
/// ```php
/// class D { function __destruct() { unset($GLOBALS['arr']['kkkkkkkkkk']); echo "d,"; } }
/// $o = new D; $k = 'kkkkk'; $v = 'vvvvv';
/// $arr = ['o' => 1, $k . $k => $v . $v];
/// echo extract($arr, EXTR_REFS), ',', $kkkkkkkkkk ?? 'unset', "\n";
/// class E { function __destruct() { $GLOBALS['brr']['b'] = 'new'; echo "e,"; } }
/// $p = new E; $brr = ['p' => 1, 'b' => 'old'];
/// echo extract($brr, EXTR_REFS), ',', $b, "\n";
/// ```
/// ```expect
/// d,1,unset
/// e,2,new
/// ```
///
/// A row made a reference is held by the array and by the variable, and
/// by nothing of the walk's, so a destructor its binding serves reads
/// php's count of it (#2159):
///
/// ```php
/// class D { function __destruct() { debug_zval_dump($GLOBALS['arr']); } }
/// $a = new D;
/// $arr = ['a' => str_repeat('x', 2)];
/// extract($arr, EXTR_REFS);
/// ```
/// ```expect
/// array(1) refcount(2){
///   ["a"]=>
///   reference refcount(2) {
///     string(2) "xx" refcount(1)
///   }
/// }
/// ```
///
/// ```php
/// class F { function __destruct() { $GLOBALS['keep'] = $GLOBALS['crr']; $GLOBALS['crr'] = 5; echo "f,"; } }
/// $f = new F; $crr = ['f' => 1, 'g' => 2];
/// echo extract($crr, EXTR_REFS), ',', $g, "\n";
/// class G { function __destruct() { throw new Exception('thrown'); } }
/// $h = new G; $drr = ['h' => 1, 'i' => 2];
/// try { extract($drr, EXTR_REFS); } catch (Exception $e) { echo $e->getMessage(), ',', $i, "\n"; }
/// ```
/// ```expect
/// f,2,2
/// thrown,2
/// ```
///
/// `EXTR_OVERWRITE` binds the reference before it releases what the
/// variable held, and the other flags release it first, so the
/// destructor reads the new binding under the one and itself under the
/// others:
///
/// ```php
/// class S { function __construct(public $n) {} function __destruct() { echo $this->n, ' sees ', get_debug_type($GLOBALS[$this->n]), "\n"; } }
/// $y = new S('y'); $a = ['y' => 9]; extract($a, EXTR_REFS);
/// $z = new S('z'); $b = ['z' => 9]; extract($b, EXTR_REFS | EXTR_IF_EXISTS);
/// $w = new S('p_w'); $p_w = $w; unset($w); $c = ['w' => 9]; extract($c, EXTR_REFS | EXTR_PREFIX_ALL, 'p');
/// ```
/// ```expect
/// y sees int
/// z sees S
/// p_w sees S
/// ```
fn extracted_refs(
    req: &mut Request,
    separated: Separated,
    kind: i64,
    prefix: &[u8],
) -> Result<i64, Thrown> {
    let table = separated.table;
    let mut count = 0i64;
    // `php_extract_ref_overwrite` writes the reference and releases its
    // `garbage` after; the other `php_extract_ref_*` helpers
    // `zval_ptr_dtor(orig_var)` before `ZVAL_REF`.
    let order = if kind == EXTR_OVERWRITE {
        Overwrite::After
    } else {
        Overwrite::InPlace
    };
    // `ZEND_HASH_MAP_FOREACH_STR_KEY_VAL(arr, ...)` in each of the
    // `php_extract_ref_*` helpers (`php_extract_ref_overwrite`,
    // `Z/ext/standard/array.c:1936`), which holds nothing: the
    // reference holds the table.
    let walked = walk::foreach(req, table, Hold::None, |req, row| {
        if let Some(target) = target(req, kind, prefix, row.key())? {
            let cell = match row.make_ref(req) {
                Ok(cell) => Owner::take(req, cell.as_value()),
                Err(thrown) => {
                    target.give_up(req);
                    return Err(thrown);
                }
            };
            req.frames_mut().set_caller_var(*target, cell, order);
            target.give_up(req);
            count += 1;
        }
        Ok(ControlFlow::<()>::Continue(()))
    });
    separated.give_up(req);
    walked.map(|_| count)
}

/// `extract`'s checks of its flags and prefix, php's order, and
/// `zend_forbid_dynamic_call`: the prefix to put before a name, empty
/// where none was passed.
fn checked(
    req: &mut Request,
    kind: i64,
    prefix_given: bool,
    prefix: Option<Vec<u8>>,
) -> Result<Vec<u8>, Thrown> {
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
    req.frames_mut().forbid_dynamic_call()?;
    Ok(prefix.unwrap_or_default())
}

/// The array `extract` under `EXTR_REFS` makes its rows references in,
/// after php's `SEPARATE_ARRAY`: the array the call's reference holds,
/// separated in the reference where it was shared, or a temporary as
/// the call holds it where nothing else does and otherwise a copy of it
/// the call owns, which nobody reads after.
struct Separated {
    table: PhpArray,
    /// The call's copy of a shared temporary.
    copy: Option<Owner<PhpArray>>,
}

impl Separated {
    /// `SEPARATE_ARRAY(var_array_param)`; `None` where the argument
    /// holds no array.
    fn of(req: &mut Request, given: &Ref<PhpArray>) -> Result<Option<Separated>, Thrown> {
        if let Some(reference) = given.reference() {
            return Ok(reference
                .separate_array(req)?
                .map(|table| Separated { table, copy: None }));
        }
        let Some(array) = given.get().copied() else {
            return Ok(None);
        };
        let held = array.to_value();
        if held.is_refcounted() && held.refcount() == 1 {
            return Ok(Some(Separated {
                table: array,
                copy: None,
            }));
        }
        let mut copy = Owner::take(req, array);
        if let Err(thrown) = copy.separate(req) {
            copy.give_up(req);
            return Err(thrown);
        }
        Ok(Some(Separated {
            table: *copy,
            copy: Some(copy),
        }))
    }

    /// The call's copy given up, where it made one.
    fn give_up(self, req: &mut Request) {
        if let Some(copy) = self.copy {
            copy.give_up(req);
        }
    }
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
) -> Result<Option<Owner<PhpString>>, Thrown> {
    let text: &[u8] = &KeyBytes::of(key);
    let numeric = matches!(key, Key::Int(_));
    let name = match kind {
        EXTR_OVERWRITE | EXTR_IF_EXISTS | EXTR_SKIP | EXTR_PREFIX_SAME | EXTR_PREFIX_IF_EXISTS
            if numeric =>
        {
            None
        }
        EXTR_OVERWRITE => {
            if !valid_var_name(text)
                || (text == b"GLOBALS" && req.frames_mut().caller_var(text).is_some())
            {
                None
            } else {
                Some(Cow::Borrowed(text))
            }
        }
        EXTR_IF_EXISTS => {
            if req.frames_mut().caller_var(text).is_none()
                || !valid_var_name(text)
                || text == b"GLOBALS"
            {
                None
            } else {
                Some(Cow::Borrowed(text))
            }
        }
        EXTR_SKIP => {
            if !valid_var_name(text)
                || text == b"this"
                || req.frames_mut().caller_var(text).is_some()
            {
                None
            } else {
                Some(Cow::Borrowed(text))
            }
        }
        EXTR_PREFIX_SAME => {
            if text.is_empty() {
                None
            } else {
                let exists = req.frames_mut().caller_var(text).is_some();
                if exists || text == b"this" {
                    if !exists && !valid_var_name(text) {
                        None
                    } else {
                        Some(Cow::Owned(prefixed(prefix, text))).filter(|name| valid_var_name(name))
                    }
                } else {
                    Some(Cow::Borrowed(text)).filter(|text| valid_var_name(text))
                }
            }
        }
        EXTR_PREFIX_IF_EXISTS => {
            if req.frames_mut().caller_var(text).is_none() {
                None
            } else {
                Some(Cow::Owned(prefixed(prefix, text))).filter(|name| valid_var_name(name))
            }
        }
        EXTR_PREFIX_ALL => {
            if text.is_empty() {
                None
            } else {
                Some(Cow::Owned(prefixed(prefix, text))).filter(|name| valid_var_name(name))
            }
        }
        _ => {
            // EXTR_PREFIX_INVALID.
            if numeric || !valid_var_name(text) || text == b"this" {
                Some(Cow::Owned(prefixed(prefix, text))).filter(|name| valid_var_name(name))
            } else {
                Some(Cow::Borrowed(text))
            }
        }
    };
    // The variable is named by the key itself where it is the key's
    // text, as php's `zend_hash_update_ind` keys it by `var_name`, and
    // by the string `php_prefix_varname` made otherwise.
    match (name, key) {
        (None, _) => Ok(None),
        (Some(name), _) if *name == *b"this" => Err(reassign_this(req)),
        (Some(name), Key::Str(s)) if s.as_bytes() == &*name => Ok(Some(Owner::take(req, *s))),
        (Some(name), _) => PhpString::new(req, &name).map(Some),
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
) -> Result<Owner<PhpArray>, Thrown> {
    req.frames_mut().forbid_dynamic_call()?;
    let arguments: Vec<Value> = std::iter::once(var_name.0)
        .chain(var_names.0.into_iter().map(|name| name.0))
        .collect();
    let mut out = PhpArray::new(req)?;
    for (at, argument) in arguments.iter().enumerate() {
        let position = at + 1;
        if let Err(thrown) = compact_var(req, &mut out, argument, position) {
            out.give_up(req);
            return Err(thrown);
        }
    }
    Ok(out)
}

/// `php_compact_var`.
fn compact_var(
    req: &mut Request,
    out: &mut Owner<PhpArray>,
    entry: &Value,
    position: usize,
) -> Result<(), Thrown> {
    let entry = entry.deref();
    match entry.as_ref() {
        ValueRef::Str(name) => {
            let name = *name;
            let bytes = name.as_bytes().to_vec();
            if let Some(value) = req.frames_mut().caller_var(&bytes) {
                let value = value.deref();
                let value = Owner::take(req, value);
                // `zend_hash_update` under the name the argument holds.
                let key = Owner::take(req, Key::Str(name));
                out.set(req, key, value)?;
            } else if bytes == b"this" {
                // `zend_get_this_object`: the object the caller runs on,
                // which no symbol table holds.
                if let Some(object) = req.frames().this_object() {
                    let value = object.as_value();
                    let value = Owner::take(req, value);
                    let key = Owner::take(req, Key::Str(name));
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
            // `ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(entry), value_ptr)`
            // (`Z/ext/standard/array.c:2707`), no owner of the table taken:
            // the argument holds it while an undefined variable's warning
            // reaches the program.
            let done = walk::foreach(req, *array, Hold::None, |req, row| -> walk::Step {
                compact_var(req, out, row.value(), position)?;
                Ok(ControlFlow::Continue(()))
            });
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

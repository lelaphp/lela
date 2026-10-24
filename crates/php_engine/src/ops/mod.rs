//! The kernels (design §5.8): one semantic in one function, called by the
//! handlers, by `api` and, when the compiled tier lands, by the kernel
//! list `tier.lock` names. A handler above them does operand addressing
//! and the inline pairs of [`inline`], whose arithmetic the kernels take
//! too, and nothing else, so every caller of a semantic gets the same one.
//!
//! Each kernel carries a witness, which is the manifest's
//! `witness.coverage` rule over this directory.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_unit::OperandKind;
use php_unit::decl::Bare;
use std::cmp::Ordering;
use std::ops::ControlFlow;

pub(crate) mod inline;

use inline::{Arith, Binary, Bitwise, Shift};

use php_heap::{Exponent, Number, Scalar, ZvalType, double_to_string, numeric_prefix};

use crate::api::abi::{DimensionRead, PropertiesFor, PropertyPurpose};
use crate::api::coerce::BinaryOp;
use crate::api::raising::{
    Level, Thrown, cannot_add_element, deprecated, error, illegal_container_offset, none_pending,
    throw, throw_error, type_error, warning,
};
use crate::api::request::{HookedTable, Request};
use crate::api::values::walk::{self, Table};
use crate::api::values::{
    Key, Layout, Owner, PhpArray, PhpObject, PhpString, TypeTag, Value, ValueRef,
};

/// `zval_get_string` (`Z/Zend/zend_operators.c:1066`): a value as the
/// string php converts it to, which is what `ECHO`, `print`, `.` and
/// string interpolation write. An array is php's warning and the word
/// `Array`; a resource its `Resource id #` and number; an object is
/// what its `__toString` returns, or the `Error` php throws for one
/// with none ([`object_to_string`]). The string is the
/// caller's to own and release, as `zval_get_string`'s is: a string
/// value's own with one more owner, a new one of the request's
/// otherwise, and the `Err` is the memory limit ending the request.
///
/// ```php
/// echo 1, "|", true, "|", false, "|", null, "|", "as written", "|", 2.5, "|", NAN;
/// ```
/// ```expect
/// 1|1|||as written|2.5|
/// Warning: unexpected NAN value was coerced to string in %s on line 2
/// NAN
/// ```
pub(crate) fn to_string(req: &mut Request, value: &Value) -> Result<PhpString, Thrown> {
    let value = &value.deref();
    match value.as_ref() {
        ValueRef::Str(s) => {
            req.addref(value);
            Ok(s)
        }
        // `zend_long_to_str` and `zend_double_to_str` mark what they
        // make valid UTF-8, the digits being ASCII.
        ValueRef::Int(n) => req
            .string(crate::c_number::Long::of(n).as_bytes())
            .inspect(PhpString::mark_valid_utf8),
        ValueRef::Float(f) => {
            // `zend_double_to_str`: the text, then the warning php raises
            // when the value it converted was NAN.
            let text =
                req.string(double_to_string(f, req.ini().precision(), Exponent::Upper).as_bytes())?;
            text.mark_valid_utf8();
            if f.is_nan() {
                warning(req, "unexpected NAN value was coerced to string");
            }
            Ok(text)
        }
        ValueRef::Bool(true) => req.string(b"1"),
        ValueRef::Array(_) => {
            warning(req, "Array to string conversion");
            req.string(b"Array")
        }
        ValueRef::Resource(r) => req.string(format!("Resource id #{}", r.id()).as_bytes()),
        ValueRef::Object(o) => object_to_string(req, o),
        // `false` and `null` are both the empty string; a reference was
        // followed above.
        ValueRef::Undef | ValueRef::Null | ValueRef::Bool(false) | ValueRef::Reference(_) => {
            req.string(b"")
        }
    }
}

/// `zval_get_string_func` over an object: its class's `cast_object`
/// slot asked for a string, `zend_std_cast_object_tostring`
/// (`Z/Zend/zend_object_handlers.c:2558`) at `Std`, the class's
/// `__toString` served on the object with no arguments, and what it
/// returned when that is a string, the caller's to own as the method's
/// return was. A class whose slot fails, one with no `__toString` at
/// `Std`, is php's `Error` naming the class; a `__toString` that
/// returned something else is the handler's own `Error`, unless the
/// method left a throwable pending, which stands as it is. A user
/// `__toString` is declared `: string` by the compiler and coerces its
/// return before this reads it, so the second `Error` is a native's
/// alone.
///
/// ```php
/// class B {}
/// echo new B;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Object of class B could not be converted to string in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// A native class's `__toString` is served as any native method is,
/// with the object as its receiver.
///
/// ```php
/// echo new ReflectionFunction('ctype_alpha');
/// ```
/// ```expect
/// Function [ <internal:ctype> function ctype_alpha ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> mixed $text ]
///   }
///   - Return [ bool ]
/// }
/// ```
///
/// A class whose own slot casts, as `GMP`'s `gmp_cast_object` does,
/// is read through it, and so is an argument a string parameter holds
/// under the weak rule.
///
/// ```php
/// echo gmp_init(-42), "|", strlen(gmp_pow(2, 70)), "\n";
/// ```
/// ```expect
/// -42|22
/// ```
pub(crate) fn object_to_string(
    req: &mut Request,
    object: crate::api::values::PhpObject,
) -> Result<PhpString, Thrown> {
    let class = object.class();
    let table = req.object_handlers(object.heap());
    if let Some(returned) = table.cast_object(req, object, TypeTag::String)? {
        if let Some(text) = returned.as_php_string() {
            returned.hand_on();
            return Ok(text);
        }
        returned.give_up(req);
    }
    let name = req.symbols().class_name(class);
    Err(error(
        req,
        &format!("Object of class {name} could not be converted to string"),
    ))
}

/// Where a `SWITCH_LONG`, `SWITCH_STRING` or `MATCH` sends the loop:
/// the opline its jump table names for the subject, the default target
/// in its `extended_value`, or, for the two `SWITCH_*`, the opline after
/// it, which is the `CASE` chain php falls back to for a subject of
/// another type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Switched {
    Target(u32),
    Default,
    Chain,
}

/// `ZEND_SWITCH_LONG` (`Z/Zend/zend_vm_def.h:9295`): a `long` subject,
/// a reference to one followed, looked up in the table by
/// `zend_hash_index_find`, the default where it is not there; any other
/// subject, an undefined variable among them, is left to the chain,
/// whose `IS_EQUAL` compares it as `==` does and warns where the
/// subject is undefined.
///
/// ```php
/// $a = 3;
/// switch ($a) { case 1: echo "a"; break; case 2: echo "b"; break; case 3: echo "c"; break; case 4: echo "d"; break; case 5: echo "e"; break; default: echo "f"; }
/// $a = 9;
/// switch ($a) { case 1: echo "a"; break; case 2: echo "b"; break; case 3: echo "c"; break; case 4: echo "d"; break; case 5: echo "e"; break; default: echo "f"; }
/// $a = "3";
/// switch ($a) { case 1: echo "a"; break; case 2: echo "b"; break; case 3: echo "c"; break; case 4: echo "d"; break; case 5: echo "e"; break; default: echo "f"; }
/// $a = 3.0;
/// switch ($a) { case 1: echo "a"; break; case 2: echo "b"; break; case 3: echo "c"; break; case 4: echo "d"; break; case 5: echo "e"; break; default: echo "f"; }
/// $a = 2;
/// switch ($a) { case 1: echo "a"; case 2: echo "b"; case 3: echo "c"; break; case 4: echo "d"; case 5: echo "e"; }
/// switch ($u) { case 1: echo "a"; break; case 2: echo "b"; break; case 3: echo "c"; break; case 4: echo "d"; break; case 5: echo "e"; break; default: echo "f"; }
/// ```
/// ```expect
/// cfccbc
/// Warning: Undefined variable $u in %s on line 12
///
/// Warning: Undefined variable $u in %s on line 12
///
/// Warning: Undefined variable $u in %s on line 12
///
/// Warning: Undefined variable $u in %s on line 12
///
/// Warning: Undefined variable $u in %s on line 12
/// f
/// ```
pub(crate) fn switch_long(subject: &Value, table: PhpArray) -> Switched {
    match subject.deref().as_ref() {
        ValueRef::Int(n) => found(table.get(&Key::Int(n))),
        _ => Switched::Chain,
    }
}

/// `ZEND_SWITCH_STRING` (`Z/Zend/zend_vm_def.h:9323`): a string
/// subject, a reference to one followed, looked up in the table by
/// `zend_hash_find`, its bytes and nothing else, since the compiler
/// keeps a numeric string out of the table; the default where it is not
/// there; any other subject is left to the chain.
///
/// ```php
/// $a = "y";
/// switch ($a) { case "x": echo "a"; break; case "y": echo "b"; break; default: echo "c"; }
/// $a = "z";
/// switch ($a) { case "x": echo "a"; break; case "y": echo "b"; break; default: echo "c"; }
/// $a = 0;
/// switch ($a) { case "x": echo "a"; break; case "y": echo "b"; break; default: echo "c"; }
/// $a = "1e1";
/// switch ($a) { case "x": echo "a"; break; case "10": echo "b"; break; default: echo "c"; }
/// $a = "x";
/// switch ($a) { case "x": echo "a"; case "y": echo "b"; break; default: echo "c"; }
/// ```
/// ```expect
/// bccbab
/// ```
pub(crate) fn switch_string(subject: &Value, table: PhpArray) -> Switched {
    match subject.deref().as_ref() {
        ValueRef::Str(s) => found(table.get(&Key::Str(s))),
        _ => Switched::Chain,
    }
}

/// `ZEND_MATCH` (`Z/Zend/zend_vm_def.h:9356`): a `long` subject looked
/// up by `zend_hash_index_find` and a string one by `zend_hash_find`,
/// each its own kind and nothing else, which is `===`; a reference
/// followed; every other subject the default, an undefined variable
/// among them, which the handler has warned about (`ZVAL_UNDEFINED_OP1`)
/// before it asks.
///
/// ```php
/// $a = 1;
/// echo match ($a) { 1 => "x", "s" => "y", default => "z" };
/// $a = "s";
/// echo match ($a) { 1 => "x", "s" => "y", default => "z" };
/// $a = "1";
/// echo match ($a) { 1 => "x", "s" => "y", default => "z" };
/// $a = 1.0;
/// echo match ($a) { 1 => "x", "s" => "y", default => "z" };
/// $a = 3;
/// echo match ($a) { 1, 3 => "x", "s" => "y", default => "z" };
/// echo match ($u) { 1 => "x", "s" => "y", default => "z" };
/// echo match (true) { $a > 2 => "big", default => "small" };
/// ```
/// ```expect
/// xyzzx
/// Warning: Undefined variable $u in %s on line 12
/// zbig
/// ```
pub(crate) fn match_target(subject: &Value, table: PhpArray) -> Switched {
    match subject.deref().as_ref() {
        ValueRef::Int(n) => found(table.get(&Key::Int(n))),
        ValueRef::Str(s) => found(table.get(&Key::Str(s))),
        _ => Switched::Default,
    }
}

/// `ZEND_IN_ARRAY` (`Z/Zend/zend_vm_def.h:9408`): whether the needle is
/// in the table `zend_compile_func_in_array` built, a reference followed.
/// A string is looked up by `zend_hash_find`, its bytes and nothing
/// else; under `$strict` a long by `zend_hash_index_find` and
/// anything else is not there. Without it an undefined variable,
/// `null` and `false` are the empty string's key, and any other needle
/// is compared with each key by `zend_compare`, which is `==` and may
/// call user code, `__toString` among it. An undefined variable is the
/// handler's to have warned about.
///
/// ```php
/// class S { function __toString(): string { return "s"; } }
/// $x = 'b'; $r = &$x; $i = 1;
/// var_dump(in_array($x, ['a', 'b']), in_array($i, [1, 'a'], true), in_array('1', [1], true), in_array(1.0, [1], true));
/// var_dump(in_array(false, ['', 'a']), in_array(null, ['a']), in_array(0, ['a']), in_array(true, ['a']), in_array(new S, ['s']), in_array([], ['a']));
/// var_dump(in_array($u, ['a', 'b'], true), in_array($v, ['', 'b']));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
///
/// Warning: Undefined variable $u in %s on line 6
///
/// Warning: Undefined variable $v in %s on line 6
/// bool(false)
/// bool(true)
/// ```
pub(crate) fn in_array(
    req: &mut Request,
    needle: &Value,
    table: PhpArray,
    strict: bool,
) -> Result<bool, Thrown> {
    let needle = needle.deref();
    match needle.as_ref() {
        ValueRef::Str(s) => return Ok(table.contains(&Key::Str(s))),
        ValueRef::Int(n) if strict => return Ok(table.contains(&Key::Int(n))),
        _ if strict => return Ok(false),
        ValueRef::Undef | ValueRef::Null | ValueRef::Bool(false) => {
            return Ok(table.find(b"").is_some());
        }
        _ => {}
    }
    // `ZEND_HASH_MAP_FOREACH_STR_KEY` (`Z/Zend/zend_vm_def.h`,
    // `ZEND_IN_ARRAY`) over the literal table, which `compare` may serve
    // `__toString` from: a foreach walk holding nothing, as php's.
    let found = walk::foreach(req, table, walk::Hold::None, |req, row| -> walk::Step<()> {
        let Key::Str(key) = row.key() else {
            return Ok(ControlFlow::Continue(()));
        };
        if compare(req, &needle, &Value::string(*key))? == Ordering::Equal {
            return Ok(ControlFlow::Break(()));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(found.is_some())
}

/// The target a table holds under a key, or the default where none.
fn found(target: Option<Value>) -> Switched {
    match target.and_then(|target| target.as_int()) {
        Some(target) => Switched::Target(u32::try_from(target).unwrap_or(u32::MAX)),
        None => Switched::Default,
    }
}

/// `zend_match_unhandled_error` (`Z/Zend/zend_execute.c:884`): the
/// `UnhandledMatchError` a `MATCH_ERROR` throws over the subject no arm
/// took, quoted by [`Request::zval_text`] cut at
/// `zend.exception_string_param_max_len` bytes. The subject is named by
/// its type instead, `of type` and `zend_zval_type_name`, a class for an
/// object, under `zend.exception_ignore_args`, for a string when the cut
/// is `0`, and where `zval_text` has no text: an array, an object that
/// is no enum case, a resource, and a reference, which php tests before
/// it follows one.
///
/// ```php
/// $a = 9;
/// echo match ($a) { 1 => "x", "s" => "y" };
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught UnhandledMatchError: Unhandled match case 9 in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// ```php
/// $a = "a long string\nwith more";
/// echo match ($a) { 1 => "x" };
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught UnhandledMatchError: Unhandled match case 'a long string\nw...' in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// ```php
/// $a = 1.5;
/// echo match ($a) { 1 => "x", 2 => "y" };
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught UnhandledMatchError: Unhandled match case 1.5 in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// ```php
/// $a = true;
/// echo match ($a) { 1 => "x", 2 => "y" };
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught UnhandledMatchError: Unhandled match case true in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
///
/// An undefined subject is warned about at the `MATCH` and is `NULL`
/// at the `MATCH_ERROR`, which reads the CV as it stands.
///
/// ```php
/// echo match ($u) { 1 => "x", 2 => "y" };
/// ```
/// ```expect exit=255
/// Warning: Undefined variable $u in %s on line 2
///
/// Fatal error: Uncaught UnhandledMatchError: Unhandled match case NULL in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// A double keeps its `.0` and an enum case is `Enum::Case`, as a
/// trace's argument is; a subject that is a reference is named by the
/// type it refers to.
///
/// ```php
/// enum E { case A; }
/// $a = 5; $r = &$a;
/// foreach ([5.0, E::A, "long enough to be cut"] as $v) { try { echo match ($v) { 1 => 'x' }; } catch (UnhandledMatchError $e) { echo $e->getMessage(), "\n"; } }
/// try { echo match ($a) { 1 => 'x' }; } catch (UnhandledMatchError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Unhandled match case 5.0
/// Unhandled match case E::A
/// Unhandled match case 'long enough to ...'
/// Unhandled match case of type int
/// ```
///
/// Under `zend.exception_ignore_args` every subject is named by its
/// type, and so is a string when the cut is `0`.
///
/// ```php
/// ini_set('zend.exception_ignore_args', '1');
/// class Beep {}
/// foreach ([null, 5.0, "foo", [1], new Beep] as $v) { try { echo match ($v) { 1 => 'x' }; } catch (UnhandledMatchError $e) { echo $e->getMessage(), "\n"; } }
/// ini_set('zend.exception_ignore_args', '0'); ini_set('zend.exception_string_param_max_len', '0');
/// try { echo match ("foo") { 1 => 'x' }; } catch (UnhandledMatchError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Unhandled match case of type null
/// Unhandled match case of type float
/// Unhandled match case of type string
/// Unhandled match case of type array
/// Unhandled match case of type Beep
/// Unhandled match case of type string
/// ```
pub(crate) fn match_unhandled_error(req: &mut Request, subject: &Value) -> Thrown {
    let cut = req.exception_string_param_max_len();
    let quoted = if req.exception_ignore_args() || (subject.tag() == TypeTag::String && cut == 0) {
        None
    } else {
        req.zval_text(subject, cut)
    };
    let text = quoted.unwrap_or_else(|| format!("of type {}", type_name(req, subject)));
    throw(
        req,
        "UnhandledMatchError",
        &format!("Unhandled match case {text}"),
    )
}

/// `zend_zval_type_name` (`Z/Zend/zend_API.c:174`): `ZVAL_DEREF`, then
/// a class for an object, `null` for an undefined value, and
/// `zend_get_type_by_const`'s word for the rest, which is `resource`
/// for a closed resource as for an open one. What `UnhandledMatchError`
/// names a subject that is no scalar as.
///
/// ```php
/// class classA {} try { echo match (new classA) { 1 => 'one' }; } catch (UnhandledMatchError $e) { echo $e->getMessage(), "\n"; } $f = fopen('php://memory', 'r'); fclose($f); try { echo match ($f) { 1 => 'one' }; } catch (UnhandledMatchError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Unhandled match case of type classA
/// Unhandled match case of type resource
/// ```
pub(crate) fn type_name(req: &Request, value: &Value) -> String {
    let value = value.deref();
    match value.as_ref() {
        ValueRef::Object(o) => req.symbols().class_decl(o.class()).map_or_else(
            || String::from("object"),
            |decl| String::from_utf8_lossy(&decl.name.0).into_owned(),
        ),
        ValueRef::Undef => String::from("null"),
        ValueRef::Resource(_) => String::from("resource"),
        _ => String::from(value.type_name()),
    }
}

/// `zend_is_true` (`Z/Zend/zend_operators.h:409`, `i_zend_is_true`): a
/// value as a condition reads it, which is what `JMPNZ` and its kin
/// test and what a bool parameter coerces to. `null`, `false`, `0`,
/// `0.0`, the empty string, `"0"` and the empty array are false and
/// everything else is true; `"00"` and `" "` among the strings, and
/// `NAN` among the doubles, with the warning 8.5 raises for a NAN
/// coerced to bool. An object is true unless its handlers say
/// otherwise, and no class here has handlers of its own yet.
///
/// Each false value ends a loop before its body runs, and a `while`
/// tests it where php's compiler leaves it, at the `JMPNZ`; a true one
/// would go on until the execution timer, which is the witness beside
/// [`crate::interrupt::interrupted`].
///
/// ```php
/// while (0) echo 'a'; while (0.0) echo 'b'; while ('') echo 'c'; while ('0') echo 'd'; while (null) echo 'e'; while (false) echo 'f'; while (extension_loaded('nope')) echo 'g'; for (; 1, 0;) echo 'h';
/// echo 'none ran';
/// ```
/// ```expect
/// none ran
/// ```
///
/// A NAN is true, so `memory_get_usage(NAN)` reads the same number as
/// `memory_get_usage(true)`, and it warns on the way.
///
/// ```php
/// var_dump(memory_get_usage(NAN) - memory_get_usage(true));
/// ```
/// ```expect
/// Warning: unexpected NAN value was coerced to bool in %s on line 2
/// int(0)
/// ```
pub(crate) fn is_true(req: &mut Request, value: &Value) -> Result<bool, Thrown> {
    Ok(match value.deref().as_ref() {
        ValueRef::Undef | ValueRef::Null | ValueRef::Bool(false) => false,
        ValueRef::Bool(true) => true,
        ValueRef::Object(o) => return object_is_true(req, o),
        ValueRef::Int(n) => n != 0,
        ValueRef::Float(f) => {
            if f.is_nan() {
                warning(req, "unexpected NAN value was coerced to bool");
                left_by_fatal(req)?;
            }
            f != 0.0
        }
        ValueRef::Str(s) => Scalar::Str(s.as_bytes()).is_true(),
        ValueRef::Array(a) => !a.is_empty(),
        ValueRef::Resource(r) => r.id() != 0,
        // `deref` followed the reference already.
        ValueRef::Reference(r) => return is_true(req, &r.get()),
    })
}

/// `zend_bailout` from a diagnostic a truth raised: the fatal the
/// built-in handler served, or one a user handler raised, longjmps out
/// of whoever asked, so the asker is handed the `Thrown` that carries no
/// throwable. A throwable a handler raised stays pending, php's
/// `EG(exception)`, and the asker goes on as php's does.
fn left_by_fatal(req: &Request) -> Result<(), Thrown> {
    if req.diagnostics().bailing_out() {
        return Err(Thrown::of(None));
    }
    Ok(())
}

/// `zend_object_is_true` (`Z/Zend/zend_operators.c:2911`): the class's
/// `cast_object` to `_IS_BOOL`, which is `true` at `Std`, whether the
/// number is not zero for `BcMath\Number`, and whether the element exists
/// for `SimpleXMLElement`; php's `E_RECOVERABLE_ERROR` and `false` for a
/// class whose handler refuses the cast, and a throw from the handler left
/// pending, as php's `EG(exception)`. The `E_RECOVERABLE_ERROR` is
/// `zend_error`'s: where no user error handler takes it, `php_error_cb`
/// bails out of it as of every fatal level, so the request ends there,
/// and the asker, a handler of the loop or a native, is handed the
/// fatal as its `Err` and leaves by it, as `zend_bailout` longjmps out
/// of both; a handler that takes it lets the request go on with
/// `false`. `ext/zend_test`'s `LongCastableNoOperations`, whose handler
/// casts to an int alone, is the witness.
///
/// ```php
/// var_dump((bool) new stdClass, (bool) new BcMath\Number('0.0'), !new BcMath\Number('-1'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
///
/// ```php
/// $e = simplexml_load_string('<r><a/></r>');
/// var_dump((bool) $e->a, (bool) $e->b, !$e->b, $e->b ? 'y' : 'n', (bool) new stdClass);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// string(1) "n"
/// bool(true)
/// ```
fn object_is_true(req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
    let table = req.object_handlers(object.heap());
    match table.cast_object(req, object, TypeTag::Bool) {
        Ok(Some(value)) => {
            let truth = value.as_bool() == Some(true);
            value.give_up(req);
            Ok(truth)
        }
        Ok(None) => {
            let class = req.symbols().class_name(object.class());
            let message = format!("Object of class {class} could not be converted to bool");
            if req.note_error(Level::RecoverableError, &message) {
                req.bail_out();
            }
            left_by_fatal(req)?;
            Ok(false)
        }
        Err(thrown) => {
            req.leave_pending(&thrown);
            left_by_fatal(req)?;
            Ok(false)
        }
    }
}

/// `convert_object_to_type` (`Z/Zend/zend_operators.c:232`) over one
/// object: its class's `cast_object` to `to`, and php's warning naming
/// `shown` where the class answers none; a throw is left pending, as
/// php leaves `EG(exception)` set.
fn object_converted(
    req: &mut Request,
    object: PhpObject,
    to: TypeTag,
    shown: &str,
) -> Option<Value> {
    let table = req.object_handlers(object.heap());
    match table.cast_object(req, object, to) {
        Ok(Some(converted)) => Some(converted.hand_on()),
        Ok(None) => {
            not_converted(req, &object.as_value(), shown);
            None
        }
        Err(thrown) => {
            req.leave_pending(&thrown);
            None
        }
    }
}

/// `ZEND_ISSET_ISEMPTY_CV`'s `isset` arm (`Z/Zend/zend_vm_def.h:7465`):
/// whether the slot holds more than `null`, which a slot nothing wrote
/// does not (`Z_TYPE_P(value) > IS_NULL`), with a reference followed one
/// step, so a name bound to a `null` is as unset as the `null` itself
/// (`!Z_ISREF_P(value) || Z_TYPE_P(Z_REFVAL_P(value)) != IS_NULL`).
/// Nothing is read into anything and no warning is raised: `isset` over
/// a variable nothing wrote is the one read of a CV php leaves silent.
///
/// ```php
/// $a = 1; $n = null; $s = ""; $f = false; $z = "0";
/// var_dump(isset($a), isset($n), isset($s), isset($f), isset($z), isset($u));
/// $r = &$n; var_dump(isset($r)); $r = 1; var_dump(isset($n));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// ```
pub(crate) fn is_set(value: &Value) -> bool {
    match value.as_ref() {
        ValueRef::Undef | ValueRef::Null => false,
        ValueRef::Reference(r) => r.get().tag() != TypeTag::Null,
        _ => true,
    }
}

/// What `ZEND_CAST` converts to, the five arms of its handler
/// (`Z/Zend/zend_vm_def.h:6495`), named by the type constant the
/// opline's `extended_value` carries. `(bool)` is `ZEND_BOOL` and never
/// reaches here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CastTo {
    /// `IS_LONG`, `zval_get_long`.
    Long,
    /// `IS_DOUBLE`, `zval_get_double`.
    Double,
    /// `IS_STRING`, `zval_get_string`.
    Str,
    /// `IS_ARRAY`, `zend_cast_zval_to_array`.
    Array,
    /// `IS_OBJECT`, `zend_cast_zval_to_object`.
    Object,
}

impl CastTo {
    /// The arm for php's type constant, nothing for one that is not a
    /// cast's.
    pub(crate) fn of(code: u32) -> Option<CastTo> {
        match code {
            4 => Some(CastTo::Long),
            5 => Some(CastTo::Double),
            6 => Some(CastTo::Str),
            7 => Some(CastTo::Array),
            8 => Some(CastTo::Object),
            _ => None,
        }
    }
}

/// `ZEND_CAST` (`Z/Zend/zend_vm_def.h:6495`): a value as `(int)`,
/// `(float)` or `(string)` read it, which is `zval_get_long` with
/// `is_strict` off, `zval_get_double` and [`to_string`]. The cast is the
/// silent reading: a string is the number it opens with or zero with no
/// word about what follows, and a float is truncated with no deprecation
/// where it lost its fraction; what does speak is the warning
/// `zend_dval_to_lval` raises for a float past the long's range or not
/// finite, whose long is then the modular one, and the warning
/// `convert_object_to_type` raises for an object, whose number is 1. A
/// float-string past the range saturates instead (`zend_dval_to_lval_cap`),
/// which is `strtol`'s edge kept. An array is 1 or 0 by whether it holds
/// anything. Nothing is folded at compile time, so each cast below is
/// the opcode.
///
/// ```php
/// echo (int)"12abc", "|", (int)" 1e3 ", "|", (int)"9223372036854775808", "|", (int)"abc", "|", (int)1.9, "|", (int)-1.9, "|", (int)true, "|", (int)null, "|", (float)"1.5x", "|", (float)"abc", "|", (float)3, "|", (string)1.0, "|", (string)false, "|", (int)1e30, "|", (int)NAN;
/// ```
/// ```expect
/// 12|1000|9223372036854775807|0|1|-1|1|0|1.5|0|3|1||
/// Warning: The float 1.0E+30 is not representable as an int, cast occurred in %s on line 2
/// 5076964154930102272|
/// Warning: The float NAN is not representable as an int, cast occurred in %s on line 2
/// 0
/// ```
///
/// `(array)` and `(object)` are the handler's other two arms,
/// [`to_array`] and [`to_object`], each with the witness beside it.
pub(crate) fn cast(req: &mut Request, value: &Value, to: CastTo) -> Result<Value, Thrown> {
    match to {
        CastTo::Long => Ok(Value::int(get_long(req, value))),
        CastTo::Double => Ok(Value::float(get_double(req, value))),
        CastTo::Str => to_string(req, value).map(Value::string),
        CastTo::Array => to_array(req, value).map(Value::array),
        CastTo::Object => to_object(req, value).map(Value::object),
    }
}

/// `zend_cast_zval_to_array` (`Z/Zend/zend_execute.h:250`) under the
/// `Z_TYPE_P(expr) == IS_ARRAY` arm of `ZEND_CAST` above it: an array
/// is itself with one more owner (`ZVAL_COPY_VALUE`, `Z_ADDREF_P`);
/// `null` is the process's empty array (`ZVAL_EMPTY_ARRAY`); a scalar, a resource and a `Closure` are
/// the one-element array holding it at `0` with one more owner, a NAN
/// warning on the way (`zend_nan_coerced_to_type_warning`); and any
/// other object is its properties as `zend_get_properties_for` under
/// `ZEND_PROP_PURPOSE_ARRAY_CAST` reads them, the class's
/// `get_properties_for` first and `get_properties` where that slot is
/// null, each declared name mangled as `zend_declare_typed_property`
/// mangles it (`\0*\0p` for a protected one, `\0C\0p` for a private
/// one) and a numeric name an integer key (`zend_proptable_to_symtable`,
/// `ZEND_HANDLE_NUMERIC`). The array handed back is the caller's to
/// own, and the `Err` is the memory limit ending the request.
///
/// ```php
/// class A { public $a = 1; protected $b = 2; private $c = 3; public $u; }
/// $x = token_get_all("1");
/// var_dump((array)null, (array)1.5, (array)"s", (array)$x, (array)new A, (array)NAN);
/// ```
/// ```expect
/// Warning: unexpected NAN value was coerced to array in %s on line 4
/// array(0) {
/// }
/// array(1) {
///   [0]=>
///   float(1.5)
/// }
/// array(1) {
///   [0]=>
///   string(1) "s"
/// }
/// array(1) {
///   [0]=>
///   array(3) {
///     [0]=>
///     int(267)
///     [1]=>
///     string(1) "1"
///     [2]=>
///     int(1)
///   }
/// }
/// array(4) {
///   ["a"]=>
///   int(1)
///   ["%0*%0b"]=>
///   int(2)
///   ["%0A%0c"]=>
///   int(3)
///   ["u"]=>
///   NULL
/// }
/// array(1) {
///   [0]=>
///   float(NAN)
/// }
/// ```
///
/// An object whose table nothing has laid down is cast without laying
/// it (`zend_std_build_object_properties_array`), and one whose table a
/// reader laid down is cast from that table, as php casts each:
///
/// ```php
/// class K { public $d = 1; }
/// $k = new K; $m = memory_get_usage(); $c = (array) $k; echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $v = var_export($k, true); $c2 = (array) $k; echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 376
/// 816
/// ```
pub(crate) fn to_array(req: &mut Request, value: &Value) -> Result<PhpArray, Thrown> {
    let value = value.deref();
    match value.as_ref() {
        ValueRef::Undef | ValueRef::Null => Ok(PhpArray::empty()),
        ValueRef::Array(a) => {
            req.addref(&value);
            Ok(a)
        }
        ValueRef::Object(o) if req.class_named(Bare::resolved(b"Closure")) != Some(o.class()) => {
            // `zend_std_build_object_properties_array`, the cast's walk
            // that rebuilds no table (`Z/Zend/zend_execute.h:267`).
            if let Some(slots) = o.slots_without_table(req)? {
                return mangled_array(req, o, slots);
            }
            // `ZEND_PROP_PURPOSE_ARRAY_CAST`, `zend_get_properties_no_lazy_init`
            // under a class that answers no table of its own: a lazy
            // object is cast as it stands.
            match req.properties_answered(o, PropertyPurpose::ArrayCast)? {
                PropertiesFor::Table(table) => match object_symtable(req, o)? {
                    Some(array) => Ok(array),
                    None => mangled_array(req, o, table),
                },
                PropertiesFor::Temporary(table)
                | PropertiesFor::Hooked(HookedTable { table, .. }) => {
                    let made = mangled_array(req, o, Table::Array(*table));
                    table.give_up_nogc(req);
                    made
                }
                PropertiesFor::Debug(info) => {
                    let made = mangled_array(req, o, Table::Array(*info.table));
                    info.table.give_up_nogc(req);
                    match info.thrown {
                        Some(thrown) => Err(thrown),
                        None => made,
                    }
                }
                // `zend_get_properties_for` answering `NULL`:
                // `ZVAL_EMPTY_ARRAY` (`Z/Zend/zend_execute.h:280`).
                PropertiesFor::Null => Ok(PhpArray::empty()),
            }
        }
        _ => {
            if value.as_float().is_some_and(f64::is_nan) {
                warning(req, "unexpected NAN value was coerced to array");
            }
            let mut array = req.array()?;
            array.append(req, Owner::take(req, value))?;
            Ok(array)
        }
    }
}

/// `zend_proptable_to_symtable` over an object's rows: a new array of
/// the request's whose keys are the rows' names, a declared one mangled
/// by `zend_mangle_property_name` and a dynamic one read as a symtable
/// key, so `"1"` is `1`, each value with one more owner. What the
/// `(array)` cast makes of what `get_properties_for` answered, and what
/// `get_mangled_object_vars()` makes of the table itself, which asks no
/// purpose and so reads a `Closure`'s empty table where the cast wraps
/// the closure.
///
/// ```php
/// class P { private $p = 1; protected $q = 2; }
/// class C extends P { public $p = 3; }
/// var_dump(get_mangled_object_vars(new C), get_mangled_object_vars((object)['1' => 4]), get_mangled_object_vars(fn () => 1));
/// ```
/// ```expect
/// array(3) {
///   ["%0P%0p"]=>
///   int(1)
///   ["%0*%0q"]=>
///   int(2)
///   ["p"]=>
///   int(3)
/// }
/// array(1) {
///   [1]=>
///   int(4)
/// }
/// array(0) {
/// }
/// ```
///
/// A declared row is keyed by `prop_info->name`, the mangled name the
/// class's compile interned as it declared the property
/// ([`php_compile::named`]), so a cast costs the table it
/// lays down and makes no string, the first as every later one; a
/// subclass's private row of the same name is keyed by its own class:
///
/// ```php
/// class A { protected $pp = 1; private $qq = 2; public $rrr = 3; }
/// $o = new A;
/// $m = memory_get_usage(); $x = (array) $o; echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $y = (array) $o; echo memory_get_usage() - $m, "\n";
/// class B extends A { private $qq = 4; }
/// $b = new B;
/// $m = memory_get_usage(); $z = get_mangled_object_vars($b); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 376
/// 376
/// 752
/// ```
///
/// php hands the object's own table back where it can
/// ([`object_symtable`]), and copies it where the class declares a
/// property, has handlers of its own or is inside a walk of itself; a
/// copy holds each row as [`Request::dup_value`] holds an element, so
/// the cast of a property bound by reference once is no reference:
///
/// ```php
/// class Foo { public $foo; }
/// $o = new Foo;
/// (function ($p, $o) { foreach ($p as $n => &$v) { $o->$n = &$v; } })(['foo' => 1], $o);
/// $a = (array) $o;
/// $o->foo = 2;
/// var_dump(ReflectionReference::fromArrayElement($a, 'foo'), $a['foo']);
/// ```
/// ```expect
/// NULL
/// int(1)
/// ```
///
/// A dynamic row named as a parent's private slot nothing holds stays
/// unmangled, since the slot is no row:
///
/// ```php
/// class B { private $s = 0; function __construct() { unset($this->s); } }
/// #[AllowDynamicProperties] class K extends B {}
/// $o = new K; $o->s = 1;
/// foreach ((array) $o as $k => $v) echo str_replace("\0", '\0', $k), "=$v\n";
/// ```
/// ```expect
/// s=1
/// ```
///
/// The array is laid down at the size php's is, never grown from eight
/// while it fills ([`symtable_laid`]): from the slots alone where no
/// table stands, `zend_new_array(ce->default_properties_count)`, and
/// otherwise at the size of the table php duplicates, so
/// `memory_get_peak_usage()` reads no table the copy outgrew:
///
/// ```php
/// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); echo memory_get_peak_usage() - $s, ' ', memory_get_usage() - $s, "\n"; }
/// class Nine { public $a=1; public $b=1; public $c=1; public $d=1; public $e=1; public $f=1; public $g=1; public $h=1; public $i=1; }
/// $n = new Nine;
/// pk(fn () => (array) $n);
/// pk(fn () => get_mangled_object_vars($n));
/// $i = new DateInterval('P1D'); get_object_vars($i);
/// pk(fn () => (array) $i);
/// #[AllowDynamicProperties] class Dyn { public $a = 1; }
/// $d = new Dyn; foreach (range(1, 9) as $k) { $d->{"p$k"} = $k; }
/// unset($d->p1, $d->p2, $d->p3, $d->p4, $d->p5);
/// pk(fn () => (array) $d);
/// ```
/// ```expect
/// 696 728
/// 696 696
/// 696 696
/// 696 696
/// ```
pub(crate) fn mangled_array(
    req: &mut Request,
    object: PhpObject,
    table: Table,
) -> Result<PhpArray, Thrown> {
    let o = object.heap();
    let eager = o.laziness() == php_heap::Laziness::Eager;
    // A table that holds copies of the declared rows holds them under
    // the names php's does, so nothing of it is declared.
    let declared = o.declared_rows() == php_heap::DeclaredRows::Slots
        && req.placed(object.class()).any(|(slot, _, _)| {
            let i = u32::try_from(slot).unwrap_or(u32::MAX);
            !eager || i >= o.slots() || !o.slot(i).is_undef()
        });
    let source = o.properties();
    let duplicate = declared
        || req.own_handlers(object.class()).is_some()
        || source.is_some_and(|table| table.is_recursive());
    // What php lays the array down from: the slots alone where no table
    // stands (`zend_std_build_object_properties_array`), and otherwise
    // the table `zend_proptable_to_symtable` copies, with its element
    // count, every declared row among them.
    let laid_from = match &table {
        Table::Object(rows) if rows.owner().heap().properties().is_none() => None,
        Table::Object(rows) => Some((rows.dynamic(), rows.positions().len())),
        Table::Array(rows) => Some((*rows, u32::try_from(rows.len()).unwrap_or(u32::MAX))),
    };
    // A declared row's mangled name is no number; a dynamic one is read
    // as a symbol table reads it, so `"1"` is `1`, and a table with one
    // that reads so is the one php converts.
    let mut convert = false;
    let rows: Vec<(Key, Value)> = walk::rows(req, table)
        .map(|(key, entry)| match key {
            Key::Int(n) => (Key::Int(n), entry),
            Key::Str(name) => {
                let key = Key::lookup(name);
                convert |= matches!(key, Key::Int(_));
                (key, entry)
            }
        })
        .collect();
    let mut array = match laid_from {
        // `zend_new_array(ce->default_properties_count)`, laid down at
        // once where there is a slot (`zend_hash_real_init_mixed`).
        None => {
            let slots = o.slots();
            req.array_sized(slots, slots == 0)?
        }
        Some((source, elements)) => symtable_laid(req, source, elements, convert)?.hand_on(),
    };
    for (key, entry) in rows {
        // The table php hands on as it stands holds each row as it is;
        // a copy of it holds each as its copy does.
        let entry = if duplicate {
            req.dup_value(entry, source)
        } else {
            req.addref(&entry);
            entry
        };
        let entry = Owner::adopt(entry);
        array.set(req, Owner::take(req, key), entry)?;
    }
    Ok(array)
}

/// The array `zend_proptable_to_symtable` (`Z/Zend/zend_hash.c:3396`)
/// makes of an object's table `source`, laid down empty for a caller
/// that fills it row by row, at the size php's is: `zend_new_array` of
/// `elements`, php's `zend_hash_num_elements` of the source, where a
/// name reads as an integer (`convert`), and otherwise
/// `zend_array_dup`'s table, the source's own size and layout, or the
/// uninitialised one where the source holds nothing. So a copy of nine
/// or more rows is never grown from eight while it fills, and
/// `memory_get_peak_usage()` never reads the table it outgrew. A table
/// with a name that reads as an integer is converted at its element
/// count, and one rows were deleted from is copied at the size it grew
/// to:
///
/// ```php
/// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); echo memory_get_peak_usage() - $s, ' ', memory_get_usage() - $s, "\n"; }
/// $i = new DateInterval('P1D'); @$i->{'7'} = 0;
/// pk(fn () => (array) $i);
/// pk(fn () => get_object_vars($i));
/// #[AllowDynamicProperties] class Dyn { public $a = 1; }
/// $d = new Dyn; foreach (range(1, 20) as $k) { $d->{"p$k"} = $k; }
/// foreach (range(1, 15) as $k) { unset($d->{"p$k"}); }
/// pk(fn () => get_mangled_object_vars($d));
/// ```
/// ```expect
/// 1016 728
/// 1016 696
/// 1336 1336
/// ```
pub(crate) fn symtable_laid(
    req: &mut Request,
    source: PhpArray,
    elements: u32,
    convert: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    if convert {
        return PhpArray::init_size(req, elements);
    }
    if elements == 0 {
        return PhpArray::new(req);
    }
    let source = source.heap();
    let layout = if source.is_packed() {
        Layout::Packed
    } else {
        Layout::Hash
    };
    PhpArray::sized(req, source.table_size(), layout)
}

/// `zend_proptable_to_symtable` over the object's own table where php
/// hands that table on (`Z/Zend/zend_execute.h:274`,
/// `Z/Zend/zend_builtin_functions.c:855`, `:939`): an object of no lazy
/// kind whose class declares no property and answers the standard
/// `get_properties`, with a table no walk of itself is inside
/// (`GC_IS_RECURSIVE`). The table itself with one more owner, so the
/// `(array)` cast, `get_object_vars()` and `get_mangled_object_vars()`
/// cost what php's do; a copy of it where the class's handlers are its
/// own (`always_duplicate`), and a converted one where a name reads as
/// an integer ([`Request::proptable_to_symtable`]). `None` where the
/// table is not all php's would hold, which [`mangled_array`] builds
/// from the rows.
///
/// ```php
/// $o = new stdClass; $o->a = str_repeat('a', 40);
/// $m = memory_get_usage(); $x = (array) $o; $y = get_object_vars($o); $z = get_mangled_object_vars($o); echo memory_get_usage() - $m, "\n";
/// #[AllowDynamicProperties] class D { public $d = 1; }
/// $d = new D; $d->e = 2;
/// $m = memory_get_usage(); $w = (array) $d; echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 0
/// 376
/// ```
pub(crate) fn object_symtable(
    req: &mut Request,
    object: PhpObject,
) -> Result<Option<PhpArray>, Thrown> {
    let o = object.heap();
    let Some(table) = o.properties() else {
        return Ok(None);
    };
    if o.slots() > 0
        || o.laziness() != php_heap::Laziness::Eager
        || table.is_recursive()
        || req.object_handlers(o).properties_overloaded()
    {
        return Ok(None);
    }
    let always_duplicate = req.own_handlers(object.class()).is_some();
    let array = req.proptable_to_symtable(table, always_duplicate)?;
    Ok(Some(PhpArray::of(array)))
}

/// The value a type check over `value` lands: `value` where the check
/// took it as it is, the coercion where it made one, `value` given up
/// either way it does not land, as php's `zval_ptr_dtor` gives up the
/// operand of a refused or a coerced assignment.
///
/// ```php
/// class C { public int $n = 0; public static float $f = 0.0; }
/// $c = new C;
/// $c->n = "5";
/// C::$f = 1;
/// $r = &$c->n;
/// $r = "7";
/// var_dump($c->n, C::$f);
/// try { $c->n = "x"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { $r = []; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($c->n);
/// ```
/// ```expect
/// int(7)
/// float(1)
/// Cannot assign string to property C::$n of type int
/// Cannot assign array to reference held by property C::$n of type int
/// int(7)
/// ```
pub(crate) fn landed(
    req: &mut Request,
    value: Value,
    verdict: Result<Option<Value>, Thrown>,
) -> Result<Value, Thrown> {
    if !matches!(verdict, Ok(None)) {
        req.release(value);
    }
    verdict.map(|coerced| coerced.unwrap_or(value))
}

/// `zend_verify_property_type` (`Z/Zend/zend_object_handlers.c:1065`)
/// under `zend_assign_to_typed_prop` (`Z/Zend/zend_execute.c:3583`):
/// what a write to a property with a declared type is held to. Nothing
/// where the value already satisfies the type (`zend_check_type_slow`);
/// the converted value where the weak rule converts it, which is
/// `zend_verify_weak_scalar_type_hint`, the one an argument takes, and
/// nothing under `strict_types` but an int where a float is asked for;
/// and php's `TypeError`, `Cannot assign %s to property %s::$%s of type
/// %s`, where it neither satisfies nor converts. The value stays the
/// caller's: `None` says write what it holds, `Some` says write this
/// instead and give the old one up, which is php's `tmp` standing in
/// for the value it copied.
///
/// The three writers share it: `ASSIGN_OBJ` over a declared slot,
/// `ASSIGN_STATIC_PROP` over the class's own members table, and
/// `Request::set_static_property`, the one Reflection reaches through,
/// whose `strict` is php's `zend_update_static_property_ex`, false
/// however the file was declared (`Z/Zend/zend_API.c:1585`).
///
/// ```php
/// class C { public int $n = 1; public ?string $s = null; public float $f = 0.0; }
/// $c = new C;
/// $c->n = 5; var_dump($c->n);
/// $c->n = "7"; var_dump($c->n);
/// $c->n = true; var_dump($c->n);
/// $c->f = 3; var_dump($c->f);
/// $c->s = 5; var_dump($c->s);
/// $c->s = null; var_dump($c->s);
/// ```
/// ```expect
/// int(5)
/// int(7)
/// int(1)
/// float(3)
/// string(1) "5"
/// NULL
/// ```
///
/// ```php
/// class C { public int $n = 1; }
/// $c = new C;
/// $c->n = "x";
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: Cannot assign string to property C::$n of type int in %s:4
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 4
/// ```
///
/// Under `strict_types` the weak conversion is gone and the same write
/// is the `TypeError`, save for the int a float type takes, which php
/// converts there too. That arm is served and carries no witness: the
/// compiler does not read a `declare` statement yet, so no snippet can
/// ask for strict typing (#411).
pub(crate) fn verify_property_type(
    req: &mut Request,
    value: &Value,
    ty: &php_unit::decl::Type,
    declaring: crate::api::values::ClassId,
    name: &[u8],
    strict: bool,
) -> Result<Option<Value>, Thrown> {
    let held = value.deref();
    match property_type_admits(req, &held, ty, declaring, strict)? {
        Admitted::As => Ok(None),
        Admitted::Converted(coerced) => Ok(Some(coerced)),
        Admitted::Refused => Err(property_type_error(req, &held, ty, declaring, name)),
    }
}

/// What a declared property type makes of a value, `i_zend_check_property_type`
/// without the raise: the value as it is, the weak rule's conversion, or a
/// refusal the caller words, since php words it by where the value came from.
pub(crate) enum Admitted {
    As,
    Converted(Value),
    Refused,
}

/// [`verify_property_type`]'s test alone, over a dereferenced value; the
/// `Err` is only what a conversion itself threw.
///
/// Every typed property write reads it: a value the type holds as it is,
/// one the weak rule converts, and one it refuses.
///
/// ```php
/// class C { public int $i = 0; public ?string $s = null; }
/// $c = new C; $c->i = 7; $c->s = 5;
/// try { $c->i = "x"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($c->i, $c->s);
/// ```
/// ```expect
/// Cannot assign string to property C::$i of type int
/// int(7)
/// string(1) "5"
/// ```
pub(crate) fn property_type_admits(
    req: &mut Request,
    held: &Value,
    ty: &php_unit::decl::Type,
    declaring: crate::api::values::ClassId,
    strict: bool,
) -> Result<Admitted, Thrown> {
    Ok(match type_assignable(req, held, ty, declaring, strict) {
        Assignable::Yes => Admitted::As,
        Assignable::Coerce => {
            match crate::api::coerce::scalar_type_hint(req, held, ty.mask(), strict)? {
                Some(coerced) => Admitted::Converted(coerced),
                None => Admitted::Refused,
            }
        }
        Assignable::No => Admitted::Refused,
    })
}

/// What `i_zend_verify_type_assignable_zval`
/// (`Z/Zend/zend_execute.c:3918`) answers with its `1`, `0` and `-1`:
/// the value satisfies the type, it cannot satisfy it however it is
/// converted, or the weak rule may still convert it. One reader tells
/// the last two apart, `zend_verify_ref_assignable_zval`, which must
/// know whether a source *needed* a conversion before it compares one
/// source's conversion with another's.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Assignable {
    /// php's `1`.
    Yes,
    /// php's `0`.
    No,
    /// php's `-1`.
    Coerce,
}

/// `i_zend_verify_type_assignable_zval`: the declared type against the
/// value, with no conversion made and nothing raised. The class arm is
/// `zend_check_and_resolve_property_or_class_constant_class_type`, the
/// named types of the declaration resolved and the object held to each.
fn type_assignable(
    req: &mut Request,
    held: &Value,
    ty: &php_unit::decl::Type,
    declaring: crate::api::values::ClassId,
    strict: bool,
) -> Assignable {
    if crate::api::coerce::holds(
        req,
        held,
        ty,
        crate::api::coerce::Scopes {
            own: Some(declaring),
            called: Some(declaring),
            callables: crate::api::request::Deprecations::Suppressed,
        },
    ) {
        return Assignable::Yes;
    }
    let mask = ty.mask();
    if strict {
        // The one exception php makes under `strict_types`: an int
        // where the type accepts a float, which is converted.
        return match (mask & MAY_BE_DOUBLE != 0, held.as_ref()) {
            (true, ValueRef::Int(_)) => Assignable::Coerce,
            _ => Assignable::No,
        };
    }
    // A nullable type has already been answered by `contains_code`, so a
    // null here is one the type does not accept and no conversion makes
    // acceptable.
    if matches!(held.as_ref(), ValueRef::Null | ValueRef::Undef) {
        return Assignable::No;
    }
    if mask & (MAY_BE_LONG | MAY_BE_DOUBLE | MAY_BE_STRING) == 0
        && mask & MAY_BE_BOOL != MAY_BE_BOOL
    {
        return Assignable::No;
    }
    Assignable::Coerce
}

/// `zend_verify_ref_assignable_zval` (`Z/Zend/zend_execute.c:3959`):
/// what a write through a cell a typed property's slot holds is held
/// to. The value must satisfy *every* source's type, and where two
/// sources convert it they must convert it to the same value; php's
/// first source to need a conversion is the one the comparison is made
/// against, and its conversion is what lands in the cell. `Ok(None)`
/// says write the value as it stands, `Ok(Some)` says write this
/// instead, and the `Err` is php's `TypeError`.
///
/// ```php
/// class C { public static int $a = 1; public static int $b = 2; }
/// $x = 1;
/// C::$a = &$x;
/// C::$b = &$x;
/// $x = "7";
/// var_dump($x, C::$a, C::$b);
/// try { $x = []; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($x);
/// ```
/// ```expect
/// int(7)
/// int(7)
/// int(7)
/// Cannot assign array to reference held by property C::$a of type int
/// int(7)
/// ```
pub(crate) fn verify_ref_assignable(
    req: &mut Request,
    cell: php_heap::Ref,
    value: &Value,
    strict: bool,
) -> Result<Option<Value>, Thrown> {
    let held = value.deref();
    let sources = req.ref_type_sources(cell);
    let listed: Vec<(crate::api::values::ClassId, Vec<u8>, php_unit::decl::Type)> = sources
        .iter()
        .map(|s| (s.declaring, s.name.clone(), s.ty.clone()))
        .collect();
    let mut first: Option<usize> = None;
    let mut coerced: Option<Value> = None;
    for (i, (declaring, name, ty)) in listed.iter().enumerate() {
        let verdict = type_assignable(req, &held, ty, *declaring, strict);
        if verdict == Assignable::Coerce {
            let made = crate::api::coerce::scalar_type_hint(req, &held, ty.mask(), strict)?;
            let Some(made) = made else {
                if let Some(old) = coerced {
                    req.release(old);
                }
                return Err(ref_type_error(req, &held, ty, *declaring, name));
            };
            match (first, coerced) {
                (None, _) => {
                    first = Some(i);
                    coerced = Some(made);
                }
                // A source before this one took the value as it stands,
                // so the two cannot agree.
                (Some(at), None) => {
                    req.release(made);
                    return Err(conflicting_coercion(req, &held, &listed, at, i));
                }
                (Some(at), Some(old)) => {
                    let agrees = identical(req, &old, &made);
                    req.release(made);
                    match agrees {
                        Ok(true) => coerced = Some(old),
                        Ok(false) => {
                            req.release(old);
                            return Err(conflicting_coercion(req, &held, &listed, at, i));
                        }
                        Err(thrown) => {
                            req.release(old);
                            return Err(thrown);
                        }
                    }
                }
            }
        } else if verdict == Assignable::No {
            if let Some(old) = coerced {
                req.release(old);
            }
            return Err(ref_type_error(req, &held, ty, *declaring, name));
        } else if first.is_none() {
            first = Some(i);
        } else if let Some(old) = coerced {
            // A source before this one needed a conversion and this one
            // does not, so the two cannot agree.
            req.release(old);
            let at = first.unwrap_or(i);
            return Err(conflicting_coercion(req, &held, &listed, at, i));
        }
    }
    Ok(coerced)
}

/// `zend_verify_prop_assignable_by_ref`
/// (`Z/Zend/zend_execute.c:4121`): what a typed property's slot is held
/// to before a cell is bound into it. A cell no typed property holds
/// yet is the plain check, which converts the value *in the cell*, so
/// `$x = "5"` bound into an `int` slot leaves both names holding
/// `int(5)`; a cell another typed property already holds converts
/// nothing, since a conversion would be one name's value changing under
/// the other, and php names both properties in the refusal.
/// `Ok(Some)` is the value the cell takes instead of what it holds.
///
/// ```php
/// class C { public static int $n = 1; public static string $s = "x"; }
/// $x = "5";
/// C::$n = &$x;
/// var_dump($x, C::$n);
/// $y = 1;
/// C::$n = &$y;
/// try { C::$s = &$y; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(5)
/// int(5)
/// Reference with value of type int held by property C::$n of type int is not compatible with property C::$s of type string
/// ```
pub(crate) fn verify_prop_assignable_by_ref(
    req: &mut Request,
    cell: php_heap::Ref,
    ty: &php_unit::decl::Type,
    declaring: crate::api::values::ClassId,
    name: &[u8],
    strict: bool,
) -> Result<Option<Value>, Thrown> {
    let held = crate::api::values::PhpRef::of(cell).get().deref();
    if req.ref_type_sources(cell).is_empty() {
        return verify_property_type(req, &held, ty, declaring, name, strict);
    }
    if typed_ref_holds(req, cell, ty, declaring, name, strict)? {
        return Ok(None);
    }
    Err(property_type_error(req, &held, ty, declaring, name))
}

/// `zend_verify_prop_assignable_by_ref_ex`'s arm over a cell some typed
/// property is already a source of, which converts nothing, since a
/// conversion would be one name's value changing under the other:
/// `true` where the type holds the value as it is, `false` where it
/// holds it in no form, the caller's own error, and php's `TypeError`
/// naming both properties where it would hold it only converted
/// (`zend_throw_ref_type_error_type`).
///
/// ```php
/// class C { public static int $n = 1; public static float $f = 0.5; public static ?int $m = null; }
/// $y = 1; C::$n = &$y;
/// try { C::$f = &$y; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// C::$m = &$y;
/// $y = 2;
/// var_dump(C::$m);
/// ```
/// ```expect
/// Reference with value of type int held by property C::$n of type int is not compatible with property C::$f of type float
/// int(2)
/// ```
pub(crate) fn typed_ref_holds(
    req: &mut Request,
    cell: php_heap::Ref,
    ty: &php_unit::decl::Type,
    declaring: crate::api::values::ClassId,
    name: &[u8],
    strict: bool,
) -> Result<bool, Thrown> {
    let held = crate::api::values::PhpRef::of(cell).get().deref();
    let first = req
        .ref_type_sources(cell)
        .first()
        .map(|s| (s.declaring, s.name.clone(), s.ty.clone()));
    let Some((first_at, first_name, first_ty)) = first else {
        return Ok(false);
    };
    match type_assignable(req, &held, ty, declaring, strict) {
        Assignable::Yes => return Ok(true),
        Assignable::Coerce => {
            if let Some(made) = crate::api::coerce::scalar_type_hint(req, &held, ty.mask(), strict)?
            {
                req.release(made);
                let class = req.symbols().class_name(first_at);
                let two = req.symbols().class_name(declaring);
                let one_declared = php_unit::type_to_string(&first_ty);
                let two_declared = php_unit::type_to_string(ty);
                let given = type_name(req, &held.undef_as_null());
                return Err(type_error(
                    req,
                    &format!(
                        "Reference with value of type {given} held by property {class}::${} of type {one_declared} is not compatible with property {two}::${} of type {two_declared}",
                        String::from_utf8_lossy(&first_name),
                        String::from_utf8_lossy(name)
                    ),
                ));
            }
        }
        Assignable::No => {}
    }
    Ok(false)
}

/// `zend_throw_incdec_prop_error` (`Z/Zend/zend_execute.c:2211`): the
/// `TypeError` a step raises where an `int` slot would have to hold the
/// float it stepped into, and the value the slot takes instead, which
/// is php's own `ZEND_LONG_MAX` or `ZEND_LONG_MIN`; over a reference
/// the property holds, `zend_throw_incdec_ref_error`, whose text names
/// the reference ([`Holder`]).
///
/// ```php
/// class C { public static int $n = PHP_INT_MIN; public int $p = PHP_INT_MAX; }
/// try { --C::$n; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(C::$n);
/// $o = new C; $r = &$o->p;
/// try { $r++; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($o->p);
/// ```
/// ```expect
/// Cannot decrement property C::$n of type int past its minimal value
/// int(-9223372036854775808)
/// Cannot increment a reference held by property C::$p of type int past its maximal value
/// int(9223372036854775807)
/// ```
pub(crate) fn incdec_prop_error(
    req: &mut Request,
    ty: &php_unit::decl::Type,
    declaring: crate::api::values::ClassId,
    name: &[u8],
    (increment, holder): (bool, Holder),
) -> (Thrown, Value) {
    let class = req.symbols().class_name(declaring);
    let declared = php_unit::type_to_string(ty);
    let (word, edge, limit) = if increment {
        ("increment", "maximal", i64::MAX)
    } else {
        ("decrement", "minimal", i64::MIN)
    };
    let what = match holder {
        Holder::Property => "",
        Holder::Reference => "a reference held by ",
    };
    let thrown = type_error(
        req,
        &format!(
            "Cannot {word} {what}property {class}::${} of type {declared} past its {edge} value",
            String::from_utf8_lossy(name)
        ),
    );
    (thrown, Value::int(limit))
}

/// What holds the value a step [`incdec_prop_error`] refuses changed:
/// the property's own slot, or a reference the property's slot holds.
#[derive(Clone, Copy)]
pub(crate) enum Holder {
    Property,
    Reference,
}

/// Whether a step into a float is the one php refuses: the slot held an
/// int, the step made a float and the declared type accepts no float,
/// which is the check `zend_incdec_typed_prop` and the `IS_LONG` fast
/// path beside it both make.
///
/// ```php
/// class C { public static int $n = PHP_INT_MAX; public static float $f = PHP_INT_MAX; }
/// var_dump(++C::$f);
/// try { ++C::$n; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// float(9.223372036854776E+18)
/// Cannot increment property C::$n of type int past its maximal value
/// ```
pub(crate) fn steps_past_int(held: &Value, stepped: &Value, ty: &php_unit::decl::Type) -> bool {
    matches!(held.as_ref(), ValueRef::Int(_))
        && matches!(stepped.as_ref(), ValueRef::Float(_))
        && ty.mask() & MAY_BE_DOUBLE == 0
}

/// `zend_throw_ref_type_error_zval` (`Z/Zend/zend_execute.c:3891`): the
/// `TypeError` a write through a cell raises, which names the property
/// that holds the cell and not the name written through.
///
/// ```php
/// class C { public int $n = 1; }
/// $c = new C; $r = &$c->n;
/// try { $r = "x"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($c->n);
/// ```
/// ```expect
/// Cannot assign string to reference held by property C::$n of type int
/// int(1)
/// ```
pub(crate) fn ref_type_error(
    req: &mut Request,
    value: &Value,
    ty: &php_unit::decl::Type,
    declaring: crate::api::values::ClassId,
    name: &[u8],
) -> Thrown {
    let class = req.symbols().class_name(declaring);
    let declared = php_unit::type_to_string(ty);
    let given = value_name(req, &value.undef_as_null());
    type_error(
        req,
        &format!(
            "Cannot assign {given} to reference held by property {class}::${} of type {declared}",
            String::from_utf8_lossy(name)
        ),
    )
}

/// `zend_throw_conflicting_coercion_error`
/// (`Z/Zend/zend_execute.c:3902`): two properties hold one cell and the
/// value would land in it as two different values, which php refuses
/// rather than pick one.
fn conflicting_coercion(
    req: &mut Request,
    value: &Value,
    listed: &[(crate::api::values::ClassId, Vec<u8>, php_unit::decl::Type)],
    first: usize,
    second: usize,
) -> Thrown {
    let (Some((one_at, one_name, one_ty)), Some((two_at, two_name, two_ty))) =
        (listed.get(first), listed.get(second))
    else {
        return type_error(req, "Cannot assign to reference");
    };
    let one_class = req.symbols().class_name(*one_at);
    let two_class = req.symbols().class_name(*two_at);
    let one_declared = php_unit::type_to_string(one_ty);
    let two_declared = php_unit::type_to_string(two_ty);
    let given = value_name(req, &value.undef_as_null());
    type_error(
        req,
        &format!(
            "Cannot assign {given} to reference held by property {one_class}::${} of type {one_declared} and property {two_class}::${} of type {two_declared}, as this would result in an inconsistent type conversion",
            String::from_utf8_lossy(one_name),
            String::from_utf8_lossy(two_name)
        ),
    )
}

/// `MAY_BE_*` (`Z/Zend/zend_type_info.h:26-32`), the bits
/// [`type_assignable`] reads of a declared type's mask to say whether a
/// conversion could still land.
const MAY_BE_BOOL: u32 = (1 << 2) | (1 << 3);
/// `MAY_BE_LONG`.
const MAY_BE_LONG: u32 = 1 << 4;
/// `MAY_BE_DOUBLE`.
const MAY_BE_DOUBLE: u32 = 1 << 5;
/// `MAY_BE_STRING`.
const MAY_BE_STRING: u32 = 1 << 6;

/// `zend_verify_property_type_error` (`Z/Zend/zend_execute.c:961`): the
/// `TypeError` naming the value, the class that declared the property,
/// the property and the declared type, in php's words.
/// `zend_verify_class_constant_type` (`Z/Zend/zend_execute.c:1524`),
/// what `zend_update_class_constant` holds a typed constant's value to
/// once its expression is evaluated: the type as a property's is held,
/// strictly, so the one conversion is an int to a float, and php's
/// `TypeError` naming the value's type where it neither satisfies nor
/// converts, named as `zend_zval_type_name` names it, an object by its
/// class. The value is the caller's to have back; the one it is refused
/// is not given up here, since a constant's read hands out the value
/// the table holds.
///
/// ```php
/// class A { const int X = self::Y; const Y = 's'; const float F = self::I; const I = 2; }
/// var_dump(A::F);
/// try { var_dump(A::X); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// float(2)
/// Cannot assign string to class constant A::X of type int
/// ```
pub(crate) fn verify_class_constant_type(
    req: &mut Request,
    value: Value,
    ty: &php_unit::decl::Type,
    declaring: crate::api::values::ClassId,
    name: &[u8],
) -> Result<Value, Thrown> {
    let held = value.deref();
    let coerced = match type_assignable(req, &held, ty, declaring, true) {
        Assignable::Yes => return Ok(value),
        // The one conversion a strict check makes is an int to a
        // float, so what is given up for it holds nothing.
        Assignable::Coerce => crate::api::coerce::scalar_type_hint(req, &held, ty.mask(), true)?,
        Assignable::No => None,
    };
    if let Some(coerced) = coerced {
        return Ok(coerced);
    }
    let class = req.symbols().class_name(declaring);
    let declared = php_unit::type_to_string(ty);
    let given = match held.as_object() {
        Some(object) => req.symbols().class_name(object.class()),
        None => held.type_name().to_string(),
    };
    Err(type_error(
        req,
        &format!(
            "Cannot assign {given} to class constant {class}::{} of type {declared}",
            String::from_utf8_lossy(name)
        ),
    ))
}

fn property_type_error(
    req: &mut Request,
    value: &Value,
    ty: &php_unit::decl::Type,
    declaring: crate::api::values::ClassId,
    name: &[u8],
) -> Thrown {
    // `if (EG(exception)) return;`: nothing is raised over a pending
    // throwable, the write is refused and that one leaves: one more owner
    // of the slot's, and none past a `while_pending` step's, whose slot
    // owns none.
    if let Some(standing) = req.pending() {
        if !req.stepping_past(standing) {
            req.addref(&standing.as_value());
        }
        return Thrown::of(Some(standing));
    }
    let class = req.symbols().class_name(declaring);
    let declared = php_unit::type_to_string(ty);
    let given = value_name(req, &value.undef_as_null());
    type_error(
        req,
        &format!(
            "Cannot assign {given} to property {class}::${} of type {declared}",
            String::from_utf8_lossy(name)
        ),
    )
}

/// `zend_cast_zval_to_object` (`Z/Zend/zend_execute.h:225`) under the
/// `Z_TYPE_P(expr) == IS_OBJECT` arm of `ZEND_CAST` above it: an object
/// is itself with one more owner; `null` is an empty `stdClass`; an
/// array is a `stdClass` whose table is the array itself, one more
/// owner of it, where its keys are all strings, and otherwise a copy of
/// it with each integer key spelt in decimal
/// ([`Request::properties_from_array`], whose witness holds the cast's
/// `memory_get_usage()` delta to php's); and a scalar or a resource is
/// a `stdClass` with the one property `scalar` holding it, a NAN
/// warning on the way. The
/// object handed back is the caller's to own, and the `Err` is the
/// memory limit ending the request.
///
/// ```php
/// class A { public $a = 1; }
/// $o = new A; $x = token_get_all("1");
/// var_dump((object)null, (object)1.5, (object)"s", (object)$x, (object)$o === $o, (object)$o, (object)NAN);
/// ```
/// ```expect
/// Warning: unexpected NAN value was coerced to object in %s on line 4
/// object(stdClass)#2 (0) {
/// }
/// object(stdClass)#3 (1) {
///   ["scalar"]=>
///   float(1.5)
/// }
/// object(stdClass)#4 (1) {
///   ["scalar"]=>
///   string(1) "s"
/// }
/// object(stdClass)#5 (1) {
///   ["0"]=>
///   array(3) {
///     [0]=>
///     int(267)
///     [1]=>
///     string(1) "1"
///     [2]=>
///     int(1)
///   }
/// }
/// bool(true)
/// object(A)#1 (1) {
///   ["a"]=>
///   int(1)
/// }
/// object(stdClass)#6 (1) {
///   ["scalar"]=>
///   float(NAN)
/// }
/// ```
pub(crate) fn to_object(req: &mut Request, value: &Value) -> Result<PhpObject, Thrown> {
    let value = value.deref();
    if let ValueRef::Object(o) = value.as_ref() {
        req.addref(&value);
        return Ok(o);
    }
    let object = req.new_object_of("stdClass")?;
    match value.as_ref() {
        ValueRef::Undef | ValueRef::Null => {}
        ValueRef::Array(a) => req.properties_from_array(object, a)?,
        _ => {
            if value.as_float().is_some_and(f64::is_nan) {
                warning(req, "unexpected NAN value was coerced to object");
            }
            let scalar = Owner::take(req, value);
            crate::api::call::write_prop(req, &object, b"scalar", scalar)?;
        }
    }
    Ok(object)
}

/// `zval_get_long_func` with `is_strict` off (`Z/Zend/zend_operators.c:957`),
/// what `(int)` and `zval_get_long` read: the number a string opens
/// with and nothing for none, silently, a double past the range capped
/// to it, an array its emptiness, and an object php's warning and one.
///
/// ```php
/// var_dump((int)"2abc", (int)" 12 ", (int)"1e3x", (int)"abc", (int)"9223372036854775808", (int)"1.9e19", (int)[1], (int)[], (int)null, (int)true, (int)new stdClass);
/// ```
/// ```expect
///
/// Warning: Object of class stdClass could not be converted to int in %s on line 2
/// int(2)
/// int(12)
/// int(1000)
/// int(0)
/// int(9223372036854775807)
/// int(9223372036854775807)
/// int(1)
/// int(0)
/// int(0)
/// int(1)
/// int(1)
/// ```
pub(crate) fn get_long(req: &mut Request, value: &Value) -> i64 {
    match value.deref().as_ref() {
        ValueRef::Undef | ValueRef::Null | ValueRef::Bool(false) => 0,
        ValueRef::Bool(true) => 1,
        ValueRef::Resource(r) => r.id(),
        ValueRef::Int(n) => n,
        ValueRef::Float(d) => dval_to_lval(req, d),
        // `is_numeric_string` with `allow_errors`: the number the string
        // opens with, a double among them capped, and zero for none.
        ValueRef::Str(s) => match numeric_prefix(s.as_bytes()) {
            Some((Number::Long(n), _)) => n,
            Some((Number::Double(d), _)) => dval_to_lval_cap(d),
            None => 0,
        },
        ValueRef::Array(a) => i64::from(!a.is_empty()),
        ValueRef::Object(o) => match object_converted(req, o, TypeTag::Int, "int") {
            Some(converted) => converted.as_int().unwrap_or(1),
            None => 1,
        },
        // `deref` followed the reference already.
        ValueRef::Reference(r) => get_long(req, &r.get()),
    }
}

/// `zval_get_double_func` (`Z/Zend/zend_operators.c:1027`), what
/// `(float)` and `zval_get_double` read: `zend_strtod` over a string,
/// the number it opens with and zero for none, silently, and an object
/// php's warning and one.
///
/// ```php
/// var_dump((float)"9a", (float)" 1.5e2x", (float)"abc", (float)"0x1A", (float)[0], (float)false, (float)7, (float)new stdClass);
/// ```
/// ```expect
///
/// Warning: Object of class stdClass could not be converted to float in %s on line 2
/// float(9)
/// float(150)
/// float(0)
/// float(0)
/// float(1)
/// float(0)
/// float(7)
/// float(1)
/// ```
///
/// `zend_strtod` keeps the sign of a zero, which an integer read would
/// lose:
///
/// ```php
/// var_dump((float)"-0", +(float)" -0");
/// ```
/// ```expect
/// float(-0)
/// float(-0)
/// ```
pub(crate) fn get_double(req: &mut Request, value: &Value) -> f64 {
    match value.deref().as_ref() {
        ValueRef::Undef | ValueRef::Null | ValueRef::Bool(false) => 0.0,
        ValueRef::Bool(true) => 1.0,
        ValueRef::Resource(r) => wide(r.id()),
        ValueRef::Int(n) => wide(n),
        ValueRef::Float(d) => d,
        // `zend_strtod` over the string: the number it opens with after
        // any leading whitespace, read to a double, and zero for none.
        ValueRef::Str(s) => match numeric_prefix(s.as_bytes()) {
            Some((Number::Long(0), _)) if negative_text(s.as_bytes()) => -0.0,
            Some((Number::Long(n), _)) => wide(n),
            Some((Number::Double(d), _)) => d,
            None => 0.0,
        },
        ValueRef::Array(a) => f64::from(u8::from(!a.is_empty())),
        ValueRef::Object(o) => match object_converted(req, o, TypeTag::Float, "float") {
            Some(converted) => converted.as_float().unwrap_or(1.0),
            None => 1.0,
        },
        ValueRef::Reference(r) => get_double(req, &r.get()),
    }
}

/// Whether the number a string opens with, past its whitespace, is
/// written with a `-`.
fn negative_text(bytes: &[u8]) -> bool {
    bytes.iter().find(|b| !crate::c_number::is_whitespace(**b)) == Some(&b'-')
}

/// `convert_object_to_type`'s warning, for an object whose class has no
/// conversion to the type named.
fn not_converted(req: &mut Request, value: &Value, to: &str) {
    let class = value_name(req, value);
    warning(
        req,
        &format!("Object of class {class} could not be converted to {to}"),
    );
}

/// `add_function` (`Zend/zend_operators.c:1200`), its fast type pairs and
/// the slow path behind them: the sum of two longs, promoted to a double
/// where it overflowed; a double where either side is one; two arrays
/// merged with the left side's keys kept (`add_function_array`); and
/// `zend_binop_error`'s `TypeError` for an operand php will not read as a
/// number.
///
/// Each fence calls a native so that php's compiler leaves the opcode
/// standing: `zend_try_ct_eval_binary_op` folds arithmetic over two
/// constants where it is written, and what a call returned is constant to
/// no compiler.
///
/// ```php
/// echo 1 + (2 + extension_loaded('Core')), "|", 1.5 + (2.5 + extension_loaded('Core')), "|", "1" + (1 + extension_loaded('Core')), "|", 9223372036854775807 + (1 + extension_loaded('Core'));
/// ```
/// ```expect
/// 4|5|3|9.2233720368548E+18
/// ```
///
/// A string with a number in front of it and anything else behind it is
/// php's warning and the number it read (`_is_numeric_string_ex` with
/// `allow_errors`), which is why php leaves the opcode standing for one.
///
/// ```php
/// echo "n:", "1abc" + (1 + extension_loaded('Core'));
/// ```
/// ```expect
/// n:
/// Warning: A non-numeric value encountered in %s on line 2
/// 3
/// ```
pub(crate) fn add(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    if let Some(value) = inline::binary_values(Binary::Arith(Arith::Add), left, right) {
        return Ok(value);
    }
    if let (Some(a), Some(b)) = (left.deref().as_array(), right.deref().as_array()) {
        return Ok(merged(req, a, b)?.into_value().hand_on());
    }
    if let Some(value) = object_operation(req, BinaryOp::Add, left, right)? {
        return Ok(value);
    }
    let (a, b) = numbers(req, "+", left, right)?;
    Ok(inline::arith_numbers(Arith::Add, a, b))
}

/// `sub_function` (`Zend/zend_operators.c:1259`): the same type pairs as
/// [`add`] without the array one, which php refuses for every operator
/// but `+`.
///
/// ```php
/// echo 1 - (2 - extension_loaded('Core')), "|", 1.5 - (2.5 - extension_loaded('Core')), "|", "3" - (1 + extension_loaded('Core')), "|", -9223372036854775807 - (1 + extension_loaded('Core'));
/// ```
/// ```expect
/// 0|0|1|-9.2233720368548E+18
/// ```
pub(crate) fn sub(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    if let Some(value) = inline::binary_values(Binary::Arith(Arith::Sub), left, right) {
        return Ok(value);
    }
    if let Some(value) = object_operation(req, BinaryOp::Sub, left, right)? {
        return Ok(value);
    }
    let (a, b) = numbers(req, "-", left, right)?;
    Ok(inline::arith_numbers(Arith::Sub, a, b))
}

/// `mul_function` (`Zend/zend_operators.c:1322`), whose long pair is
/// `ZEND_SIGNED_MULTIPLY_LONG`: the product, or the double it overflowed
/// into.
///
/// ```php
/// echo 2 * (3 * extension_loaded('Core')), "|", 1.5 * (2.5 * extension_loaded('Core')), "|", "3" * (1 + extension_loaded('Core')), "|", 4611686018427387904 * (1 + extension_loaded('Core'));
/// ```
/// ```expect
/// 6|3.75|6|9.2233720368548E+18
/// ```
pub(crate) fn mul(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    if let Some(value) = inline::binary_values(Binary::Arith(Arith::Mul), left, right) {
        return Ok(value);
    }
    if let Some(value) = object_operation(req, BinaryOp::Mul, left, right)? {
        return Ok(value);
    }
    let (a, b) = numbers(req, "*", left, right)?;
    Ok(inline::arith_numbers(Arith::Mul, a, b))
}

/// `div_function` and `div_function_base`
/// (`Zend/zend_operators.c:1454`): a long where the division comes out
/// whole and a double where it does not, php's own guard against
/// `ZEND_LONG_MIN / -1`, and `DivisionByZeroError` thrown where the
/// opcode stands for a divisor of zero, `-0.0` among them.
///
/// ```php
/// echo 6 / (1 + extension_loaded('Core')), "|", 7.5 / (1.5 * extension_loaded('Core')), "|", "7" / (1 + extension_loaded('Core')), "|", (-9223372036854775807 - extension_loaded('Core')) / (0 - extension_loaded('Core'));
/// ```
/// ```expect
/// 3|5|3.5|9.2233720368548E+18
/// ```
///
/// The divisor of zero is the one pair php's compiler will not fold, so
/// two constants exercise the opcode in both engines; uncaught, the
/// throwable names the opline's line as its site.
///
/// ```php
/// echo 1/0;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught DivisionByZeroError: Division by zero in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn div(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    if let Some(value) = inline::binary_values(Binary::Div, left, right) {
        return Ok(value);
    }
    if let Some(value) = object_operation(req, BinaryOp::Div, left, right)? {
        return Ok(value);
    }
    let (a, b) = numbers(req, "/", left, right)?;
    if let Some(n) = inline::quotient(a, b) {
        return Ok(inline::number_value(n));
    }
    if b.double() == 0.0 {
        return Err(throw_error(req, "DivisionByZeroError", "Division by zero"));
    }
    // php's own case, "prevent overflow error/crash": `PHP_INT_MIN / -1`
    // as a double, the one other pair the rule refuses.
    Ok(Value::float(a.double() / b.double()))
}

/// `mod_function` (`Zend/zend_operators.c:1541`): both operands as the
/// longs `convert_op1_op2_long` makes of them, the left one first,
/// `DivisionByZeroError` with php's own text for a divisor of zero, and
/// php's `% -1` case, which is zero where the division would overflow.
///
/// ```php
/// echo 7 % (2 * extension_loaded('Core')), '|', -7 % (2 * extension_loaded('Core')), '|', '7' % (2 * extension_loaded('Core')), '|', true % (2 * extension_loaded('Core')), '|', (-9223372036854775807 - extension_loaded('Core')) % (0 - extension_loaded('Core'));
/// ```
/// ```expect
/// 1|-1|1|1|0
/// ```
///
/// A double is what `zend_dval_to_lval_safe` makes of it: truncated,
/// with the deprecation where the truncation loses something; past a
/// long's range, php's warning and its modular reading
/// (`zend_dval_to_lval_slow`); and for NAN, which `ZEND_DOUBLE_FITS_LONG`
/// does not refuse, both.
///
/// ```php
/// echo 7.5 % (2 * extension_loaded('Core')), '|', 8.0 % (3 * extension_loaded('Core')), '|', 1e19 % (7 * extension_loaded('Core')), '|', NAN % (2 * extension_loaded('Core'));
/// ```
/// ```expect
///
/// Deprecated: Implicit conversion from float 7.5 to int loses precision in %s on line 2
/// 1|2|
/// Warning: The float 1.0E+19 is not representable as an int, cast occurred in %s on line 2
/// -6|
/// Warning: The float NAN is not representable as an int, cast occurred in %s on line 2
///
/// Deprecated: Implicit conversion from float NAN to int loses precision in %s on line 2
/// 0
/// ```
///
/// A string is read by [`numeric_prefix`] with the warning for what
/// follows the number, then a double among them is capped to the long's
/// range (`zend_dval_to_lval_cap`) with the float-string deprecation
/// where the cap or the truncation lost something.
///
/// ```php
/// echo '7abc' % (2 * extension_loaded('Core')), '|', '1.5' % (2 * extension_loaded('Core')), '|', '1e19' % (2 * extension_loaded('Core'));
/// ```
/// ```expect
///
/// Warning: A non-numeric value encountered in %s on line 2
/// 1|
/// Deprecated: Implicit conversion from float-string "1.5" to int loses precision in %s on line 2
/// 1|
/// Deprecated: Implicit conversion from float-string "1e19" to int loses precision in %s on line 2
/// 1
/// ```
///
/// A divisor of zero is the one pair php's compiler will not fold, so
/// two constants exercise the opcode in both engines.
///
/// ```php
/// echo 1 % 0;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught DivisionByZeroError: Modulo by zero in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn r#mod(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    if let Some(value) = inline::binary_values(Binary::Mod, left, right) {
        return Ok(value);
    }
    if let Some(value) = object_operation(req, BinaryOp::Mod, left, right)? {
        return Ok(value);
    }
    let (a, b) = longs(req, "%", left, right)?;
    match inline::remainder(a, b) {
        Some(n) => Ok(Value::int(n)),
        None => Err(throw(req, "DivisionByZeroError", "Modulo by zero")),
    }
}

/// `pow_function` and `pow_function_base`
/// (`Zend/zend_operators.c:1358`): [`php_heap::power`] over a long pair
/// whose exponent is not negative, and `safe_pow` over two doubles for
/// every other pair, which is C's `pow` behind the deprecation php
/// raises for a zero base and a negative exponent.
///
/// ```php
/// echo 2 ** (3 * extension_loaded('Core')), '|', 2 ** (0.5 * extension_loaded('Core')), '|', 2.5 ** (2 * extension_loaded('Core')), '|', 2 ** (-1 * extension_loaded('Core')), '|', '3' ** (2 * extension_loaded('Core')), '|', 2 ** (63 * extension_loaded('Core')), '|', 3 ** (40 * extension_loaded('Core'));
/// ```
/// ```expect
/// 8|1.4142135623731|6.25|0.5|9|9.2233720368548E+18|1.2157665459057E+19
/// ```
///
/// The zero base under a negative exponent is the one pair php's
/// compiler will not fold, so two constants exercise the opcode in both
/// engines; at the pin the deprecation is what php raises there, and
/// `INF` is what `pow` computes.
///
/// ```php
/// echo 0 ** -1, '|', 0.0 ** (-1 * extension_loaded('Core'));
/// ```
/// ```expect
///
/// Deprecated: Power of base 0 and negative exponent is deprecated in %s on line 2
/// INF|
/// Deprecated: Power of base 0 and negative exponent is deprecated in %s on line 2
/// INF
/// ```
pub(crate) fn pow(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    if let Some(value) = object_operation(req, BinaryOp::Pow, left, right)? {
        return Ok(value);
    }
    let (a, b) = numbers(req, "**", left, right)?;
    if let (Number::Long(a), Number::Long(b)) = (a, b)
        && let Ok(exponent) = u64::try_from(b)
    {
        return Ok(match php_heap::power(a, exponent) {
            Number::Long(n) => Value::int(n),
            Number::Double(d) => Value::float(d),
        });
    }
    let (base, exponent) = (a.double(), b.double());
    if base == 0.0 && exponent < 0.0 {
        deprecated(req, "Power of base 0 and negative exponent is deprecated");
    }
    Ok(Value::float(base.powf(exponent)))
}

/// `shift_left_function` (`Zend/zend_operators.c:1944`):
/// [`php_heap::shift_left`] over the longs `convert_op1_op2_long` makes
/// of both sides, and `ArithmeticError` thrown where the opcode stands
/// for a negative count.
///
/// ```php
/// echo 1 << (3 * extension_loaded('Core')), "|", 1 << (64 * extension_loaded('Core')), "|", -1 << (1 * extension_loaded('Core')), "|", "2" << (1 * extension_loaded('Core')), "|", 1 << (2.0 * extension_loaded('Core')), "|", 1.5 << (1 * extension_loaded('Core'));
/// ```
/// ```expect
/// 8|0|-2|4|4|
/// Deprecated: Implicit conversion from float 1.5 to int loses precision in %s on line 2
/// 2
/// ```
///
/// The negative count is the one pair php's compiler will not fold, so
/// two constants exercise the opcode in both engines.
///
/// ```php
/// echo 1 << -1;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArithmeticError: Bit shift by negative number in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn shift_left(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    shift(req, Shift::Left, left, right)
}

/// `shift_right_function` (`Zend/zend_operators.c:1981`):
/// [`php_heap::shift_right`], the same way.
///
/// ```php
/// echo 8 >> (1 * extension_loaded('Core')), "|", -8 >> (1 * extension_loaded('Core')), "|", -1 >> (64 * extension_loaded('Core')), "|", 1 >> (64 * extension_loaded('Core')), "|", "8" >> (2 * extension_loaded('Core'));
/// ```
/// ```expect
/// 4|-4|-1|0|2
/// ```
///
/// ```php
/// echo 1 >> -1;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArithmeticError: Bit shift by negative number in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn shift_right(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    shift(req, Shift::Right, left, right)
}

/// [`shift_left`] and [`shift_right`], which differ only in the
/// direction: the inline pair, a class's `do_operation`, then two longs,
/// a count of 64 or more, which the rule leaves here, shifted by
/// [`php_heap`]'s and a negative one php's `ArithmeticError`.
fn shift(req: &mut Request, side: Shift, left: &Value, right: &Value) -> Result<Value, Thrown> {
    if let Some(value) = inline::binary_values(Binary::Shift(side), left, right) {
        return Ok(value);
    }
    let (op, word, by_heap): (_, _, fn(i64, u64) -> i64) = match side {
        Shift::Left => (BinaryOp::ShiftLeft, "<<", php_heap::shift_left),
        Shift::Right => (BinaryOp::ShiftRight, ">>", php_heap::shift_right),
    };
    if let Some(value) = object_operation(req, op, left, right)? {
        return Ok(value);
    }
    let (a, b) = longs(req, word, left, right)?;
    if let Some(n) = inline::shifted(side, a, b) {
        return Ok(Value::int(n));
    }
    match u64::try_from(b) {
        Ok(by) => Ok(Value::int(by_heap(a, by))),
        Err(_) => Err(throw(
            req,
            "ArithmeticError",
            "Bit shift by negative number",
        )),
    }
}

/// What [`bitwise`] reads of its operator past the long pair
/// [`Bitwise::long`] computes.
impl Bitwise {
    /// The operator as php's grammar spells it, which is what
    /// `zend_binop_error` puts between the two type names.
    const fn spelling(self) -> &'static str {
        match self {
            Bitwise::Or => "|",
            Bitwise::Xor => "^",
            Bitwise::And => "&",
        }
    }

    /// The opcode a class's `do_operation` is asked for.
    const fn binary_op(self) -> BinaryOp {
        match self {
            Bitwise::Or => BinaryOp::BitwiseOr,
            Bitwise::Xor => BinaryOp::BitwiseXor,
            Bitwise::And => BinaryOp::BitwiseAnd,
        }
    }

    /// The operator over one pair of bytes.
    const fn byte(self, left: u8, right: u8) -> u8 {
        match self {
            Bitwise::Or => left | right,
            Bitwise::Xor => left ^ right,
            Bitwise::And => left & right,
        }
    }
}

/// `bitwise_or_function` (`Zend/zend_operators.c:1698`),
/// `bitwise_and_function` (`:1780`) and `bitwise_xor_function` (`:1862`),
/// which differ only in the operator: two longs joined by it; two
/// strings joined byte by byte over the shorter of the two, as long as
/// the longer side for an `|`, whose tail is copied after them, and as
/// long as the shorter for an `^` and an `&`, with php's `ZVAL_CHAR`
/// where both sides are one byte; and every other pair read as
/// `convert_op1_op2_long` reads it, which is [`to_long`] on each with
/// `zend_binop_error`'s `TypeError` for the one that failed.
fn bitwise(req: &mut Request, op: Bitwise, left: &Value, right: &Value) -> Result<Value, Thrown> {
    if let Some(value) = inline::binary_values(Binary::Bitwise(op), left, right) {
        return Ok(value);
    }
    let (a, b) = (left.deref(), right.deref());
    if let (Some(x), Some(y)) = (a.as_php_string(), b.as_php_string()) {
        let (x, y) = (x.as_bytes(), y.as_bytes());
        // php's own fast case: two strings of one byte are the interned
        // one-character string, `ZVAL_CHAR`.
        if x.len() == 1 && y.len() == 1 {
            let byte = op.byte(x[0], y[0]);
            return Ok(Value::string(req.string(&[byte])?));
        }
        let (longer, shorter) = if x.len() >= y.len() { (x, y) } else { (y, x) };
        let len = if op == Bitwise::Or {
            longer.len()
        } else {
            shorter.len()
        };
        let joined = req.string_with(len, |bytes| {
            for (out, (l, r)) in bytes.iter_mut().zip(longer.iter().zip(shorter)) {
                *out = op.byte(*l, *r);
            }
            if let Some(tail) = bytes.get_mut(shorter.len()..) {
                tail.copy_from_slice(&longer[shorter.len()..len]);
            }
        })?;
        return Ok(Value::string(joined));
    }
    if let Some(value) = object_operation(req, op.binary_op(), left, right)? {
        return Ok(value);
    }
    let (x, y) = longs(req, op.spelling(), left, right)?;
    Ok(Value::int(op.long(x, y)))
}

/// `bitwise_or_function`: [`bitwise`] with `|`, whose string pair is as
/// long as the longer side.
///
/// php's compiler folds the operator over two constants where it can
/// (`zend_try_ct_eval_binary_op`), so each fence calls a native on one
/// side to leave the opcode standing; a string pair is the one it folds
/// whatever the two hold, and stands here as it is written.
///
/// ```php
/// $six = 6 * extension_loaded('Core');
/// var_dump($six | 3, $six | 3.0, $six | "3", "ab" | "cd", "ab" | "c", "a" | "b", true | 0, null | 5);
/// ```
/// ```expect
/// int(7)
/// int(7)
/// int(7)
/// string(2) "cf"
/// string(2) "cb"
/// string(1) "c"
/// int(1)
/// int(5)
/// ```
///
/// An operand php will not read as a number is `zend_binop_error`'s
/// `TypeError`, which names php's types around the operator as written.
///
/// ```php
/// echo [] | 1;
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: Unsupported operand types: array | int in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn bitwise_or(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    bitwise(req, Bitwise::Or, left, right)
}

/// `bitwise_xor_function`: [`bitwise`] with `^`, whose string pair is as
/// long as the shorter side, so the longer side's tail is dropped.
///
/// ```php
/// $six = 6 * extension_loaded('Core');
/// var_dump($six ^ 3, $six ^ "3", "ab" ^ "  ", "ab" ^ " ", "a" ^ " ", true ^ 1, null ^ 5);
/// ```
/// ```expect
/// int(5)
/// int(5)
/// string(2) "AB"
/// string(1) "A"
/// string(1) "A"
/// int(0)
/// int(5)
/// ```
///
/// ```php
/// echo 1 ^ [];
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: Unsupported operand types: int ^ array in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn bitwise_xor(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    bitwise(req, Bitwise::Xor, left, right)
}

/// `bitwise_and_function`: [`bitwise`] with `&`, the same shorter-side
/// string.
///
/// ```php
/// $six = 6 * extension_loaded('Core');
/// var_dump($six & 3, $six & "3", "ab" & "cd", "ab" & "c", "a" & "b", true & 1, null & 5);
/// ```
/// ```expect
/// int(2)
/// int(2)
/// string(2) "a`"
/// string(1) "a"
/// string(1) "`"
/// int(1)
/// int(0)
/// ```
///
/// A string with a number in front of it and anything else behind it is
/// the warning the arithmetic operators raise and the number php read;
/// one that opens with no number at all is refused. The three are
/// `SPEC(COMMUTATIVE)`, so `pass_two` may have swapped the operands, and
/// the error names them in the order the opline holds them: the constant
/// string was written first and is named second.
///
/// ```php
/// $one = extension_loaded('Core');
/// var_dump("6abc" & (3 * $one));
/// echo "x" & (1 * $one);
/// ```
/// ```expect exit=255
/// Warning: A non-numeric value encountered in %s on line 3
/// int(2)
///
/// Fatal error: Uncaught TypeError: Unsupported operand types: int & string in %s:4
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 4
/// ```
pub(crate) fn bitwise_and(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    bitwise(req, Bitwise::And, left, right)
}

/// `bitwise_not_function` (`Zend/zend_operators.c:1637`), what `~`
/// comes to: a long negated; a double through the long
/// `zend_dval_to_lval_safe` makes of it ([`double_to_long`]), so a
/// fraction is negated over the truncation and carries php's
/// deprecation; a string negated byte by byte, with php's `ZVAL_CHAR`
/// where it is one byte, and never a number read out of it; and php's
/// `TypeError` for everything else, `Cannot perform bitwise not on %s`
/// over `zend_zval_value_name`.
///
/// php's compiler folds the operator over a constant it can
/// (`zend_try_ct_eval_unary_op`), so each fence calls a native to leave
/// the opcode standing.
///
/// ```php
/// $six = 6 * extension_loaded('Core');
/// $ab = substr("xabx", 1, 2);
/// $one = 1.0 * extension_loaded('Core');
/// var_dump(~$six, ~-$six, ~$ab === "\x9e\x9d", ~substr($ab, 0, 1) === "\x9e", ~$one);
/// ```
/// ```expect
/// int(-7)
/// int(5)
/// bool(true)
/// bool(true)
/// int(-2)
/// ```
///
/// A double the long conversion loses something of is php's deprecation
/// and the long it was truncated to; `null`, `false`, `true`, an array
/// and an object are the `TypeError`, which names the value as
/// `zend_zval_value_name` names it.
///
/// ```php
/// $half = 1.5 * extension_loaded('Core');
/// var_dump(~$half);
/// $n = null;
/// try { echo ~$n; } catch (TypeError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// echo ~[];
/// ```
/// ```expect exit=255
///
/// Deprecated: Implicit conversion from float 1.5 to int loses precision in %s on line 3
/// int(-2)
/// TypeError: Cannot perform bitwise not on null
///
/// Fatal error: Uncaught TypeError: Cannot perform bitwise not on array in %s:6
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 6
/// ```
///
/// An object is asked first through its `do_operation` slot with no
/// right operand, `ZEND_TRY_UNARY_OBJECT_OPERATION(ZEND_BW_NOT)`, and
/// is the `TypeError` where the slot does not answer.
///
/// ```php
/// var_dump(~gmp_init(42));
/// try { ~new stdClass; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(GMP)#2 (1) {
///   ["num"]=>
///   string(3) "-43"
/// }
/// Cannot perform bitwise not on stdClass
/// ```
pub(crate) fn bitwise_not(req: &mut Request, value: &Value) -> Result<Value, Thrown> {
    let value = value.deref();
    if let ValueRef::Object(o) = value.as_ref() {
        let table = req.object_handlers(o.heap());
        if let Some(result) = table.do_operation(req, BinaryOp::BitwiseNot, &value, None)? {
            return Ok(result.hand_on());
        }
    }
    if let Some(n) = value.as_int() {
        return Ok(Value::int(!n));
    }
    if let Some(d) = value.as_float() {
        return Ok(Value::int(!double_to_long(req, d)));
    }
    if let Some(s) = value.as_php_string() {
        let bytes = s.as_bytes();
        // php's own fast case: one byte is the interned one-character
        // string, `ZVAL_CHAR`.
        if let [byte] = *bytes {
            return Ok(Value::string(req.string(&[!byte])?));
        }
        let negated = req.string_with(bytes.len(), |out| {
            for (out, b) in out.iter_mut().zip(bytes) {
                *out = !*b;
            }
        })?;
        return Ok(Value::string(negated));
    }
    let name = value_name(req, &value);
    Err(type_error(
        req,
        &format!("Cannot perform bitwise not on {name}"),
    ))
}

/// `concat_function` (`Zend/zend_operators.c:2017`): both sides as the
/// strings [`to_string`] makes of them, the left one first, and the
/// bytes of one after the other; the side that is empty leaves the other
/// as the value. A length past `ZSTR_MAX_LEN` is php's `Error`, `String
/// size overflow`, which no request under a memory limit reaches.
///
/// Each fence calls a native so that php's compiler leaves the opcode
/// standing: `zend_compile_binary_op` folds `.` over two constants and
/// converts a constant operand to a string where it stands, so a number
/// or a bool reaches the kernel only from a call, a CV or a temporary.
///
/// ```php
/// $n = 2; $t = true; $f = false; $d = 2.5;
/// echo 'a' . 'b', "|", 'n=' . $n, "|", $n . $n, "|", $t . '|' . $f . '|', $d . 'x', "|", '' . $n, "|", $n . '', "|", extension_loaded('Core') . '';
/// ```
/// ```expect
/// ab|n=2|22|1||2.5x|2|2|1
/// ```
///
/// Two constants fold where they stand, so #162's fourth program is
/// three `ECHO`s of a constant in both engines.
///
/// ```php
/// echo 7 % 2, 2 ** 3, 'a' . 'b';
/// ```
/// ```expect
/// 18ab
/// ```
///
/// A NAN is the warning `zval_get_string` raises, where the operand is
/// not a constant php's compiler converted first; that one is the
/// compiler's, raised before the op array runs (`Emit::stringified`).
///
/// ```php
/// $nan = NAN;
/// echo $nan . 'z';
/// ```
/// ```expect
///
/// Warning: unexpected NAN value was coerced to string in %s on line 3
/// NANz
/// ```
///
/// The joined string is the request's, one allocation of both sides'
/// bytes, and `memory_get_usage()` moves by that allocation's bin, a
/// constant string on one side as much as a string a native made
/// (#2661).
///
/// ```php
/// echo "joined, moved\n"; $a = str_repeat('a', 10); $b = str_repeat('b', 20); $c = $e = $g = null;
/// $m = memory_get_usage(); $c = $a . $b; echo 'strings ', memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $e = $a . 'literal'; echo 'literal ', memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $g = $b . str_repeat($a, 400); echo 'pages ', memory_get_usage() - $m, "\n";
/// ```
/// ```expect capture
/// joined, moved
/// strings 56
/// literal 48
/// pages 4096
/// ```
pub(crate) fn concat(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    let a = concat_operand(req, left)?;
    let b = match concat_operand(req, right) {
        Ok(b) => b,
        Err(thrown) => {
            req.release(Value::string(a));
            return Err(thrown);
        }
    };
    // An empty side hands the other side on as it is, with the owner
    // `to_string` took, which is php's `ZVAL_COPY` of the other operand.
    if a.is_empty() {
        req.release(Value::string(a));
        return Ok(Value::string(b));
    }
    if b.is_empty() {
        req.release(Value::string(b));
        return Ok(Value::string(a));
    }
    let joined = join(req, a, b);
    req.release(Value::string(a));
    req.release(Value::string(b));
    Ok(Value::string(joined?))
}

/// `ZEND_CONCAT` and `ZEND_FAST_CONCAT` over two strings, the arms of
/// php's handler before `concat_function` (`Z/Zend/zend_vm_def.h:382-431`,
/// `:3293-3341`), which its compiled code's `zend_jit_fast_concat_helper`
/// and `_tmp_helper` serve again (`Z/ext/opcache/jit/zend_jit_helpers.c:1824-1880`):
/// `a` with its owner where `a_kind` is a temporary the caller took out
/// of its slot, a view otherwise, and `b` a view, whose temporary the
/// caller frees after. An empty side that is no `CONST` hands the other
/// on with an owner more, `ZVAL_STR_COPY`, so `$x . ''` is a new string
/// and `$x . $e` is `$x`'s; a temporary left side grows where it stands
/// ([`Request::string_append`]), as `zend_string_extend` grows it when
/// its owner is the only one and php's alloc arm copies it otherwise; any
/// other pair is joined into a new string ([`join`]). The joined string
/// has one owner. What the join refuses, the memory limit or php's
/// `String size overflow`, is the caller's to raise at its opline.
///
/// A variable's empty side hands the other on, so it counts one more
/// owner; a constant `''` is joined as any string is; a temporary left
/// side grows into the joined string:
///
/// ```php
/// $a = str_repeat('ab', 2); $e = '';
/// debug_zval_dump($a . $e, $e . $a, $a . '', str_repeat('x', 3) . 'y');
/// ```
/// ```expect
/// string(4) "abab" refcount(3)
/// string(4) "abab" refcount(3)
/// string(4) "abab" refcount(1)
/// string(4) "xxxy" refcount(1)
/// ```
#[inline(always)] // both callers' kinds fold its arms, the handler's at compile time
pub(crate) fn concat_strings(
    req: &mut Request,
    (a, a_kind): (PhpString, OperandKind),
    (b, b_kind): (PhpString, OperandKind),
) -> Result<PhpString, Thrown> {
    let owned = matches!(a_kind, OperandKind::Tmp | OperandKind::Var);
    if a_kind != OperandKind::Const && a.is_empty() {
        req.addref(&Value::string(b));
        if owned {
            req.release(Value::string(a));
        }
        return Ok(b);
    }
    if b_kind != OperandKind::Const && b.is_empty() {
        if !owned {
            req.addref(&Value::string(a));
        }
        return Ok(a);
    }
    if owned {
        req.string_append(a, b)
    } else {
        join(req, a, b)
    }
}

/// A new string of `a`'s bytes and then `b`'s,
/// `zend_string_alloc(ZSTR_LEN(op1) + ZSTR_LEN(op2))` and the two
/// `memcpy` into it, whichever side is empty: what [`concat`] makes of
/// two non-empty sides and what `ZEND_CONCAT`'s alloc arm makes of any
/// pair it does not hand on (`Z/Zend/zend_vm_def.h:419-430`). Neither
/// side's owner is given up. A length past `ZSTR_MAX_LEN` is php's
/// `Error`, `String size overflow`.
///
/// ```php
/// $a = 'ab';
/// $b = str_repeat('c', 3);
/// var_dump($a . $b, $a . '', '' . $b);
/// ```
/// ```expect
/// string(5) "abccc"
/// string(2) "ab"
/// string(3) "ccc"
/// ```
pub(crate) fn join(req: &mut Request, a: PhpString, b: PhpString) -> Result<PhpString, Thrown> {
    if a.len() > ZSTR_MAX_LEN - b.len() {
        return Err(error(req, "String size overflow"));
    }
    let joined = req.string_with(a.len() + b.len(), |bytes| {
        let (head, tail) = bytes.split_at_mut(a.len());
        head.copy_from_slice(a.as_bytes());
        tail.copy_from_slice(b.as_bytes());
    })?;
    // `ZSTR_COPY_CONCAT_PROPERTIES_BOTH`: valid UTF-8 where both sides
    // were known to be.
    if a.is_valid_utf8() && b.is_valid_utf8() {
        joined.mark_valid_utf8();
    }
    Ok(joined)
}

/// One side of `concat_function` as a string, `zval_get_string_func`,
/// and php's `if (UNEXPECTED(EG(exception))) return FAILURE` after it: a
/// handler the conversion's warning reached that threw leaves the place
/// `.=` writes as it stood, where php's `Array` would have been joined.
///
/// ```php
/// set_error_handler(function ($type, $msg) { throw new Exception($msg); });
/// $a = [];
/// try { $a .= "foo"; } catch (Exception $e) { var_dump($a); }
/// $a = "foo";
/// try { $a .= []; } catch (Exception $e) { var_dump($a); }
/// try { $b = [] . "x"; } catch (Exception $e) { var_dump(isset($b)); }
/// ```
/// ```expect
/// array(0) {
/// }
/// string(3) "foo"
/// bool(false)
/// ```
fn concat_operand(req: &mut Request, value: &Value) -> Result<PhpString, Thrown> {
    let text = to_string(req, value)?;
    if req.is_pending() && value.deref().as_php_string().is_none() {
        req.release(Value::string(text));
        return Err(Thrown::of(None));
    }
    Ok(text)
}

/// `concat_function` where `result` is `op1` (`Z/Zend/zend_operators.c:2125`),
/// `$a .= $b`: the value is written back where `left` came from, so the
/// left string is grown where it stands when the place held its one
/// owner, and copied only when it is interned or shared
/// ([`Request::string_append`], `zend_string_extend`). A loop of `.=`
/// is then linear in what it appends. `ASSIGN_OP` over a CV,
/// `ASSIGN_DIM_OP` over an array's element, `ASSIGN_OBJ_OP` over a
/// declared property and `ASSIGN_STATIC_PROP_OP` call it where php's
/// `zend_binary_op(var_ptr, var_ptr, value)` stands; a property served
/// by `__get` and `__set` or an `ArrayAccess` offset is read and written
/// back, as php's is, and takes [`concat`].
///
/// php converts both sides first, holding one more owner of the left
/// string while a `__toString` on the right is served, and only then
/// destroys what the place holds (`i_zval_ptr_dtor(result)`) and leaves
/// it `null` until the joined string lands; `vacate` is that, the caller's
/// place given up, called once both sides are strings, and an `Err`
/// leaves the place as it stood. A right side that is
/// already a string, beside a left one, is read where it stands and
/// takes no owner, as php's `Z_STR_P(op2)`, so `$s .= $s` grows `$s` in
/// place too. The `Ok` is the value for the place, with one owner.
///
/// A string two variables hold is copied, and the other keeps its
/// bytes; a reference's cell is the one place, so both names read the
/// append:
///
/// ```php
/// $a = str_repeat('ab', 2); $b = $a; $a .= 'c'; $c = 'lit'; $c .= $c;
/// $r = &$c; $c .= '!'; var_dump($a, $b, $c, $r, 'lit');
/// ```
/// ```expect
/// string(5) "ababc"
/// string(4) "abab"
/// string(7) "litlit!"
/// string(7) "litlit!"
/// string(3) "lit"
/// ```
///
/// The right side is converted while the variable still holds the left
/// string, read here through the reference a class's property holds, and
/// a value that is no string is converted before it is grown:
///
/// ```php
/// class T { static $r; function __toString(): string { echo "[", T::$r, "]"; return "t"; } }
/// $s = "ab"; T::$r = &$s; $s .= new T; $n = 12; $n .= 3; var_dump($s, $n);
/// ```
/// ```expect
/// [ab]string(3) "abt"
/// string(3) "123"
/// ```
///
/// What the appends take is what php's `erealloc` takes: the slot grows
/// with its bin, then a `run` of pages with the pages after it, and the
/// peak is where the larger of the two footprints put it, not both.
///
/// ```php
/// $s = str_repeat('x', 100); memory_reset_peak_usage(); $m = memory_get_usage();
/// for ($i = 0; $i < 1000; $i++) { $s .= 'abcdefghij'; }
/// var_dump(strlen($s), memory_get_usage() - $m, memory_get_peak_usage() - $m);
/// ```
/// ```expect
/// int(10100)
/// int(12128)
/// int(12128)
/// ```
///
/// One byte at a time from empty, `memory_get_usage()` steps where the
/// string's allocation crosses into the next bin, then the next `run`
/// of pages, each step php's (#2661); a string another variable holds is
/// copied, its new allocation counted whole. The program writes its
/// first line before it measures, since the first byte a request writes
/// holds the thirty-two of the default mimetype.
///
/// ```php
/// echo "appended, moved\n"; $s = ''; $d = $p = 0; $m = memory_get_usage();
/// for ($i = 1; $i <= 5000; $i++) { $s .= 'x'; $d = memory_get_usage() - $m; if ($d !== $p) echo $i, ' ', $d, "\n"; $p = $d; }
/// $t = $s; $m = memory_get_usage(); $t .= 'y'; echo 'shared ', memory_get_usage() - $m, "\n";
/// ```
/// ```expect capture
/// appended, moved
/// 2 32
/// 8 40
/// 16 48
/// 24 56
/// 32 64
/// 40 80
/// 56 96
/// 72 112
/// 88 128
/// 104 160
/// 136 192
/// 168 224
/// 200 256
/// 232 320
/// 296 384
/// 360 448
/// 424 512
/// 488 640
/// 616 768
/// 744 896
/// 872 1024
/// 1000 1280
/// 1256 1536
/// 1512 1792
/// 1768 2048
/// 2024 2560
/// 2536 3072
/// 3048 4096
/// 4072 8192
/// shared 8192
/// ```
///
/// A declared property, a typed one over a string, a class's property
/// and an array's element are each such a place, and each grows where it
/// stands, the peak not moving past what the four already held; the
/// copy an array's separation made, the string read out of a property
/// and the class's own string appended to it keep their bytes:
///
/// ```php
/// class C { public $p; public string $q = ''; public static $s; }
/// $o = new C; $o->p = str_repeat('a', 5000); $o->q = str_repeat('a', 5000); C::$s = str_repeat('a', 5000); $a = ['k' => str_repeat('a', 5000)];
/// $t = str_repeat('b', 2000); memory_reset_peak_usage(); $m = memory_get_usage();
/// $o->p .= $t; $o->q .= $t; C::$s .= $t; $a['k'] .= $t;
/// var_dump(memory_get_peak_usage() - $m, strlen($o->p), strlen($o->q), strlen(C::$s), strlen($a['k']));
/// $b = $a; $a['k'] .= '!'; $p = $o->p; $o->p .= '?'; $s = C::$s; C::$s .= $s;
/// var_dump(strlen($b['k']), strlen($a['k']), strlen($p), strlen($o->p), strlen($s), strlen(C::$s));
/// ```
/// ```expect
/// int(0)
/// int(7000)
/// int(7000)
/// int(7000)
/// int(7000)
/// int(7000)
/// int(7001)
/// int(7000)
/// int(7001)
/// int(7000)
/// int(14000)
/// ```
///
/// An append the limit refuses leaves each place holding the string it
/// held, as php's `zend_string_extend` leaves it, for the shutdown
/// functions to read: a class's property, an array's element, a declared
/// property and a reference's cell, as a variable
/// (`php_heap`'s `Mm::realloc_huge`). A left side converted to a string
/// had its place `null`ed before the append, and it stays `null`:
///
/// ```php
/// ini_set('memory_limit', '7M');
/// class C { public static $s; }
/// C::$s = str_repeat('x', 3000000);
/// register_shutdown_function(function () { var_dump(strlen(C::$s)); });
/// C::$s .= C::$s;
/// ```
/// ```expect exit=255
/// Fatal error: Allowed memory size of 7340032 bytes exhausted (tried to allocate 6000032 bytes) in %s on line %d
/// int(3000000)
/// ```
///
/// ```php
/// ini_set('memory_limit', '7M');
/// $a = ['k' => str_repeat('x', 3000000)];
/// register_shutdown_function(function () { var_dump(strlen($GLOBALS['a']['k'])); });
/// $a['k'] .= $a['k'];
/// ```
/// ```expect exit=255
/// Fatal error: Allowed memory size of 7340032 bytes exhausted (tried to allocate 6000032 bytes) in %s on line %d
/// int(3000000)
/// ```
///
/// ```php
/// ini_set('memory_limit', '7M');
/// class O { public $p; }
/// $o = new O; $o->p = str_repeat('x', 3000000);
/// register_shutdown_function(function () { var_dump(strlen($GLOBALS['o']->p)); });
/// $o->p .= $o->p;
/// ```
/// ```expect exit=255
/// Fatal error: Allowed memory size of 7340032 bytes exhausted (tried to allocate 6000032 bytes) in %s on line %d
/// int(3000000)
/// ```
///
/// ```php
/// ini_set('memory_limit', '7M');
/// $s = str_repeat('x', 3000000); $r = &$s;
/// register_shutdown_function(function () { var_dump(strlen($GLOBALS['s'])); });
/// $r .= $r;
/// ```
/// ```expect exit=255
/// Fatal error: Allowed memory size of 7340032 bytes exhausted (tried to allocate 6000032 bytes) in %s on line %d
/// int(3000000)
/// ```
///
/// ```php
/// ini_set('memory_limit', '7M');
/// $n = 12; $t = str_repeat('x', 3000000);
/// register_shutdown_function(function () { var_dump($GLOBALS['n']); });
/// $n .= $t;
/// ```
/// ```expect exit=255
/// Fatal error: Allowed memory size of 7340032 bytes exhausted (tried to allocate 3000032 bytes) in %s on line %d
/// NULL
/// ```
pub(crate) fn concat_assign(
    req: &mut Request,
    left: &Value,
    right: &Value,
    vacate: impl FnOnce(&mut Request),
    reinstate: impl FnOnce(&mut Request, Value),
) -> Result<Value, Thrown> {
    // Only a place that holds a string is given up without running
    // anything, so the right side is read unowned only then: what a
    // destructor `vacate` served could free is held.
    let quiet = left.deref().as_php_string().is_some();
    let a = concat_operand(req, left)?;
    let right = right.deref();
    let (b, owned) = match right.as_php_string() {
        Some(b) if quiet => (b, false),
        _ => match concat_operand(req, &right) {
            Ok(b) => (b, true),
            Err(thrown) => {
                req.release(Value::string(a));
                return Err(thrown);
            }
        },
    };
    if a.len() > ZSTR_MAX_LEN - b.len() {
        req.release(Value::string(a));
        if owned {
            req.release(Value::string(b));
        }
        return Err(error(req, "String size overflow"));
    }
    vacate(req);
    // An empty side hands the other side on, `ZVAL_STR_COPY` of it.
    if a.is_empty() {
        if !owned {
            req.addref(&Value::string(b));
        }
        req.release(Value::string(a));
        return Ok(Value::string(b));
    }
    let joined = if b.is_empty() {
        Ok(a)
    } else {
        req.string_append(a, b)
    };
    if owned {
        req.release(Value::string(b));
    }
    match joined {
        Ok(joined) => Ok(Value::string(joined)),
        // The limit refused the append, and the left string stands as it
        // was. php grows it with `zend_string_extend` while the place
        // still holds it, so the place keeps it for the shutdown
        // functions; a left side converted to a string had its place
        // `null`ed first, `ZVAL_NULL(result)`, and stays so.
        Err(thrown) => {
            if quiet {
                reinstate(req, Value::string(a));
            } else {
                req.release(Value::string(a));
            }
            Err(thrown)
        }
    }
}

/// `zend_compare` (`Z/Zend/zend_operators.c:2312`), the one ordering
/// every comparison opcode reads: [`php_heap::compare`] over the scalar
/// pairs, which the compiler folds `<` through as well, and here the
/// pairs an array, an object or a resource makes. Two arrays compare as
/// `zend_compare_arrays`, by count and then element by element on the
/// left one's keys; a null or a bool against any of the three compares
/// as the other side's truth; a resource against a number or a string
/// is its id, converted silently as `_zendi_convert_scalar_to_number_silent`
/// converts both; and an array against a number or a string is the
/// greater, whichever side it is on. An object is equal to itself and
/// greater than a null; against anything else it is the `compare` slot
/// of its handler table (design §5.4), which at `Std` is
/// `zend_std_compare_objects` ([`Request::compare_objects`]): two of
/// one class property by property, two of different classes
/// `ZEND_UNCOMPARABLE`, which is `1`, and an object against a scalar
/// through `cast_object`. The `Err` is what a property's own comparison
/// or a `__toString` threw, and the `Error` the recursion guard raises.
///
/// The long pair and the double pairs are the arms php's `ZEND_IS_SMALLER`
/// handler takes before it calls `zend_compare`, and `zend_compare`'s own
/// first arms, so the kernel is the whole of it.
///
/// ```php
/// $one = extension_loaded('Core');
/// var_dump(1 < $one + 1, 2 < $one, 1.5 < $one + 1, $one + 1 < 1.5, "10" < $one + 9, "abc" < $one, 9223372036854775807 < $one + 9223372036854775807);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
///
/// A NAN is after everything, so it is smaller than nothing and nothing
/// is smaller than it, and the opcode warns of nothing on the way.
///
/// ```php
/// $nan = NAN; $one = extension_loaded('Core');
/// var_dump($nan < $one, $one < $nan, $nan < "1", "1" < $nan);
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
///
/// Two objects of one class compare declared slot by declared slot in
/// declaration order, each pair by this kernel, so `==` is true when
/// every property is loosely equal and `<` orders on the first pair
/// that differs; two objects of two classes are uncomparable, `1`
/// whichever side each is on, so neither `==` nor `<` and `<=>` is `1`
/// both ways.
///
/// ```php
/// class M { public $a = 1; public $b; } class N { public $a = 1; }
/// $m = new M; $n = new M; $p = new M; $p->a = 2; $o = new N;
/// var_dump($m == $n, $m == $p, $m < $p, $m <=> $p, $p <=> $m, $m == $o, $m < $o, $m <=> $o, $o <=> $m);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// int(-1)
/// int(1)
/// bool(false)
/// bool(false)
/// int(1)
/// int(1)
/// ```
///
/// An object against an int or a float is `cast_object`'s answer, which
/// no class here has, so it is php's notice and then `1` or `1.0` in
/// the object's place; against a string it is the class's `__toString`,
/// and the greater side for a class with none; against a null or a bool
/// its truth, which is true, with no notice. An object with a dynamic
/// property compares as `zend_compare_symbol_tables` compares the two
/// rebuilt tables, the count first; that arm waits on #335 for a witness,
/// since the engine's creation of one raises no deprecation yet.
///
/// ```php
/// class M { public $a = 1; }
/// $m = new M;
/// var_dump($m == 1, $m < 2, $m <=> 1, 1 <=> $m, $m == 1.0, $m == 2, $m == "1", $m == null, $m == true, $m == []);
/// ```
/// ```expect
///
/// Notice: Object of class M could not be converted to int in %s on line 4
///
/// Notice: Object of class M could not be converted to int in %s on line 4
///
/// Notice: Object of class M could not be converted to int in %s on line 4
///
/// Notice: Object of class M could not be converted to int in %s on line 4
///
/// Notice: Object of class M could not be converted to float in %s on line 4
///
/// Notice: Object of class M could not be converted to int in %s on line 4
/// bool(true)
/// bool(true)
/// int(0)
/// int(0)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
///
/// A property walk that reaches the object it started from is php's
/// `Error`, since the object is protected for the walk
/// (`Z_PROTECT_RECURSION_P`).
///
/// ```php
/// class M { public $a; }
/// $r = new M; $r->a = $r; $s = new M; $s->a = $s;
/// var_dump($r == $s);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Nesting level too deep - recursive dependency? in %s:4
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 4
/// ```
///
/// A pair whose dynamic properties hold each other never reaches that
/// guard: either object carrying a dynamic table sends php down
/// `zend_compare_symbol_tables`, and the `Error` is `zend_hash_compare`'s
/// own, over the left table ([`compare_arrays`]).
///
/// ```php
/// class M {}
/// $a = new M; $b = new M; $a->x = $b; $b->x = $a;
/// var_dump($a == $b);
/// ```
/// ```expect exit=255
/// Deprecated: Creation of dynamic property M::$x is deprecated in %s on line 3
///
/// Deprecated: Creation of dynamic property M::$x is deprecated in %s on line 3
///
/// Fatal error: Uncaught Error: Nesting level too deep - recursive dependency? in %s:4
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 4
/// ```
pub(crate) fn compare(req: &mut Request, left: &Value, right: &Value) -> Result<Ordering, Thrown> {
    if left.as_php_ref().is_some() || right.as_php_ref().is_some() {
        return compare_dereferenced(req, left, right);
    }
    match (left.as_ref(), right.as_ref()) {
        (ValueRef::Array(a), ValueRef::Array(b)) => compare_arrays(req, a, b),
        // `TYPE_PAIR(IS_OBJECT, IS_OBJECT)`: one object is equal to
        // itself, and two are the left one's `compare` handler's, which
        // `ZEND_UNCOMPARABLE` answers as `1`. Here and not in
        // [`compare_values`], so a nest of objects compared level by
        // level keeps this frame between two handlers and no other
        // (#3159).
        (ValueRef::Object(a), ValueRef::Object(b)) => {
            if a == b {
                Ok(Ordering::Equal)
            } else {
                handler_compare(req, a, left, right)
            }
        }
        _ => compare_values(req, left, right),
    }
}

/// `Z_OBJ_HANDLER_P(object, compare)(op1, op2)`: the comparison
/// `object`'s handler table answers for the two sides, `None`,
/// `ZEND_UNCOMPARABLE`, as `1`.
#[inline]
fn handler_compare(
    req: &mut Request,
    object: PhpObject,
    left: &Value,
    right: &Value,
) -> Result<Ordering, Thrown> {
    Ok(req
        .object_handlers(object.heap())
        .compare(req, left, right)?
        .unwrap_or(Ordering::Greater))
}

/// [`compare`] over what a reference on either side holds. A call of its
/// own, so the values it reads out are off the stack of a nest compared
/// level by level through [`compare`], which holds none (#3159).
#[inline(never)]
fn compare_dereferenced(
    req: &mut Request,
    left: &Value,
    right: &Value,
) -> Result<Ordering, Thrown> {
    compare(req, &left.deref(), &right.deref())
}

/// [`compare`] past `TYPE_PAIR(IS_ARRAY, IS_ARRAY)` and
/// `TYPE_PAIR(IS_OBJECT, IS_OBJECT)`, which it never sees: a call of its
/// own, so that what the other pairs take is off the stack while two
/// nested arrays or objects are compared level by level (#2221, #3159).
#[inline(never)]
fn compare_values(req: &mut Request, left: &Value, right: &Value) -> Result<Ordering, Thrown> {
    if let (Some(a), Some(b)) = (scalar(left), scalar(right)) {
        return Ok(php_heap::compare(a, b, req.ini().precision()));
    }
    Ok(match (left.as_ref(), right.as_ref()) {
        // `TYPE_PAIR(IS_OBJECT, IS_NULL)` and its mirror: an object is
        // greater than a null whatever its truth.
        (ValueRef::Object(_), ValueRef::Undef | ValueRef::Null) => Ordering::Greater,
        (ValueRef::Undef | ValueRef::Null, ValueRef::Object(_)) => Ordering::Less,
        // php's default arm: a null or a bool as the other side's truth,
        // which is also what its own `IS_NULL` and `IS_OBJECT` pairs
        // answer and what `cast_object` to `_IS_BOOL` answers for an
        // object; then an object's handler, then an array, then both
        // sides as numbers.
        (ValueRef::Undef | ValueRef::Null | ValueRef::Bool(_), _)
        | (_, ValueRef::Undef | ValueRef::Null | ValueRef::Bool(_)) => {
            is_true(req, left)?.cmp(&is_true(req, right)?)
        }
        // `Z_OBJ_HANDLER_P(object, compare)(op1, op2)`, the object side's
        // table; `None` is `ZEND_UNCOMPARABLE`, which is `1`.
        (ValueRef::Object(o), _) | (_, ValueRef::Object(o)) => {
            handler_compare(req, o, left, right)?
        }
        (ValueRef::Array(_), _) => Ordering::Greater,
        (_, ValueRef::Array(_)) => Ordering::Less,
        (a, b) => php_heap::compare(silent_number(a), silent_number(b), req.ini().precision()),
    })
}

/// `is_smaller_function` (`Z/Zend/zend_operators.c:2669`): whether
/// [`compare`] orders the left side first, which is what `ZEND_IS_SMALLER`
/// leaves in its temporary and what `<` reads.
///
/// ```php
/// for ($i = 0; $i < 3; $i++) { echo $i; }
/// ```
/// ```expect
/// 012
/// ```
pub(crate) fn is_smaller(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    Ok(Value::bool(compare(req, left, right)? == Ordering::Less))
}

/// `is_smaller_or_equal_function` (`Z/Zend/zend_operators.c:2543`):
/// whether [`compare`] does not order the right side first, which is
/// what `ZEND_IS_SMALLER_OR_EQUAL` leaves and what `<=` reads, and `>=`
/// with its operands swapped. A NAN orders after everything on either
/// side, so it is `<=` nothing and nothing is `<=` it.
///
/// ```php
/// $i = extension_loaded('Core') + 0; $nan = NAN;
/// var_dump(1 <= $i, 2 <= $i, "abc" <= "ab" . $i, "10" <= "9" . $i, 1.5 <= "1.5" . $i, 2.5 <= "1.5" . $i, $nan <= $i, $i <= $nan, $i >= 1, $i >= 2);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
pub(crate) fn is_smaller_or_equal(
    req: &mut Request,
    left: &Value,
    right: &Value,
) -> Result<Value, Thrown> {
    Ok(Value::bool(compare(req, left, right)? != Ordering::Greater))
}

/// `is_equal_function` (`Z/Zend/zend_operators.c:2522`): whether
/// [`compare`] orders the two sides the same, which is what
/// `ZEND_IS_EQUAL` leaves and what `==` reads: `1 == "1"`, `"1e3" ==
/// "1000"` and `null == false` through the same conversions `<` makes,
/// and a non-numeric string against a number by the number's text
/// since 8.0.
///
/// ```php
/// $i = extension_loaded('Core') + 0;
/// var_dump($i == 1, $i == 2, "abc" == "ab" . $i, "1e3" == "100" . $i - 1, 1.5 == "1." . 5 * $i, 1.5 == "abc" . $i, null == $i - 1, "1" == "0" . $i);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// ```
pub(crate) fn is_equal(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    Ok(Value::bool(compare(req, left, right)? == Ordering::Equal))
}

/// `is_not_equal_function` (`Z/Zend/zend_operators.c:2529`): the
/// negation of [`is_equal`], which is what `ZEND_IS_NOT_EQUAL` leaves
/// and what `!=` and `<>` read. A NAN is not equal to anything, itself
/// included, since [`compare`] orders it after.
///
/// ```php
/// $i = extension_loaded('Core') + 0; $nan = NAN;
/// var_dump($i != 1, $i != 2, "abc" != "ab" . $i, "10" <> "1e" . $i, 1.5 != "1." . 5 * $i, 1.5 != "abc" . $i, $nan != $nan);
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// ```
pub(crate) fn is_not_equal(
    req: &mut Request,
    left: &Value,
    right: &Value,
) -> Result<Value, Thrown> {
    Ok(Value::bool(compare(req, left, right)? != Ordering::Equal))
}

/// `compare_function` (`Z/Zend/zend_operators.c:2253`): [`compare`]'s
/// ordering as the long `-1`, `0` or `1`, which is what `ZEND_SPACESHIP`
/// leaves and what `<=>` reads. A NAN on either side is `1`, since
/// `ZEND_THREEWAY_COMPARE` answers `1` for a pair that is neither equal
/// nor ordered.
///
/// ```php
/// $i = extension_loaded('Core') + 0; $nan = NAN;
/// var_dump($i <=> 2, $i <=> 1, $i <=> 0, "abc" <=> "abd" . $i, "1" . $i - 1 <=> "9", 1.5 <=> "1." . 5 * $i, 1.5 <=> "abc" . $i, $nan <=> $i, $i <=> $nan);
/// ```
/// ```expect
/// int(-1)
/// int(0)
/// int(1)
/// int(-1)
/// int(1)
/// int(0)
/// int(-1)
/// int(1)
/// int(1)
/// ```
pub(crate) fn spaceship(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    Ok(Value::int(match compare(req, left, right)? {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }))
}

/// `zend_is_identical` (`Z/Zend/zend_operators.c:2479`): the same type,
/// and then the same value. The scalars are [`php_heap::identical`]'s,
/// a string by its bytes and a double by `==`, so `1 === "1"` and
/// `null === false` are false and a NAN is identical to nothing. Two
/// arrays are identical when `zend_hash_compare` with
/// `hash_zval_identical_function` and `ordered` on finds no difference:
/// the same count, and walked together the same key at each step, an
/// integer key against a string key never the same, and identical
/// values, each side's reference followed first. An object is identical
/// to itself and a resource to itself, by id, so two resources of
/// one kind are not. A reference is followed on both sides
/// (`GET_OP*_ZVAL_PTR_DEREF`).
///
/// ```php
/// $i = extension_loaded('Core') + 0; $t = extension_loaded('Core'); $nan = NAN;
/// var_dump($i === 1, $i === 1.0, $t === true, $t === 1, "abc" === "ab" . chr(99 * $i), "1" === "01" . $i - 1, 1.5 === "1." . 5 * $i, $nan === $nan, null === $t - 1);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
///
/// Two arrays walk as [`compare_arrays`] walks, under the protection of
/// the left one, so an array that holds itself is php's `Error` once
/// the walk comes back to it; one table against itself is identical
/// before anything is walked.
///
/// ```php
/// $a = [&$a];
/// try { var_dump($a === [[]]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $l = [1, [2]];
/// var_dump($a === $a, $l === [1, [2]], $l === [1 => [2], 0 => 1]);
/// ```
/// ```expect
/// Nesting level too deep - recursive dependency?
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
pub(crate) fn identical(req: &mut Request, left: &Value, right: &Value) -> Result<bool, Thrown> {
    let (left, right) = (left.deref(), right.deref());
    if let (Some(a), Some(b)) = (left.as_array(), right.as_array()) {
        return identical_arrays(req, a, b);
    }
    Ok(identical_values(&left, &right))
}

/// [`identical`] past two arrays, which it never sees: a call of its
/// own, as [`compare_values`] is, so that what the other pairs take is
/// off the stack while two nested arrays are walked level by level.
#[inline(never)]
fn identical_values(left: &Value, right: &Value) -> bool {
    if let (Some(a), Some(b)) = (scalar(left), scalar(right)) {
        return php_heap::identical(a, b);
    }
    match (left.as_ref(), right.as_ref()) {
        (ValueRef::Object(a), ValueRef::Object(b)) => a == b,
        (ValueRef::Resource(a), ValueRef::Resource(b)) => a == b,
        _ => false,
    }
}

/// `zend_hash_compare` with `hash_zval_identical_function` and
/// `ordered` on (`Z/Zend/zend_hash.c:3206`), for [`identical`]: the
/// protection [`compare_arrays`] takes, over the ordered walk, the same
/// count and at each step the same key and identical values.
fn identical_arrays(req: &mut Request, left: PhpArray, right: PhpArray) -> Result<bool, Thrown> {
    if left.heap().is_same(right.heap()) {
        return Ok(true);
    }
    if left.is_recursive() {
        return Err(nesting_too_deep(req));
    }
    if left.len() != right.len() {
        return Ok(false);
    }
    crate::api::raising::call_stack_end(req)?;
    left.protect();
    let walked = identical_entries(req, left, right);
    left.unprotect();
    walked
}

/// The ordered walk of `zend_hash_compare_impl` under the protection
/// [`identical_arrays`] takes.
fn identical_entries(req: &mut Request, left: PhpArray, right: PhpArray) -> Result<bool, Thrown> {
    // `zend_hash_compare_impl`'s ordered loop: `ht1`'s rows walked, each
    // beside the next live bucket of `ht2`.
    let mut at = 0;
    let differs = walk::foreach(req, left, walk::Hold::None, |req, row| -> walk::Step<()> {
        let Some((found, key, value)) = right.live_from(at) else {
            return Ok(ControlFlow::Break(()));
        };
        at = found.saturating_add(1);
        if *row.key() != key || !identical(req, row.value(), &value)? {
            return Ok(ControlFlow::Break(()));
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(differs.is_none())
}

/// `is_identical_function` (`Z/Zend/zend_operators.c:2508`):
/// [`identical`] as the bool `ZEND_IS_IDENTICAL` leaves, which `===`
/// reads where neither side is a constant null or bool; against one of
/// those the compiler emits `ZEND_TYPE_CHECK` instead.
///
/// ```php
/// $i = extension_loaded('Core') + 0;
/// var_dump($i === 1, "a" === chr(96 + $i), 1.0 === "1" . $i - 1, 1.0 === $i * 1.0);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
pub(crate) fn is_identical(
    req: &mut Request,
    left: &Value,
    right: &Value,
) -> Result<Value, Thrown> {
    Ok(Value::bool(identical(req, left, right)?))
}

/// `is_not_identical_function` (`Z/Zend/zend_operators.c:2515`): the
/// negation of [`identical`], which is what `ZEND_IS_NOT_IDENTICAL`
/// leaves and what `!==` reads.
///
/// ```php
/// $i = extension_loaded('Core') + 0; $nan = NAN;
/// var_dump($i !== 1, $i !== "1", "abc" !== "ab" . chr(98 + $i), "1" !== "01" . $i - 1, 1.5 !== "1." . 5 * $i, $nan !== $nan);
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
pub(crate) fn is_not_identical(
    req: &mut Request,
    left: &Value,
    right: &Value,
) -> Result<Value, Thrown> {
    Ok(Value::bool(!identical(req, left, right)?))
}

/// The scalar a value is, for [`php_heap::compare`], as
/// [`Scalar::of`] reads its zval; nothing for an array, an object, a
/// resource or a reference not yet followed. No value is a constant
/// expression, an `IS_INDIRECT` or an `IS_PTR`, which its reader made
/// `null` ([`Value::of_slot`]).
fn scalar(value: &Value) -> Option<Scalar<'_>> {
    let zval = value.as_zval();
    // php's `IS_NULL` arm; an uninitialised zval reads as null here.
    if zval.is_undef() {
        return Some(Scalar::Null);
    }
    Scalar::of(zval)
}

/// `_zendi_convert_scalar_to_number_silent` over what [`compare`]'s
/// default arm has left: a resource is its id, and a string the number
/// it opens with or zero, read with `allow_errors` and no warning. A
/// number is itself; nothing else reaches here.
fn silent_number(value: ValueRef) -> Scalar<'static> {
    match value {
        ValueRef::Int(n) => Scalar::Long(n),
        ValueRef::Float(f) => Scalar::Double(f),
        ValueRef::Resource(r) => Scalar::Long(r.id()),
        ValueRef::Str(s) => match numeric_prefix(s.as_bytes()) {
            Some((Number::Long(n), _)) => Scalar::Long(n),
            Some((Number::Double(d), _)) => Scalar::Double(d),
            None => Scalar::Long(0),
        },
        _ => Scalar::Long(0),
    }
}

/// `zend_compare_arrays`, which is `zend_hash_compare` with
/// `hash_zval_compare_function` and `ordered` off
/// (`Z/Zend/zend_hash.c:3205`): one table against itself is equal, a
/// table the walk is already inside is php's `Error`, and otherwise the
/// counts first, then each key of the left array looked up in the right
/// one, a key the right one lacks making the left the greater, and the
/// two values compared by [`compare`] until one pair differs; also
/// `zend_compare_symbol_tables`, the same walk over two objects' tables,
/// which is what ends a pair of objects whose dynamic properties hold
/// each other.
///
/// php protects the left table alone and says why: the right one may be
/// reached from it, and protecting both would call a shared table a
/// cycle.
///
/// php asks no stack limit here, so two arrays nested deeper than
/// `zend.max_allowed_stack_size` allows are compared whole, as long as
/// the thread holds them; the walk is held to the thread's floor
/// ([`crate::api::raising::call_stack_end`]), which the small-stack
/// regime's 256K does not move, and [`identical_arrays`] with it.
///
/// ```php
/// $a = $b = [];
/// for ($i = 0; $i < 1000; $i++) { $a = [$a]; $b = [$b]; }
/// var_dump($a == $b, $a === $b, $a <=> [[1]]);
/// ```
/// ```expect regime=small-stack
/// bool(true)
/// bool(true)
/// int(1)
/// ```
///
/// Two tables that are not one are held for the walk, as
/// `zend_compare_symbol_tables` (`Z/Zend/zend_operators.c:3478`) holds
/// them with `GC_TRY_ADDREF`, so a table a compared value's `__toString`
/// or `compare` lets go of stands until the walk ends; each is given up
/// after it with `GC_TRY_DTOR_NO_REF`, which offers one another owner
/// still holds to the cycle collector as a possible root. An immutable
/// table takes neither, and `===` holds nothing ([`identical_arrays`]):
///
/// ```php
/// function t() {
///     $a = [1]; $a[] = 2; $b = [1]; $b[] = 3; $c = [$a, 0]; $d = [$b, 0];
///     gc_collect_cycles();
///     var_dump($a == $b, gc_status()['roots']);
///     var_dump($a === $b, $c === $d, gc_status()['roots']);
///     var_dump($c < $d, gc_status()['roots']);
/// }
/// t();
/// ```
/// ```expect
/// bool(false)
/// int(2)
/// bool(false)
/// bool(false)
/// int(2)
/// bool(true)
/// int(4)
/// ```
///
/// A call of its own, so the walk it keeps across each element's compare
/// is off the stack of a nest of objects compared through [`compare`]
/// (#3159).
#[inline(never)]
fn compare_arrays(req: &mut Request, left: PhpArray, right: PhpArray) -> Result<Ordering, Thrown> {
    if left.heap().is_same(right.heap()) {
        return Ok(Ordering::Equal);
    }
    // `GC_TRY_ADDREF` and `GC_TRY_DTOR_NO_REF` touch no immutable table,
    // so two constant arrays are compared with nothing held.
    let held = (!left.heap().is_immutable() || !right.heap().is_immutable())
        .then(|| hold_tables(req, left, right));
    let ordering = compare_held(req, left, right);
    if let Some((left, right)) = held {
        req.release(left);
        req.release(right);
    }
    ordering
}

/// One owner of each of two tables [`compare_arrays`] walks, as
/// `GC_TRY_ADDREF` takes it, nothing of an immutable one.
fn hold_tables(req: &Request, left: PhpArray, right: PhpArray) -> (Value, Value) {
    let held = (Value::array(left), Value::array(right));
    req.addref(&held.0);
    req.addref(&held.1);
    held
}

/// `zend_hash_compare` over two tables [`compare_arrays`] holds: the
/// left one already inside the walk is php's `Error`, and otherwise the
/// walk under its protection.
#[inline(always)] // the body of `compare_arrays`, apart for its hold
fn compare_held(req: &mut Request, left: PhpArray, right: PhpArray) -> Result<Ordering, Thrown> {
    if left.is_recursive() {
        return Err(nesting_too_deep(req));
    }
    crate::api::raising::call_stack_end(req)?;
    left.protect();
    let ordering = compare_entries(req, left, right);
    left.unprotect();
    ordering
}

/// php's `Error` for a table a comparison's walk is already inside,
/// `zend_hash_compare`'s and `zend_std_compare_objects`' alike.
///
/// ```php
/// $o = new stdClass; $o->self = $o;
/// $p = new stdClass; $p->self = $p;
/// try { var_dump($o == $p); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Nesting level too deep - recursive dependency?
/// ```
pub(crate) fn nesting_too_deep(req: &mut Request) -> Thrown {
    crate::api::raising::error(req, "Nesting level too deep - recursive dependency?")
}

/// `zend_hash_compare_impl` under the protection
/// [`compare_arrays`] takes.
fn compare_entries(req: &mut Request, left: PhpArray, right: PhpArray) -> Result<Ordering, Thrown> {
    if left.len() != right.len() {
        return Ok(left.len().cmp(&right.len()));
    }
    // `zend_hash_compare_impl`'s loop over `ht1`'s buckets
    // (`Z/Zend/zend_hash.c:3115`), which takes no owner of either table:
    // [`compare_arrays`] holds them while `compare` serves `__toString`
    // or a class's `compare`. The walk fixes the end as it begins where php's
    // loop reads `nNumUsed` at each step, which only a row appended
    // mid-compare could tell apart.
    let decided = crate::api::values::walk::foreach(
        req,
        left,
        crate::api::values::walk::Hold::None,
        |req, row| -> crate::api::values::walk::Step<Ordering> {
            let Some(other) = right.get(row.key()) else {
                return Ok(std::ops::ControlFlow::Break(Ordering::Greater));
            };
            match compare(req, &row.value().deref(), &other)? {
                Ordering::Equal => Ok(std::ops::ControlFlow::Continue(())),
                ordering => Ok(std::ops::ControlFlow::Break(ordering)),
            }
        },
    )?;
    Ok(decided.unwrap_or(Ordering::Equal))
}

/// `n` stepped by one, up for [`Arith::Add`] and down for
/// [`Arith::Sub`], as `++` and `--` step a number.
fn stepped(op: Arith, n: Number) -> Value {
    inline::arith_numbers(op, n, Number::Long(1))
}

/// `increment_function` (`Z/Zend/zend_operators.c:2712`): the value
/// `++` leaves in the variable. A long is one more, and the double past
/// the long's edge where that overflowed (`fast_long_increment_function`);
/// a double is one more; null is `1`; a numeric string is the number it
/// holds, one more, the long's edge stepping to a double as the long's
/// does; any other string steps as `increment_string` steps it, `'a'`
/// to `'b'`, `'Az'` to `'Ba'`, `'z'` to `'aa'` and `'a9'` to `'b0'`, the
/// empty string to `'1'`, behind the deprecation 8.3 raises for one; a
/// bool is php's warning and no change; and an array, a resource or an
/// object is `TypeError`, naming what it could not increment. The value
/// is handed back rather than written in place, and it carries one
/// owner in every arm, the arms that step nothing included, so the
/// caller writes it where php's in-place step wrote and releases what
/// stood there.
///
/// ```php
/// $i = 1; $i++; $d = 1.5; $d++; $m = 9223372036854775807; $m++; $n = null; $n++; $t = "9"; $t++; $f = "1.5"; $f++; $e = "9223372036854775807"; $e++;
/// var_dump($i, $d, $m, $n, $t, $f, $e);
/// ```
/// ```expect
/// int(2)
/// float(2.5)
/// float(9.223372036854776E+18)
/// int(1)
/// int(10)
/// float(2.5)
/// float(9.223372036854776E+18)
/// ```
///
/// ```php
/// $s = "a"; $s++; $z = "Az"; $z++; $w = "z"; $w++; $n = "a9"; $n++; $e = ""; $e++; $x = "a-"; $x++;
/// var_dump($s, $z, $w, $n, $e, $x);
/// ```
/// ```expect
///
/// Deprecated: Increment on non-numeric string is deprecated, use str_increment() instead in %s on line 2
///
/// Deprecated: Increment on non-numeric string is deprecated, use str_increment() instead in %s on line 2
///
/// Deprecated: Increment on non-numeric string is deprecated, use str_increment() instead in %s on line 2
///
/// Deprecated: Increment on non-numeric string is deprecated, use str_increment() instead in %s on line 2
///
/// Deprecated: Increment on non-numeric string is deprecated, use str_increment() instead in %s on line 2
///
/// Deprecated: Increment on non-numeric string is deprecated, use str_increment() instead in %s on line 2
/// string(1) "b"
/// string(2) "Ba"
/// string(2) "aa"
/// string(2) "b0"
/// string(1) "1"
/// string(2) "a-"
/// ```
///
/// ```php
/// $b = true; $b++; var_dump($b);
/// ```
/// ```expect
///
/// Warning: Increment on type bool has no effect, this will change in the next major version of PHP in %s on line 2
/// bool(true)
/// ```
///
/// `$i++` leaves what the variable held and `++$i` what it holds now,
/// and a variable nothing has written is php's warning and null, which
/// steps to `1`.
///
/// ```php
/// $u++; var_dump($u); echo ++$v, "|", $w++, "|", $w, "|", $x = 5, $x++, ++$x, $x;
/// ```
/// ```expect
///
/// Warning: Undefined variable $u in %s on line 2
/// int(1)
///
/// Warning: Undefined variable $v in %s on line 2
/// 1|
/// Warning: Undefined variable $w in %s on line 2
/// |1|5577
/// ```
pub(crate) fn increment(req: &mut Request, value: &Value) -> Result<Value, Thrown> {
    let value = &value.deref();
    match value.as_ref() {
        ValueRef::Int(n) => Ok(stepped(Arith::Add, Number::Long(n))),
        ValueRef::Float(f) => Ok(stepped(Arith::Add, Number::Double(f))),
        // php's `IS_NULL` arm; an uninitialised zval reads as null here.
        ValueRef::Undef | ValueRef::Null => Ok(Value::int(1)),
        ValueRef::Str(s) => {
            // `is_numeric_str_function`, which is the reader with
            // `allow_errors` off: a string with anything past its number
            // is not a number here.
            match numeric_prefix(s.as_bytes()) {
                Some((n, false)) => Ok(stepped(Arith::Add, n)),
                _ => Ok(Value::string(increment_string(req, s.as_bytes())?)),
            }
        }
        ValueRef::Bool(_) => {
            warning(
                req,
                "Increment on type bool has no effect, this will change in the next major version of PHP",
            );
            none_pending(req)?;
            // php steps nothing here, so the variable keeps what it
            // holds; the value handed back is a second owner of it,
            // which the caller's write-back gives the variable again.
            req.addref(value);
            Ok(*value)
        }
        ValueRef::Object(_)
        | ValueRef::Array(_)
        | ValueRef::Resource(_)
        | ValueRef::Reference(_) => {
            stepped_other(req, value, BinaryOp::Add, "increment", increment)
        }
    }
}

/// `decrement_function` (`Z/Zend/zend_operators.c:2797`): the value
/// `--` leaves in the variable, and not [`increment`] with the sign
/// turned. A long is one less, and the double past the long's edge
/// where that underflowed (`fast_long_decrement_function`); a double is
/// one less; the empty string is `-1`, behind the deprecation 8.3
/// raises for it, tested before the string is read as a number; a
/// numeric string is the number it holds, one less, the long's edge
/// stepping to a double as the long's does; any other string is left as
/// it is, behind 8.3's other deprecation; null and a bool are php's
/// warning and no change, null having no `-1` where `++` has its `1`;
/// and an array, a resource or an object is `TypeError`, naming what it
/// could not decrement. The value is handed back rather than written in
/// place and carries one owner, as [`increment`]'s does.
///
/// ```php
/// $i = 1; $i--; $d = 1.5; $d--; $m = -9223372036854775807-1; $m--; $n = null; $n--; $t = "9"; $t--; $f = "1.5"; $f--; $e = "-9223372036854775807-1"; $g = "-9223372036854775808"; $g--;
/// var_dump($i, $d, $m, $n, $t, $f, $g);
/// ```
/// ```expect
///
/// Warning: Decrement on type null has no effect, this will change in the next major version of PHP in %s on line 2
/// int(0)
/// float(0.5)
/// float(-9.223372036854776E+18)
/// NULL
/// int(8)
/// float(0.5)
/// float(-9.223372036854776E+18)
/// ```
///
/// ```php
/// $s = "a"; $s--; $z = "Az"; $z--; $e = ""; $e--; $x = "9x"; $x--;
/// var_dump($s, $z, $e, $x);
/// ```
/// ```expect
///
/// Deprecated: Decrement on non-numeric string has no effect and is deprecated in %s on line 2
///
/// Deprecated: Decrement on non-numeric string has no effect and is deprecated in %s on line 2
///
/// Deprecated: Decrement on empty string is deprecated as non-numeric in %s on line 2
///
/// Deprecated: Decrement on non-numeric string has no effect and is deprecated in %s on line 2
/// string(1) "a"
/// string(2) "Az"
/// int(-1)
/// string(2) "9x"
/// ```
///
/// ```php
/// $b = true; $b--; $c = false; $c--; var_dump($b, $c);
/// ```
/// ```expect
///
/// Warning: Decrement on type bool has no effect, this will change in the next major version of PHP in %s on line 2
///
/// Warning: Decrement on type bool has no effect, this will change in the next major version of PHP in %s on line 2
/// bool(true)
/// bool(false)
/// ```
pub(crate) fn decrement(req: &mut Request, value: &Value) -> Result<Value, Thrown> {
    let value = &value.deref();
    match value.as_ref() {
        ValueRef::Int(n) => Ok(stepped(Arith::Sub, Number::Long(n))),
        ValueRef::Float(f) => Ok(stepped(Arith::Sub, Number::Double(f))),
        ValueRef::Str(s) if s.as_bytes().is_empty() => {
            deprecated(
                req,
                "Decrement on empty string is deprecated as non-numeric",
            );
            none_pending(req)?;
            Ok(Value::int(-1))
        }
        ValueRef::Str(s) => {
            if let Some((n, false)) = numeric_prefix(s.as_bytes()) {
                return Ok(stepped(Arith::Sub, n));
            }
            deprecated(
                req,
                "Decrement on non-numeric string has no effect and is deprecated",
            );
            none_pending(req)?;
            // As [`increment`]'s bool arm: nothing is stepped, and the
            // value handed back is a second owner of the string the
            // variable holds.
            req.addref(value);
            Ok(*value)
        }
        // php's `IS_NULL` arm; an uninitialised zval reads as null here.
        ValueRef::Undef | ValueRef::Null => {
            warning(
                req,
                "Decrement on type null has no effect, this will change in the next major version of PHP",
            );
            none_pending(req)?;
            Ok(Value::null())
        }
        ValueRef::Bool(_) => {
            warning(
                req,
                "Decrement on type bool has no effect, this will change in the next major version of PHP",
            );
            none_pending(req)?;
            req.addref(value);
            Ok(*value)
        }
        ValueRef::Object(_)
        | ValueRef::Array(_)
        | ValueRef::Resource(_)
        | ValueRef::Reference(_) => {
            stepped_other(req, value, BinaryOp::Sub, "decrement", decrement)
        }
    }
}

/// The arms `increment_function` and `decrement_function` share past
/// the scalars: an object's `do_operation` with one where the class
/// answers it, then `cast_object` to `_IS_NUMBER` and `step` over the
/// number; an object that answers neither, an array and a resource are
/// the `TypeError` naming what could not be stepped.
fn stepped_other(
    req: &mut Request,
    value: &Value,
    op: BinaryOp,
    verb: &str,
    step: fn(&mut Request, &Value) -> Result<Value, Thrown>,
) -> Result<Value, Thrown> {
    if let ValueRef::Object(o) = value.as_ref() {
        let table = req.object_handlers(o.heap());
        if let Some(stepped) = table.do_operation(req, op, value, Some(&Value::int(1)))? {
            return Ok(stepped.hand_on());
        }
        if let Some(number) = object_number(req, o)? {
            return step(req, &number);
        }
    }
    let name = value_name(req, value);
    Err(type_error(req, &format!("Cannot {verb} {name}")))
}

/// `increment_string` (`Z/Zend/zend_operators.c:2613`), "Perl style
/// string increment", behind its deprecation: from the last byte back, a
/// letter or a digit steps to the next of its kind, with `z`, `Z` and
/// `9` wrapping to `a`, `A` and `0` and carrying into the byte before;
/// a byte that is none of the three stops the carry where it stands; and
/// a carry off the front prepends the first of the last kind stepped. The
/// empty string is `"1"`.
///
/// A handler the deprecation reached that threw leaves the variable as
/// it stood, `if (EG(exception)) return false`, as each diagnostic of
/// `increment_function` and `decrement_function` does.
///
/// ```php
/// set_error_handler(function ($severity, $m) { throw new Exception($m); });
/// foreach (['', 'A1', true, null] as $value) {
///     try { $value++; } catch (Exception $e) { echo $e->getMessage(), "\n"; }
///     var_dump($value);
///     try { $value--; } catch (Exception $e) { echo $e->getMessage(), "\n"; }
///     var_dump($value);
/// }
/// ```
/// ```expect
/// Increment on non-numeric string is deprecated, use str_increment() instead
/// string(0) ""
/// Decrement on empty string is deprecated as non-numeric
/// string(0) ""
/// Increment on non-numeric string is deprecated, use str_increment() instead
/// string(2) "A1"
/// Decrement on non-numeric string has no effect and is deprecated
/// string(2) "A1"
/// Increment on type bool has no effect, this will change in the next major version of PHP
/// bool(true)
/// Decrement on type bool has no effect, this will change in the next major version of PHP
/// bool(true)
/// int(1)
/// int(0)
/// ```
fn increment_string(req: &mut Request, bytes: &[u8]) -> Result<PhpString, Thrown> {
    deprecated(
        req,
        "Increment on non-numeric string is deprecated, use str_increment() instead",
    );
    none_pending(req)?;
    if bytes.is_empty() {
        return req.string(b"1");
    }
    let mut out = bytes.to_vec();
    let mut last = None;
    let mut carry = false;
    for byte in out.iter_mut().rev() {
        let (first, past) = match *byte {
            b'a'..=b'z' => (b'a', b'z'),
            b'A'..=b'Z' => (b'A', b'Z'),
            b'0'..=b'9' => (b'0', b'9'),
            _ => {
                carry = false;
                break;
            }
        };
        last = Some(first);
        if *byte == past {
            *byte = first;
            carry = true;
        } else {
            *byte += 1;
            carry = false;
            break;
        }
    }
    if carry && let Some(first) = last {
        // The `1`, `A` or `a` php writes before the string once the carry
        // has run off its front.
        out.insert(0, if first == b'0' { b'1' } else { first });
    }
    req.string(&out)
}

/// `ZEND_STRLEN`'s handler (`Z/Zend/zend_vm_def.h:8759`), the length of
/// the string an operand is, which is what php's compiler emits for
/// `strlen($a)` in place of the call (`zend_compile_func_strlen`),
/// so the native of the same name serves `strlen` only where the
/// compiler could not see it. A string is its byte length. Anything else
/// goes the way `Z_PARAM_STR` would take it: under `strict_types` a
/// `TypeError`; otherwise `null` is the 8.1 deprecation and zero, and
/// anything else is the length of the string the weak rule makes of it,
/// [`crate::api::coerce::scalar_type_hint`] (`zend_parse_arg_str_weak`,
/// so a NAN warns as [`to_string`] warns and an object goes through its
/// `__toString`), and an array, a resource or an object with no
/// `__toString` is
/// php's `TypeError`, both texts the handler's own literals with
/// `zend_zval_value_name` in the second. A variable nothing has written
/// is the undefined-variable warning and then null's deprecation, in
/// that order.
///
/// ```php
/// $n = NAN; $t = true; $f = false; $d = 1.5; $i = 12345; $u = null;
/// var_dump(strlen($n), strlen($t), strlen($f), strlen($d), strlen($i), strlen($u), strlen($undef), strlen("héllo"));
/// ```
/// ```expect
/// Warning: unexpected NAN value was coerced to string in %s on line 3
///
/// Deprecated: strlen(): Passing null to parameter #1 ($string) of type string is deprecated in %s on line 3
///
/// Warning: Undefined variable $undef in %s on line 3
///
/// Deprecated: strlen(): Passing null to parameter #1 ($string) of type string is deprecated in %s on line 3
/// int(3)
/// int(1)
/// int(0)
/// int(3)
/// int(5)
/// int(0)
/// int(0)
/// int(6)
/// ```
///
/// The opcode throws from the frame it stands in, so an uncaught
/// `TypeError` carries no frame of `strlen`'s own above `{main}`, where
/// the native's would.
///
/// ```php
/// var_dump(strlen(token_get_all('<?php 1;')));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: strlen(): Argument #1 ($string) must be of type string, array given in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn strlen(req: &mut Request, value: &Value) -> Result<Value, Thrown> {
    let value = &value.deref();
    if let Some(n) = inline::string_length(value.to_zval()) {
        return Ok(Value::int(n));
    }
    if !req.strict_types() {
        match value.as_ref() {
            ValueRef::Undef | ValueRef::Null => {
                deprecated(
                    req,
                    "strlen(): Passing null to parameter #1 ($string) of type string is deprecated",
                );
                return Ok(Value::int(0));
            }
            _ => {
                // `zend_parse_arg_str_weak` over a copy of the operand,
                // the one weak rule a `string` parameter takes.
                let mask = php_unit::decl::Type::String.mask();
                if let Some(text) = crate::api::coerce::scalar_type_hint(req, value, mask, false)? {
                    let len = text
                        .as_php_string()
                        .map_or(0, |text| i64::try_from(text.len()).unwrap_or(i64::MAX));
                    req.release(text);
                    return Ok(Value::int(len));
                }
            }
        }
    }
    let name = value_name(req, value);
    Err(type_error(
        req,
        &format!("strlen(): Argument #1 ($string) must be of type string, {name} given"),
    ))
}

/// `ZEND_TYPE_CHECK`'s handler (`Z/Zend/zend_vm_def.h:8821`): whether
/// the value's type is one of the `MAY_BE_*` bits in `mask`, which is
/// `1 << Z_TYPE` for one type and the bits of a bool, a long, a double
/// and a string for `is_scalar` (`zend_compile_func_typecheck`,
/// `zend_compile_func_is_scalar`). The one type with more to it is a
/// resource: a closed one, whose `zend_rsrc_list_get_rsrc_type` is
/// null, is not one, so `is_resource` answers `false` for it. A
/// reference is followed to what it holds, and a variable nothing has
/// written reads as null after the warning, so `is_null($undef)` is the
/// warning and `true`.
///
/// ```php
/// $s = "1"; $t = token_get_all("<?php 1;"); $u = null; $f = 1.5; $c = false; var_dump(is_int($s), is_int(1), is_string($s), is_scalar($s), is_scalar($t), is_scalar($u), is_scalar($f), is_scalar($c), is_null($u), is_null($undef), is_bool($c), is_array($t), is_float($f), is_object($t), is_resource($t), is_long(1), is_integer(1), is_double($f));
/// ```
/// ```expect
/// Warning: Undefined variable $undef in %s on line 2
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
pub(crate) fn type_check(value: &Value, mask: u32) -> bool {
    let value = &value.deref();
    if !inline::has_type(mask, php_type(value)) {
        return false;
    }
    match value.as_ref() {
        ValueRef::Resource(r) if mask == MAY_BE_RESOURCE => !r.is_closed(),
        _ => true,
    }
}

/// `ZEND_DEFINED`'s handler (`Z/Zend/zend_vm_def.h:8857`) and
/// `zend_quick_check_constant` under it: whether a constant of exactly
/// this name exists as the request serves, which is what php's compiler
/// emits for `defined('NAME')` over a name it could not substitute
/// (`zend_compile_func_defined`), and what the native of the same name
/// reads once it has taken the namespace or the class off the name. A
/// name is looked up as written, then as one of `true`, `false` and
/// `null` in any case (`zend_get_special_const`), which the opcode never
/// asks for, since the compiler substitutes those, and the native does.
/// php keeps the answer in the opline's cache slot (design §5.4); the
/// slot is `extended_value` here and nothing reads it yet, since the
/// answer is the same whether or not it was kept.
///
/// ```php
/// var_dump(defined("NOPE"), defined("PHP_VERSION"), defined("E_ALL"), defined("php_version"), defined("PHP_EOL"), $n = "PHP_VERSION", defined($n), defined("\\PHP_VERSION"), defined("Foo\\PHP_VERSION"), defined("A::B"), defined("TRUE"), defined("nulL"), defined("\\true"), defined("\\"), defined(""), defined(1));
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// string(11) "PHP_VERSION"
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
pub(crate) fn defined(req: &Request, name: &[u8]) -> bool {
    if req.constant_named(name).is_some() {
        return true;
    }
    matches!(
        name.to_ascii_lowercase().as_slice(),
        b"true" | b"false" | b"null"
    )
}

/// `ZEND_COUNT`'s handler (`Z/Zend/zend_vm_def.h:9480`), which is what
/// php's compiler emits for `count($a)` and `sizeof($a)` in place of the
/// call (`zend_compile_func_count`), and the one arm the native of the
/// same name shares with it: an array is its element count; an object
/// is served by its `count_elements` handler, `WeakMap`'s, and
/// otherwise, where its class implements `Countable`, by its `count()`
/// method, whose return is read as `zval_get_long` reads it; any other
/// object is the `TypeError`, and anything else is too, naming the
/// function as it
/// was written, `sizeof` where `extended_value` says so, and the value
/// as `zend_zval_value_name` names it. A variable nothing has written is
/// the warning and then the error for null.
///
/// ```php
/// var_dump(count(token_get_all("<?php 1;")), sizeof(token_get_all("<?php 1;")));
/// ```
/// ```expect
/// int(3)
/// int(3)
/// ```
///
/// ```php
/// class N implements Countable { function count(): int { return 3; } }
/// $m = new WeakMap; $k = new stdClass; $m[$k] = 1;
/// var_dump(count(new N), sizeof(new N), new N instanceof Countable);
/// var_dump(count($m), sizeof($m));
/// ```
/// ```expect
/// int(3)
/// int(3)
/// bool(true)
/// int(1)
/// int(1)
/// ```
///
/// ```php
/// var_dump(count($undef));
/// ```
/// ```expect exit=255
/// Warning: Undefined variable $undef in %s on line 2
///
/// Fatal error: Uncaught TypeError: count(): Argument #1 ($value) must be of type Countable|array, null given in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// var_dump(sizeof(true));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: sizeof(): Argument #1 ($value) must be of type Countable|array, true given in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// The object is held across its `count()` method, `GC_ADDREF` and
/// `OBJ_RELEASE` (GH-21605), so a method that frees the variable
/// holding it reads a live `$this`, destroyed only after the call:
///
/// ```php
/// class C implements Countable { public $p = 1; function count(): int { global $c; $c = null; var_dump($this); return 42; } function __destruct() { echo "gone\n"; } }
/// $c = new C; var_dump(count($c));
/// ```
/// ```expect
/// object(C)#1 (1) {
///   ["p"]=>
///   int(1)
/// }
/// gone
/// int(42)
/// ```
pub(crate) fn count(req: &mut Request, value: &Value, sizeof: bool) -> Result<i64, Thrown> {
    let value = &value.deref();
    if let Some(array) = value.as_array() {
        return Ok(i64::try_from(array.len()).unwrap_or(i64::MAX));
    }
    if let Some(object) = value.as_object() {
        let table = req.object_handlers(object.heap());
        if let Some(count) = table.count_elements(req, object)? {
            return Ok(count);
        }
        let class = object.class();
        if let Some(countable) = req.class_named(Bare::resolved(b"Countable"))
            && req.symbols().instance_of(class, countable)
            && let Some(method) = req.symbols().method_named(class, b"count")
        {
            // `GC_ADDREF(zobj)` before the call and `OBJ_RELEASE` after
            // (GH-21605): a method that frees the variable holding the
            // object still has its `$this`.
            let held = Owner::take(req, object);
            let returned = req.serve_method(method, Some(object), class, &[], &[]);
            held.give_up(req);
            let returned = returned?;
            let count = get_long(req, &returned);
            returned.give_up(req);
            return Ok(count);
        }
    }
    let function = if sizeof { "sizeof" } else { "count" };
    let name = value_name(req, value);
    Err(type_error(
        req,
        &format!(
            "{function}(): Argument #1 ($value) must be of type Countable|array, {name} given"
        ),
    ))
}

/// What `ADD_ARRAY_ELEMENT`'s `op2` names: the next index where the
/// operand is `UNUSED`, or the offset as the operand holds it. A
/// `constant` offset is a `CONST` operand, whose numeric string the
/// compiler already made a long of (`zend_handle_numeric_op`), so the
/// handler reads it as the string it still is: `['07' => 1]` keeps the
/// key `"07"` and `[$k => 1]` over `$k = '7'` makes it `7`.
#[derive(Clone, Copy)]
pub(crate) enum Offset {
    Next,
    At { key: Value, constant: bool },
}

/// `ZEND_ADD_ARRAY_ELEMENT`'s body past its operand addressing
/// (`Z/Zend/zend_vm_def.h:6224`), the one kernel `INIT_ARRAY` dispatches
/// into for its first element and `ADD_ARRAY_ELEMENT` for the rest:
/// `value`, the kernel's to give up from here, written into `array`,
/// which has one owner, at the key `offset` makes. No offset is
/// `zend_hash_next_index_insert`, and php's `Error` when
/// `nNextFreeElement` is `ZEND_LONG_MAX` and taken
/// (`zend_cannot_add_element`). An offset is `zend_hash_update` over a
/// string, made a long where it is a canonical decimal and no constant
/// (`ZEND_HANDLE_NUMERIC`), `zend_hash_index_update` over a long, a
/// reference followed, `null` the deprecation and the key `""`, a float
/// `zend_dval_to_lval_safe` with its deprecation, `false` and `true` 0
/// and 1, a resource its id with php's warning, a slot nothing
/// wrote the key `""` after the handler's own warning, and an array or
/// an object the `TypeError` `zend_illegal_array_offset_access` throws,
/// naming an object by its class as `zend_zval_type_name` does, with
/// `value` given up. A user handler that throws under the `null`
/// deprecation ends the opline as `HANDLE_EXCEPTION` does, the value
/// given up.
///
/// ```php
/// $k = '2'; $f = 1.5; $n = null; $t = true; $s = 'k';
/// var_dump([$k => 'a', $f => 'b', $n => 'c', $t => 'd', '07' => 'e', 8, $u => 'f', $s => 'g']);
/// ```
/// ```expect
/// Deprecated: Implicit conversion from float 1.5 to int loses precision in %s on line 3
///
/// Deprecated: Using null as an array offset is deprecated, use an empty string instead in %s on line 3
///
/// Warning: Undefined variable $u in %s on line 3
/// array(6) {
///   [2]=>
///   string(1) "a"
///   [1]=>
///   string(1) "d"
///   [""]=>
///   string(1) "f"
///   ["07"]=>
///   string(1) "e"
///   [3]=>
///   int(8)
///   ["k"]=>
///   string(1) "g"
/// }
/// ```
///
/// ```php
/// $o = [1]; $a = [1, $o => 2];
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: Cannot access offset of type array on array in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// $a = [9223372036854775807 => 1, 2];
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Cannot add element to the array as the next element is already occupied in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn add_array_element(
    req: &mut Request,
    array: PhpArray,
    offset: Offset,
    value: Value,
) -> Result<(), Thrown> {
    let a = array.heap();
    let key = match offset {
        Offset::Next => {
            if req.array_append(a, value)? {
                return Ok(());
            }
            return Err(cannot_add_element(req));
        }
        Offset::At { key, constant } => match array_offset(req, &key, constant) {
            Ok(key) => key,
            Err(thrown) => {
                req.release(value);
                return Err(thrown);
            }
        },
    };
    req.array_update(a, key, value)
}

/// php's `type` argument to `zend_fetch_dimension_address_read`, of
/// which the two reads use two values: `BP_VAR_R`, which warns of a key
/// the table has not got, an offset past the end of a string and a
/// container that is neither; and `BP_VAR_IS`, which warns of none of
/// the three and reads `null` instead, since what asks is an `isset` or
/// a `??` and the answer is the question.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DimRead {
    Warning,
    Silent,
}

/// `zend_fetch_dimension_const` (`Z/Zend/zend_execute.c`), a dimension
/// of a constant expression read over two values the evaluation holds:
/// `FETCH_DIM_R`'s arms with no operand to read, [`DimRead::Silent`]
/// php's `BP_VAR_IS` under a `??`. The value is the caller's, one owner.
///
/// ```php
/// const A = [1][5] ?? "x";
/// const B = (null)["k"];
/// const C = "ab"[1];
/// var_dump(A, B, C);
/// ```
/// ```expect
///
/// Warning: Trying to access array offset on null in %s on line 3
/// string(1) "x"
/// NULL
/// string(1) "b"
/// ```
pub(crate) fn dimension_const(
    req: &mut Request,
    container: &Value,
    dim: &Value,
    mode: DimRead,
) -> Result<Value, Thrown> {
    let container = container.deref();
    if let Some(array) = container.as_array() {
        let key = array_offset(req, dim, false)?;
        return Ok(array_entry(req, array, key, mode));
    }
    if let Some(text) = container.as_php_string() {
        return string_offset(req, text, dim, mode);
    }
    if mode == DimRead::Silent {
        return Ok(Value::null());
    }
    let named = value_name(req, &container.undef_as_null());
    warning(req, &format!("Trying to access array offset on {named}"));
    Ok(Value::null())
}

/// `zend_fetch_dimension_address_inner` under `BP_VAR_R`
/// (`Z/Zend/zend_execute.c:2802`): the entry at `key`, copied with one
/// more owner and a reference it holds followed (`ZVAL_COPY_DEREF`);
/// php's `Undefined array key` warning and `null` where the table has
/// none, the key rendered as `zend_undefined_offset` renders a long and
/// `zend_undefined_index` a string.
///
/// ```php
/// $a = [1, "k" => 2];
/// var_dump($a[0], $a["k"], $a[7], $a["n"]);
/// ```
/// ```expect
///
/// Warning: Undefined array key 7 in %s on line 3
///
/// Warning: Undefined array key "n" in %s on line 3
/// int(1)
/// int(2)
/// NULL
/// NULL
/// ```
pub(crate) fn array_entry(
    req: &mut Request,
    array: PhpArray,
    key: php_heap::Key,
    mode: DimRead,
) -> Value {
    if let Some(found) = array.heap().get(key) {
        let value = Value::of_slot(found).deref();
        req.addref(&value);
        return value;
    }
    if mode == DimRead::Silent {
        return Value::null();
    }
    let message = match key {
        php_heap::Key::Long(n) => format!("Undefined array key {n}"),
        php_heap::Key::Str(s) => format!(
            "Undefined array key \"{}\"",
            String::from_utf8_lossy(s.bytes())
        ),
    };
    warning(req, &message);
    Value::null()
}

/// `zend_check_string_offset` (`Z/Zend/zend_execute.c`), which
/// `try_string_offset` runs over the same set: the offset php reads out
/// of a dimension over a string, for a read and for a write alike.
///
/// A long is itself, and a reference is followed once. A string is
/// `is_numeric_string_ex` with php's errors allowed: an integer is the
/// offset, with php's warning `Illegal string offset` where anything but
/// whitespace follows it, and everything else, a written float among
/// them, is `zend_illegal_string_offset`'s `TypeError`. `null`, a bool, a
/// float and an unwritten variable warn `String offset cast occurred` and
/// then convert, the float silently since the warning has been raised
/// ([`dval_to_lval_silent`]). An array, an object and a resource are the
/// `TypeError`, the offset named as `zend_zval_type_name` names it, an
/// object by its class, under a `??` as under a plain read.
///
/// ```php
/// $s = "abc";
/// var_dump($s["1x"], $s[true], isset($s["x"]));
/// ```
/// ```expect
///
/// Warning: Illegal string offset "1x" in %s on line 3
///
/// Warning: String offset cast occurred in %s on line 3
/// string(1) "b"
/// string(1) "b"
/// bool(false)
/// ```
///
/// ```php
/// $s = "string";
/// try { var_dump($s[new stdClass()] ?? 'default'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { var_dump($s[new ArrayObject]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { var_dump($s[[]] ?? 'default'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Cannot access offset of type stdClass on string
/// Cannot access offset of type ArrayObject on string
/// Cannot access offset of type array on string
/// ```
pub(crate) fn string_dim_offset(
    req: &mut Request,
    dim: &Value,
    mode: DimRead,
) -> Result<Option<i64>, Thrown> {
    let dim = dim.deref();
    match dim.as_ref() {
        ValueRef::Int(n) => Ok(Some(n)),
        ValueRef::Str(s) => match numeric_prefix(s.as_bytes()) {
            Some((Number::Long(n), trailing)) => {
                if trailing {
                    warning(
                        req,
                        &format!(
                            "Illegal string offset \"{}\"",
                            String::from_utf8_lossy(s.as_bytes())
                        ),
                    );
                }
                Ok(Some(n))
            }
            // `if (type == BP_VAR_IS) { ZVAL_NULL(result); return; }`,
            // the one arm that stands before `zend_illegal_string_offset`.
            _ if mode == DimRead::Silent => Ok(None),
            _ => Err(illegal_container_offset(
                req,
                "string",
                &dim,
                DimensionRead::Warning,
            )),
        },
        ValueRef::Undef | ValueRef::Null | ValueRef::Bool(_) | ValueRef::Float(_) => {
            if mode == DimRead::Warning {
                warning(req, "String offset cast occurred");
            }
            Ok(Some(match dim.as_ref() {
                ValueRef::Float(f) => dval_to_lval_silent(f),
                ValueRef::Bool(true) => 1,
                _ => 0,
            }))
        }
        ValueRef::Array(_)
        | ValueRef::Object(_)
        | ValueRef::Resource(_)
        | ValueRef::Reference(_) => Err(illegal_container_offset(
            req,
            "string",
            &dim,
            DimensionRead::Warning,
        )),
    }
}

/// `try_string_offset` and the `ZVAL_CHAR` after it
/// (`zend_fetch_dimension_address_read`, `Z/Zend/zend_execute.c:3073`):
/// the offset php reads out of the dimension ([`string_dim_offset`]),
/// and the one byte at it as a string of one byte.
///
/// A negative offset counts from the end. An offset the string is not
/// long enough for is php's warning `Uninitialized string offset` and
/// the empty string.
///
/// ```php
/// $s = "abc";
/// var_dump($s[1], $s[-1], $s["1"], $s[1.7], $s[5], $s["0x1"]);
/// ```
/// ```expect
/// Warning: String offset cast occurred in %s on line 3
///
/// Warning: Uninitialized string offset 5 in %s on line 3
///
/// Warning: Illegal string offset "0x1" in %s on line 3
/// string(1) "b"
/// string(1) "c"
/// string(1) "b"
/// string(1) "b"
/// string(0) ""
/// string(1) "a"
/// ```
///
/// A dimension that is no offset php reads is its `TypeError`.
///
/// ```php
/// $s = "abc";
/// var_dump($s["x"]);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: Cannot access offset of type string on string in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
pub(crate) fn string_offset(
    req: &mut Request,
    text: PhpString,
    dim: &Value,
    mode: DimRead,
) -> Result<Value, Thrown> {
    let Some(offset) = string_dim_offset(req, dim, mode)? else {
        return Ok(Value::null());
    };
    let len = i64::try_from(text.len()).unwrap_or(i64::MAX);
    let at = if offset < 0 {
        len.saturating_add(offset)
    } else {
        offset
    };
    if at < 0 || at >= len {
        // `if (type != BP_VAR_IS)`: the warning and the empty string are
        // the read's, and `null` with nothing raised is the other's.
        if mode == DimRead::Silent {
            return Ok(Value::null());
        }
        warning(req, &format!("Uninitialized string offset {offset}"));
        return Ok(Value::string(req.string(b"")?));
    }
    let byte = text
        .as_bytes()
        .get(usize::try_from(at).unwrap_or(0))
        .copied()
        .unwrap_or(0);
    Ok(Value::string(req.string(&[byte])?))
}

/// The key an array offset makes, the `add_again` ladder of
/// `ADD_ARRAY_ELEMENT`'s handler, which `slow_index_convert_w` is over
/// the same set for a dimension write: a `bool` is zero or one, a float
/// is truncated with php's precision deprecation, a resource is its id
/// with php's warning, and an array or an object is php's `TypeError`.
/// The `""` of a `null` and of an unwritten slot is the interned empty
/// string, `ZSTR_EMPTY_ALLOC`, nobody's to release, and `null` carries
/// php's 8.1 deprecation. A `CONST` string is the key as written, since
/// `zend_handle_numeric_dim` made the compiler's constant a long
/// already; any other string is `ZEND_HANDLE_NUMERIC_STR` here.
///
/// ```php
/// $a[true] = 1; $a[2.5] = 2; $a[null] = 3; $a["7"] = 4; var_dump($a);
/// ```
/// ```expect
/// Deprecated: Implicit conversion from float 2.5 to int loses precision in %s on line 2
///
/// Deprecated: Using null as an array offset is deprecated, use an empty string instead in %s on line 2
/// array(4) {
///   [1]=>
///   int(1)
///   [2]=>
///   int(2)
///   [""]=>
///   int(3)
///   [7]=>
///   int(4)
/// }
/// ```
pub(crate) fn array_offset(
    req: &mut Request,
    offset: &Value,
    constant: bool,
) -> Result<php_heap::Key, Thrown> {
    let empty = |req: &mut Request| Ok(php_heap::Key::Str(req.string(b"")?.heap()));
    match offset.as_ref() {
        ValueRef::Str(s) if constant => Ok(php_heap::Key::Str(s.heap())),
        ValueRef::Str(s) => Ok(Key::lookup(s).heap()),
        ValueRef::Int(n) => Ok(php_heap::Key::Long(n)),
        ValueRef::Reference(r) => array_offset(req, &r.get(), false),
        ValueRef::Null => {
            deprecated(
                req,
                "Using null as an array offset is deprecated, use an empty string instead",
            );
            none_pending(req)?;
            empty(req)
        }
        ValueRef::Float(f) => Ok(php_heap::Key::Long(double_to_long(req, f))),
        ValueRef::Bool(b) => Ok(php_heap::Key::Long(i64::from(b))),
        ValueRef::Resource(r) => Ok(php_heap::Key::Long(resource_offset(req, r.id()))),
        ValueRef::Undef => empty(req),
        ValueRef::Array(_) | ValueRef::Object(_) => Err(illegal_container_offset(
            req,
            "array",
            offset,
            DimensionRead::Warning,
        )),
    }
}

/// `zend_array_key_exists_fast` (`Z/Zend/zend_execute.c:3372`), what
/// `ZEND_ARRAY_KEY_EXISTS` asks of an array: whether the key the value
/// makes is in it. A string is read as `ZEND_HANDLE_NUMERIC` reads it
/// and a long is itself, with nothing raised; a variable nothing wrote
/// is `undefined`'s diagnostic, the handler's `ZVAL_UNDEFINED_OP1`, and
/// then the empty string; `null` is the empty string behind php's 8.5
/// deprecation, which names the function; and every other key is
/// [`array_offset`]'s: a float truncated behind its deprecation, a bool
/// zero or one, a resource its id behind the warning, and an array or an
/// object php's `TypeError`. A handler a diagnostic reaches may give the
/// array up, so the array is held across the conversion, php's
/// `GC_TRY_ADDREF(ht)` around the float's, and a key it was the last
/// owner of is not in it.
///
/// ```php
/// $a = ["1" => 1, "" => 2, "x" => null, 0 => 3];
/// var_dump(array_key_exists("1", $a), array_key_exists(1.9, $a), array_key_exists(true, $a), array_key_exists("x", $a), array_key_exists("01", $a), array_key_exists(null, $a), array_key_exists($u, $a));
/// try { array_key_exists([], $a); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { array_key_exists(new stdClass, $a); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
///
/// Deprecated: Implicit conversion from float 1.9 to int loses precision in %s on line 3
///
/// Deprecated: Using null as the key parameter for array_key_exists() is deprecated, use an empty string instead in %s on line 3
///
/// Warning: Undefined variable $u in %s on line 3
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// Cannot access offset of type array on array
/// Cannot access offset of type stdClass on array
/// ```
///
/// ```php
/// set_error_handler(function ($no, $message) { echo $message, "\n"; $GLOBALS['a'] = null; return true; });
/// $a = [1 => 1]; $a[2] = 2; $b = $a;
/// var_dump(array_key_exists(1.5, $a), $a);
/// $a = [1 => 1]; $a[2] = 2;
/// var_dump(array_key_exists(1.5, $a), $a);
/// ```
/// ```expect
/// Implicit conversion from float 1.5 to int loses precision
/// bool(true)
/// NULL
/// Implicit conversion from float 1.5 to int loses precision
/// bool(false)
/// NULL
/// ```
pub(crate) fn array_key_exists(
    req: &mut Request,
    array: PhpArray,
    key: &Value,
    undefined: impl FnOnce(&mut Request),
) -> Result<bool, Thrown> {
    let key = key.deref();
    match key.as_ref() {
        ValueRef::Str(s) => return Ok(array.heap().contains(Key::lookup(s).heap())),
        ValueRef::Int(n) => return Ok(array.heap().contains(php_heap::Key::Long(n))),
        _ => {}
    }
    let held = Owner::take(req, Value::array(array));
    let converted = match key.as_ref() {
        ValueRef::Undef => {
            undefined(req);
            req.string(b"").map(|s| php_heap::Key::Str(s.heap()))
        }
        ValueRef::Null => {
            deprecated(
                req,
                "Using null as the key parameter for array_key_exists() is deprecated, use an empty string instead",
            );
            req.string(b"").map(|s| php_heap::Key::Str(s.heap()))
        }
        _ => array_offset(req, &key, false),
    };
    if held.let_go(req).freed() {
        return Ok(false);
    }
    let key = converted?;
    none_pending(req)?;
    Ok(array.heap().contains(key))
}

/// `zend_zval_get_legacy_type` (`Z/Zend/zend_API.c:191`), the name
/// `ZEND_GET_TYPE` answers for a value: `NULL`, `boolean`, `integer`,
/// `double`, `string`, `array`, `object`, and `resource` or `resource
/// (closed)`; a reference is the value it holds, and a variable nothing
/// wrote is `null`, which the handler warned of as it read it.
///
/// ```php
/// $f = fopen('php://memory', 'r');
/// $r = &$f;
/// echo gettype(null), ' ', gettype(true), ' ', gettype(1), ' ', gettype(1.5), "\n";
/// echo gettype('s'), ' ', gettype([]), ' ', gettype(new stdClass), ' ', gettype($r), "\n";
/// fclose($f);
/// echo gettype($f), "\n";
/// echo gettype($undefined), "\n";
/// ```
/// ```expect
/// NULL boolean integer double
/// string array object resource
/// resource (closed)
///
/// Warning: Undefined variable $undefined in %s on line 8
/// NULL
/// ```
pub(crate) fn legacy_type(value: &Value) -> &'static [u8] {
    match value.deref().as_ref() {
        ValueRef::Undef | ValueRef::Null => b"NULL",
        ValueRef::Bool(_) => b"boolean",
        ValueRef::Int(_) => b"integer",
        ValueRef::Float(_) => b"double",
        ValueRef::Str(_) => b"string",
        ValueRef::Array(_) => b"array",
        ValueRef::Object(_) => b"object",
        ValueRef::Resource(resource) if resource.is_closed() => b"resource (closed)",
        ValueRef::Resource(_) => b"resource",
        // `deref` followed it; php's `default` arm names none.
        ValueRef::Reference(_) => b"unknown type",
    }
}

/// `ZEND_ISSET_ISEMPTY_DIM_OBJ`'s array arm (`Z/Zend/zend_vm_def.h:7546`,
/// the `isset_dim_obj_array` label): the entry at the key the offset
/// makes, asked for rather than fetched, so no `Undefined array key`
/// warning is raised on any path. Under `isset` the answer is that the
/// entry stands and is neither `null` nor a reference to one
/// (`Z_TYPE_P(value) > IS_NULL`); under `ZEND_ISEMPTY` it is that the
/// entry is absent or [`is_true`] says no.
///
/// ```php
/// $a = [0, null, "", "x", false];
/// var_dump(isset($a[0]), isset($a[1]), isset($a[2]), isset($a[9]));
/// var_dump(empty($a[0]), empty($a[3]), empty($a[9]));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
///
/// A float offset's warning may reach a handler that takes the
/// container's last owner; the table is destroyed and the entry is not
/// found:
///
/// ```php
/// set_error_handler(function ($no, $str) { global $a; $a = null; echo $str, "\n"; });
/// $a = [random_int(1, 1)];
/// var_dump(isset($a[1.0E+42]));
/// ```
/// ```expect
/// The float 1.0E+42 is not representable as an int, cast occurred
/// bool(false)
/// ```
pub(crate) fn isset_dim(
    req: &mut Request,
    array: PhpArray,
    offset: &Value,
    constant: bool,
    empty: bool,
) -> Result<bool, Thrown> {
    // `GC_TRY_ADDREF(ht)` around a float's warning in
    // `zend_find_array_dim_slow`: a handler that took the container's
    // last owner destroys the table, and the entry is then not found.
    let key = if offset.deref().as_float().is_some() {
        let held = Owner::take(req, array.to_value());
        let key = isset_offset(req, offset, constant);
        if held.let_go(req).freed() {
            return key.map(|_| empty);
        }
        key?
    } else {
        isset_offset(req, offset, constant)?
    };
    let found = array.heap().get(key).map(Value::of_slot);
    if empty {
        return Ok(match found {
            Some(value) => !is_true(req, &value.deref())?,
            None => true,
        });
    }
    Ok(found.is_some_and(|value| !matches!(value.deref().as_ref(), ValueRef::Null)))
}

/// `zend_find_array_dim_slow` (`Z/Zend/zend_execute.c:3247`), the key
/// `zend_find_array_dim_slow` (`Z/Zend/zend_execute.c:3247`), the key
/// ladder `ZEND_ISSET_ISEMPTY_DIM_OBJ` walks over an array: the same one
/// [`array_offset`] walks, with `zend_illegal_array_offset_isset`'s
/// sentence in place of the read's, `Cannot access offset of type %s in
/// isset or empty`, which is the one arm of the two that differs.
///
/// ```php
/// $a = [1];
/// try { var_dump(isset($a[[]])); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { var_dump(empty($a[new stdClass])); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Cannot access offset of type array in isset or empty
/// Cannot access offset of type stdClass in isset or empty
/// ```
pub(crate) fn isset_offset(
    req: &mut Request,
    offset: &Value,
    constant: bool,
) -> Result<php_heap::Key, Thrown> {
    // `zend_zval_type_name`, which is the class's name for an object
    // and the type's for everything else.
    let held = offset.deref();
    match held.as_ref() {
        ValueRef::Array(_) | ValueRef::Object(_) => Err(illegal_container_offset(
            req,
            "array",
            &held,
            DimensionRead::Silent,
        )),
        _ => array_offset(req, &held, constant),
    }
}

/// `ZEND_UNSET_DIM`'s body over an array (`Z/Zend/zend_vm_def.h:6735`,
/// the `unset_dim_array` label): `zend_hash_del` or
/// `zend_hash_index_del` at the key the offset makes, over the array the
/// container holds, which the handler separated first
/// (`SEPARATE_ARRAY`). The ladder is the `offset_again` chain: a string
/// key, made a long where it is a canonical decimal and no constant,
/// since a constant one the compiler already made a long
/// (`zend_handle_numeric_dim`); a long as itself; a reference followed;
/// `null` and a slot nothing wrote the key `""`, with no deprecation,
/// which is where this differs from a write (`ZEND_HANDLE_NUMERIC` is
/// not reached and `zend_hash_del` takes the empty string);  a float
/// `zend_dval_to_lval_safe` with its deprecation; `false` and `true` 0
/// and 1; a resource its id with php's warning; and an array or an
/// object the `TypeError` `zend_illegal_array_offset_unset` throws,
/// naming an object by its class. A key nothing holds is no error and
/// no warning, as `zend_hash_del` is silent over one.
///
/// php holds the table alive across the float's deprecation, since a
/// user handler may unset the container the array hangs from
/// (`GC_TRY_ADDREF`/`GC_DELREF` around it); the same guard stands here,
/// and a handler that took the last owner ends the opline with the
/// element still there, as php's `break` does.
///
/// ```php
/// $a = [1, 2, 'k' => 3, '' => 4, 5 => 'five'];
/// $i = 1; $s = 'k'; $n = null; $t = true; $d = '5';
/// unset($a[0], $a[$i], $a[$s], $a[$n], $a[$d], $a['nope']);
/// var_dump($a);
/// ```
/// ```expect
/// array(0) {
/// }
/// ```
///
/// A float offset is truncated under php's deprecation, and a resource
/// is its id under php's warning.
///
/// ```php
/// $a = [1 => 'a', 2 => 'b'];
/// $f = 1.5;
/// unset($a[$f]);
/// var_dump($a);
/// ```
/// ```expect
/// Deprecated: Implicit conversion from float 1.5 to int loses precision in %s on line 4
/// array(1) {
///   [2]=>
///   string(1) "b"
/// }
/// ```
///
/// An array or an object as the offset is php's `TypeError`, whose words
/// are the unset's own and not a read's.
///
/// ```php
/// $a = [1]; $k = [0];
/// unset($a[$k]);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: Cannot unset offset of type array on array in %s:3
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 3
/// ```
pub(crate) fn unset_dim(
    req: &mut Request,
    array: PhpArray,
    offset: &Value,
    constant: bool,
) -> Result<(), Thrown> {
    let a = array.heap();
    let key = match offset.deref().as_ref() {
        ValueRef::Str(s) if constant => php_heap::Key::Str(s.heap()),
        ValueRef::Str(s) => Key::lookup(s).heap(),
        ValueRef::Int(n) => php_heap::Key::Long(n),
        ValueRef::Bool(b) => php_heap::Key::Long(i64::from(b)),
        ValueRef::Float(f) => {
            // `GC_TRY_ADDREF(ht)` before the deprecation and `GC_DELREF`
            // after it: a handler that unset the container took the last
            // owner, and the table is destroyed here rather than
            // deleted from.
            let held = Owner::take(req, array.to_value());
            let n = double_to_long(req, f);
            if held.let_go(req).freed() || req.is_pending() {
                return Ok(());
            }
            php_heap::Key::Long(n)
        }
        ValueRef::Resource(r) => php_heap::Key::Long(resource_offset(req, r.id())),
        // `ZSTR_EMPTY_ALLOC()`, the key a `null` offset and a slot
        // nothing wrote both delete; the string is this function's to
        // give back, since `zend_hash_del` takes no owner of its key.
        ValueRef::Null | ValueRef::Undef => {
            let empty = req.string(b"")?;
            req.array_del(a, php_heap::Key::Str(empty.heap()));
            req.release(Value::string(empty));
            return Ok(());
        }
        ValueRef::Array(_) | ValueRef::Object(_) => {
            return Err(illegal_container_offset(
                req,
                "array",
                offset,
                DimensionRead::Unset,
            ));
        }
        // `deref` followed the reference already.
        ValueRef::Reference(_) => return Ok(()),
    };
    req.array_del(a, key);
    Ok(())
}

/// `MAY_BE_RESOURCE`, `1 << IS_RESOURCE`: the one mask `ZEND_TYPE_CHECK`
/// reads past the type byte.
const MAY_BE_RESOURCE: u32 = 1 << (ZvalType::Resource as u32);

/// `Z_TYPE_P`: php's number for a value's type, the `IS_*` of
/// `zend_types.h`, which `ZEND_TYPE_CHECK` shifts its mask by. A bool is
/// two of them, `IS_FALSE` and `IS_TRUE`.
fn php_type(value: &Value) -> ZvalType {
    match value.as_ref() {
        ValueRef::Undef => ZvalType::Undef,
        ValueRef::Null => ZvalType::Null,
        ValueRef::Bool(false) => ZvalType::False,
        ValueRef::Bool(true) => ZvalType::True,
        ValueRef::Int(_) => ZvalType::Long,
        ValueRef::Float(_) => ZvalType::Double,
        ValueRef::Str(_) => ZvalType::String,
        ValueRef::Array(_) => ZvalType::Array,
        ValueRef::Object(_) => ZvalType::Object,
        ValueRef::Resource(_) => ZvalType::Resource,
        ValueRef::Reference(_) => ZvalType::Reference,
    }
}

/// `zend_zval_value_name` (`Z/Zend/zend_API.c:157`): what a `TypeError`
/// names a value as, which is its class for an object, `true` or `false`
/// for a bool, and its type for the rest; what `INIT_METHOD_CALL` names
/// a receiver that is no object as.
///
/// ```php
/// $o = true; $o->m();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Call to a member function m() on true in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn value_name(req: &Request, value: &Value) -> String {
    match value.as_ref() {
        ValueRef::Object(_) => type_name(req, value),
        ValueRef::Bool(b) => String::from(if b { "true" } else { "false" }),
        _ => String::from(value.type_name()),
    }
}

/// `zend_zval_type_name` (`Z/Zend/zend_API.c:174`): an object is its
/// class's name as `%s` prints it, and every other value its type's.
///
/// ```php
/// try { $r = new stdClass + 1; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { $r = 1 | new ArrayObject; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { $r = [] - true; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Unsupported operand types: stdClass + int
/// Unsupported operand types: ArrayObject | int
/// Unsupported operand types: array - bool
/// ```
pub(crate) fn zval_type_name(req: &Request, value: &Value) -> String {
    match value.as_ref() {
        ValueRef::Object(o) => req.symbols().class_name(o.class()),
        _ => String::from(value.type_name()),
    }
}

/// `ZSTR_MAX_LEN` (`Z/Zend/zend_string.h:108`): `SIZE_MAX` less the
/// aligned header and terminator of a `zend_string`, 32 bytes on the
/// oracle's platform.
const ZSTR_MAX_LEN: usize = usize::MAX - 32;

/// php's `(double) lval`, whose rounding is the behaviour being copied.
#[expect(
    clippy::cast_precision_loss,
    reason = "php's own conversion, which this exists to reproduce"
)]
fn wide(n: i64) -> f64 {
    n as f64
}

/// `add_function_array`: a copy of the left array (`zend_array_dup`)
/// with every key of the right one it does not already carry, which is
/// `zend_hash_merge` with php's `overwrite` off and `zval_add_ref` on
/// each element taken. The copy has one owner, the caller's. The keys
/// the right yields are views, and [`PhpArray::set`] takes a string
/// key's owner, so the union takes one more of each it adds: a right
/// operand freed after the union leaves its keys standing, and a string
/// of a key's size allocated after is not handed the key's block.
///
/// ```php
/// $k = 'kkkkk'; $z = 'ZZZZZ';
/// function row($k) { return [$k . $k => 1]; }
/// $u = ['a' => 0] + row($k);
/// $x = $z . $z;
/// foreach ($u as $key => $_) { echo $key, ','; }
/// ```
/// ```expect
/// a,kkkkkkkkkk,
/// ```
fn merged(req: &mut Request, left: PhpArray, right: PhpArray) -> Result<Owner<PhpArray>, Thrown> {
    let mut union = left.copy(req)?;
    for (key, value) in lacking(req, *union, right) {
        let key = Owner::take(req, key);
        union.set(req, key, Owner::take(req, value))?;
    }
    Ok(union)
}

/// What `zend_hash_merge(result, op2, zval_add_ref, 0)` writes: the rows
/// of `right` whose keys `union` lacks, read under the borrowed request
/// before any is written. It writes only those keys, so it releases
/// nothing and runs no code of the program.
fn lacking(req: &mut Request, union: PhpArray, right: PhpArray) -> Vec<(Key, Value)> {
    let rows = crate::api::values::walk::rows(req, right);
    rows.filter(|(key, _)| !union.contains(key)).collect()
}

/// [`add`] as a compound assignment runs it, `add_function_array` with
/// `result == op1` (`Z/Zend/zend_operators.c:1129-1142`): `$a += $a`
/// leaves the place as it stands, and otherwise the left array is the
/// place's own, so it is separated where it stands (`SEPARATE_ARRAY`)
/// rather than copied. One the place holds alone is
/// merged into in place and handed back with one owner more, the one the
/// place's write takes, so the place keeps the same array and the owner
/// the handler then gives up of what it wrote over is that one again;
/// one with other owners, or an immutable one, is copied as [`add`]
/// copies it. Every other pair is [`add`]'s.
///
/// Merged in place, the array the place held is never destroyed, so an
/// element another name also holds is not given up and offered to the
/// collector, as a copy and the original's destruction would offer it:
///
/// ```php
/// $shared = [str_repeat('x', 2)];
/// $outer = ['n' => $shared];
/// $outer += ['m' => 1];
/// var_dump(gc_status()['roots'], $outer['m']);
/// ```
/// ```expect
/// int(0)
/// int(1)
/// ```
pub(crate) fn add_assign(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    let held = left.deref();
    let (Some(mut array), Some(b)) = (held.as_array(), right.deref().as_array()) else {
        return add(req, left, right);
    };
    // `$a += $a`: the place keeps its array as it stands, separated or not.
    if array.is_same(b) {
        return Ok(Owner::take(req, array).into_value().hand_on());
    }
    if held.is_refcounted() && held.refcount() == 1 {
        for (key, value) in lacking(req, array, b) {
            let key = Owner::take(req, key);
            array.set(req, key, Owner::take(req, value))?;
        }
        return Ok(Owner::take(req, array).into_value().hand_on());
    }
    add(req, left, right)
}

/// `ZEND_TRY_BINARY_OBJECT_OPERATION`: an object operand's `do_operation`
/// slot asked first, the left side's then the right side's, before an
/// operator converts either; `None` where neither side is an object or
/// neither table answers, which leaves the operator to its own
/// conversions and their `TypeError`. Every arithmetic, shift and bitwise
/// kernel asks it where php's does.
///
/// ```php
/// $n = new BcMath\Number('1.5');
/// var_dump((string) ($n + 1), (string) (2 * $n), (string) ($n ** 2));
/// $n++; var_dump((string) $n);
/// try { $n | 1; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "2.5"
/// string(3) "3.0"
/// string(4) "2.25"
/// string(3) "2.5"
/// Unsupported operand types: BcMath\Number | int
/// ```
fn object_operation(
    req: &mut Request,
    op: BinaryOp,
    left: &Value,
    right: &Value,
) -> Result<Option<Value>, Thrown> {
    let (left, right) = (left.deref(), right.deref());
    for side in [left, right] {
        if let ValueRef::Object(o) = side.as_ref() {
            let table = req.object_handlers(o.heap());
            if let Some(value) = table.do_operation(req, op, &left, Some(&right))? {
                return Ok(Some(value.hand_on()));
            }
        }
    }
    Ok(None)
}

/// Both operands as `zendi_try_convert_scalar_to_number` converts them,
/// or `zend_binop_error`'s `TypeError`, which names php's types around
/// the operator as written. php converts the left operand first and stops
/// there when it fails, so a right operand that would warn does not.
fn numbers(
    req: &mut Request,
    operator: &str,
    left: &Value,
    right: &Value,
) -> Result<(Number, Number), Thrown> {
    operands(req, operator, left, right, to_number)
}

/// Both operands as `convert_op1_op2_long` converts them, which is
/// [`to_long`] over the left one and then the right, or
/// `zend_binop_error`'s `TypeError` for the one that failed, which names
/// php's types around the operator as written.
fn longs(
    req: &mut Request,
    operator: &str,
    left: &Value,
    right: &Value,
) -> Result<(i64, i64), Thrown> {
    operands(req, operator, left, right, to_long)
}

/// Both operands as `convert` converts them, the left first, or
/// `zend_binop_error`'s `TypeError` for the first that fails, naming
/// php's types around the operator as written.
fn operands<T>(
    req: &mut Request,
    operator: &str,
    left: &Value,
    right: &Value,
    convert: fn(&mut Request, &Value) -> Result<Option<T>, Thrown>,
) -> Result<(T, T), Thrown> {
    let (left, right) = (&left.deref(), &right.deref());
    if let Some(a) = convert(req, left)?
        && let Some(b) = convert(req, right)?
    {
        return Ok((a, b));
    }
    let message = format!(
        "Unsupported operand types: {} {operator} {}",
        zval_type_name(req, left),
        zval_type_name(req, right)
    );
    Err(type_error(req, &message))
}

/// `zendi_try_get_long`: the long php works in, or nothing for an array,
/// an object or a resource, which is the failure `zend_binop_error`
/// reports. A double goes through [`double_to_long`]; a string is read
/// by [`numeric_prefix`] with php's warning for what follows its number,
/// and a double among them is `zend_dval_to_lval_cap`'s saturated long,
/// with the float-string deprecation where that lost something. The
/// string is held across the warning, php's `op_str` copy, since a
/// handler it reaches may give up the variable the string hangs from and
/// the deprecation names it after.
///
/// ```php
/// set_error_handler(function ($no, $message) {
///     echo $message, "\n";
///     unset($GLOBALS['x']);
/// });
/// $x = str_repeat("3e33", 2);
/// var_dump($x & true);
/// $x = str_repeat("7", 2) . "x";
/// var_dump($x | 0);
/// ```
/// ```expect
/// A non-numeric value encountered
/// Implicit conversion from float-string "3e333e33" to int loses precision
/// int(0)
/// A non-numeric value encountered
/// int(77)
/// ```
pub(crate) fn to_long(req: &mut Request, value: &Value) -> Result<Option<i64>, Thrown> {
    if let Some(n) = value.as_int() {
        return Ok(Some(n));
    }
    if let Some(d) = value.as_float() {
        return Ok(Some(double_to_long(req, d)));
    }
    if let Some(s) = value.as_php_string() {
        let Some((number, trailing)) = numeric_prefix(s.as_bytes()) else {
            return Ok(None);
        };
        let d = match number {
            Number::Long(n) => {
                if trailing {
                    warning(req, "A non-numeric value encountered");
                }
                return Ok(Some(n));
            }
            Number::Double(d) => d,
        };
        // `op_str = zend_string_copy(Z_STR_P(op))`: a handler the warning
        // reaches may give up the string's every other owner, and the
        // deprecation after it names the string.
        let held = Owner::take(req, Value::string(s));
        if trailing {
            warning(req, "A non-numeric value encountered");
        }
        let capped = dval_to_lval_cap(d);
        if !long_compatible(d, capped) {
            float_string_loses_precision(req, s.as_bytes());
        }
        held.let_go(req);
        return Ok(Some(capped));
    }
    if let Some(b) = value.as_bool() {
        return Ok(Some(i64::from(b)));
    }
    if let Some(o) = value.as_object() {
        // `cast_object(zobj, &dst, IS_LONG)`, failing where the class
        // converts to nothing or threw.
        let table = req.object_handlers(o.heap());
        return Ok(table
            .cast_object(req, o, TypeTag::Int)?
            .and_then(|c| c.as_int()));
    }
    Ok(match value.tag() {
        // php's `IS_NULL` arm; an uninitialised zval reads as null here.
        TypeTag::Null | TypeTag::Undef => Some(0),
        _ => None,
    })
}

/// `zend_parse_arg_long_weak` (`Z/Zend/zend_API.c:567`), the two arms a
/// native's `Z_PARAM_LONG` reads a double and a string through when the
/// caller is not strict; the long, or nothing for what the parser
/// refuses, which is the `TypeError` the face raises in its own words. A
/// double that is NAN or past the long's range is refused before it is
/// read; one in range is `zend_dval_to_lval`'s truncation, with the 8.1
/// deprecation (`zend_incompatible_double_to_long_error`) where the long
/// is not the double. A string is read by [`numeric_prefix`] as
/// `is_numeric_str_function` reads it, whole, so one with anything past
/// its number is refused, and a double among them is held to the same
/// range and truncated the same way, its deprecation
/// `zend_incompatible_string_to_long_error`'s, which names the string as
/// it was written.
///
/// ```php
/// var_dump(str_repeat('x', 2.0), str_repeat('x', -0.5), str_repeat('x', ' 2.0'), str_repeat('x', '1e0'), str_repeat('x', " 2\n"));
/// str_repeat('x', '2abc');
/// ```
/// ```expect exit=255
/// Deprecated: Implicit conversion from float -0.5 to int loses precision in %s on line 2
/// string(2) "xx"
/// string(0) ""
/// string(2) "xx"
/// string(1) "x"
/// string(2) "xx"
///
/// Fatal error: Uncaught TypeError: str_repeat(): Argument #2 ($times) must be of type int, string given in %s:3
/// Stack trace:
/// #0 %s(3): str_repeat('x', '2abc')
/// #1 {main}
///   thrown in %s on line 3
/// ```
pub(crate) fn arg_long_weak(req: &mut Request, value: &Value) -> Option<i64> {
    if let Some(d) = value.as_float() {
        if d.is_nan() || !fits_long(d) {
            return None;
        }
        return Some(double_to_long(req, d));
    }
    let s = value.as_php_string()?;
    let (number, trailing) = numeric_prefix(s.as_bytes())?;
    if trailing {
        return None;
    }
    match number {
        Number::Long(n) => Some(n),
        Number::Double(d) => {
            if d.is_nan() || !fits_long(d) {
                return None;
            }
            let l = truncated(d);
            if !long_compatible(d, l) {
                float_string_loses_precision(req, s.as_bytes());
            }
            Some(l)
        }
    }
}

/// `zend_dval_to_lval_safe`: `zend_dval_to_lval`, which is the truncated
/// long for a double in range and, with php's warning, zero for one that
/// is not finite and the modular reading of `zend_dval_to_lval_slow` for
/// one past the range; then the deprecation where the double fits and
/// the long is not it. `ZEND_DOUBLE_FITS_LONG` is two comparisons NAN
/// fails neither of, so NAN is warned on and then deprecated.
///
/// ```php
/// $a = []; $a[2.5] = 1; var_dump($a);
/// $b = []; $b[NAN] = 2; var_dump($b);
/// ```
/// ```expect
/// Deprecated: Implicit conversion from float 2.5 to int loses precision in %s on line 2
/// array(1) {
///   [2]=>
///   int(1)
/// }
///
/// Warning: The float NAN is not representable as an int, cast occurred in %s on line 3
///
/// Deprecated: Implicit conversion from float NAN to int loses precision in %s on line 3
/// array(1) {
///   [0]=>
///   int(2)
/// }
/// ```
pub(crate) fn double_to_long(req: &mut Request, d: f64) -> i64 {
    if fits_long(d) {
        double_to_long_strict(req, d)
    } else {
        dval_to_lval(req, d)
    }
}

/// `zval_get_long_func` with `is_strict` on over a double
/// (`Z/Zend/zend_operators.c:971`), what `zval_get_long_ex(offset, true)`
/// reads a float offset over a string as under `isset` and `empty`:
/// `zend_dval_to_lval` with its warning, and then the deprecation
/// wherever the long is not the double, a double past the long's range
/// among them, where [`double_to_long`] deprecates only one in range.
///
/// ```php
/// $s = "abc";
/// var_dump(isset($s[1.5]), isset($s[1.8446744073709552E+19]), empty($s[NAN]));
/// ```
/// ```expect
///
/// Deprecated: Implicit conversion from float 1.5 to int loses precision in %s on line 3
///
/// Warning: The float 1.8446744073709552E+19 is not representable as an int, cast occurred in %s on line 3
///
/// Deprecated: Implicit conversion from float 1.8446744073709552E+19 to int loses precision in %s on line 3
///
/// Warning: The float NAN is not representable as an int, cast occurred in %s on line 3
///
/// Deprecated: Implicit conversion from float NAN to int loses precision in %s on line 3
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
pub(crate) fn double_to_long_strict(req: &mut Request, d: f64) -> i64 {
    let l = dval_to_lval(req, d);
    if !long_compatible(d, l) {
        let text = rendered(d);
        deprecated(
            req,
            &format!("Implicit conversion from float {text} to int loses precision"),
        );
    }
    l
}

/// `zend_dval_to_lval`: the truncated long for a double in range and,
/// with php's warning (`zend_oob_double_to_long_error`), zero for one
/// that is not finite and the modular reading of `zend_dval_to_lval_slow`
/// for one past the range. What `(int)` reads a float as, and the first
/// half of [`double_to_long`].
fn dval_to_lval(req: &mut Request, d: f64) -> i64 {
    if !d.is_finite() || !fits_long(d) {
        let text = rendered(d);
        warning(
            req,
            &format!("The float {text} is not representable as an int, cast occurred"),
        );
    }
    dval_to_lval_silent(d)
}

/// `zend_dval_to_lval_silent` (`Z/Zend/zend_operators.h:138`):
/// [`dval_to_lval`] with no diagnostic at all, which is how a string
/// offset reads a float dimension after `String offset cast occurred`
/// has already been warned (`zend_fetch_dimension_address_read`'s
/// `IS_DOUBLE` arm), so php warns once and not twice: the truncation,
/// zero for what is not finite, and `zend_dval_to_lval_slow`'s modular
/// reading past the long's range, each with no second diagnostic.
///
/// ```php
/// $s = "abcdef";
/// var_dump($s[1.7], $s[-1.7], $s[NAN], $s[INF], $s[1e20]);
/// ```
/// ```expect
/// Warning: String offset cast occurred in %s on line 3
///
/// Warning: String offset cast occurred in %s on line 3
///
/// Warning: String offset cast occurred in %s on line 3
///
/// Warning: String offset cast occurred in %s on line 3
///
/// Warning: String offset cast occurred in %s on line 3
///
/// Warning: Uninitialized string offset 7766279631452241920 in %s on line 3
/// string(1) "b"
/// string(1) "f"
/// string(1) "a"
/// string(1) "a"
/// string(0) ""
/// ```
pub(crate) fn dval_to_lval_silent(d: f64) -> i64 {
    if !d.is_finite() {
        0
    } else if fits_long(d) {
        truncated(d)
    } else {
        modular(d)
    }
}

/// `zend_dval_to_lval_cap`: zero for what is not finite, and the long's
/// own edge past its range, which is `strtol`'s answer kept for a
/// float-string.
fn dval_to_lval_cap(d: f64) -> i64 {
    if !d.is_finite() {
        0
    } else if !fits_long(d) {
        if d > 0.0 { i64::MAX } else { i64::MIN }
    } else {
        truncated(d)
    }
}

/// A double as php's `%H` renders it in a message: `zend_gcvt` at the
/// shortest digits, which is [`double_to_string`] under
/// `serialize_precision`'s -1.
fn rendered(d: f64) -> String {
    String::from_utf8_lossy(double_to_string(d, -1, Exponent::Upper).as_bytes()).into_owned()
}

/// `ZEND_DOUBLE_FITS_LONG` on a 64-bit long: `(double) ZEND_LONG_MAX`
/// rounds up out of the range, so the upper edge is exclusive. Written
/// as php writes it, two comparisons NAN fails neither of, which a range
/// test would turn into the one NAN fails.
#[expect(
    clippy::manual_range_contains,
    reason = "php's own two comparisons, whose answer for NAN is the behaviour being copied"
)]
fn fits_long(d: f64) -> bool {
    !(d >= 9_223_372_036_854_775_808.0 || d < -9_223_372_036_854_775_808.0)
}

/// `zend_is_long_compatible`: php's `(double) l == d`.
#[expect(
    clippy::float_cmp,
    reason = "php's `(double) l == d`, the exact equality being copied"
)]
fn long_compatible(d: f64, l: i64) -> bool {
    wide(l) == d
}

/// php's `(zend_long) d` over a double that fits, and past the range
/// the long's own edge, which is `zend_dval_to_lval_cap`'s answer and
/// the cast's.
#[expect(
    clippy::cast_possible_truncation,
    reason = "php's `(zend_long) d`, whose truncation is the behaviour being copied"
)]
fn truncated(d: f64) -> i64 {
    d as i64
}

/// `zend_dval_to_lval_slow` on a 64-bit long: the double modulo 2^64,
/// made positive, read as an unsigned long and then as the signed one
/// with the same bits. The double is past the long's range, so it is
/// whole and the remainder is too.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_possible_wrap,
    reason = "php's `(zend_long)(zend_ulong) dmod`, the bit pattern being copied"
)]
fn modular(d: f64) -> i64 {
    let two_pow_64 = 18_446_744_073_709_551_616.0_f64;
    let mut dmod = d % two_pow_64;
    if dmod < 0.0 {
        dmod += two_pow_64;
    }
    (dmod as u64) as i64
}

/// The object's `cast_object` to `_IS_NUMBER`, an int or a float value,
/// or none where its class converts to no number: what
/// `increment_function` and `decrement_function` step in the object's
/// place, as `zendi_try_convert_scalar_to_number` reads it for the other
/// operators.
///
/// ```php
/// $e = simplexml_load_string('<r><a>1</a><b>2.5</b></r>');
/// $e->a++; $e->b--; $n = $e->a; $n++;
/// var_dump((string) $e->a, (string) $e->b, $n);
/// try { $o = new stdClass; $o++; } catch (TypeError $t) { echo $t->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "2"
/// string(3) "1.5"
/// int(3)
/// Cannot increment stdClass
/// ```
fn object_number(req: &mut Request, object: PhpObject) -> Result<Option<Value>, Thrown> {
    let table = req.object_handlers(object.heap());
    Ok(match table.cast_object(req, object, TypeTag::Number)? {
        Some(number) if matches!(number.as_ref(), ValueRef::Int(_) | ValueRef::Float(_)) => {
            Some(number.hand_on())
        }
        Some(other) => {
            other.give_up(req);
            None
        }
        None => None,
    })
}

/// `zendi_try_convert_scalar_to_number`: the number php works in, or
/// nothing for an array, a resource or an object whose class converts
/// to no number, which is the failure `zend_binop_error` reports; what
/// an object's `cast_object` to `_IS_NUMBER` threw is the `Err`. A
/// string is read by [`numeric_prefix`], the one reader php's compiler
/// folds through as well, and one with anything past the number it opens
/// with raises php's warning and keeps the number.
///
/// ```php
/// $e = simplexml_load_string('<r><a>5</a><b>1.5</b><c>x</c></r>');
/// var_dump($e->a * 2, $e->b + 1, $e->c + 0);
/// try { $r = new stdClass + 1; } catch (TypeError $t) { echo $t->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(10)
/// float(2.5)
/// int(0)
/// Unsupported operand types: stdClass + int
/// ```
fn to_number(req: &mut Request, value: &Value) -> Result<Option<Number>, Thrown> {
    if let Some(n) = value.as_int() {
        return Ok(Some(Number::Long(n)));
    }
    if let Some(f) = value.as_float() {
        return Ok(Some(Number::Double(f)));
    }
    if let Some(s) = value.as_php_string() {
        let Some((number, trailing)) = numeric_prefix(s.as_bytes()) else {
            return Ok(None);
        };
        if trailing {
            warning(req, "A non-numeric value encountered");
        }
        return Ok(Some(number));
    }
    if let Some(b) = value.as_bool() {
        return Ok(Some(Number::Long(i64::from(b))));
    }
    if let Some(o) = value.as_object() {
        return Ok(object_number(req, o)?.and_then(|c| match c.as_ref() {
            ValueRef::Int(n) => Some(Number::Long(n)),
            ValueRef::Float(d) => Some(Number::Double(d)),
            _ => None,
        }));
    }
    Ok(match value.tag() {
        // php's `IS_NULL` arm; an uninitialised zval reads as null here.
        TypeTag::Null | TypeTag::Undef => Some(Number::Long(0)),
        _ => None,
    })
}

/// The deprecation of a numeric string read as an int that is a float
/// the int does not hold whole.
fn float_string_loses_precision(req: &mut Request, s: &[u8]) {
    let shown = String::from_utf8_lossy(s);
    deprecated(
        req,
        &format!("Implicit conversion from float-string \"{shown}\" to int loses precision"),
    );
}

/// A resource used as an array offset: php's warning, then its id.
fn resource_offset(req: &mut Request, id: i64) -> i64 {
    warning(
        req,
        &format!("Resource ID#{id} used as offset, casting to integer ({id})"),
    );
    id
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::values::Key;
    use crate::engine::Engine;
    use crate::test::Capture;

    /// `==` and `===` over two arrays nested deeper than the thread
    /// holds are refused at its floor, whatever
    /// `zend.max_allowed_stack_size` says: the
    /// test harness's thread is 2 MiB and the nesting two hundred
    /// thousand, and the refusal is the `Error` a program can catch,
    /// after which the request goes on and frees both.
    #[test]
    fn a_nesting_deeper_than_the_thread_is_refused_at_its_floor() {
        let source = "<?php\n$a = $b = [];\nfor ($i = 0; $i < 200000; $i++) { $a = [$a]; $b = [$b]; }\ntry { $a == $b; } catch (Error $e) { echo $e->getMessage(), \"\\n\"; }\ntry { $a === $b; } catch (Error $e) { echo $e->getMessage(), \"\\n\"; }\necho 'done';";
        for limit in ["0", "-1", "256K"] {
            let served = Engine::cli_builder()
                .ini("zend.max_allowed_stack_size", limit)
                .build()
                .serve_capture(source, Capture::default());
            let stdout = served.stdout_text();
            let lines: Vec<&str> = stdout.lines().collect();
            assert_eq!(
                lines.len(),
                3,
                "under zend.max_allowed_stack_size={limit}: {stdout}"
            );
            for refused in &lines[..2] {
                assert!(
                    refused.starts_with("Maximum call stack size of ")
                        && refused.ends_with(" bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached. Infinite recursion?"),
                    "under zend.max_allowed_stack_size={limit}: {refused}"
                );
            }
            assert_eq!(lines[2], "done");
        }
    }

    /// `var_dump()`'s array elements and `define()`'s walks, which php
    /// leaves unguarded, are held to the thread's floor too: the dump
    /// writes its refusal where the next element would stand, and
    /// `define()` throws.
    #[test]
    fn var_dump_and_define_are_refused_at_the_floor() {
        let source = "<?php\n$a = []; for ($i = 0; $i < 200000; $i++) { $a = [$a]; }\ntry { define('DEEP', $a); } catch (Error $e) { echo $e->getMessage(), \"\\n\"; }\nvar_dump($a);";
        let served = Engine::cli_builder()
            .build()
            .serve_capture(source, Capture::default());
        let stdout = served.stdout_text();
        let (refused, dumped) = stdout
            .split_once('\n')
            .unwrap_or_else(|| panic!("a refusal and a dump: {stdout}"));
        assert!(
            refused.starts_with("Maximum call stack size of ")
                && refused.ends_with(" reached. Infinite recursion?"),
            "{refused}"
        );
        assert!(dumped.starts_with("array(1) {\n  [0]=>\n  array(1) {\n"));
        assert!(dumped.contains("nesting level too deep"));
        assert_eq!(served.fatal, None);
    }

    /// A union keeps the string keys it takes from the right operand
    /// when that operand is a temporary freed after it: the key is one
    /// more owner, so the string a later allocation of its size is
    /// handed is not the key's block.
    #[test]
    fn a_union_owns_the_string_keys_it_takes_from_a_freed_right_operand() {
        let source = "<?php\n$k = 'kkkkk'; $z = 'ZZZZZ';\nfunction row($k) { return [$k . $k => 1]; }\n$u = ['a' => 0] + row($k);\n$x = $z . $z;\nforeach ($u as $key => $_) { echo $key, ','; }";
        let served = Engine::cli_builder()
            .build()
            .serve_capture(source, Capture::default());
        served.assert_clean("a,kkkkkkkkkk,");
    }

    fn array(req: &mut Request, entries: Vec<(Key, Value)>) -> Value {
        let mut a = req.array().expect("an array");
        // The entries are the test's to hand on, made for the array.
        for (k, v) in entries {
            let (k, v) = (Owner::adopt(k), Owner::adopt(v));
            a.set(req, k, v).expect("an entry set");
        }
        Value::array(a)
    }

    /// `zend_hash_compare` with `hash_zval_identical_function` and
    /// `ordered` on: the same count, the same key at each step and
    /// identical values; `[1, 2]` against `[1 => 2, 0 => 1]` is the
    /// same map in another order and is not identical.
    #[test]
    fn two_arrays_are_identical_by_count_then_by_each_key_and_value_in_order() {
        let mut req = crate::test::request();
        let start = req.memory().usage(false);
        let list = || vec![(Key::Int(0), Value::int(1)), (Key::Int(1), Value::int(2))];
        let (a, b) = (array(&mut req, list()), array(&mut req, list()));
        assert_eq!(identical(&mut req, &a, &b), Ok(true));
        let one = array(&mut req, vec![(Key::Int(0), Value::int(1))]);
        assert_eq!(identical(&mut req, &a, &one), Ok(false));
        let swapped = array(
            &mut req,
            vec![(Key::Int(1), Value::int(2)), (Key::Int(0), Value::int(1))],
        );
        assert_eq!(identical(&mut req, &a, &swapped), Ok(false));
        let floated = array(
            &mut req,
            vec![
                (Key::Int(0), Value::int(1)),
                (Key::Int(1), Value::float(2.0)),
            ],
        );
        assert_eq!(identical(&mut req, &a, &floated), Ok(false));
        let inner = (array(&mut req, list()), array(&mut req, list()));
        let nested = (
            array(&mut req, vec![(Key::Int(0), inner.0)]),
            array(&mut req, vec![(Key::Int(0), inner.1)]),
        );
        assert_eq!(identical(&mut req, &nested.0, &nested.1), Ok(true));
        assert_eq!(identical(&mut req, &a, &Value::int(1)), Ok(false));
        for value in [a, b, one, swapped, floated, nested.0, nested.1] {
            req.release(value);
        }
        assert_eq!(req.memory().usage(false), start, "every array went back");
    }
}

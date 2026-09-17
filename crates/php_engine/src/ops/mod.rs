//! The kernels (design §5.8): one semantic in one function, called by the
//! handlers, by `api` and, when the compiled tier lands, by the kernel
//! list `tier.lock` names. A handler above them does operand addressing
//! and nothing else, so every caller of a semantic gets the same one.
//!
//! Each kernel carries a witness, which is the manifest's
//! `witness.coverage` rule over this directory.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::cmp::Ordering;

use php_heap::{Number, Scalar, ZvalType, double_to_string, numeric_prefix};

use crate::api::raising::{Thrown, deprecated, throw, warning};
use crate::api::request::{Request, append_escaped};
use crate::api::values::{Key, PhpArray, PhpString, TypeTag, Value, ValueRef};

/// `zval_get_string` (`Z/Zend/zend_operators.c:1066`): a value as the
/// string php converts it to, which is what `ECHO`, `print`, `.` and
/// string interpolation write. An array is php's warning and the word
/// `Array`; a resource its `Resource id #` and number; an object is the
/// `Error` php throws for one with no `__toString`, and no class here
/// has one yet, so that arm waits with the classes. The string is the
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
            Ok(*s)
        }
        ValueRef::Int(n) => PhpString::new(req, n.to_string().as_bytes()),
        ValueRef::Float(f) => {
            // `zend_double_to_str`: the text, then the warning php raises
            // when the value it converted was NAN.
            let text = PhpString::new(req, double_to_string(f, req.precision()).as_bytes())?;
            if f.is_nan() {
                warning(req, "unexpected NAN value was coerced to string");
            }
            Ok(text)
        }
        ValueRef::Bool(true) => PhpString::new(req, b"1"),
        ValueRef::Array(_) => {
            warning(req, "Array to string conversion");
            PhpString::new(req, b"Array")
        }
        ValueRef::Resource(r) => PhpString::new(req, format!("Resource id #{}", r.id()).as_bytes()),
        // `false` and `null` are both the empty string, and so is an
        // object until its arm lands.
        ValueRef::Undef
        | ValueRef::Null
        | ValueRef::Bool(false)
        | ValueRef::Object(_)
        | ValueRef::Reference(_) => PhpString::new(req, b""),
    }
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
        ValueRef::Str(s) => found(table.get(&Key::Str(*s))),
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
        ValueRef::Str(s) => found(table.get(&Key::Str(*s))),
        _ => Switched::Default,
    }
}

/// The target a table holds under a key, or the default where none.
fn found(target: Option<Value>) -> Switched {
    match target.and_then(|target| target.as_int()) {
        Some(target) => Switched::Target(u32::try_from(target).unwrap_or(u32::MAX)),
        None => Switched::Default,
    }
}

/// `zend_match_unhandled_error` (`Z/Zend/zend_execute.c`): the
/// `UnhandledMatchError` a `MATCH_ERROR` throws over the subject no arm
/// took. A scalar is rendered by `smart_str_append_scalar`: `NULL`,
/// `true` or `false`, a long in decimal, a double at `precision`
/// (`smart_str_append_double`), a string in `'` escaped as
/// `smart_str_append_escaped` escapes it and cut at
/// `zend.exception_string_param_max_len` bytes with `...`; anything else
/// is named by its type, `zend_zval_type_name`, a class for an object.
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
pub(crate) fn match_unhandled_error(req: &mut Request, subject: &Value) -> Thrown {
    let subject = subject.deref();
    let message = match subject.as_ref() {
        ValueRef::Undef | ValueRef::Null => String::from("Unhandled match case NULL"),
        ValueRef::Bool(b) => format!("Unhandled match case {}", if b { "true" } else { "false" }),
        ValueRef::Int(n) => format!("Unhandled match case {n}"),
        ValueRef::Float(f) => {
            let precision = match req.precision() {
                0 => 1,
                precision => precision,
            };
            format!("Unhandled match case {}", double_to_string(f, precision))
        }
        ValueRef::Str(s) => {
            let mut text = String::from("Unhandled match case '");
            let cut = req.exception_string_param_max_len();
            append_escaped(&mut text, &s.as_bytes()[..cut.min(s.len())]);
            if s.len() > cut {
                text.push_str("...");
            }
            text.push('\'');
            text
        }
        ValueRef::Array(_)
        | ValueRef::Object(_)
        | ValueRef::Resource(_)
        | ValueRef::Reference(_) => {
            format!("Unhandled match case of type {}", type_name(req, &subject))
        }
    };
    throw(req, "UnhandledMatchError", &message)
}

/// `zend_zval_type_name`: a class for an object, `null` for an
/// undefined value, and php's type name for the rest.
fn type_name(req: &Request, value: &Value) -> String {
    match value.as_ref() {
        ValueRef::Object(o) => req.class_decl(o.class()).map_or_else(
            || String::from("object"),
            |decl| String::from_utf8_lossy(&decl.name.0).into_owned(),
        ),
        ValueRef::Undef => String::from("null"),
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
pub(crate) fn is_true(req: &mut Request, value: &Value) -> bool {
    match value.deref().as_ref() {
        ValueRef::Undef | ValueRef::Null | ValueRef::Bool(false) => false,
        ValueRef::Bool(true) | ValueRef::Object(_) => true,
        ValueRef::Int(n) => n != 0,
        ValueRef::Float(f) => {
            if f.is_nan() {
                warning(req, "unexpected NAN value was coerced to bool");
            }
            f != 0.0
        }
        ValueRef::Str(s) => !(s.is_empty() || s.as_bytes() == b"0"),
        ValueRef::Array(a) => !a.is_empty(),
        ValueRef::Resource(r) => r.id() != 0,
        // `deref` followed the reference already.
        ValueRef::Reference(r) => is_true(req, &r.get()),
    }
}

/// What `ZEND_CAST` converts to, the three scalar arms of its handler
/// (`Z/Zend/zend_vm_def.h:6495`), named by the type constant the
/// opline's `extended_value` carries. `(bool)` is `ZEND_BOOL` and never
/// reaches here; `(array)` and `(object)` wait on the tree and the
/// object store's conversions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CastTo {
    /// `IS_LONG`, `zval_get_long`.
    Long,
    /// `IS_DOUBLE`, `zval_get_double`.
    Double,
    /// `IS_STRING`, `zval_get_string`.
    Str,
}

impl CastTo {
    /// The arm for php's type constant, nothing for one this engine has
    /// no conversion for.
    pub(crate) fn of(code: u32) -> Option<CastTo> {
        match code {
            4 => Some(CastTo::Long),
            5 => Some(CastTo::Double),
            6 => Some(CastTo::Str),
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
pub(crate) fn cast(req: &mut Request, value: &Value, to: CastTo) -> Result<Value, Thrown> {
    match to {
        CastTo::Long => Ok(Value::int(get_long(req, value))),
        CastTo::Double => Ok(Value::float(get_double(req, value))),
        CastTo::Str => to_string(req, value).map(Value::string),
    }
}

/// `zval_get_long_func` with `is_strict` off (`Z/Zend/zend_operators.c:957`).
fn get_long(req: &mut Request, value: &Value) -> i64 {
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
        ValueRef::Object(_) => {
            not_converted(req, value, "int");
            1
        }
        // `deref` followed the reference already.
        ValueRef::Reference(r) => get_long(req, &r.get()),
    }
}

/// `zval_get_double_func` (`Z/Zend/zend_operators.c:1027`).
fn get_double(req: &mut Request, value: &Value) -> f64 {
    match value.deref().as_ref() {
        ValueRef::Undef | ValueRef::Null | ValueRef::Bool(false) => 0.0,
        ValueRef::Bool(true) => 1.0,
        ValueRef::Resource(r) => wide(r.id()),
        ValueRef::Int(n) => wide(n),
        ValueRef::Float(d) => d,
        // `zend_strtod` over the string: the number it opens with after
        // any leading whitespace, read to a double, and zero for none.
        ValueRef::Str(s) => match numeric_prefix(s.as_bytes()) {
            Some((Number::Long(n), _)) => wide(n),
            Some((Number::Double(d), _)) => d,
            None => 0.0,
        },
        ValueRef::Array(a) => f64::from(u8::from(!a.is_empty())),
        ValueRef::Object(_) => {
            not_converted(req, value, "float");
            1.0
        }
        ValueRef::Reference(r) => get_double(req, &r.get()),
    }
}

/// `convert_object_to_type`'s warning, for an object whose class has no
/// conversion to the type named, which is every class here.
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
    if let (Some(a), Some(b)) = (left.deref().as_array(), right.deref().as_array()) {
        return Ok(Value::array(merged(req, *a, *b)?));
    }
    let (a, b) = numbers(req, "+", left, right)?;
    Ok(match (a, b) {
        (Number::Long(a), Number::Long(b)) => promoted(a.checked_add(b), wide(a) + wide(b)),
        (a, b) => Value::float(a.double() + b.double()),
    })
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
    let (a, b) = numbers(req, "-", left, right)?;
    Ok(match (a, b) {
        (Number::Long(a), Number::Long(b)) => promoted(a.checked_sub(b), wide(a) - wide(b)),
        (a, b) => Value::float(a.double() - b.double()),
    })
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
    let (a, b) = numbers(req, "*", left, right)?;
    Ok(match (a, b) {
        (Number::Long(a), Number::Long(b)) => promoted(a.checked_mul(b), wide(a) * wide(b)),
        (a, b) => Value::float(a.double() * b.double()),
    })
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
    let (a, b) = numbers(req, "/", left, right)?;
    match (a, b) {
        (Number::Long(a), Number::Long(b)) => {
            if b == 0 {
                return Err(throw(req, "DivisionByZeroError", "Division by zero"));
            }
            // php's own case, "prevent overflow error/crash".
            if b == -1 && a == i64::MIN {
                return Ok(Value::float(wide(a) / -1.0));
            }
            Ok(if a % b == 0 {
                Value::int(a / b)
            } else {
                Value::float(wide(a) / wide(b))
            })
        }
        (a, b) => {
            let (a, b) = (a.double(), b.double());
            if b == 0.0 {
                return Err(throw(req, "DivisionByZeroError", "Division by zero"));
            }
            Ok(Value::float(a / b))
        }
    }
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
    let (a, b) = longs(req, "%", left, right)?;
    if b == 0 {
        return Err(throw(req, "DivisionByZeroError", "Modulo by zero"));
    }
    // php's own case, "prevent overflow error/crash if op1==LONG_MIN".
    if b == -1 {
        return Ok(Value::int(0));
    }
    Ok(Value::int(a % b))
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
    let (a, b) = longs(req, "<<", left, right)?;
    match u64::try_from(b) {
        Ok(by) => Ok(Value::int(php_heap::shift_left(a, by))),
        Err(_) => Err(throw(
            req,
            "ArithmeticError",
            "Bit shift by negative number",
        )),
    }
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
    let (a, b) = longs(req, ">>", left, right)?;
    match u64::try_from(b) {
        Ok(by) => Ok(Value::int(php_heap::shift_right(a, by))),
        Err(_) => Err(throw(
            req,
            "ArithmeticError",
            "Bit shift by negative number",
        )),
    }
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
pub(crate) fn concat(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    let a = to_string(req, left)?;
    let b = to_string(req, right)?;
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
    if a.len() > ZSTR_MAX_LEN - b.len() {
        req.release(Value::string(a));
        req.release(Value::string(b));
        return Err(throw(req, "Error", "String size overflow"));
    }
    // `zend_string_alloc(ZSTR_LEN(op1) + ZSTR_LEN(op2))` and the two
    // `memcpy` into it; then the two operands' strings are given up.
    let joined = PhpString::filled(req, a.len() + b.len(), |bytes| {
        let (head, tail) = bytes.split_at_mut(a.len());
        head.copy_from_slice(a.as_bytes());
        tail.copy_from_slice(b.as_bytes());
    });
    req.release(Value::string(a));
    req.release(Value::string(b));
    Ok(Value::string(joined?))
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
/// greater than a null; against anything else it is the greater side,
/// which is `zend_std_compare_objects`'s answer when the cast fails, and
/// the property walk of two objects of one class and the warning of a
/// cast to a number wait with the classes.
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
pub(crate) fn compare(req: &mut Request, left: &Value, right: &Value) -> Ordering {
    let (left, right) = (&left.deref(), &right.deref());
    if let (Some(a), Some(b)) = (scalar(left), scalar(right)) {
        return php_heap::compare(a, b, req.precision());
    }
    match (left.as_ref(), right.as_ref()) {
        (ValueRef::Array(a), ValueRef::Array(b)) => compare_arrays(req, *a, *b),
        (ValueRef::Object(a), ValueRef::Object(b)) if a == b => Ordering::Equal,
        // php's default arm: a null or a bool as the other side's truth,
        // which is also what its own `IS_NULL` and `IS_OBJECT` pairs
        // answer; then an object, then an array, then both sides as
        // numbers.
        (ValueRef::Undef | ValueRef::Null | ValueRef::Bool(false), _) => {
            if is_true(req, right) {
                Ordering::Less
            } else {
                Ordering::Equal
            }
        }
        (ValueRef::Bool(true), _) => {
            if is_true(req, right) {
                Ordering::Equal
            } else {
                Ordering::Greater
            }
        }
        (_, ValueRef::Undef | ValueRef::Null | ValueRef::Bool(false)) => {
            if is_true(req, left) {
                Ordering::Greater
            } else {
                Ordering::Equal
            }
        }
        (_, ValueRef::Bool(true)) => {
            if is_true(req, left) {
                Ordering::Equal
            } else {
                Ordering::Less
            }
        }
        (ValueRef::Object(_), _) => Ordering::Greater,
        (_, ValueRef::Object(_)) => Ordering::Less,
        (ValueRef::Array(_), _) => Ordering::Greater,
        (_, ValueRef::Array(_)) => Ordering::Less,
        (a, b) => php_heap::compare(silent_number(a), silent_number(b), req.precision()),
    }
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
#[expect(
    clippy::unnecessary_wraps,
    reason = "the shape every binary kernel has, so one handler body serves them all"
)]
pub(crate) fn is_smaller(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    Ok(Value::bool(compare(req, left, right) == Ordering::Less))
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
#[expect(
    clippy::unnecessary_wraps,
    reason = "the shape every binary kernel has, so one handler body serves them all"
)]
pub(crate) fn is_smaller_or_equal(
    req: &mut Request,
    left: &Value,
    right: &Value,
) -> Result<Value, Thrown> {
    Ok(Value::bool(compare(req, left, right) != Ordering::Greater))
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
#[expect(
    clippy::unnecessary_wraps,
    reason = "the shape every binary kernel has, so one handler body serves them all"
)]
pub(crate) fn is_equal(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    Ok(Value::bool(compare(req, left, right) == Ordering::Equal))
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
#[expect(
    clippy::unnecessary_wraps,
    reason = "the shape every binary kernel has, so one handler body serves them all"
)]
pub(crate) fn is_not_equal(
    req: &mut Request,
    left: &Value,
    right: &Value,
) -> Result<Value, Thrown> {
    Ok(Value::bool(compare(req, left, right) != Ordering::Equal))
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
#[expect(
    clippy::unnecessary_wraps,
    reason = "the shape every binary kernel has, so one handler body serves them all"
)]
pub(crate) fn spaceship(req: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    Ok(Value::int(match compare(req, left, right) {
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
/// (`GET_OP*_ZVAL_PTR_DEREF`). The witness reaches the scalars; the
/// array arm is held by the test beside it until the compiler reads an
/// array.
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
pub(crate) fn identical(left: &Value, right: &Value) -> bool {
    let (left, right) = (&left.deref(), &right.deref());
    if let (Some(a), Some(b)) = (scalar(left), scalar(right)) {
        return php_heap::identical(a, b);
    }
    match (left.as_ref(), right.as_ref()) {
        (ValueRef::Array(a), ValueRef::Array(b)) => {
            a.len() == b.len()
                && a.iter()
                    .zip(b.iter())
                    .all(|((k, v), (l, w))| k == l && identical(&v, &w))
        }
        (ValueRef::Object(a), ValueRef::Object(b)) => a == b,
        (ValueRef::Resource(a), ValueRef::Resource(b)) => a == b,
        _ => false,
    }
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
#[expect(
    clippy::unnecessary_wraps,
    reason = "the shape every binary kernel has, so one handler body serves them all"
)]
pub(crate) fn is_identical(_: &mut Request, left: &Value, right: &Value) -> Result<Value, Thrown> {
    Ok(Value::bool(identical(left, right)))
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
#[expect(
    clippy::unnecessary_wraps,
    reason = "the shape every binary kernel has, so one handler body serves them all"
)]
pub(crate) fn is_not_identical(
    _: &mut Request,
    left: &Value,
    right: &Value,
) -> Result<Value, Thrown> {
    Ok(Value::bool(!identical(left, right)))
}

/// The scalar a value is, for [`php_heap::compare`]; nothing for an
/// array, an object, a resource or a reference not yet followed.
fn scalar(value: &Value) -> Option<Scalar<'_>> {
    Some(match value.as_ref() {
        // php's `IS_NULL` arm; an uninitialised zval reads as null here.
        ValueRef::Undef | ValueRef::Null => Scalar::Null,
        ValueRef::Bool(b) => Scalar::Bool(b),
        ValueRef::Int(n) => Scalar::Long(n),
        ValueRef::Float(f) => Scalar::Double(f),
        ValueRef::Str(s) => Scalar::Str(s.as_bytes()),
        ValueRef::Array(_)
        | ValueRef::Object(_)
        | ValueRef::Resource(_)
        | ValueRef::Reference(_) => {
            return None;
        }
    })
}

/// `_zendi_convert_scalar_to_number_silent` over what [`compare`]'s
/// default arm has left: a resource is its id, and a string the number
/// it opens with or zero, read with `allow_errors` and no warning. A
/// number is itself; nothing else reaches here.
fn silent_number(value: ValueRef<'_>) -> Scalar<'static> {
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
/// (`Z/Zend/zend_hash.c:3155`): the counts first, then each key of the
/// left array looked up in the right one, a key the right one lacks
/// making the left the greater, and the two values compared by
/// [`compare`] until one pair differs.
fn compare_arrays(req: &mut Request, left: PhpArray, right: PhpArray) -> Ordering {
    if left.len() != right.len() {
        return left.len().cmp(&right.len());
    }
    for (key, value) in left.snapshot() {
        let Some(other) = right.get(&key) else {
            return Ordering::Greater;
        };
        let ordering = compare(req, &value, &other);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
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
/// is handed back rather than written in place: a reference the
/// variable holds is followed with the memory model (design §11, Lane A).
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
        ValueRef::Int(n) => Ok(match n.checked_add(1) {
            Some(more) => Value::int(more),
            None => Value::float(wide(i64::MAX) + 1.0),
        }),
        ValueRef::Float(f) => Ok(Value::float(f + 1.0)),
        // php's `IS_NULL` arm; an uninitialised zval reads as null here.
        ValueRef::Undef | ValueRef::Null => Ok(Value::int(1)),
        ValueRef::Str(s) => {
            // `is_numeric_str_function`, which is the reader with
            // `allow_errors` off: a string with anything past its number
            // is not a number here.
            match numeric_prefix(s.as_bytes()) {
                Some((Number::Long(i64::MAX), false)) => Ok(Value::float(wide(i64::MAX) + 1.0)),
                Some((Number::Long(n), false)) => Ok(Value::int(n + 1)),
                Some((Number::Double(d), false)) => Ok(Value::float(d + 1.0)),
                _ => Ok(Value::string(increment_string(req, s.as_bytes())?)),
            }
        }
        ValueRef::Bool(_) => {
            warning(
                req,
                "Increment on type bool has no effect, this will change in the next major version of PHP",
            );
            Ok(*value)
        }
        ValueRef::Array(_)
        | ValueRef::Resource(_)
        | ValueRef::Object(_)
        | ValueRef::Reference(_) => {
            let name = value_name(req, value);
            Err(throw(req, "TypeError", &format!("Cannot increment {name}")))
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
/// place, as [`increment`]'s is.
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
        ValueRef::Int(n) => Ok(match n.checked_sub(1) {
            Some(less) => Value::int(less),
            None => Value::float(wide(i64::MIN) - 1.0),
        }),
        ValueRef::Float(f) => Ok(Value::float(f - 1.0)),
        ValueRef::Str(s) if s.as_bytes().is_empty() => {
            deprecated(
                req,
                "Decrement on empty string is deprecated as non-numeric",
            );
            Ok(Value::int(-1))
        }
        ValueRef::Str(s) => match numeric_prefix(s.as_bytes()) {
            Some((Number::Long(i64::MIN), false)) => Ok(Value::float(wide(i64::MIN) - 1.0)),
            Some((Number::Long(n), false)) => Ok(Value::int(n - 1)),
            Some((Number::Double(d), false)) => Ok(Value::float(d - 1.0)),
            _ => {
                deprecated(
                    req,
                    "Decrement on non-numeric string has no effect and is deprecated",
                );
                Ok(*value)
            }
        },
        // php's `IS_NULL` arm; an uninitialised zval reads as null here.
        ValueRef::Undef | ValueRef::Null => {
            warning(
                req,
                "Decrement on type null has no effect, this will change in the next major version of PHP",
            );
            Ok(Value::null())
        }
        ValueRef::Bool(_) => {
            warning(
                req,
                "Decrement on type bool has no effect, this will change in the next major version of PHP",
            );
            Ok(*value)
        }
        ValueRef::Array(_)
        | ValueRef::Resource(_)
        | ValueRef::Object(_)
        | ValueRef::Reference(_) => {
            let name = value_name(req, value);
            Err(throw(req, "TypeError", &format!("Cannot decrement {name}")))
        }
    }
}

/// `increment_string` (`Z/Zend/zend_operators.c:2613`), "Perl style
/// string increment", behind its deprecation: from the last byte back, a
/// letter or a digit steps to the next of its kind, with `z`, `Z` and
/// `9` wrapping to `a`, `A` and `0` and carrying into the byte before;
/// a byte that is none of the three stops the carry where it stands; and
/// a carry off the front prepends the first of the last kind stepped. The
/// empty string is `"1"`.
fn increment_string(req: &mut Request, bytes: &[u8]) -> Result<PhpString, Thrown> {
    deprecated(
        req,
        "Increment on non-numeric string is deprecated, use str_increment() instead",
    );
    if bytes.is_empty() {
        return PhpString::new(req, b"1");
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
    PhpString::new(req, &out)
}

/// `ZEND_STRLEN`'s handler (`Z/Zend/zend_vm_def.h:8759`), the length of
/// the string an operand is, which is what php's compiler emits for
/// `strlen($a)` in place of the call (`zend_compile_func_strlen`),
/// so the native of the same name serves `strlen` only where the
/// compiler could not see it. A string is its byte length. Anything else
/// goes the way `Z_PARAM_STR` would take it: under `strict_types` a
/// `TypeError`; otherwise `null` is the 8.1 deprecation and zero, a
/// bool, a long or a double is the length of the string it converts to
/// (`zend_parse_arg_str_weak`, so a NAN warns as [`to_string`] warns),
/// and an array, a resource or an object with no `__toString` is
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
    if let Some(s) = value.as_php_string() {
        return Ok(Value::int(i64::try_from(s.len()).unwrap_or(i64::MAX)));
    }
    if !req.caller_strict_types() {
        match value.as_ref() {
            ValueRef::Undef | ValueRef::Null => {
                deprecated(
                    req,
                    "strlen(): Passing null to parameter #1 ($string) of type string is deprecated",
                );
                return Ok(Value::int(0));
            }
            ValueRef::Bool(_) | ValueRef::Int(_) | ValueRef::Float(_) => {
                let text = to_string(req, value)?;
                let len = i64::try_from(text.len()).unwrap_or(i64::MAX);
                req.release(Value::string(text));
                return Ok(Value::int(len));
            }
            _ => {}
        }
    }
    let name = value_name(req, value);
    Err(throw(
        req,
        "TypeError",
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
    if (mask >> php_type(value)) & 1 == 0 {
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
/// is served by its `count_elements` handler or its `Countable::count`,
/// neither of which any class here has yet, so it is the `TypeError`;
/// and anything else is the `TypeError` too, naming the function as it
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
pub(crate) fn count(req: &mut Request, value: &Value, sizeof: bool) -> Result<i64, Thrown> {
    let value = &value.deref();
    if let Some(array) = value.as_array() {
        return Ok(i64::try_from(array.len()).unwrap_or(i64::MAX));
    }
    let function = if sizeof { "sizeof" } else { "count" };
    let name = value_name(req, value);
    Err(throw(
        req,
        "TypeError",
        &format!(
            "{function}(): Argument #1 ($value) must be of type Countable|array, {name} given"
        ),
    ))
}

/// `MAY_BE_RESOURCE`, `1 << IS_RESOURCE`: the one mask `ZEND_TYPE_CHECK`
/// reads past the type byte.
const MAY_BE_RESOURCE: u32 = 1 << (ZvalType::Resource as u32);

/// `Z_TYPE_P`: php's number for a value's type, the `IS_*` of
/// `zend_types.h`, which `ZEND_TYPE_CHECK` shifts its mask by. A bool is
/// two of them, `IS_FALSE` and `IS_TRUE`.
fn php_type(value: &Value) -> u32 {
    let ty = match value.as_ref() {
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
    };
    ty as u32
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
        ValueRef::Object(o) => req.class_decl(o.class()).map_or_else(
            || String::from("object"),
            |decl| String::from_utf8_lossy(&decl.name.0).into_owned(),
        ),
        ValueRef::Bool(b) => String::from(if b { "true" } else { "false" }),
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

/// What `fast_long_add_function` and its kin leave: the long where the
/// operation stayed in range, and the double php computed again where it
/// did not.
fn promoted(stayed: Option<i64>, overflowed: f64) -> Value {
    stayed.map_or_else(|| Value::float(overflowed), Value::int)
}

/// `add_function_array`: a copy of the left array (`zend_array_dup`)
/// with every key of the right one it does not already carry, which is
/// `zend_hash_merge` with php's `overwrite` off and `zval_add_ref` on
/// each element taken. The copy has one owner, the caller's.
fn merged(req: &mut Request, left: PhpArray, right: PhpArray) -> Result<PhpArray, Thrown> {
    let mut union = left.copy(req)?;
    for (key, value) in right.snapshot_refs() {
        if !union.contains(&key) {
            req.addref(&value);
            union.set(req, key, value)?;
        }
    }
    Ok(union)
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
    let (left, right) = (&left.deref(), &right.deref());
    let unsupported = |req: &mut Request| {
        throw(
            req,
            "TypeError",
            &format!(
                "Unsupported operand types: {} {operator} {}",
                left.type_name(),
                right.type_name()
            ),
        )
    };
    let Some(a) = to_number(req, left) else {
        return Err(unsupported(req));
    };
    let Some(b) = to_number(req, right) else {
        return Err(unsupported(req));
    };
    Ok((a, b))
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
    let (left, right) = (&left.deref(), &right.deref());
    let unsupported = |req: &mut Request| {
        throw(
            req,
            "TypeError",
            &format!(
                "Unsupported operand types: {} {operator} {}",
                left.type_name(),
                right.type_name()
            ),
        )
    };
    let Some(a) = to_long(req, left) else {
        return Err(unsupported(req));
    };
    let Some(b) = to_long(req, right) else {
        return Err(unsupported(req));
    };
    Ok((a, b))
}

/// `zendi_try_get_long`: the long php works in, or nothing for an array,
/// an object or a resource, which is the failure `zend_binop_error`
/// reports. A double goes through [`double_to_long`]; a string is read
/// by [`numeric_prefix`] with php's warning for what follows its number,
/// and a double among them is `zend_dval_to_lval_cap`'s saturated long,
/// with the float-string deprecation where that lost something.
fn to_long(req: &mut Request, value: &Value) -> Option<i64> {
    if let Some(n) = value.as_int() {
        return Some(n);
    }
    if let Some(d) = value.as_float() {
        return Some(double_to_long(req, d));
    }
    if let Some(s) = value.as_php_string() {
        let (number, trailing) = numeric_prefix(s.as_bytes())?;
        if trailing {
            warning(req, "A non-numeric value encountered");
        }
        return Some(match number {
            Number::Long(n) => n,
            Number::Double(d) => {
                let capped = dval_to_lval_cap(d);
                if !long_compatible(d, capped) {
                    deprecated(
                        req,
                        &format!(
                            "Implicit conversion from float-string \"{}\" to int loses precision",
                            String::from_utf8_lossy(s.as_bytes())
                        ),
                    );
                }
                capped
            }
        });
    }
    if let Some(b) = value.as_bool() {
        return Some(i64::from(b));
    }
    match value.tag() {
        // php's `IS_NULL` arm; an uninitialised zval reads as null here.
        TypeTag::Null | TypeTag::Undef => Some(0),
        _ => None,
    }
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
                deprecated(
                    req,
                    &format!(
                        "Implicit conversion from float-string \"{}\" to int loses precision",
                        String::from_utf8_lossy(s.as_bytes())
                    ),
                );
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
fn double_to_long(req: &mut Request, d: f64) -> i64 {
    let l = dval_to_lval(req, d);
    if !long_compatible(d, l) && fits_long(d) {
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
    let oob = |req: &mut Request| {
        let text = rendered(d);
        warning(
            req,
            &format!("The float {text} is not representable as an int, cast occurred"),
        );
    };
    if !d.is_finite() {
        oob(req);
        0
    } else if !fits_long(d) {
        oob(req);
        modular(d)
    } else {
        truncated(d)
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
    String::from_utf8_lossy(double_to_string(d, -1).as_bytes()).into_owned()
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

/// `zendi_try_convert_scalar_to_number`: the number php works in, or
/// nothing for an array, an object or a resource, which is the failure
/// `zend_binop_error` reports. A string is read by [`numeric_prefix`],
/// the one reader php's compiler folds through as well, and one with
/// anything past the number it opens with raises php's warning and keeps
/// the number.
fn to_number(req: &mut Request, value: &Value) -> Option<Number> {
    if let Some(n) = value.as_int() {
        return Some(Number::Long(n));
    }
    if let Some(f) = value.as_float() {
        return Some(Number::Double(f));
    }
    if let Some(s) = value.as_php_string() {
        let (number, trailing) = numeric_prefix(s.as_bytes())?;
        if trailing {
            warning(req, "A non-numeric value encountered");
        }
        return Some(number);
    }
    if let Some(b) = value.as_bool() {
        return Some(Number::Long(i64::from(b)));
    }
    match value.tag() {
        // php's `IS_NULL` arm; an uninitialised zval reads as null here.
        TypeTag::Null | TypeTag::Undef => Some(Number::Long(0)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::values::Key;

    fn array(req: &mut Request, entries: Vec<(Key, Value)>) -> Value {
        let Ok(mut a) = PhpArray::new(req) else {
            return Value::null();
        };
        for (k, v) in entries {
            let _ = a.set(req, k, v);
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
        let start = req.memory_usage(false);
        let list = || vec![(Key::Int(0), Value::int(1)), (Key::Int(1), Value::int(2))];
        let (a, b) = (array(&mut req, list()), array(&mut req, list()));
        assert!(identical(&a, &b));
        let one = array(&mut req, vec![(Key::Int(0), Value::int(1))]);
        assert!(!identical(&a, &one));
        let swapped = array(
            &mut req,
            vec![(Key::Int(1), Value::int(2)), (Key::Int(0), Value::int(1))],
        );
        assert!(!identical(&a, &swapped));
        let floated = array(
            &mut req,
            vec![
                (Key::Int(0), Value::int(1)),
                (Key::Int(1), Value::float(2.0)),
            ],
        );
        assert!(!identical(&a, &floated));
        let inner = (array(&mut req, list()), array(&mut req, list()));
        let nested = (
            array(&mut req, vec![(Key::Int(0), inner.0)]),
            array(&mut req, vec![(Key::Int(0), inner.1)]),
        );
        assert!(identical(&nested.0, &nested.1));
        assert!(!identical(&a, &Value::int(1)));
        for value in [a, b, one, swapped, floated, nested.0, nested.1] {
            req.release(value);
        }
        assert_eq!(req.memory_usage(false), start, "every array went back");
    }
}

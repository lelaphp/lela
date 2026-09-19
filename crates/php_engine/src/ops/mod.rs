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

use php_unit::decl::Visibility;

use crate::api::abi::PropertyPurpose;
use crate::api::raising::{Thrown, deprecated, throw, warning};
use crate::api::request::{Request, append_escaped};
use crate::api::values::{Key, PhpArray, PhpObject, PhpString, TypeTag, Value, ValueRef};

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
        ValueRef::Object(o) => object_to_string(req, *o),
        // `false` and `null` are both the empty string; a reference was
        // followed above.
        ValueRef::Undef | ValueRef::Null | ValueRef::Bool(false) | ValueRef::Reference(_) => {
            PhpString::new(req, b"")
        }
    }
}

/// `zend_std_cast_object_tostring` (`Z/Zend/zend_object_handlers.c:2558`)
/// under `zval_get_string`: the class's `__toString` served on the
/// object with no arguments, and what it returned when that is a
/// string, the caller's to own as the method's return was. A class
/// with no `__toString` is php's `Error` naming the class, thrown by
/// `zval_get_string_func` once the handler failed; a `__toString` that
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
pub(crate) fn object_to_string(
    req: &mut Request,
    object: crate::api::values::PhpObject,
) -> Result<PhpString, Thrown> {
    let class = object.class();
    if req.method_named(class, b"__tostring").is_some() {
        let returned = crate::api::call::call_method(req, &object, b"__toString", &[])?;
        if let Some(text) = returned.as_php_string() {
            return Ok(*text);
        }
        req.release(returned);
        let name = req.class_name(class);
        return Err(throw(
            req,
            "Error",
            &format!("Method {name}::__toString() must return a string value"),
        ));
    }
    let name = req.class_name(class);
    Err(throw(
        req,
        "Error",
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
/// `null` is the empty array; a scalar, a resource and a `Closure` are
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
pub(crate) fn to_array(req: &mut Request, value: &Value) -> Result<PhpArray, Thrown> {
    let value = value.deref();
    match value.as_ref() {
        ValueRef::Undef | ValueRef::Null => PhpArray::new(req),
        ValueRef::Array(a) => {
            req.addref(&value);
            Ok(*a)
        }
        ValueRef::Object(&o) if req.class_named(b"Closure") != Some(o.class()) => {
            let table = req.object_handlers(o.heap());
            let rows = match table.get_properties_for(req, o, PropertyPurpose::ArrayCast)? {
                Some(rows) => rows,
                None => table.get_properties(req, o)?,
            };
            let mangled = mangled_names(req, o.class());
            let mut array = PhpArray::new(req)?;
            let mut at = 0;
            for (name, entry) in rows {
                let key = if let Some(prefix) = mangled_prefix(&mangled, &mut at, name) {
                    let bytes = [prefix.as_slice(), name.as_bytes()].concat();
                    Key::Str(PhpString::new(req, &bytes)?)
                } else {
                    req.addref(&Value::string(name));
                    Key::from_string(name)
                };
                req.addref(&entry);
                array.set(req, key, entry)?;
            }
            Ok(array)
        }
        _ => {
            if value.as_float().is_some_and(f64::is_nan) {
                warning(req, "unexpected NAN value was coerced to array");
            }
            let mut array = PhpArray::new(req)?;
            req.addref(&value);
            array.append(req, value)?;
            Ok(array)
        }
    }
}

/// One declared instance property as `zend_mangle_property_name` reads
/// it: the name as written, and the prefix the mangled key carries,
/// `None` for a public property, which is mangled with nothing.
struct MangledName {
    name: Box<[u8]>,
    prefix: Option<Vec<u8>>,
}

/// The prefix `zend_mangle_property_name` puts before each declared
/// instance property of `class` that is not public: `\0*\0` for a
/// protected one, `\0Class\0` for a private one, and none for a public
/// one. Every declared slot is a row, in slot order, so a caller that
/// walks the object's rows beside this one keeps its place.
fn mangled_names(req: &Request, class: crate::api::values::ClassId) -> Vec<MangledName> {
    req.placed_properties(class)
        .into_iter()
        .map(|(declaring, p)| {
            let prefix = match p.visibility {
                Visibility::Public => None,
                Visibility::Protected => Some(b"\0*\0".to_vec()),
                // `property_info->ce->name`: a private property is
                // mangled with the class that declared it, which for an
                // inherited one is a class above.
                Visibility::Private => req
                    .class_decl(declaring)
                    .map(|d| [&b"\0"[..], &d.name.0, &b"\0"[..]].concat()),
            };
            MangledName {
                name: p.name.0,
                prefix,
            }
        })
        .collect()
}

/// The prefix the row named `name` carries, the declared names walked
/// with a cursor rather than looked up: a class that declares `$p` over
/// a parent's private `$p` has two slots of that name, and the second
/// row is the second slot. `at` is the caller's place in that walk and
/// stands where the row was found.
fn mangled_prefix(mangled: &[MangledName], at: &mut usize, name: PhpString) -> Option<Vec<u8>> {
    let found = mangled
        .iter()
        .skip(*at)
        .position(|slot| &*slot.name == name.as_bytes())
        .map(|i| i + *at)?;
    *at = found + 1;
    mangled.get(found).and_then(|slot| slot.prefix.clone())
}

/// Every class name a declared type holds, in the order it was written:
/// the one name of a class type, and each member of a union or an
/// intersection that is one, which `zend_check_type_slow` walks with
/// `ZEND_TYPE_LIST_FOREACH`. What reads it is every check of a value
/// against a declared type that a class name can stand in: a return, a
/// parameter and a property.
///
/// ```php
/// class A {} class B extends A {}
/// function f(): A { return new B; }
/// class C { public A $a; }
/// $c = new C; $c->a = new B;
/// var_dump(f() instanceof B, $c->a instanceof B);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
pub(crate) fn named_types(
    ty: &php_unit::decl::Type,
) -> Box<dyn Iterator<Item = &php_unit::decl::Name> + '_> {
    use php_unit::decl::Type;
    match ty {
        Type::Named(name) => Box::new(std::iter::once(name)),
        Type::Nullable(inner) => named_types(inner),
        Type::Union(members) | Type::Intersection(members) => {
            Box::new(members.iter().flat_map(named_types))
        }
        _ => Box::new(std::iter::empty()),
    }
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
    match type_assignable(req, &held, ty, strict) {
        Assignable::Yes => Ok(None),
        Assignable::Coerce => {
            match crate::api::coerce::scalar_type_hint(req, &held, ty.mask(), strict)? {
                Some(coerced) => Ok(Some(coerced)),
                None => Err(property_type_error(req, &held, ty, declaring, name)),
            }
        }
        Assignable::No => Err(property_type_error(req, &held, ty, declaring, name)),
    }
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
    strict: bool,
) -> Assignable {
    if ty.contains_code(held.type_code()) {
        return Assignable::Yes;
    }
    if let Some(object) = held.as_object()
        && named_types(ty).any(|named| {
            req.class_named(&named.0)
                .is_some_and(|of| req.instance_of(object.class(), of))
        })
    {
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
        let verdict = type_assignable(req, &held, ty, strict);
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
                    let agrees = identical(&old, &made);
                    req.release(made);
                    if !agrees {
                        req.release(old);
                        return Err(conflicting_coercion(req, &held, &listed, at, i));
                    }
                    coerced = Some(old);
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
    let first = req
        .ref_type_sources(cell)
        .first()
        .map(|s| (s.declaring, s.name.clone(), s.ty.clone()));
    let Some((first_at, first_name, first_ty)) = first else {
        return verify_property_type(req, &held, ty, declaring, name, strict);
    };
    match type_assignable(req, &held, ty, strict) {
        Assignable::Yes => return Ok(None),
        Assignable::Coerce => {
            // The value would convert, so what refuses the bind is the
            // conversion itself: one name's value cannot change because
            // the other was bound.
            if let Some(made) = crate::api::coerce::scalar_type_hint(req, &held, ty.mask(), strict)?
            {
                req.release(made);
                let class = req.class_name(first_at);
                let two = req.class_name(declaring);
                let one_declared = crate::api::coerce::type_name(&first_ty);
                let two_declared = crate::api::coerce::type_name(ty);
                let given = type_name(req, &held.undef_as_null());
                return Err(throw(
                    req,
                    "TypeError",
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
    Err(property_type_error(req, &held, ty, declaring, name))
}

/// `zend_throw_incdec_prop_error` (`Z/Zend/zend_execute.c:2211`): the
/// `TypeError` a step raises where an `int` slot would have to hold the
/// float it stepped into, and the value the slot takes instead, which
/// is php's own `ZEND_LONG_MAX` or `ZEND_LONG_MIN`.
///
/// ```php
/// class C { public static int $n = PHP_INT_MIN; }
/// try { --C::$n; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(C::$n);
/// ```
/// ```expect
/// Cannot decrement property C::$n of type int past its minimal value
/// int(-9223372036854775808)
/// ```
pub(crate) fn incdec_prop_error(
    req: &mut Request,
    ty: &php_unit::decl::Type,
    declaring: crate::api::values::ClassId,
    name: &[u8],
    increment: bool,
) -> (Thrown, Value) {
    let class = req.class_name(declaring);
    let declared = crate::api::coerce::type_name(ty);
    let (word, edge, limit) = if increment {
        ("increment", "maximal", i64::MAX)
    } else {
        ("decrement", "minimal", i64::MIN)
    };
    let thrown = throw(
        req,
        "TypeError",
        &format!(
            "Cannot {word} property {class}::${} of type {declared} past its {edge} value",
            String::from_utf8_lossy(name)
        ),
    );
    (thrown, Value::int(limit))
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
fn ref_type_error(
    req: &mut Request,
    value: &Value,
    ty: &php_unit::decl::Type,
    declaring: crate::api::values::ClassId,
    name: &[u8],
) -> Thrown {
    let class = req.class_name(declaring);
    let declared = crate::api::coerce::type_name(ty);
    let given = value_name(req, &value.undef_as_null());
    throw(
        req,
        "TypeError",
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
    let Some((one_at, one_name, one_ty)) = listed.get(first) else {
        return throw(req, "TypeError", "Cannot assign to reference");
    };
    let Some((two_at, two_name, two_ty)) = listed.get(second) else {
        return throw(req, "TypeError", "Cannot assign to reference");
    };
    let one_class = req.class_name(*one_at);
    let two_class = req.class_name(*two_at);
    let one_declared = crate::api::coerce::type_name(one_ty);
    let two_declared = crate::api::coerce::type_name(two_ty);
    let given = value_name(req, &value.undef_as_null());
    throw(
        req,
        "TypeError",
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
fn property_type_error(
    req: &mut Request,
    value: &Value,
    ty: &php_unit::decl::Type,
    declaring: crate::api::values::ClassId,
    name: &[u8],
) -> Thrown {
    let class = req.class_name(declaring);
    let declared = crate::api::coerce::type_name(ty);
    let given = value_name(req, &value.undef_as_null());
    throw(
        req,
        "TypeError",
        &format!(
            "Cannot assign {given} to property {class}::${} of type {declared}",
            String::from_utf8_lossy(name)
        ),
    )
}

/// `zend_get_properties_for(object, purpose)`, the rows one reader of
/// an object's properties sees: the class's `get_properties_for` first and
/// `get_properties` where that slot is null, each declared name carrying
/// the prefix `zend_mangle_property_name` puts before it, `\0*\0p` for a
/// protected property and `\0C\0p` for a private one, so a reader
/// unmangles the name as php's `print_hash` does and never asks the
/// engine for a visibility beside it. Views, no owner taken, and a
/// mangled name is interned as php's is, so a second walk of the same
/// object allocates nothing.
///
/// ```php
/// class A { public $a = 1; protected $b = 2; private $c = 3; }
/// class B extends A { private $c = 4; }
/// print_r(new B);
/// ```
/// ```expect
/// B Object
/// (
///     [a] => 1
///     [b:protected] => 2
///     [c:A:private] => 3
///     [c:B:private] => 4
/// )
/// ```
pub(crate) fn properties_for(
    req: &mut Request,
    object: PhpObject,
    purpose: PropertyPurpose,
) -> Result<Vec<(PhpString, Value)>, Thrown> {
    let table = req.object_handlers(object.heap());
    // What `get_properties_for` answers is the table php hands back as
    // it stands, whose names are already what a reader shows; what
    // `get_properties` answers is the members, whose names the slot
    // carries unmangled here.
    let rows = match table.get_properties_for(req, object, purpose)? {
        Some(rows) => return Ok(rows),
        None => table.get_properties(req, object)?,
    };
    let mangled = mangled_names(req, object.class());
    let mut at = 0;
    let mut out = Vec::with_capacity(rows.len());
    for (name, entry) in rows {
        let name = match mangled_prefix(&mangled, &mut at, name) {
            Some(prefix) => {
                let bytes = [prefix.as_slice(), name.as_bytes()].concat();
                req.interned(&bytes)?
            }
            None => name,
        };
        out.push((name, entry));
    }
    Ok(out)
}

/// `zend_cast_zval_to_object` (`Z/Zend/zend_execute.h:225`) under the
/// `Z_TYPE_P(expr) == IS_OBJECT` arm of `ZEND_CAST` above it: an object
/// is itself with one more owner; `null` is an empty `stdClass`; an
/// array is a `stdClass` whose properties are its elements, an integer
/// key spelt in decimal (`zend_symtable_to_proptable`), each value with
/// one more owner; and a scalar or a resource is a `stdClass` with the
/// one property `scalar` holding it, a NAN warning on the way. The
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
    if let ValueRef::Object(&o) = value.as_ref() {
        req.addref(&value);
        return Ok(o);
    }
    let object = req.new_object_of("stdClass")?;
    match value.as_ref() {
        ValueRef::Undef | ValueRef::Null => {}
        ValueRef::Array(a) => {
            for (key, entry) in a.snapshot_refs() {
                let name = key.to_prop_name(req)?;
                if matches!(key, Key::Str(_)) {
                    req.addref(&Value::string(name));
                }
                req.addref(&entry);
                object.set_prop(req, name, entry)?;
            }
        }
        _ => {
            if value.as_float().is_some_and(f64::is_nan) {
                warning(req, "unexpected NAN value was coerced to object");
            }
            let name = PhpString::new(req, b"scalar")?;
            req.addref(&value);
            object.set_prop(req, name, value)?;
        }
    }
    Ok(object)
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

/// Which of the three bitwise operators a call of [`bitwise`] is: the
/// one thing `bitwise_or_function`, `bitwise_xor_function` and
/// `bitwise_and_function` differ by.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bitwise {
    Or,
    Xor,
    And,
}

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

    /// The operator over one pair of bytes.
    const fn byte(self, left: u8, right: u8) -> u8 {
        match self {
            Bitwise::Or => left | right,
            Bitwise::Xor => left ^ right,
            Bitwise::And => left & right,
        }
    }

    /// The operator over two longs.
    const fn long(self, left: i64, right: i64) -> i64 {
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
        let joined = PhpString::filled(req, len, |bytes| {
            for (out, (l, r)) in bytes.iter_mut().zip(longer.iter().zip(shorter)) {
                *out = op.byte(*l, *r);
            }
            if let Some(tail) = bytes.get_mut(shorter.len()..) {
                tail.copy_from_slice(&longer[shorter.len()..len]);
            }
        })?;
        return Ok(Value::string(joined));
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
pub(crate) fn bitwise_not(req: &mut Request, value: &Value) -> Result<Value, Thrown> {
    let value = value.deref();
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
        let negated = PhpString::filled(req, bytes.len(), |out| {
            for (out, b) in out.iter_mut().zip(bytes) {
                *out = !*b;
            }
        })?;
        return Ok(Value::string(negated));
    }
    let name = value_name(req, &value);
    Err(throw(
        req,
        "TypeError",
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
    let (left, right) = (&left.deref(), &right.deref());
    if let (Some(a), Some(b)) = (scalar(left), scalar(right)) {
        return Ok(php_heap::compare(a, b, req.precision()));
    }
    Ok(match (left.as_ref(), right.as_ref()) {
        (ValueRef::Array(a), ValueRef::Array(b)) => compare_arrays(req, *a, *b)?,
        (ValueRef::Object(a), ValueRef::Object(b)) if a == b => Ordering::Equal,
        // php's default arm: a null or a bool as the other side's truth,
        // which is also what its own `IS_NULL` and `IS_OBJECT` pairs
        // answer and what `cast_object` to `_IS_BOOL` answers for an
        // object; then an object's handler, then an array, then both
        // sides as numbers.
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
        // `Z_OBJ_HANDLER_P(object, compare)(op1, op2)`, the object side's
        // table; `None` is `ZEND_UNCOMPARABLE`, which is `1`.
        (ValueRef::Object(o), _) | (_, ValueRef::Object(o)) => req
            .object_handlers(o.heap())
            .compare(req, left, right)?
            .unwrap_or(Ordering::Greater),
        (ValueRef::Array(_), _) => Ordering::Greater,
        (_, ValueRef::Array(_)) => Ordering::Less,
        (a, b) => php_heap::compare(silent_number(a), silent_number(b), req.precision()),
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
fn compare_arrays(req: &mut Request, left: PhpArray, right: PhpArray) -> Result<Ordering, Thrown> {
    if left.heap().is_same(right.heap()) {
        return Ok(Ordering::Equal);
    }
    if left.is_recursive() {
        return Err(crate::api::raising::throw(
            req,
            "Error",
            "Nesting level too deep - recursive dependency?",
        ));
    }
    left.protect();
    let ordering = compare_entries(req, left, right);
    left.unprotect();
    ordering
}

/// `zend_hash_compare_impl` under the protection
/// [`compare_arrays`] takes.
fn compare_entries(req: &mut Request, left: PhpArray, right: PhpArray) -> Result<Ordering, Thrown> {
    if left.len() != right.len() {
        return Ok(left.len().cmp(&right.len()));
    }
    for (key, value) in left.snapshot() {
        let Some(other) = right.get(&key) else {
            return Ok(Ordering::Greater);
        };
        let ordering = compare(req, &value, &other)?;
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(Ordering::Equal)
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
            // php steps nothing here, so the variable keeps what it
            // holds; the value handed back is a second owner of it,
            // which the caller's write-back gives the variable again.
            req.addref(value);
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
                // As [`increment`]'s bool arm: nothing is stepped, and
                // the value handed back is a second owner of the string
                // the variable holds.
                req.addref(value);
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
            req.addref(value);
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
    if !req.strict_types() {
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
            return Err(throw(
                req,
                "Error",
                "Cannot add element to the array as the next element is already occupied",
            ));
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
    let empty = |req: &mut Request| Ok(php_heap::Key::Str(PhpString::new(req, b"")?.heap()));
    match offset.as_ref() {
        ValueRef::Str(s) if constant => Ok(php_heap::Key::Str(s.heap())),
        ValueRef::Str(s) => Ok(Key::from_string(*s).heap()),
        ValueRef::Int(n) => Ok(php_heap::Key::Long(n)),
        ValueRef::Reference(r) => array_offset(req, &r.get(), false),
        ValueRef::Null => {
            deprecated(
                req,
                "Using null as an array offset is deprecated, use an empty string instead",
            );
            if req.is_pending() {
                return Err(Thrown::of(None));
            }
            empty(req)
        }
        ValueRef::Float(f) => Ok(php_heap::Key::Long(double_to_long(req, f))),
        ValueRef::Bool(b) => Ok(php_heap::Key::Long(i64::from(b))),
        ValueRef::Resource(r) => {
            let id = r.id();
            warning(
                req,
                &format!("Resource ID#{id} used as offset, casting to integer ({id})"),
            );
            Ok(php_heap::Key::Long(id))
        }
        ValueRef::Undef => empty(req),
        ValueRef::Array(_) => Err(throw(
            req,
            "TypeError",
            "Cannot access offset of type array on array",
        )),
        ValueRef::Object(o) => {
            let class = req.class_name(o.class());
            Err(throw(
                req,
                "TypeError",
                &format!("Cannot access offset of type {class} on array"),
            ))
        }
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
pub(crate) fn isset_dim(
    req: &mut Request,
    array: PhpArray,
    offset: &Value,
    constant: bool,
    empty: bool,
) -> Result<bool, Thrown> {
    let key = isset_offset(req, offset, constant)?;
    let found = array.heap().get(key).map(crate::exec::value_of);
    if empty {
        return Ok(match found {
            Some(value) => !is_true(req, &value.deref()),
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
        ValueRef::Array(_) => Err(throw(
            req,
            "TypeError",
            "Cannot access offset of type array in isset or empty",
        )),
        ValueRef::Object(object) => {
            let class = req.class_name(object.class());
            Err(throw(
                req,
                "TypeError",
                &format!("Cannot access offset of type {class} in isset or empty"),
            ))
        }
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
        ValueRef::Str(s) => Key::from_string(*s).heap(),
        ValueRef::Int(n) => php_heap::Key::Long(n),
        ValueRef::Bool(b) => php_heap::Key::Long(i64::from(b)),
        ValueRef::Float(f) => {
            // `GC_TRY_ADDREF(ht)` before the deprecation and `GC_DELREF`
            // after it: a handler that unset the container took the last
            // owner, and the table is destroyed here rather than
            // deleted from.
            let held = array.to_value();
            req.addref(&held);
            let n = double_to_long(req, f);
            let alone = a.refcount() <= 1;
            req.release(held);
            if alone || req.is_pending() {
                return Ok(());
            }
            php_heap::Key::Long(n)
        }
        ValueRef::Resource(r) => {
            let id = r.id();
            warning(
                req,
                &format!("Resource ID#{id} used as offset, casting to integer ({id})"),
            );
            php_heap::Key::Long(id)
        }
        // `ZSTR_EMPTY_ALLOC()`, the key a `null` offset and a slot
        // nothing wrote both delete; the string is this function's to
        // give back, since `zend_hash_del` takes no owner of its key.
        ValueRef::Null | ValueRef::Undef => {
            let empty = PhpString::new(req, b"")?;
            req.array_del(a, php_heap::Key::Str(empty.heap()));
            req.release(Value::string(empty));
            return Ok(());
        }
        ValueRef::Array(_) => {
            return Err(throw(
                req,
                "TypeError",
                "Cannot unset offset of type array on array",
            ));
        }
        ValueRef::Object(o) => {
            let class = req.class_name(o.class());
            return Err(throw(
                req,
                "TypeError",
                &format!("Cannot unset offset of type {class} on array"),
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

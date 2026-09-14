//! The `standard` extension: `php-src/ext/standard` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade; one entry, `extension()`; its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/standard/tests`, through `xtask phpt --subset`.
//!
//! The natives so far, declared and paired with their entries as
//! `php_engine`'s own are, since the attributes under `php_ext::attr` hand
//! an item back unchanged until their expansion lands, in the order
//! `basic_functions.stub.php` declares them: `count` and its alias
//! `sizeof`, `ord`, `chr`, `str_repeat`, and the `is_*` family with its
//! aliases. The compiler serves a call of `count`, `sizeof`, `chr`,
//! `ord` or an `is_*` name itself where it reads the name, as a
//! constant or as `ZEND_COUNT` and `ZEND_TYPE_CHECK` over the engine's
//! kernels (`zend_try_compile_special_func`, #219), and reads it only
//! because the bundle declares the native; what reaches the native is a
//! call the compiler could not shape, which for the type checks is a
//! call of the wrong count of arguments until dynamic calls land, for
//! `count` the two-argument call, and for `chr` and `ord` an argument
//! not written as a value.
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::coerce::{coerce_param, is_type};
use php_ext::decl::{ConstDecl, ConstExpr, FunctionDecl, Name, Parameter, Type};
use php_ext::raising::{
    Level, Thrown, arg_count_error, arg_null_deprecated, arg_value_error, docref,
};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpString, TypeTag, Value, ValueRef};

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or synchronised.
#[derive(Default)]
struct State {}

/// The extension value (design §7.1): declarations paired with entries,
/// and the state declared on it.
///
/// ```php
/// var_dump(extension_loaded('standard'));
/// ```
/// ```expect
/// bool(true)
/// ```
pub fn extension() -> Extension {
    let countable = || {
        Type::Union(vec![
            Type::Named(Name(b"Countable".as_slice().into())),
            Type::Array,
        ])
    };
    let mode = || Parameter {
        default: Some(ConstExpr::Constant(Name(b"COUNT_NORMAL".as_slice().into()))),
        ..parameter(b"mode", Type::Int)
    };
    ExtensionBuilder::default()
        .functions(vec![
            (
                declared(
                    b"count",
                    &[parameter(b"value", countable()), mode()],
                    Type::Int,
                ),
                count,
            ),
            (
                declared(
                    b"sizeof",
                    &[parameter(b"value", countable()), mode()],
                    Type::Int,
                ),
                count,
            ),
            (
                declared(b"ord", &[parameter(b"character", Type::String)], Type::Int),
                ord,
            ),
            (
                declared(b"chr", &[parameter(b"codepoint", Type::Int)], Type::String),
                chr,
            ),
            (
                declared(
                    b"str_repeat",
                    &[
                        parameter(b"string", Type::String),
                        parameter(b"times", Type::Int),
                    ],
                    Type::String,
                ),
                str_repeat,
            ),
            (type_check_declared(b"is_null"), is_null),
            (type_check_declared(b"is_resource"), is_resource),
            (type_check_declared(b"is_bool"), is_bool),
            (type_check_declared(b"is_int"), is_int),
            (type_check_declared(b"is_integer"), is_int),
            (type_check_declared(b"is_long"), is_int),
            (type_check_declared(b"is_float"), is_float),
            (type_check_declared(b"is_double"), is_float),
            (type_check_declared(b"is_string"), is_string),
            (type_check_declared(b"is_array"), is_array),
            (type_check_declared(b"is_object"), is_object),
            (type_check_declared(b"is_scalar"), is_scalar),
        ])
        .constants(vec![
            constant(b"COUNT_NORMAL", COUNT_NORMAL),
            constant(b"COUNT_RECURSIVE", COUNT_RECURSIVE),
        ])
        .request_state::<State>()
        .build("standard")
}

/// `PHP_COUNT_NORMAL` and `PHP_COUNT_RECURSIVE`, what `count`'s
/// `$mode` is one of.
const COUNT_NORMAL: i64 = 0;
const COUNT_RECURSIVE: i64 = 1;

/// `count(Countable|array $value, int $mode = COUNT_NORMAL): int` and
/// its alias `sizeof`, php's `PHP_FUNCTION(count)`
/// (`Z/ext/standard/array.c:648`): the one-argument call is `ZEND_COUNT`
/// over the engine's kernel and never reaches here, so what does is the
/// call with a `$mode`. One that is neither constant is php's
/// `ValueError`, raised before the value is looked at; then the kernel's
/// arm through the face, `php_ext::coerce::count`, which is the element
/// count or the `TypeError`, and under `COUNT_RECURSIVE` the count of
/// every nested array added in (`php_count_recursive`), a reference
/// followed to what it holds. The `TypeError` names the function as it
/// was called, `sizeof` for the alias, since php reads the name off the
/// active function.
///
/// ```php
/// var_dump(count(token_get_all("<?php 1;"), COUNT_RECURSIVE), sizeof(token_get_all("<?php 1;"), 1), count(token_get_all("<?php 1;"), 0), count(token_get_all("<?php 1;"), true), count(token_get_all("<?php 1;"), "1"), count(token_get_all("<?php 1;"), null));
/// ```
/// ```expect
/// Deprecated: count(): Passing null to parameter #2 ($mode) of type int is deprecated in %s on line 2
/// int(9)
/// int(9)
/// int(3)
/// int(9)
/// int(9)
/// int(3)
/// ```
///
/// ```php
/// var_dump(count("x", 2));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ValueError: count(): Argument #2 ($mode) must be either COUNT_NORMAL or COUNT_RECURSIVE in %s:2
/// Stack trace:
/// #0 %s(2): count('x', 2)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// var_dump(sizeof(false, 0));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: sizeof(): Argument #1 ($value) must be of type Countable|array, false given in %s:2
/// Stack trace:
/// #0 %s(2): sizeof(false, 0)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn count(req: &mut Request) -> Result<Value, Thrown> {
    let given = req.argument_count();
    if !(1..=2).contains(&given) {
        return Err(arg_count_error(
            req,
            &format!(
                "{} {} given",
                if given < 1 {
                    "at least 1 argument,"
                } else {
                    "at most 2 arguments,"
                },
                given
            ),
        ));
    }
    let value = req.argument(1).unwrap_or_default();
    let mode = if given == 2 {
        long_argument(req, 2)?
    } else {
        COUNT_NORMAL
    };
    if mode != COUNT_NORMAL && mode != COUNT_RECURSIVE {
        return Err(arg_value_error(
            req,
            2,
            "be either COUNT_NORMAL or COUNT_RECURSIVE",
        ));
    }
    match value.deref().as_array() {
        Some(array) if mode == COUNT_RECURSIVE => Ok(Value::int(count_recursive(array))),
        _ => php_ext::coerce::count(req, &value).map(Value::int),
    }
}

/// `php_count_recursive`: the elements, and every nested array's
/// elements counted the same way, a reference followed to what it
/// holds. php protects the walk against an array that holds itself;
/// no array a native builds here does, and the recursion guard lands
/// with the memory model's arrays (#241).
fn count_recursive(array: &PhpArray) -> i64 {
    let own = i64::try_from(array.len()).unwrap_or(i64::MAX);
    array
        .iter()
        .filter_map(|(_, element)| element.deref().as_array())
        .fold(own, |count, nested| {
            count.saturating_add(count_recursive(nested))
        })
}

/// `ord(string $character): int`, php's `PHP_FUNCTION(ord)`
/// (`Z/ext/standard/string.c:2650`): the first byte of the string, with
/// the 8.5 deprecation for a string that is not one byte long, and the
/// terminator's zero for the empty string behind its own. A one-byte
/// string written as such never reaches here, since the compiler folds
/// it (`zend_compile_func_ord`); what does is any other argument, read
/// as `Z_PARAM_STR` reads it.
///
/// ```php
/// var_dump(ord("ab"), ord(""), ord(65), ord("a" . extension_loaded('Core')));
/// ```
/// ```expect
/// Deprecated: ord(): Providing a string that is not one byte long is deprecated. Use ord($str[0]) instead in %s on line 2
///
/// Deprecated: ord(): Providing an empty string is deprecated in %s on line 2
///
/// Deprecated: ord(): Providing a string that is not one byte long is deprecated. Use ord($str[0]) instead in %s on line 2
///
/// Deprecated: ord(): Providing a string that is not one byte long is deprecated. Use ord($str[0]) instead in %s on line 2
/// int(97)
/// int(0)
/// int(54)
/// int(97)
/// ```
fn ord(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, count));
    }
    let string = string_argument(req, 1)?;
    let bytes = string.as_bytes();
    if bytes.len() != 1 {
        let what = if bytes.is_empty() {
            "Providing an empty string is deprecated"
        } else {
            "Providing a string that is not one byte long is deprecated. Use ord($str[0]) instead"
        };
        docref_deprecated(req, what);
    }
    Ok(Value::int(i64::from(bytes.first().copied().unwrap_or(0))))
}

/// `chr(int $codepoint): string`, php's `PHP_FUNCTION(chr)`
/// (`Z/ext/standard/string.c:2673`): the one-byte string of the long's
/// low byte, with the 8.5 deprecation for a value outside 0 to 255. A
/// long in that range written as such never reaches here, since the
/// compiler folds it (`zend_compile_func_chr`); what does is any other
/// argument, read as `Z_PARAM_LONG` reads it. The byte of `chr(null)` is
/// read back through `ord`, since a NUL is no byte a fence pins.
///
/// ```php
/// var_dump(chr(256 + 65), chr(-191), chr(1 + 64), chr("65"), ord(chr(null)));
/// ```
/// ```expect
/// Deprecated: chr(): Providing a value not in-between 0 and 255 is deprecated, this is because a byte value must be in the [0, 255] interval. The value used will be constrained using % 256 in %s on line 2
///
/// Deprecated: chr(): Providing a value not in-between 0 and 255 is deprecated, this is because a byte value must be in the [0, 255] interval. The value used will be constrained using % 256 in %s on line 2
///
/// Deprecated: chr(): Passing null to parameter #1 ($codepoint) of type int is deprecated in %s on line 2
/// string(1) "A"
/// string(1) "A"
/// string(1) "A"
/// string(1) "A"
/// int(0)
/// ```
fn chr(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, count));
    }
    let codepoint = long_argument(req, 1)?;
    if !(0..=255).contains(&codepoint) {
        docref_deprecated(
            req,
            "Providing a value not in-between 0 and 255 is deprecated, this is because a byte value must be in the [0, 255] interval. The value used will be constrained using % 256",
        );
    }
    // `c &= 0xff`, then `RETURN_CHAR(c)`.
    let byte = (codepoint & 0xff).to_le_bytes()[0];
    Ok(Value::string(PhpString::new(req, &[byte])?))
}

/// `php_error_docref(NULL, E_DEPRECATED, ...)`.
fn docref_deprecated(req: &mut Request, what: &str) {
    docref(req, Level::Deprecated, what);
}

/// `is_null(mixed $value): bool`, `php_is_type` under `IS_NULL`
/// (`Z/ext/standard/type.c:239`) through the face's `is_type`. The
/// one-argument call is `ZEND_TYPE_CHECK` over the engine's kernel and
/// never reaches here, as with every native of the family; what does is
/// the call `ZEND_PARSE_PARAMETERS_START(1, 1)` refuses.
///
/// ```php
/// var_dump(is_null());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_null() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_null()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn is_null(req: &mut Request) -> Result<Value, Thrown> {
    type_check(req, TypeTag::Null)
}

/// `is_resource(mixed $value): bool`, `php_is_type` under `IS_RESOURCE`:
/// a closed resource is not one.
///
/// ```php
/// var_dump(is_resource());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_resource() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_resource()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn is_resource(req: &mut Request) -> Result<Value, Thrown> {
    type_check(req, TypeTag::Resource)
}

/// `is_bool(mixed $value): bool`, `PHP_FUNCTION(is_bool)`: `IS_FALSE`
/// or `IS_TRUE`.
///
/// ```php
/// var_dump(is_bool());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_bool() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_bool()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn is_bool(req: &mut Request) -> Result<Value, Thrown> {
    type_check(req, TypeTag::Bool)
}

/// `is_int(mixed $value): bool` and its aliases `is_integer` and
/// `is_long`, `php_is_type` under `IS_LONG`.
///
/// ```php
/// var_dump(is_int());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_int() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_int()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn is_int(req: &mut Request) -> Result<Value, Thrown> {
    type_check(req, TypeTag::Int)
}

/// `is_float(mixed $value): bool` and its alias `is_double`,
/// `php_is_type` under `IS_DOUBLE`.
///
/// ```php
/// var_dump(is_float());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_float() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_float()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn is_float(req: &mut Request) -> Result<Value, Thrown> {
    type_check(req, TypeTag::Float)
}

/// `is_string(mixed $value): bool`, `php_is_type` under `IS_STRING`.
///
/// ```php
/// var_dump(is_string());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_string() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_string()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn is_string(req: &mut Request) -> Result<Value, Thrown> {
    type_check(req, TypeTag::String)
}

/// `is_array(mixed $value): bool`, `php_is_type` under `IS_ARRAY`.
///
/// ```php
/// var_dump(is_array());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_array() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_array()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn is_array(req: &mut Request) -> Result<Value, Thrown> {
    type_check(req, TypeTag::Array)
}

/// `is_object(mixed $value): bool`, `php_is_type` under `IS_OBJECT`.
///
/// ```php
/// var_dump(is_object());
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_object() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): is_object()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn is_object(req: &mut Request) -> Result<Value, Thrown> {
    type_check(req, TypeTag::Object)
}

/// `is_scalar(mixed $value): bool`, `PHP_FUNCTION(is_scalar)`
/// (`Z/ext/standard/type.c:389`): a bool, a long, a double or a string,
/// which is the face's `is_type` asked four times.
///
/// ```php
/// var_dump(is_scalar(1, 2));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: is_scalar() expects exactly 1 argument, 2 given in %s:2
/// Stack trace:
/// #0 %s(2): is_scalar(1, 2)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn is_scalar(req: &mut Request) -> Result<Value, Thrown> {
    let value = one_argument(req)?;
    Ok(Value::bool(
        [TypeTag::Bool, TypeTag::Int, TypeTag::Float, TypeTag::String]
            .into_iter()
            .any(|tag| is_type(&value, tag)),
    ))
}

/// `php_is_type`: the one `Z_PARAM_ZVAL` and whether it is of `tag`.
fn type_check(req: &mut Request, tag: TypeTag) -> Result<Value, Thrown> {
    let value = one_argument(req)?;
    Ok(Value::bool(is_type(&value, tag)))
}

/// `ZEND_PARSE_PARAMETERS_START(1, 1)` over one `Z_PARAM_ZVAL`: the
/// argument as it is, or the `ArgumentCountError` for any other count.
fn one_argument(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 1 {
        return Err(wrong_argument_count(req, 1, count));
    }
    Ok(req.argument(1).unwrap_or_default())
}

/// `is_*(mixed $value): bool`, the declaration every native of the
/// family shares.
fn type_check_declared(name: &[u8]) -> FunctionDecl {
    declared(name, &[parameter(b"value", Type::Mixed)], Type::Bool)
}

/// One `REGISTER_LONG_CONSTANT`.
fn constant(name: &[u8], value: i64) -> ConstDecl {
    ConstDecl {
        name: Name(name.into()),
        value: ConstExpr::Long(value),
    }
}

/// `str_repeat(string $string, int $times): string`, php's
/// `PHP_FUNCTION(str_repeat)` (`Z/ext/standard/string.c:5489`): the
/// string laid end to end `$times` times, and the empty string for an
/// empty string or no times, before anything is allocated. The
/// arguments are read as `Z_PARAM_STR` and `Z_PARAM_LONG` read them, so
/// `12` and `true` are strings and `'2'` is a count.
///
/// ```php
/// var_dump(str_repeat("ab", 3), str_repeat("", 5), str_repeat("x", 0), str_repeat("x", 1), str_repeat(12, 2), str_repeat("x", "2"), str_repeat(true, 2));
/// ```
/// ```expect
/// string(6) "ababab"
/// string(0) ""
/// string(0) ""
/// string(1) "x"
/// string(4) "1212"
/// string(2) "xx"
/// string(2) "11"
/// ```
///
/// A count below zero is php's `ValueError`, `Argument #2 ($times) must
/// be greater than or equal to 0`, thrown with both arguments in hand,
/// which the trace prints as php prints them: the string in `'`, cut at
/// `zend.exception_string_param_max_len` with `...` after it, and the
/// count as its digits. A call outside the arity of two is its
/// `ArgumentCountError`; `the_count_below_zero_and_the_arity_are_php_s`
/// in `tests/capture.rs` holds that without the oracle.
///
/// ```php
/// str_repeat('a longer string than fifteen', -1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ValueError: str_repeat(): Argument #2 ($times) must be greater than or equal to 0 in %s:2
/// Stack trace:
/// #0 %s(2): str_repeat('a longer string...', -1)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// The return's bytes are `zend_string_safe_alloc(len, times, 0)`, and
/// so is what they cost: the product plus the aligned header, so
/// `str_repeat("ab", 3)` moves `memory_get_usage()` by the 40-byte bin
/// and not the 32 a six-byte string takes, as php's does. A product that
/// overflows is php's `Possible integer overflow in memory allocation
/// (len * times + 32)` fatal, raised before a byte is asked for, and one
/// the request's `memory_limit` refuses is the allocator's own fatal,
/// raised where the allocation is asked for and not by this native.
///
/// ```php
/// $b = memory_get_usage(); $s = str_repeat("ab", 3); var_dump(memory_get_usage() - $b);
/// ```
/// ```expect
/// int(40)
/// ```
///
/// ```php
/// $s = str_repeat("x", 200000000);
/// ```
/// ```expect exit=255
/// Fatal error: Allowed memory size of 134217728 bytes exhausted (tried to allocate 200000032 bytes) in %s on line 2
/// ```
fn str_repeat(req: &mut Request) -> Result<Value, Thrown> {
    let count = req.argument_count();
    if count != 2 {
        return Err(wrong_argument_count(req, 2, count));
    }
    let string = string_argument(req, 1)?;
    let times = long_argument(req, 2)?;
    if times < 0 {
        return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
    }
    if string.is_empty() || times == 0 {
        return Ok(Value::string(PhpString::new(req, b"")?));
    }
    let times = usize::try_from(times).unwrap_or(usize::MAX);
    let pattern = string.as_bytes();
    // `zend_string_safe_alloc(ZSTR_LEN(input_str), mult, 0, 0)`, then
    // php's two fills: one byte `memset`, else `memcpy` of the pattern
    // and then of the result onto itself, doubling what is laid down.
    let repeated = PhpString::product(req, pattern.len(), times, 0, |result| {
        if pattern.len() == 1 {
            result.fill(pattern[0]);
            return;
        }
        result[..pattern.len()].copy_from_slice(pattern);
        let mut laid = pattern.len();
        while laid < result.len() {
            let step = laid.min(result.len() - laid);
            let (done, rest) = result.split_at_mut(laid);
            rest[..step].copy_from_slice(&done[..step]);
            laid += step;
        }
    })?;
    Ok(Value::string(repeated))
}

/// `zend_wrong_parameters_count_error` for a native whose minimum is its
/// maximum: `exactly N arguments, M given`.
fn wrong_argument_count(req: &mut Request, exactly: usize, given: usize) -> Thrown {
    let word = if exactly == 1 {
        "argument"
    } else {
        "arguments"
    };
    arg_count_error(req, &format!("exactly {exactly} {word}, {given} given"))
}

/// One `Z_PARAM_STR` of a native, the argument at `position` read off
/// the call frame: `null` is the 8.1 deprecation and then the empty
/// string, and what is there is coerced as php coerces a string
/// parameter.
fn string_argument(req: &mut Request, position: u32) -> Result<PhpString, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    if matches!(given.deref().as_ref(), ValueRef::Null | ValueRef::Undef) {
        arg_null_deprecated(req, position, "string");
        return PhpString::new(req, b"");
    }
    let coerced = coerce_param(req, &given, &Type::String, position)?;
    match coerced.as_php_string() {
        Some(s) => Ok(*s),
        None => PhpString::new(req, b""),
    }
}

/// One `Z_PARAM_LONG` of a native, the argument at `position` read off
/// the call frame: `null` is the 8.1 deprecation and then zero, and what
/// is there is coerced as php coerces an int parameter.
fn long_argument(req: &mut Request, position: u32) -> Result<i64, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    if matches!(given.deref().as_ref(), ValueRef::Null | ValueRef::Undef) {
        arg_null_deprecated(req, position, "int");
        return Ok(0);
    }
    let coerced = coerce_param(req, &given, &Type::Int, position)?;
    Ok(coerced.as_int().unwrap_or(0))
}

/// One declared parameter, by value, with no default.
fn parameter(name: &[u8], ty: Type) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty: Some(ty),
        by_ref: false,
        variadic: false,
        default: None,
        attributes: Vec::new(),
    }
}

/// One native's declaration, as the attributes will derive it from the
/// Rust signature.
fn declared(name: &[u8], parameters: &[Parameter], returns: Type) -> FunctionDecl {
    FunctionDecl {
        name: Name(name.into()),
        parameters: parameters.to_vec(),
        return_type: Some(returns),
        returns_by_ref: false,
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
    }
}

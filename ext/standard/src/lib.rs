//! The `standard` extension: `php-src/ext/standard` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade; one entry, `extension()`; its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/standard/tests`, through `xtask phpt --subset`.
//!
//! One native so far, `str_repeat`, declared and paired with its entry as
//! `php_engine`'s own are, since the attributes under `php_ext::attr` hand
//! an item back unchanged until their expansion lands.
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
use php_ext::coerce::coerce_param;
use php_ext::decl::{FunctionDecl, Name, Parameter, Type};
use php_ext::raising::{Thrown, arg_count_error, arg_null_deprecated, arg_value_error, fatal};
use php_ext::request::Request;
use php_ext::values::{PhpString, Value, ValueRef};

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
    ExtensionBuilder::default()
        .functions(vec![(
            declared(
                b"str_repeat",
                &[
                    parameter(b"string", Type::String),
                    parameter(b"times", Type::Int),
                ],
                Type::String,
            ),
            str_repeat,
        )])
        .request_state::<State>()
        .build("standard")
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
/// be greater than or equal to 0`, and a call outside the arity of two
/// is its `ArgumentCountError`; `the_count_below_zero_and_the_arity_are_php_s`
/// in `tests/capture.rs` holds both, since the trace a witness would
/// print carries the call's arguments and this engine's does not yet
/// (#179).
///
/// The return's bytes are `zend_string_safe_alloc(len, times, 0)`: a
/// product that overflows is php's compile-time-shaped fatal, `Possible
/// integer overflow in memory allocation (len * times + 32)`, the 32
/// being the aligned header and NUL of an empty string. One the machine
/// refuses is php's `Out of memory`. What the request's `memory_limit`
/// refuses is the allocator's to say, and the string's bytes are on the
/// system allocator until the memory model backs a `Value` (design §11,
/// Lane A), so that fatal is not raised here yet; #174 names it.
fn str_repeat(req: &mut Request, arguments: &[Value]) -> Result<Value, Thrown> {
    if arguments.len() != 2 {
        return Err(wrong_argument_count(req, 2, arguments.len()));
    }
    let string = string_argument(req, &arguments[0], 1)?;
    let times = long_argument(req, &arguments[1], 2)?;
    if times < 0 {
        return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
    }
    if string.is_empty() || times == 0 {
        return Ok(Value::string(PhpString::default()));
    }
    let times = usize::try_from(times).unwrap_or(usize::MAX);
    let len = string.len();
    // `zend_safe_address_guarded(len, times, 32)`: the product and the
    // header checked before a byte is asked for.
    let Some(size) = len
        .checked_mul(times)
        .and_then(|bytes| bytes.checked_add(32))
    else {
        return Err(fatal(
            req,
            &format!("Possible integer overflow in memory allocation ({len} * {times} + 32)"),
        ));
    };
    let mut bytes = Vec::new();
    if bytes.try_reserve_exact(size - 32).is_err() {
        return Err(fatal(
            req,
            &format!(
                "Out of memory (allocated {}) (tried to allocate {size} bytes)",
                req.memory_usage(true)
            ),
        ));
    }
    for _ in 0..times {
        bytes.extend_from_slice(string.as_bytes());
    }
    Ok(Value::string(PhpString::from_bytes(&bytes)))
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

/// One `Z_PARAM_STR` of a native: `null` is the 8.1 deprecation and then
/// the empty string, and what is there is coerced as php coerces a
/// string parameter.
fn string_argument(req: &mut Request, given: &Value, position: u32) -> Result<PhpString, Thrown> {
    if matches!(given.deref().as_ref(), ValueRef::Null | ValueRef::Undef) {
        arg_null_deprecated(req, position, "string");
        return Ok(PhpString::default());
    }
    let coerced = coerce_param(req, given, &Type::String, position)?;
    Ok(coerced.as_php_string().cloned().unwrap_or_default())
}

/// One `Z_PARAM_LONG` of a native: `null` is the 8.1 deprecation and
/// then zero, and what is there is coerced as php coerces an int
/// parameter.
fn long_argument(req: &mut Request, given: &Value, position: u32) -> Result<i64, Thrown> {
    if matches!(given.deref().as_ref(), ValueRef::Null | ValueRef::Undef) {
        arg_null_deprecated(req, position, "int");
        return Ok(0);
    }
    let coerced = coerce_param(req, given, &Type::Int, position)?;
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

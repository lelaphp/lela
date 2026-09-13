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
use php_ext::raising::{Thrown, arg_count_error, arg_null_deprecated, arg_value_error};
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

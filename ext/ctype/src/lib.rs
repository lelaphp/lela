//! The `ctype` extension: `php-src/ext/ctype` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade; one entry, `extension()`; its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/ctype/tests`, through `xtask phpt --subset`.
//!
//! Eleven natives, `ctype_impl` in `ctype.c` at the tag over one of
//! `<ctype.h>`'s classes each: a string is true when every byte is in
//! the class and false when it is empty; anything else is the 8.1
//! deprecation and `ctype_fallback`, which reads an `int` as one byte
//! where it fits and answers by the class's `allow_digits` and
//! `allow_minus` otherwise. The classes are the oracle's, php's libc
//! under the recipe's `LC_ALL=C`, which on the pinned build classes
//! the bytes above 127 as Latin-1: [`class_of`] pins the table the
//! oracle answered for every byte, ranges and all.
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
use php_ext::decl::{FunctionDecl, Name, Parameter, Type};
use php_ext::raising::{Level, Thrown, docref, too_few_arguments};
use php_ext::request::Request;
use php_ext::values::{Value, ValueRef};

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or synchronised.
#[derive(Default)]
struct State {}

/// The extension value (design §7.1): declarations paired with entries,
/// and the state declared on it.
///
/// ```php
/// var_dump(extension_loaded('ctype'));
/// ```
/// ```expect
/// bool(true)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            (declared(b"ctype_alnum"), ctype_alnum as Entry),
            (declared(b"ctype_alpha"), ctype_alpha as Entry),
            (declared(b"ctype_cntrl"), ctype_cntrl as Entry),
            (declared(b"ctype_digit"), ctype_digit as Entry),
            (declared(b"ctype_lower"), ctype_lower as Entry),
            (declared(b"ctype_graph"), ctype_graph as Entry),
            (declared(b"ctype_print"), ctype_print as Entry),
            (declared(b"ctype_punct"), ctype_punct as Entry),
            (declared(b"ctype_space"), ctype_space as Entry),
            (declared(b"ctype_upper"), ctype_upper as Entry),
            (declared(b"ctype_xdigit"), ctype_xdigit as Entry),
        ])
        .request_state::<State>()
        .build("ctype")
}

/// A native's entry, the shape `ExtensionBuilder::functions` takes.
type Entry = fn(&mut Request) -> Result<Value, Thrown>;

/// `function ctype_<name>(mixed $text): bool`, as `ctype.stub.php`
/// declares each of the eleven.
fn declared(name: &[u8]) -> FunctionDecl {
    FunctionDecl {
        name: Name(name.into()),
        parameters: vec![Parameter {
            name: Name(b"text".as_slice().into()),
            ty: Some(Type::Mixed),
            by_ref: false,
            variadic: false,
            default: None,
            attributes: Vec::new(),
        }],
        return_type: Some(Type::Bool),
        returns_by_ref: false,
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        frameless: Vec::new(),
    }
}

/// One of `<ctype.h>`'s classes, as the oracle's libc answers for each
/// byte under the recipe: the ASCII class, and above 127 the Latin-1
/// bytes the pinned build's `C` locale puts in it. Each range is
/// inclusive.
#[derive(Clone, Copy)]
enum Class {
    Alnum,
    Alpha,
    Cntrl,
    Digit,
    Lower,
    Graph,
    Print,
    Punct,
    Space,
    Upper,
    Xdigit,
}

/// `iswhat(c)` for one byte: the oracle's table, read for every byte
/// from 0 to 255 under the recipe and pinned as ranges.
fn class_of(class: Class, byte: u8) -> bool {
    let ranges: &[(u8, u8)] = match class {
        Class::Alnum => &[
            (48, 57),
            (65, 90),
            (97, 122),
            (170, 170),
            (181, 181),
            (186, 186),
            (192, 214),
            (216, 246),
            (248, 255),
        ],
        Class::Alpha => &[
            (65, 90),
            (97, 122),
            (170, 170),
            (181, 181),
            (186, 186),
            (192, 214),
            (216, 246),
            (248, 255),
        ],
        Class::Cntrl => &[(0, 31), (127, 159), (173, 173)],
        Class::Digit => &[(48, 57)],
        Class::Lower => &[(97, 122), (181, 181), (223, 246), (248, 255)],
        Class::Graph => &[(33, 126), (161, 172), (174, 255)],
        Class::Print => &[(32, 126), (160, 172), (174, 255)],
        Class::Punct => &[
            (33, 47),
            (58, 64),
            (91, 96),
            (123, 126),
            (161, 161),
            (167, 167),
            (171, 171),
            (182, 183),
            (187, 187),
            (191, 191),
        ],
        Class::Space => &[(9, 13), (32, 32), (160, 160)],
        Class::Upper => &[(65, 90), (192, 214), (216, 222)],
        Class::Xdigit => &[(48, 57), (65, 70), (97, 102)],
    };
    ranges.iter().any(|&(lo, hi)| (lo..=hi).contains(&byte))
}

/// `ctype_impl(iswhat, allow_digits, allow_minus)`: `Z_PARAM_ZVAL` and
/// the string walk, then `ctype_fallback` for everything else, which
/// raises the deprecation naming the argument's type through
/// `php_error_docref` before it answers: an `int` from 0 to 255 is the
/// byte, from -128 to -1 the byte plus 256, above 255 is
/// `allow_digits`, below -128 is `allow_minus`; any other type is
/// false.
fn ctype(
    req: &mut Request,
    class: Class,
    allow_digits: bool,
    allow_minus: bool,
) -> Result<Value, Thrown> {
    let Some(given) = req.argument(1) else {
        return Err(too_few_arguments(req, 1, 0));
    };
    let given = given.deref();
    if let Some(bytes) = given.as_str_bytes() {
        return Ok(Value::bool(
            !bytes.is_empty() && bytes.iter().all(|&b| class_of(class, b)),
        ));
    }
    docref(
        req,
        Level::Deprecated,
        &format!(
            "Argument of type {} will be interpreted as string in the future",
            given.type_name()
        ),
    );
    Ok(Value::bool(match given.as_ref() {
        ValueRef::Int(n) => match u8::try_from(n).or_else(|_| u8::try_from(n + 256)) {
            Ok(byte) if (-128..=255).contains(&n) => class_of(class, byte),
            _ if n >= 0 => allow_digits,
            _ => allow_minus,
        },
        _ => false,
    }))
}

/// `ctype_alnum(mixed $text): bool`, `isalnum` with digits allowed
/// past the byte range.
///
/// ```php
/// var_dump(ctype_alnum('abc123'), ctype_alnum('ab-c'), ctype_alnum(''), ctype_alnum(48), ctype_alnum(300), ctype_alnum(-300));
/// ```
/// ```expect
/// Deprecated: ctype_alnum(): Argument of type int will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_alnum(): Argument of type int will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_alnum(): Argument of type int will be interpreted as string in the future in %s on line 2
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
fn ctype_alnum(req: &mut Request) -> Result<Value, Thrown> {
    ctype(req, Class::Alnum, true, false)
}

/// `ctype_alpha(mixed $text): bool`, `isalpha`. The fallback reads an
/// `int` in the byte range as that byte, `65` being `A`, and `-191`
/// as `65` again; past the range neither digits nor a minus are
/// letters. A float, `null` and a bool are the deprecation and false,
/// and so is an array, which the compiler cannot write `[]` for
/// yet. Above 127 the class is the oracle's Latin-1: `\xE9` is a
/// letter.
///
/// ```php
/// var_dump(ctype_alpha('abcXYZ'), ctype_alpha('Hello, World!'), ctype_alpha(''), ctype_alpha(65), ctype_alpha(-191), ctype_alpha(300), ctype_alpha(-300), ctype_alpha(1.5), ctype_alpha(null));
/// ```
/// ```expect
/// Deprecated: ctype_alpha(): Argument of type int will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_alpha(): Argument of type int will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_alpha(): Argument of type int will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_alpha(): Argument of type int will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_alpha(): Argument of type float will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_alpha(): Argument of type null will be interpreted as string in the future in %s on line 2
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
///
/// ```php
/// var_dump(ctype_alpha("\xE9"), ctype_print("\xA0"), ctype_alpha(true), ctype_digit(false));
/// ```
/// ```expect
/// Deprecated: ctype_alpha(): Argument of type bool will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_digit(): Argument of type bool will be interpreted as string in the future in %s on line 2
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
fn ctype_alpha(req: &mut Request) -> Result<Value, Thrown> {
    ctype(req, Class::Alpha, false, false)
}

/// `ctype_cntrl(mixed $text): bool`, `iscntrl`.
///
/// ```php
/// var_dump(ctype_cntrl("\t\n"), ctype_cntrl('a'), ctype_cntrl(9), ctype_cntrl(300));
/// ```
/// ```expect
/// Deprecated: ctype_cntrl(): Argument of type int will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_cntrl(): Argument of type int will be interpreted as string in the future in %s on line 2
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
fn ctype_cntrl(req: &mut Request) -> Result<Value, Thrown> {
    ctype(req, Class::Cntrl, false, false)
}

/// `ctype_digit(mixed $text): bool`, `isdigit` with digits allowed past
/// the byte range: `300` is true, `-300` false, and `-5` is the byte
/// `251`, which is no digit.
///
/// ```php
/// var_dump(ctype_digit('0123'), ctype_digit('12a'), ctype_digit(55), ctype_digit(300), ctype_digit(-300), ctype_digit(-5));
/// ```
/// ```expect
/// Deprecated: ctype_digit(): Argument of type int will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_digit(): Argument of type int will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_digit(): Argument of type int will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_digit(): Argument of type int will be interpreted as string in the future in %s on line 2
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
fn ctype_digit(req: &mut Request) -> Result<Value, Thrown> {
    ctype(req, Class::Digit, true, false)
}

/// `ctype_lower(mixed $text): bool`, `islower`.
///
/// ```php
/// var_dump(ctype_lower('abc'), ctype_lower('aBc'), ctype_lower(97));
/// ```
/// ```expect
/// Deprecated: ctype_lower(): Argument of type int will be interpreted as string in the future in %s on line 2
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
fn ctype_lower(req: &mut Request) -> Result<Value, Thrown> {
    ctype(req, Class::Lower, false, false)
}

/// `ctype_graph(mixed $text): bool`, `isgraph` with both digits and a
/// minus allowed past the byte range.
///
/// ```php
/// var_dump(ctype_graph('a!Z'), ctype_graph('a b'), ctype_graph(33), ctype_graph(300), ctype_graph(-300));
/// ```
/// ```expect
/// Deprecated: ctype_graph(): Argument of type int will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_graph(): Argument of type int will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_graph(): Argument of type int will be interpreted as string in the future in %s on line 2
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
fn ctype_graph(req: &mut Request) -> Result<Value, Thrown> {
    ctype(req, Class::Graph, true, true)
}

/// `ctype_print(mixed $text): bool`, `isprint` with both digits and a
/// minus allowed past the byte range.
///
/// ```php
/// var_dump(ctype_print('a b!'), ctype_print("a\tb"), ctype_print(32), ctype_print(300), ctype_print(-300));
/// ```
/// ```expect
/// Deprecated: ctype_print(): Argument of type int will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_print(): Argument of type int will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_print(): Argument of type int will be interpreted as string in the future in %s on line 2
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
fn ctype_print(req: &mut Request) -> Result<Value, Thrown> {
    ctype(req, Class::Print, true, true)
}

/// `ctype_punct(mixed $text): bool`, `ispunct`.
///
/// ```php
/// var_dump(ctype_punct('!?.'), ctype_punct('a!'), ctype_punct(33));
/// ```
/// ```expect
/// Deprecated: ctype_punct(): Argument of type int will be interpreted as string in the future in %s on line 2
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
fn ctype_punct(req: &mut Request) -> Result<Value, Thrown> {
    ctype(req, Class::Punct, false, false)
}

/// `ctype_space(mixed $text): bool`, `isspace`.
///
/// ```php
/// var_dump(ctype_space(" \t\n"), ctype_space('a b'), ctype_space(32));
/// ```
/// ```expect
/// Deprecated: ctype_space(): Argument of type int will be interpreted as string in the future in %s on line 2
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
fn ctype_space(req: &mut Request) -> Result<Value, Thrown> {
    ctype(req, Class::Space, false, false)
}

/// `ctype_upper(mixed $text): bool`, `isupper`.
///
/// ```php
/// var_dump(ctype_upper('ABC'), ctype_upper('AbC'), ctype_upper(65));
/// ```
/// ```expect
/// Deprecated: ctype_upper(): Argument of type int will be interpreted as string in the future in %s on line 2
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
fn ctype_upper(req: &mut Request) -> Result<Value, Thrown> {
    ctype(req, Class::Upper, false, false)
}

/// `ctype_xdigit(mixed $text): bool`, `isxdigit` with digits allowed
/// past the byte range.
///
/// ```php
/// var_dump(ctype_xdigit('0fA9'), ctype_xdigit('g'), ctype_xdigit(70), ctype_xdigit(300), ctype_xdigit(-300));
/// ```
/// ```expect
/// Deprecated: ctype_xdigit(): Argument of type int will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_xdigit(): Argument of type int will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_xdigit(): Argument of type int will be interpreted as string in the future in %s on line 2
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
fn ctype_xdigit(req: &mut Request) -> Result<Value, Thrown> {
    ctype(req, Class::Xdigit, true, false)
}

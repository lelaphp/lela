//! The `ctype` extension: `php-src/ext/ctype` as one crate under `ext/`
//! (ADR-0004). Two dependencies, the facade and `libc`; one entry, `extension()`; its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/ctype/tests`, through `xtask phpt --subset`.
//!
//! Eleven natives, each declared by `#[php_function]` from its Rust
//! signature, `mixed $text` in and `bool` out, and each `ctype_impl` in
//! `ctype.c` at the tag over one of `<ctype.h>`'s classes: a string is true when every byte is in
//! the class and false when it is empty; anything else is the 8.1
//! deprecation and `ctype_fallback`, which reads an `int` as one byte
//! where it fits and answers by the class's `allow_digits` and
//! `allow_minus` otherwise. The classes are the host C library's own
//! under the request's `LC_CTYPE` (design §5.14, revision 40), read in
//! `ffi`: ASCII alone under `C`, and under `C.UTF-8`, the locale php
//! starts a request in, and any other, what the host's tables say of
//! each byte above 0x7F.
#![deny(unsafe_code)]
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

mod ffi;

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::attr::php_function;
use php_ext::coerce::type_name;
use php_ext::raising::{Level, docref};
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
        .functions(&[
            ctype_alnum::DECLARED,
            ctype_alpha::DECLARED,
            ctype_cntrl::DECLARED,
            ctype_digit::DECLARED,
            ctype_lower::DECLARED,
            ctype_graph::DECLARED,
            ctype_print::DECLARED,
            ctype_punct::DECLARED,
            ctype_space::DECLARED,
            ctype_upper::DECLARED,
            ctype_xdigit::DECLARED,
        ])
        .request_state::<State>()
        .info(ctype_info)
        .build("ctype")
}

/// `PHP_MINFO_FUNCTION(ctype)`: the one-row table `ctype functions =>
/// enabled`, what `phpinfo()`, `php --ri ctype` and
/// `ReflectionExtension::info` print for the extension.
///
/// ```php
/// (new ReflectionExtension('ctype'))->info();
/// ```
/// ```expect
/// ctype
///
/// ctype functions => enabled
/// ```
fn ctype_info(req: &mut Request) {
    req.extensions_mut()
        .info_table(&[&["ctype functions", "enabled"]]);
}

/// One of `<ctype.h>`'s classes, which the host's C library answers
/// for each byte under the request's `LC_CTYPE`.
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

/// `ctype_impl(iswhat, allow_digits, allow_minus)` over the argument the
/// entry held (`Z_PARAM_ZVAL`, the arity the attribute's): the string walk, then `ctype_fallback` for everything else, which
/// raises the deprecation naming the argument's type as
/// `zend_zval_type_name` does, the class for an object, through
/// `php_error_docref` before it answers: an `int` from 0 to 255 is the
/// byte, from -128 to -1 the byte plus 256, above 255 is
/// `allow_digits`, below -128 is `allow_minus`; any other type is
/// false.
fn ctype(
    req: &mut Request,
    text: &Value,
    class: Class,
    allow_digits: bool,
    allow_minus: bool,
) -> bool {
    let given = text.deref();
    if let Some(bytes) = given.as_str_bytes() {
        return !bytes.is_empty() && bytes.iter().all(|&b| ffi::is(class, b));
    }
    docref(
        req,
        Level::Deprecated,
        &[
            b"Argument of type ".as_slice(),
            &type_name(req, &given),
            b" will be interpreted as string in the future",
        ]
        .concat(),
    );
    match given.as_ref() {
        ValueRef::Int(n) => match u8::try_from(n).or_else(|_| u8::try_from(n + 256)) {
            Ok(byte) if (-128..=255).contains(&n) => ffi::is(class, byte),
            _ if n >= 0 => allow_digits,
            _ => allow_minus,
        },
        _ => false,
    }
}

/// `ctype_alnum(mixed $text): bool`, `isalnum` with digits allowed
/// past the byte range.
///
/// ```php
/// setlocale(LC_CTYPE, "C"); var_dump(ctype_alnum('abc123'), ctype_alnum('ab-c'), ctype_alnum(''), ctype_alnum(48), ctype_alnum(300), ctype_alnum(-300), ctype_alnum("\xE9"));
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
/// bool(false)
/// ```
#[php_function]
fn ctype_alnum(req: &mut Request, text: Value) -> bool {
    ctype(req, &text, Class::Alnum, true, false)
}

/// `ctype_alpha(mixed $text): bool`, `isalpha`. The fallback reads an
/// `int` in the byte range as that byte, `65` being `A`, and `-191`
/// as `65` again; past the range neither digits nor a minus are
/// letters. A float, `null` and a bool are the deprecation and false,
/// and so is an array, which the compiler cannot write `[]` for
/// yet. An object is named by its class, a `__toString` read by
/// nothing, and is false. Under `LC_CTYPE=C` nothing above 0x7F is a
/// letter or printable: `\xE9` and `\xA0` are false.
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
/// class classA { public function __toString(): string { return 'abc'; } } setlocale(LC_CTYPE, "C"); var_dump(ctype_alpha("\xE9"), ctype_print("\xA0"), ctype_alpha(true), ctype_digit(false), ctype_alpha(233), ctype_alpha(new classA), ctype_alpha(new stdClass));
/// ```
/// ```expect
/// Deprecated: ctype_alpha(): Argument of type bool will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_digit(): Argument of type bool will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_alpha(): Argument of type int will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_alpha(): Argument of type classA will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_alpha(): Argument of type stdClass will be interpreted as string in the future in %s on line 2
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
///
/// Past 0x7F the class is the request's `LC_CTYPE`'s: `fr_FR.ISO8859-1`
/// reads `\xE9` as a letter, and `C` reads nothing there. Under the
/// `C.UTF-8` a request starts in, it is the C library's: macOS's classes
/// those bytes and glibc's none, which this crate's `utf8_locale` test
/// holds for macOS, so no fence pins it.
///
/// ```php
/// setlocale(LC_CTYPE, "fr_FR.ISO8859-1"); var_dump(ctype_alpha("\xE9"), ctype_lower("\xE9"), ctype_upper("\xC9"), ctype_digit("\xB2"));
/// setlocale(LC_CTYPE, "C"); var_dump(ctype_alpha("\xE9"), ctype_print("\xA0"));
/// ```
/// ```expect locale=fr_FR.ISO8859-1
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn ctype_alpha(req: &mut Request, text: Value) -> bool {
    ctype(req, &text, Class::Alpha, false, false)
}

/// `ctype_cntrl(mixed $text): bool`, `iscntrl`.
///
/// ```php
/// setlocale(LC_CTYPE, "C"); var_dump(ctype_cntrl("\t\n"), ctype_cntrl('a'), ctype_cntrl(9), ctype_cntrl(300), ctype_cntrl("\x85"));
/// ```
/// ```expect
/// Deprecated: ctype_cntrl(): Argument of type int will be interpreted as string in the future in %s on line 2
///
/// Deprecated: ctype_cntrl(): Argument of type int will be interpreted as string in the future in %s on line 2
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn ctype_cntrl(req: &mut Request, text: Value) -> bool {
    ctype(req, &text, Class::Cntrl, false, false)
}

/// `ctype_digit(mixed $text): bool`, `isdigit` with digits allowed past
/// the byte range: `300` is true, `-300` false, and `-5` is the byte
/// `251`, which is no digit.
///
/// ```php
/// setlocale(LC_CTYPE, "C"); var_dump(ctype_digit('0123'), ctype_digit('12a'), ctype_digit(55), ctype_digit(300), ctype_digit(-300), ctype_digit(-5), ctype_digit("\xB2"));
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
/// bool(false)
/// ```
#[php_function]
fn ctype_digit(req: &mut Request, text: Value) -> bool {
    ctype(req, &text, Class::Digit, true, false)
}

/// `ctype_lower(mixed $text): bool`, `islower`.
///
/// ```php
/// setlocale(LC_CTYPE, "C"); var_dump(ctype_lower('abc'), ctype_lower('aBc'), ctype_lower(97), ctype_lower("\xE9"));
/// ```
/// ```expect
/// Deprecated: ctype_lower(): Argument of type int will be interpreted as string in the future in %s on line 2
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn ctype_lower(req: &mut Request, text: Value) -> bool {
    ctype(req, &text, Class::Lower, false, false)
}

/// `ctype_graph(mixed $text): bool`, `isgraph` with both digits and a
/// minus allowed past the byte range.
///
/// ```php
/// setlocale(LC_CTYPE, "C"); var_dump(ctype_graph('a!Z'), ctype_graph('a b'), ctype_graph(33), ctype_graph(300), ctype_graph(-300), ctype_graph("\xE9"));
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
/// bool(false)
/// ```
#[php_function]
fn ctype_graph(req: &mut Request, text: Value) -> bool {
    ctype(req, &text, Class::Graph, true, true)
}

/// `ctype_print(mixed $text): bool`, `isprint` with both digits and a
/// minus allowed past the byte range.
///
/// ```php
/// setlocale(LC_CTYPE, "C"); var_dump(ctype_print('a b!'), ctype_print("a\tb"), ctype_print(32), ctype_print(300), ctype_print(-300), ctype_print("\xA0"));
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
/// bool(false)
/// ```
#[php_function]
fn ctype_print(req: &mut Request, text: Value) -> bool {
    ctype(req, &text, Class::Print, true, true)
}

/// `ctype_punct(mixed $text): bool`, `ispunct`.
///
/// ```php
/// setlocale(LC_CTYPE, "C"); var_dump(ctype_punct('!?.'), ctype_punct('a!'), ctype_punct(33), ctype_punct("\xA7"));
/// ```
/// ```expect
/// Deprecated: ctype_punct(): Argument of type int will be interpreted as string in the future in %s on line 2
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn ctype_punct(req: &mut Request, text: Value) -> bool {
    ctype(req, &text, Class::Punct, false, false)
}

/// `ctype_space(mixed $text): bool`, `isspace`.
///
/// ```php
/// setlocale(LC_CTYPE, "C"); var_dump(ctype_space(" \t\n"), ctype_space('a b'), ctype_space(32), ctype_space("\xA0"));
/// ```
/// ```expect
/// Deprecated: ctype_space(): Argument of type int will be interpreted as string in the future in %s on line 2
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn ctype_space(req: &mut Request, text: Value) -> bool {
    ctype(req, &text, Class::Space, false, false)
}

/// `ctype_upper(mixed $text): bool`, `isupper`.
///
/// ```php
/// setlocale(LC_CTYPE, "C"); var_dump(ctype_upper('ABC'), ctype_upper('AbC'), ctype_upper(65), ctype_upper("\xC9"));
/// ```
/// ```expect
/// Deprecated: ctype_upper(): Argument of type int will be interpreted as string in the future in %s on line 2
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn ctype_upper(req: &mut Request, text: Value) -> bool {
    ctype(req, &text, Class::Upper, false, false)
}

/// `ctype_xdigit(mixed $text): bool`, `isxdigit` with digits allowed
/// past the byte range.
///
/// ```php
/// setlocale(LC_CTYPE, "C"); var_dump(ctype_xdigit('0fA9'), ctype_xdigit('g'), ctype_xdigit(70), ctype_xdigit(300), ctype_xdigit(-300), ctype_xdigit("\xC1"));
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
/// bool(false)
/// ```
#[php_function]
fn ctype_xdigit(req: &mut Request, text: Value) -> bool {
    ctype(req, &text, Class::Xdigit, true, false)
}

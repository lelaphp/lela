//! `normalizer/`: the `Normalizer` class and the `normalizer_*`
//! functions over ICU's `unorm2` (`normalizer_normalize.cpp`), one body
//! each that the function and its static method alias share.

use php_ext::abi::{ExtensionBuilder, OrFalse};
use php_ext::attr::{php_class, php_function};
use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpString};

use crate::error;
use crate::ffi::normalizer::{Form, Normalizer as Icu};
use crate::ffi::{self, Code};
use crate::utf8;

/// `normalizer_*` in the stub's order.
pub(crate) fn functions(b: ExtensionBuilder) -> ExtensionBuilder {
    b.functions(vec![
        normalizer_normalize::declared(),
        normalizer_is_normalized::declared(),
        normalizer_get_raw_decomposition::declared(),
    ])
}

/// `class Normalizer`, its forms php's `NORMALIZER_*` and its methods
/// the functions' aliases.
///
/// ```php
/// var_dump(Normalizer::NFKC_CF, Normalizer::normalize("A\u{30A}", Normalizer::NFC) === "\u{C5}", Normalizer::isNormalized("A\u{30A}"), bin2hex(Normalizer::getRawDecomposition("\u{C5}")));
/// try { Normalizer::normalize('a', 3); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(48)
/// bool(true)
/// bool(false)
/// string(6) "41cc8a"
/// Normalizer::normalize(): Argument #2 ($form) must be a a valid normalization form
/// ```
#[php_class]
pub(crate) struct Normalizer;

#[php_class]
#[allow(non_snake_case)]
impl Normalizer {
    const FORM_D: i64 = 4;
    const NFD: i64 = 4;
    const FORM_KD: i64 = 8;
    const NFKD: i64 = 8;
    const FORM_C: i64 = 16;
    const NFC: i64 = 16;
    const FORM_KC: i64 = 32;
    const NFKC: i64 = 32;
    const FORM_KC_CF: i64 = 48;
    const NFKC_CF: i64 = 48;

    #[php(tentative, before_args = error::reset)]
    fn normalize(
        req: &mut Request,
        string: PhpString,
        #[php(default = Normalizer::FORM_C)] form: i64,
    ) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
        normalize(req, string, form)
    }

    #[php(tentative, before_args = error::reset)]
    fn isNormalized(
        req: &mut Request,
        string: PhpString,
        #[php(default = Normalizer::FORM_C)] form: i64,
    ) -> Result<bool, Thrown> {
        is_normalized(req, string, form)
    }

    #[php(tentative, before_args = error::reset)]
    fn getRawDecomposition(
        req: &mut Request,
        string: PhpString,
        #[php(default = Normalizer::FORM_C)] form: i64,
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        raw_decomposition(req, string, form)
    }
}

/// `NORMALIZER_FORM_*` to the instance's form, `None` for another long.
fn form_of(form: i64) -> Option<Form> {
    Some(match form {
        4 => Form::D,
        8 => Form::Kd,
        16 => Form::C,
        32 => Form::Kc,
        48 => Form::KcCasefold,
        _ => return None,
    })
}

/// The form, or php's `ValueError` for argument #2.
fn checked_form(req: &mut Request, form: i64) -> Result<Form, Thrown> {
    form_of(form).ok_or_else(|| arg_value_error(req, 2, "be a a valid normalization form"))
}

/// A failure set on the request with its code and message, answered
/// `false` (or thrown under `intl.use_exceptions`).
fn failed<T>(req: &mut Request, code: Code, message: &str) -> Result<OrFalse<T>, Thrown> {
    error::set(req, code, message)?;
    Ok(OrFalse::False)
}

/// `normalizer_normalize(string $string, int $form = Normalizer::FORM_C): string|false`.
///
/// ```php
/// var_dump(bin2hex(normalizer_normalize("\u{1E9B}\u{323}", Normalizer::FORM_KD)), normalizer_normalize(''), normalizer_normalize("\xff"), intl_get_error_message());
/// ```
/// ```expect
/// string(10) "73cca3cc87"
/// string(0) ""
/// bool(false)
/// string(85) "normalizer_normalize(): Error converting input string to UTF-16: U_INVALID_CHAR_FOUND"
/// ```
#[php_function(before_args = error::reset)]
fn normalizer_normalize(
    req: &mut Request,
    string: PhpString,
    #[php(default = Normalizer::FORM_C)] form: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    normalize(req, string, form)
}

fn normalize(
    req: &mut Request,
    string: PhpString,
    form: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let form = checked_form(req, form)?;
    let text = match ffi::to_utf16(string.as_bytes()) {
        Ok(text) => text,
        Err(code) => return failed(req, code, "Error converting input string to UTF-16"),
    };
    // php sets the message alone here, the code left as it was.
    let Ok(normalized) = Icu::of(form).and_then(|norm| norm.normalize(&text)) else {
        error::set_custom_msg(req, "Error normalizing string")?;
        return Ok(OrFalse::False);
    };
    match ffi::to_utf8(&normalized) {
        Ok(bytes) => Ok(OrFalse::Value(PhpString::new(req, &bytes)?)),
        Err(code) => failed(req, code, "error converting normalized text UTF-8"),
    }
}

/// `normalizer_is_normalized(string $string, int $form = Normalizer::FORM_C): bool`.
///
/// ```php
/// var_dump(normalizer_is_normalized("\u{C5}"), normalizer_is_normalized("A\u{30A}", Normalizer::NFD), normalizer_is_normalized("\u{C5}", Normalizer::NFD), normalizer_is_normalized("\xc3"), intl_get_error_code());
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// int(10)
/// ```
#[php_function(before_args = error::reset)]
fn normalizer_is_normalized(
    req: &mut Request,
    string: PhpString,
    #[php(default = Normalizer::FORM_C)] form: i64,
) -> Result<bool, Thrown> {
    is_normalized(req, string, form)
}

fn is_normalized(req: &mut Request, string: PhpString, form: i64) -> Result<bool, Thrown> {
    let form = checked_form(req, form)?;
    let text = match ffi::to_utf16(string.as_bytes()) {
        Ok(text) => text,
        Err(code) => {
            failed::<()>(req, code, "Error converting string to UTF-16.")?;
            return Ok(false);
        }
    };
    let tested = Icu::of(form).and_then(|norm| norm.is_normalized(&text));
    if tested.is_err() {
        error::set_custom_msg(
            req,
            "Error testing if string is the given normalization form.",
        )?;
    }
    Ok(tested.unwrap_or(false))
}

/// `normalizer_get_raw_decomposition(string $string, int $form = Normalizer::FORM_C): ?string`:
/// one code point's `Decomposition_Mapping`; a form php does not know
/// answers the empty string, as php's conversion of nothing does.
///
/// ```php
/// var_dump(bin2hex(normalizer_get_raw_decomposition("\u{2126}")), normalizer_get_raw_decomposition("a"), normalizer_get_raw_decomposition("ab"), intl_get_error_message(), normalizer_get_raw_decomposition("\xff"), intl_get_error_message(), normalizer_get_raw_decomposition("a", 3));
/// ```
/// ```expect
/// string(4) "cea9"
/// NULL
/// NULL
/// string(125) "normalizer_get_raw_decomposition(): Input string must be exactly one UTF-8 encoded code point long.: U_ILLEGAL_ARGUMENT_ERROR"
/// NULL
/// string(85) "normalizer_get_raw_decomposition(): Code point out of range: U_ILLEGAL_ARGUMENT_ERROR"
/// string(0) ""
/// ```
#[php_function(before_args = error::reset)]
fn normalizer_get_raw_decomposition(
    req: &mut Request,
    string: PhpString,
    #[php(default = Normalizer::FORM_C)] form: i64,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    raw_decomposition(req, string, form)
}

fn raw_decomposition(
    req: &mut Request,
    string: PhpString,
    form: i64,
) -> Result<Option<Owner<PhpString>>, Thrown> {
    let bytes = string.as_bytes();
    let mut offset = 0;
    let c = utf8::next(bytes, &mut offset);
    if offset != bytes.len() {
        let message = "Input string must be exactly one UTF-8 encoded code point long.";
        error::set(req, Code::ILLEGAL_ARGUMENT, message)?;
        return Ok(None);
    }
    if !(0..=0x10_ffff).contains(&c) {
        error::set(req, Code::ILLEGAL_ARGUMENT, "Code point out of range")?;
        return Ok(None);
    }
    let Some(norm) = form_of(form).and_then(|form| Icu::of(form).ok()) else {
        return PhpString::new(req, b"").map(Some);
    };
    let Some(decomposition) = norm.raw_decomposition(c) else {
        return Ok(None);
    };
    let bytes = ffi::to_utf8(&decomposition).unwrap_or_default();
    PhpString::new(req, &bytes).map(Some)
}

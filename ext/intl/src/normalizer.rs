//! `normalizer/`: the `Normalizer` class and the `normalizer_*`
//! functions over ICU's `unorm2` (`normalizer_normalize.cpp`), one body
//! each that the function and its static method alias share.

use php_ext::abi::{ExtensionBuilder, OrFalse};
use php_ext::attr::{php_class, php_function};
use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpString};

use crate::error;
use crate::ffi::heap::Block;
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
///
/// Since 8.5.12 the destination is asked for fallibly, so a text whose
/// UTF-16 copy met the memory limit is refused its destination and the
/// limit's fatal is raised as the call returns; `Normalizer::normalize`
/// is the same body:
///
/// ```php
/// ini_set('memory_limit', '8M');
/// var_dump(normalizer_normalize(str_repeat("A", 2 << 20), Normalizer::FORM_D), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 4210688 bytes) in %s on line 3
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
    let expansion = match form {
        Form::D | Form::Kd => 3,
        Form::C | Form::Kc | Form::KcCasefold => 1,
    };
    let (input, len) = match ffi::to_utf16_block(req, string.as_bytes())? {
        Ok(input) => input,
        Err(code) => return failed(req, code, "Error converting input string to UTF-16"),
    };
    let answered = normalized(req, form, &input.as_slice()[..len], len * expansion);
    input.give_back(req);
    let (out, len) = match answered? {
        Normalized::Text(out, len) => (out, len),
        Normalized::Failed => {
            // php sets the message alone here, the code left as it was.
            error::set_custom_msg(req, "Error normalizing string")?;
            return Ok(OrFalse::False);
        }
        // RETURN_OOM()
        Normalized::Refused => return Ok(OrFalse::False),
    };
    // `intl_convert_utf16_to_utf8`'s string is made while the block is
    // held, as php frees `uret_buf` after it.
    let answered = match ffi::to_utf8(&out.as_slice()[..len]) {
        Ok(bytes) => Ok(PhpString::new(req, &bytes).map(OrFalse::Value)),
        Err(code) => Err(code),
    };
    out.give_back(req);
    match answered {
        Ok(made) => made,
        Err(code) => failed(req, code, "error converting normalized text UTF-8"),
    }
}

/// What `intl_normalize` into php's destination came to.
enum Normalized {
    /// The destination and the length of its text.
    Text(Block<u16>, usize),
    /// ICU failed.
    Failed,
    /// The memory limit refused a destination (`RETURN_OOM()`).
    Refused,
}

/// `intl_normalize` into a block of `room + 1` units asked for fallibly
/// (`safe_femalloc` since 8.5.12), and once more, of the length it
/// needed, where that was more.
fn normalized(
    req: &mut Request,
    form: Form,
    input: &[u16],
    room: usize,
) -> Result<Normalized, Thrown> {
    let Some(mut out) = Block::<u16>::ask(req, room + 1, true)? else {
        return Ok(Normalized::Refused);
    };
    let Ok(norm) = Icu::of(form) else {
        out.give_back(req);
        return Ok(Normalized::Failed);
    };
    let (needed, status) = norm.normalize_into(input, out.as_mut_slice());
    if status.is_failure() && status != Code::BUFFER_OVERFLOW {
        out.give_back(req);
        return Ok(Normalized::Failed);
    }
    let needed = usize::try_from(needed).unwrap_or(0);
    if needed > room {
        out.give_back(req);
        let Some(mut grown) = Block::<u16>::ask(req, needed + 1, true)? else {
            return Ok(Normalized::Refused);
        };
        let (_, status) = norm.normalize_into(input, grown.as_mut_slice());
        if status.is_failure() {
            grown.give_back(req);
            return Ok(Normalized::Failed);
        }
        out = grown;
    }
    Ok(Normalized::Text(out, needed))
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

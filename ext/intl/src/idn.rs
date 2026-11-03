//! `idn/idn.cpp`: `idn_to_ascii` and `idn_to_utf8` over ICU's UTS #46
//! processor, with `$idna_info` filled as php fills it.

use php_ext::abi::{ExtensionBuilder, OrFalse, Ref, Untyped};
use php_ext::attr::php_function;
use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, Value};

use crate::error;
use crate::ffi::Code;
use crate::ffi::idn::Idna;

/// `idn_*` in the stub's order.
pub(crate) fn functions(b: ExtensionBuilder) -> ExtensionBuilder {
    b.functions(&[idn_to_ascii::DECLARED, idn_to_utf8::DECLARED])
}

/// `idn_to_ascii(string $domain, int $flags = IDNA_DEFAULT, int $variant = INTL_IDNA_VARIANT_UTS46, &$idna_info = null): string|false`.
///
/// ```php
/// var_dump(idn_to_ascii("bücher.example"), idn_to_ascii("a-.b", IDNA_USE_STD3_RULES, INTL_IDNA_VARIANT_UTS46, $info), $info);
/// try { idn_to_ascii(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { idn_to_ascii('a', 0, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(21) "xn--bcher-kva.example"
/// bool(false)
/// array(3) {
///   ["result"]=>
///   string(4) "a-.b"
///   ["isTransitionalDifferent"]=>
///   bool(false)
///   ["errors"]=>
///   int(16)
/// }
/// idn_to_ascii(): Argument #1 ($domain) must not be empty
/// idn_to_ascii(): Argument #2 ($flags) must be INTL_IDNA_VARIANT_UTS46
/// ```
#[php_function(before_args = error::reset)]
fn idn_to_ascii(
    req: &mut Request,
    domain: PhpString,
    #[php(default = IDNA_DEFAULT)] flags: i64,
    #[php(default = INTL_IDNA_VARIANT_UTS46)] variant: i64,
    #[php(default = null)] idna_info: Ref<Untyped<Value>>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    convert(req, domain, flags, variant, &idna_info, true)
}

/// `idn_to_utf8(string $domain, int $flags = IDNA_DEFAULT, int $variant = INTL_IDNA_VARIANT_UTS46, &$idna_info = null): string|false`.
///
/// ```php
/// var_dump(idn_to_utf8("xn--bcher-kva.example"), idn_to_utf8("xn--a.b", 0, INTL_IDNA_VARIANT_UTS46, $info), $info["errors"] === IDNA_ERROR_INVALID_ACE_LABEL);
/// ```
/// ```expect
/// string(15) "bücher.example"
/// bool(false)
/// bool(true)
/// ```
#[php_function(before_args = error::reset)]
fn idn_to_utf8(
    req: &mut Request,
    domain: PhpString,
    #[php(default = IDNA_DEFAULT)] flags: i64,
    #[php(default = INTL_IDNA_VARIANT_UTS46)] variant: i64,
    #[php(default = null)] idna_info: Ref<Untyped<Value>>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    convert(req, domain, flags, variant, &idna_info, false)
}

/// `php_intl_idn_handoff` and `php_intl_idn_to_46`.
fn convert(
    req: &mut Request,
    domain: PhpString,
    flags: i64,
    variant: i64,
    idna_info: &Ref<Untyped<Value>>,
    to_ascii: bool,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let name = domain.as_bytes();
    if name.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    if name.len() > i32::MAX as usize - 1 {
        return Err(arg_value_error(req, 1, "be less than 2147483647 bytes"));
    }
    if variant != 1 {
        return Err(arg_value_error(req, 2, "be INTL_IDNA_VARIANT_UTS46"));
    }
    let wants = idna_info.is_given();
    if wants {
        let empty = PhpArray::new(req)?;
        idna_info.set(req, Untyped(empty.into_value()))?;
    }
    // `(uint32_t)option`, C's truncation.
    let options = u32::from_le_bytes(flags.to_le_bytes()[..4].try_into().unwrap_or_default());
    let idna = match Idna::open(options) {
        Ok(idna) => idna,
        Err(code) => return failed(req, code, "failed to open UIDNA instance"),
    };
    let capacity = if to_ascii { 255 } else { 252 * 4 };
    let converted = idna.convert(name, to_ascii, capacity);
    if converted.len >= capacity {
        return Ok(OrFalse::False);
    }
    if converted.status.is_failure() {
        return failed(req, converted.status, "failed to convert name");
    }
    error::set_code(req, converted.status);
    let text = PhpString::new(req, &converted.bytes)?;
    if wants {
        let mut info = PhpArray::new(req)?;
        let pairs = [
            (&b"result"[..], Value::string(*text)),
            (
                b"isTransitionalDifferent",
                Value::bool(converted.transitional_different),
            ),
            (b"errors", Value::int(i64::from(converted.errors))),
        ];
        for (key, value) in pairs {
            info.set_str(req, key, Owner::take(req, value))?;
        }
        if let Err(thrown) = idna_info.set(req, Untyped(info.into_value())) {
            text.give_up(req);
            return Err(thrown);
        }
    }
    if converted.errors != 0 {
        text.give_up(req);
        return Ok(OrFalse::False);
    }
    Ok(OrFalse::Value(text))
}

/// `php_intl_idn_check_status` on a failure: the code and the message
/// set, and `false`.
fn failed(
    req: &mut Request,
    code: Code,
    message: &str,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    error::set(req, code, message)?;
    Ok(OrFalse::False)
}

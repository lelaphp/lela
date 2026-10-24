//! The `bcmath` extension: `php-src/ext/bcmath` as one crate under `ext/`
//! (ADR-0004): php's own `bcmath.c` and `libbcmath` written again in
//! Rust, with no library (design §7.2). One dependency, the facade; one entry,
//! `extension()`; no state (ADR-0005): `bcmath.scale` is the ini entry,
//! read where php reads `BCG(bc_precision)`. The phpt subset is
//! `php-src/ext/bcmath/tests`, through `xtask phpt --subset`.
//!
//! [`num`] is `libbcmath`, the one arithmetic core; this file is the
//! `bc*` natives of `bcmath.c` over it, and [`number`] is 8.4's
//! `BcMath\Number`, whose methods and operators reach the same core
//! through the same argument decoder, [`operand`].
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

mod num;
mod number;

use php_ext::abi::{Extension, ExtensionBuilder, Instance};
use php_ext::attr::{php_class, php_function};
use php_ext::call::read_prop;
use php_ext::decl::{IniDecl, Name};
use php_ext::raising::{Thrown, argument_error, throw};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, Value};

use num::{Num, RaiseError, RaiseModError, Rounding};

/// `bcmath.scale`, `PHP_INI_ALL`, php's `BCG(bc_precision)`.
const SCALE: &[u8] = b"bcmath.scale";

/// The largest scale php takes, `INT_MAX`.
const INT_MAX: usize = i32::MAX as usize;

/// The extension value (design §7.1): `bcmath_module_entry`, its
/// fourteen functions, `BcMath\Number`, and `bcmath.scale` with
/// `OnUpdateScale`.
///
/// ```php
/// var_dump(extension_loaded('bcmath'), ini_get('bcmath.scale'), ini_set('bcmath.scale', '-1'), ini_set('bcmath.scale', '3'), bcadd('1', '2'));
/// ```
/// ```expect
/// bool(true)
/// string(1) "0"
/// bool(false)
/// string(1) "0"
/// string(5) "3.000"
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![
            bcadd::declared(),
            bcsub::declared(),
            bcmul::declared(),
            bcdiv::declared(),
            bcmod::declared(),
            bcdivmod::declared(),
            bcpowmod::declared(),
            bcpow::declared(),
            bcsqrt::declared(),
            bccomp::declared(),
            bcscale::declared(),
            bcfloor::declared(),
            bcceil::declared(),
            bcround::declared(),
        ])
        .class::<number::Number>()
        .request_state::<number::Shared>()
        .ini(vec![IniDecl {
            name: Name(SCALE.into()),
            default: Some(b"0".as_slice().into()),
            user: true,
            per_dir: true,
            system: true,
        }])
        .on_modify(SCALE, on_update_scale)
        .info(bcmath_info)
        .build("bcmath")
}

/// `PHP_MINFO_FUNCTION(bcmath)`: the support row, then the ini entry.
///
/// ```php
/// (new ReflectionExtension('bcmath'))->info();
/// ```
/// ```expect
///
/// bcmath
///
/// BCMath support => enabled
///
/// Directive => Local Value => Master Value
/// bcmath.scale => 0 => 0
/// ```
fn bcmath_info(req: &mut Request) {
    req.extensions_mut()
        .info_table(&[&["BCMath support", "enabled"]]);
    req.ini_mut().info_ini_entries(&[SCALE]);
}

/// `OnUpdateScale`: the value read as `zend_ini_parse_quantity_warn`
/// reads it, refused outside `0..=INT_MAX`.
#[expect(
    clippy::unnecessary_wraps,
    reason = "the `on_modify` slot's shape, which a handler that throws needs"
)]
fn on_update_scale(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let scale = req
        .ini_mut()
        .parse_quantity_warn(SCALE, value.unwrap_or_default());
    Ok((0..=i64::from(i32::MAX)).contains(&scale))
}

/// `BCG(bc_precision)`: the scale `bcmath.scale` holds, which
/// [`on_update_scale`] accepted.
fn default_scale(req: &Request) -> usize {
    usize::try_from(req.ini().long(SCALE))
        .unwrap_or(0)
        .min(INT_MAX)
}

/// `bcmath_check_scale`: a scale in `0..=INT_MAX`, or php's
/// `ValueError` naming the argument.
fn check_scale(req: &mut Request, scale: i64, position: u32) -> Result<usize, Thrown> {
    match usize::try_from(scale) {
        Ok(scale) if scale <= INT_MAX => Ok(scale),
        _ => Err(value_error(
            req,
            position,
            &format!("must be between 0 and {INT_MAX}"),
        )),
    }
}

/// The scale a `?int $scale = null` argument names: the argument held
/// by [`check_scale`], or `bcmath.scale` where it is `null`.
fn scale_or_default(req: &mut Request, scale: Option<i64>, position: u32) -> Result<usize, Thrown> {
    match scale {
        Some(scale) => check_scale(req, scale, position),
        None => Ok(default_scale(req)),
    }
}

/// A number as php's bcmath holds an argument: the digits, and the
/// scale it was written with (`full_scale`), trailing zeros counted,
/// which is the scale `BcMath\Number` reports for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Decimal {
    pub(crate) num: Num,
    pub(crate) scale: usize,
}

impl Decimal {
    /// `bc_long2num`: an int's number, scale zero.
    fn from_long(value: i64) -> Decimal {
        Decimal {
            num: Num::from_long(value),
            scale: 0,
        }
    }

    /// `php_str2num_ex`: `text` read with its own scale, `None` for a
    /// string that is no number.
    fn parse(text: &[u8]) -> Option<Decimal> {
        let (num, scale) = Num::parse(text, 0, true)?;
        Some(Decimal { num, scale })
    }

    /// The number at its own scale, `bcmath_number_value_to_str`.
    fn render(&self) -> Vec<u8> {
        self.num.render(self.scale)
    }
}

/// The one argument decoder, `php_str2num` and its refusal: `text` as
/// a number, or php's `ValueError` that the argument at `position` is
/// not well-formed.
fn operand(req: &mut Request, text: &[u8], position: u32) -> Result<Decimal, Thrown> {
    match Decimal::parse(text) {
        Some(decimal) => Ok(decimal),
        None => Err(value_error(req, position, "is not well-formed")),
    }
}

/// `zend_argument_value_error`: php's `ValueError` for the argument at
/// `position`, `tail` the sentence after its name as php words it.
fn value_error(req: &mut Request, position: u32, tail: &str) -> Thrown {
    argument_error(req, "ValueError", position, tail)
}

/// A string of the request's holding `bytes`.
fn string(req: &mut Request, bytes: &[u8]) -> Result<Owner<PhpString>, Thrown> {
    PhpString::new(req, bytes)
}

/// `bcadd(string $num1, string $num2, ?int $scale = null): string`.
///
/// ```php
/// var_dump(bcadd('1.234', '5', 4), bcadd('-1.5', '1.25', 1), bcadd('0.001', '-0.001', 2), bcadd("1\0x", '1'));
/// try { bcadd('1e5', '1'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { bcadd('1', ' 1'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(6) "6.2340"
/// string(4) "-0.2"
/// string(4) "0.00"
/// string(1) "2"
/// bcadd(): Argument #1 ($num1) is not well-formed
/// bcadd(): Argument #2 ($num2) is not well-formed
/// ```
#[php_function]
fn bcadd(
    req: &mut Request,
    num1: PhpString,
    num2: PhpString,
    #[php(default = null)] scale: Option<i64>,
) -> Result<Owner<PhpString>, Thrown> {
    let scale = scale_or_default(req, scale, 3)?;
    let first = operand(req, num1.as_bytes(), 1)?;
    let second = operand(req, num2.as_bytes(), 2)?;
    let sum = num::add(&first.num, &second.num, scale);
    string(req, &sum.render(scale))
}

/// `bcsub(string $num1, string $num2, ?int $scale = null): string`.
///
/// ```php
/// var_dump(bcsub('1', '2'), bcsub('1.5', '1.25', 3), bcsub('-0.0001', '0', 2), bcsub('1', '1.009', 2));
/// ```
/// ```expect
/// string(2) "-1"
/// string(5) "0.250"
/// string(4) "0.00"
/// string(4) "0.00"
/// ```
#[php_function]
fn bcsub(
    req: &mut Request,
    num1: PhpString,
    num2: PhpString,
    #[php(default = null)] scale: Option<i64>,
) -> Result<Owner<PhpString>, Thrown> {
    let scale = scale_or_default(req, scale, 3)?;
    let first = operand(req, num1.as_bytes(), 1)?;
    let second = operand(req, num2.as_bytes(), 2)?;
    let diff = num::sub(&first.num, &second.num, scale);
    string(req, &diff.render(scale))
}

/// `bcmul(string $num1, string $num2, ?int $scale = null): string`.
///
/// ```php
/// var_dump(bcmul('2', '3'), bcmul('1.25', '-0.5', 2), bcmul('-0.1', '0.1', 1), bcmul('123456789123456789', '987654321987654321', 0));
/// ```
/// ```expect
/// string(1) "6"
/// string(5) "-0.62"
/// string(3) "0.0"
/// string(36) "121932631356500531347203169112635269"
/// ```
#[php_function]
fn bcmul(
    req: &mut Request,
    num1: PhpString,
    num2: PhpString,
    #[php(default = null)] scale: Option<i64>,
) -> Result<Owner<PhpString>, Thrown> {
    let scale = scale_or_default(req, scale, 3)?;
    let first = operand(req, num1.as_bytes(), 1)?;
    let second = operand(req, num2.as_bytes(), 2)?;
    let product = num::multiply(&first.num, &second.num, scale);
    string(req, &product.render(scale))
}

/// `bcdiv(string $num1, string $num2, ?int $scale = null): string`.
///
/// ```php
/// var_dump(bcdiv('1', '3', 5), bcdiv('-7', '2'), bcdiv('12.3', '0.01', 1), bcdiv('123456789012345678901234567890', '98765432109876543210', 10));
/// try { bcdiv('1', '0.00'); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(7) "0.33333"
/// string(2) "-3"
/// string(6) "1230.0"
/// string(21) "1249999988.6093750001"
/// Division by zero
/// ```
#[php_function]
fn bcdiv(
    req: &mut Request,
    num1: PhpString,
    num2: PhpString,
    #[php(default = null)] scale: Option<i64>,
) -> Result<Owner<PhpString>, Thrown> {
    let scale = scale_or_default(req, scale, 3)?;
    let first = operand(req, num1.as_bytes(), 1)?;
    let second = operand(req, num2.as_bytes(), 2)?;
    match num::divide(&first.num, &second.num, scale) {
        Some(quot) => string(req, &quot.render(scale)),
        None => Err(division_by_zero(req)),
    }
}

/// `bcmod(string $num1, string $num2, ?int $scale = null): string`.
///
/// ```php
/// var_dump(bcmod('10', '3'), bcmod('-10', '3'), bcmod('5.7', '1.3', 2), bcmod('10', '-3.5', 1));
/// try { bcmod('1', '0'); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "1"
/// string(2) "-1"
/// string(4) "0.50"
/// string(3) "3.0"
/// Modulo by zero
/// ```
#[php_function]
fn bcmod(
    req: &mut Request,
    num1: PhpString,
    num2: PhpString,
    #[php(default = null)] scale: Option<i64>,
) -> Result<Owner<PhpString>, Thrown> {
    let scale = scale_or_default(req, scale, 3)?;
    let first = operand(req, num1.as_bytes(), 1)?;
    let second = operand(req, num2.as_bytes(), 2)?;
    match num::modulo(&first.num, &second.num, scale) {
        Some(rem) => string(req, &rem.render(scale)),
        None => Err(modulo_by_zero(req)),
    }
}

/// `bcdivmod(string $num1, string $num2, ?int $scale = null): array`:
/// the quotient truncated to an integer and the remainder at `scale`.
///
/// ```php
/// var_dump(bcdivmod('10', '3'), bcdivmod('-7.5', '2', 2));
/// try { bcdivmod('1', '0'); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(1) "3"
///   [1]=>
///   string(1) "1"
/// }
/// array(2) {
///   [0]=>
///   string(2) "-3"
///   [1]=>
///   string(5) "-1.50"
/// }
/// Division by zero
/// ```
#[php_function]
fn bcdivmod(
    req: &mut Request,
    num1: PhpString,
    num2: PhpString,
    #[php(default = null)] scale: Option<i64>,
) -> Result<Owner<PhpArray>, Thrown> {
    let scale = scale_or_default(req, scale, 3)?;
    let first = operand(req, num1.as_bytes(), 1)?;
    let second = operand(req, num2.as_bytes(), 2)?;
    let Some((quot, rem)) = num::divmod(&first.num, &second.num, scale) else {
        return Err(division_by_zero(req));
    };
    let quot = string(req, &quot.render(0))?;
    let rem = string(req, &rem.render(scale))?;
    pair(req, quot.into_value(), rem.into_value())
}

/// `zend_new_pair`: a packed array of the two.
fn pair(
    req: &mut Request,
    first: Owner<Value>,
    second: Owner<Value>,
) -> Result<Owner<PhpArray>, Thrown> {
    let mut array = PhpArray::new(req)?;
    array.set(req, Owner::index(0), first)?;
    array.set(req, Owner::index(1), second)?;
    Ok(array)
}

/// `bcpowmod(string $num, string $exponent, string $modulus, ?int $scale
/// = null): string`.
///
/// ```php
/// var_dump(bcpowmod('4', '13', '497'), bcpowmod('-7', '3', '5'), bcpowmod('5', '0', '1', 2), bcpowmod('2', '10', '1000', 1));
/// foreach ([['1.5', '1', '2'], ['2', '1.5', '2'], ['2', '-1', '2'], ['2', '1', '2.5']] as $a) {
///     try { bcpowmod(...$a); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// try { bcpowmod('2', '1', '0'); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "445"
/// string(2) "-3"
/// string(4) "0.00"
/// string(4) "24.0"
/// bcpowmod(): Argument #1 ($num) cannot have a fractional part
/// bcpowmod(): Argument #2 ($exponent) cannot have a fractional part
/// bcpowmod(): Argument #2 ($exponent) must be greater than or equal to 0
/// bcpowmod(): Argument #3 ($modulus) cannot have a fractional part
/// Modulo by zero
/// ```
#[php_function]
fn bcpowmod(
    req: &mut Request,
    num: PhpString,
    exponent: PhpString,
    modulus: PhpString,
    #[php(default = null)] scale: Option<i64>,
) -> Result<Owner<PhpString>, Thrown> {
    let scale = scale_or_default(req, scale, 4)?;
    let base = operand(req, num.as_bytes(), 1)?;
    let exponent = operand(req, exponent.as_bytes(), 2)?;
    let modulus = operand(req, modulus.as_bytes(), 3)?;
    match num::raisemod(&base.num, &exponent.num, &modulus.num, scale) {
        Ok(result) => string(req, &result.render(scale)),
        Err(error) => Err(raisemod_error(req, error, false)),
    }
}

/// `bc_raisemod`'s refusals as `bcpowmod()` and `powmod()` word them:
/// the base an argument of the function and the receiver of the
/// method, which is `method`.
fn raisemod_error(req: &mut Request, error: RaiseModError, method: bool) -> Thrown {
    let shift = u32::from(method);
    match error {
        RaiseModError::BaseHasFractional if method => {
            php_ext::raising::value_error(req, "Base number cannot have a fractional part")
        }
        RaiseModError::BaseHasFractional => value_error(req, 1, "cannot have a fractional part"),
        RaiseModError::ExpoHasFractional => {
            value_error(req, 2 - shift, "cannot have a fractional part")
        }
        RaiseModError::ExpoIsNegative => {
            value_error(req, 2 - shift, "must be greater than or equal to 0")
        }
        RaiseModError::ModHasFractional => {
            value_error(req, 3 - shift, "cannot have a fractional part")
        }
        RaiseModError::ModIsZero => modulo_by_zero(req),
    }
}

/// `bcpow(string $num, string $exponent, ?int $scale = null): string`.
///
/// ```php
/// var_dump(bcpow('2', '10'), bcpow('1.5', '3', 2), bcpow('2', '-2', 4), bcpow('-3', '3'), bcpow('0', '0'), bcpow('5', '-1'));
/// try { bcpow('2', '1.5'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { bcpow('2', '99999999999999999999'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { bcpow('0', '-1'); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(4) "1024"
/// string(4) "3.37"
/// string(6) "0.2500"
/// string(3) "-27"
/// string(1) "1"
/// string(1) "0"
/// bcpow(): Argument #2 ($exponent) cannot have a fractional part
/// bcpow(): Argument #2 ($exponent) is too large
/// Negative power of zero
/// ```
#[php_function]
fn bcpow(
    req: &mut Request,
    num: PhpString,
    exponent: PhpString,
    #[php(default = null)] scale: Option<i64>,
) -> Result<Owner<PhpString>, Thrown> {
    let scale = scale_or_default(req, scale, 3)?;
    let base = operand(req, num.as_bytes(), 1)?;
    let exponent = operand(req, exponent.as_bytes(), 2)?;
    if exponent.num.scale() != 0 {
        return Err(value_error(req, 2, "cannot have a fractional part"));
    }
    let power = exponent.num.to_long();
    if power == 0 && exponent.num.int_is_nonzero() {
        return Err(value_error(req, 2, "is too large"));
    }
    match num::raise(&base.num, power, scale) {
        Ok(result) => string(req, &result.render(scale)),
        Err(error) => Err(raise_error(req, error, Some(2))),
    }
}

/// `bc_pow_err`: the refusal of a power, naming the exponent's
/// argument where there is one and not under an operator.
fn raise_error(req: &mut Request, error: RaiseError, position: Option<u32>) -> Thrown {
    const OVERFLOW: &str = "exponent is too large, the number of digits overflowed";
    match (error, position) {
        (RaiseError::DivideByZero, _) => {
            throw(req, "DivisionByZeroError", "Negative power of zero")
        }
        (RaiseError::Overflow, Some(position)) => value_error(req, position, OVERFLOW),
        (RaiseError::Overflow, None) => php_ext::raising::value_error(req, OVERFLOW),
    }
}

/// `bcsqrt(string $num, ?int $scale = null): string`, by php's Newton
/// iteration.
///
/// ```php
/// var_dump(bcsqrt('2', 10), bcsqrt('0.25', 3), bcsqrt('1'), bcsqrt('16.00'), bcsqrt('0.0001', 2));
/// try { bcsqrt('-1'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(12) "1.4142135623"
/// string(5) "0.500"
/// string(1) "1"
/// string(1) "4"
/// string(4) "0.01"
/// bcsqrt(): Argument #1 ($num) must be greater than or equal to 0
/// ```
#[php_function]
fn bcsqrt(
    req: &mut Request,
    num: PhpString,
    #[php(default = null)] scale: Option<i64>,
) -> Result<Owner<PhpString>, Thrown> {
    let scale = scale_or_default(req, scale, 2)?;
    let value = operand(req, num.as_bytes(), 1)?;
    match num::sqrt(&value.num, scale) {
        Some(root) => string(req, &root.render(scale)),
        None => Err(value_error(req, 1, "must be greater than or equal to 0")),
    }
}

/// `bccomp(string $num1, string $num2, ?int $scale = null): int`: the
/// two read to `scale` fraction digits alone, then compared.
///
/// ```php
/// var_dump(bccomp('1', '2'), bccomp('1.001', '1', 2), bccomp('1.001', '1', 3), bccomp('-0.0', '0'), bccomp('-1', '-2'));
/// try { bccomp('x', '1'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(-1)
/// int(0)
/// int(1)
/// int(0)
/// int(1)
/// bccomp(): Argument #1 ($num1) is not well-formed
/// ```
#[php_function]
fn bccomp(
    req: &mut Request,
    num1: PhpString,
    num2: PhpString,
    #[php(default = null)] scale: Option<i64>,
) -> Result<i64, Thrown> {
    let scale = scale_or_default(req, scale, 3)?;
    let Some((first, _)) = Num::parse(num1.as_bytes(), scale, false) else {
        return Err(value_error(req, 1, "is not well-formed"));
    };
    let Some((second, _)) = Num::parse(num2.as_bytes(), scale, false) else {
        return Err(value_error(req, 2, "is not well-formed"));
    };
    Ok(ordering(num::compare(&first, &second, scale)))
}

/// An ordering as `-1`, `0` or `1`.
fn ordering(order: std::cmp::Ordering) -> i64 {
    match order {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// `bcscale(?int $scale = null): int`: the scale before, and with an
/// argument `bcmath.scale` set to it at runtime.
///
/// ```php
/// var_dump(bcscale(), bcscale(3), bcscale(), bcadd('1', '1'), ini_get('bcmath.scale'));
/// try { bcscale(-1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(0)
/// int(0)
/// int(3)
/// string(5) "2.000"
/// string(1) "3"
/// bcscale(): Argument #1 ($scale) must be between 0 and 2147483647
/// ```
#[php_function]
fn bcscale(req: &mut Request, #[php(default = null)] scale: Option<i64>) -> Result<i64, Thrown> {
    let old = default_scale(req);
    if let Some(scale) = scale {
        let scale = check_scale(req, scale, 1)?;
        // `zend_long_to_str`'s string, which the entry keeps.
        let text = php_ext::coerce::int_to_string(req, i64::try_from(scale).unwrap_or(i64::MAX))?;
        let set = req.ini_mut().set_string(SCALE, *text);
        text.give_up(req);
        set?;
    }
    Ok(i64::try_from(old).unwrap_or(i64::from(i32::MAX)))
}

/// `bcfloor(string $num): string`.
///
/// ```php
/// var_dump(bcfloor('1.95'), bcfloor('-1.05'), bcfloor('-0.5'), bcfloor('3.000'));
/// ```
/// ```expect
/// string(1) "1"
/// string(2) "-2"
/// string(2) "-1"
/// string(1) "3"
/// ```
#[php_function]
fn bcfloor(req: &mut Request, num: PhpString) -> Result<Owner<PhpString>, Thrown> {
    let value = operand(req, num.as_bytes(), 1)?;
    string(req, &num::floor_or_ceil(&value.num, true).render(0))
}

/// `bcceil(string $num): string`.
///
/// ```php
/// var_dump(bcceil('1.05'), bcceil('-1.95'), bcceil('-0.5'), bcceil('3.000'));
/// ```
/// ```expect
/// string(1) "2"
/// string(2) "-1"
/// string(1) "0"
/// string(1) "3"
/// ```
#[php_function]
fn bcceil(req: &mut Request, num: PhpString) -> Result<Owner<PhpString>, Thrown> {
    let value = operand(req, num.as_bytes(), 1)?;
    string(req, &num::floor_or_ceil(&value.num, false).render(0))
}

/// ext/standard's `enum RoundingMode`, named for a parameter.
#[php_class(extern)]
pub(crate) struct RoundingMode;

/// `php_math_round_mode_from_enum`: the rounding a case of
/// `RoundingMode` stands for, read by its name.
fn rounding(
    req: &mut Request,
    mode: Instance<RoundingMode>,
    position: u32,
) -> Result<Rounding, Thrown> {
    let name = read_prop(req, &mode.object(), b"name")?;
    let rounding = match name.as_str_bytes() {
        Some(b"HalfAwayFromZero") => Some(Rounding::HalfUp),
        Some(b"HalfTowardsZero") => Some(Rounding::HalfDown),
        Some(b"HalfEven") => Some(Rounding::HalfEven),
        Some(b"HalfOdd") => Some(Rounding::HalfOdd),
        Some(b"TowardsZero") => Some(Rounding::TowardZero),
        Some(b"AwayFromZero") => Some(Rounding::AwayFromZero),
        Some(b"NegativeInfinity") => Some(Rounding::Floor),
        Some(b"PositiveInfinity") => Some(Rounding::Ceiling),
        _ => None,
    };
    rounding.ok_or_else(|| value_error(req, position, "is an unsupported rounding mode"))
}

/// `bcmath_check_precision`: a precision no wider than `INT_MAX`.
fn check_precision(req: &mut Request, precision: i64, position: u32) -> Result<(), Thrown> {
    if precision > i64::from(i32::MAX) {
        return Err(value_error(
            req,
            position,
            &format!("must be between {} and {}", i64::MIN, i32::MAX),
        ));
    }
    Ok(())
}

/// `bcround(string $num, int $precision = 0, RoundingMode $mode =
/// RoundingMode::HalfAwayFromZero): string`: the scale is the
/// precision's where it is not negative.
///
/// ```php
/// var_dump(bcround('1.955', 2), bcround('-1.5'), bcround('1234.5', -2), bcround('2.5', 0, RoundingMode::HalfEven), bcround('1.1', 3), bcround('-0.1', 0, RoundingMode::NegativeInfinity), bcround('5', -3, RoundingMode::AwayFromZero));
/// try { bcround('1', PHP_INT_MAX); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(4) "1.96"
/// string(2) "-2"
/// string(4) "1200"
/// string(1) "2"
/// string(5) "1.100"
/// string(2) "-1"
/// string(4) "1000"
/// bcround(): Argument #2 ($precision) must be between -9223372036854775808 and 2147483647
/// ```
#[php_function]
fn bcround(
    req: &mut Request,
    num: PhpString,
    #[php(default = 0)] precision: i64,
    #[php(default = RoundingMode::HalfAwayFromZero)] mode: Instance<RoundingMode>,
) -> Result<Owner<PhpString>, Thrown> {
    check_precision(req, precision, 2)?;
    let mode = rounding(req, mode, 3)?;
    let value = operand(req, num.as_bytes(), 1)?;
    let (rounded, scale) = num::round(&value.num, precision, mode);
    string(req, &rounded.render(scale))
}

/// "Division by zero".
fn division_by_zero(req: &mut Request) -> Thrown {
    throw(req, "DivisionByZeroError", "Division by zero")
}

/// "Modulo by zero".
fn modulo_by_zero(req: &mut Request) -> Thrown {
    throw(req, "DivisionByZeroError", "Modulo by zero")
}

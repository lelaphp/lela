//! The `gmp` extension: `php-src/ext/gmp` as one crate under `ext/`
//! (ADR-0004), `gmp.c` ported over the libgmp the pinned php links,
//! found through `pkg-config` and bound in [`ffi`], never vendored
//! (design §7.2, revision 72; #1540). This file is the extension value,
//! the argument decoder every `GMP|string|int` parameter is read
//! through, and the `gmp_*` natives; [`object`] is the `GMP` class
//! with its handlers. The phpt subset is `php-src/ext/gmp/tests`,
//! through `xtask phpt --subset`.
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
mod object;
mod throwables;

use php_ext::abi::{Extension, ExtensionBuilder, Instance, OrFalse, Raw, Union};
use php_ext::attr::{php_const, php_function};
use php_ext::coerce::{Exponent, float_format, is_space, strict_types, value_name};
use php_ext::decl::{ConstDecl, ConstExpr, Name};
use php_ext::raising::{
    Thrown, arg_null_deprecated, argument_error, deprecated, pending, throw_error,
};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, Value, ValueRef};

use crate::throwables::Throwables;
use ffi::{Mpz, RandState, Round};
use object::{GMP, made};

/// `GMP_MAX_BASE`.
const MAX_BASE: i64 = 62;

/// `INT_MAX`, the bound of a bit index over `GMP_NUMB_BITS`.
const INT_MAX: i64 = i32::MAX as i64;

/// `GMP_NUMB_BITS`, a limb's bits on the 64-bit targets.
const NUMB_BITS: i64 = 64;

/// The constants `register_gmp_symbols` registers but `GMP_VERSION`,
/// which is the library's and laid in by [`constants`].
#[php_const]
pub(crate) mod constants {
    pub(crate) const GMP_ROUND_ZERO: i64 = 0;
    pub(crate) const GMP_ROUND_PLUSINF: i64 = 1;
    pub(crate) const GMP_ROUND_MINUSINF: i64 = 2;
    pub(crate) const GMP_MSW_FIRST: i64 = 1;
    pub(crate) const GMP_LSW_FIRST: i64 = 2;
    pub(crate) const GMP_LITTLE_ENDIAN: i64 = 4;
    pub(crate) const GMP_BIG_ENDIAN: i64 = 8;
    pub(crate) const GMP_NATIVE_ENDIAN: i64 = 16;
}

use constants::{
    GMP_BIG_ENDIAN, GMP_LITTLE_ENDIAN, GMP_LSW_FIRST, GMP_MSW_FIRST, GMP_NATIVE_ENDIAN,
    GMP_ROUND_MINUSINF, GMP_ROUND_PLUSINF, GMP_ROUND_ZERO,
};

/// `gmp.stub.php`'s constants in its order, `GMP_VERSION` the string
/// the linked libgmp reports, `gmp_version`.
///
/// ```php
/// var_dump(GMP_ROUND_ZERO, GMP_ROUND_MINUSINF, GMP_VERSION === phpversion('gmp') ? 'php' : 'lib', GMP_LSW_FIRST | GMP_BIG_ENDIAN, GMP_NATIVE_ENDIAN);
/// ```
/// ```expect
/// int(0)
/// int(2)
/// string(3) "lib"
/// int(10)
/// int(16)
/// ```
fn constants() -> Vec<ConstDecl> {
    let mut all = constants::declared();
    let version = ConstDecl {
        name: Name(b"GMP_VERSION".as_slice().into()),
        value: ConstExpr::Str(ffi::version().into_bytes().into()),
        attributes: Vec::new(),
    };
    all.insert(all.len().min(3), version);
    all
}

/// The request's state, `zend_gmp_globals` less the scratch numbers:
/// the random state, made on first use and cleared with the request,
/// as `ZEND_MODULE_DEACTIVATE_D(gmp)` clears it.
#[derive(Default)]
struct State {
    rand: Option<RandState>,
}

/// The extension value (design §7.1): `gmp_module_entry`, its
/// constants, its functions in the stub's order and `GMP`.
///
/// ```php
/// var_dump(extension_loaded('gmp'), count(get_extension_funcs('gmp')), class_exists('GMP'));
/// ```
/// ```expect
/// bool(true)
/// int(51)
/// bool(true)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .constants(constants())
        .functions(vec![
            gmp_init::declared(),
            gmp_import::declared(),
            gmp_export::declared(),
            gmp_intval::declared(),
            gmp_strval::declared(),
            gmp_add::declared(),
            gmp_sub::declared(),
            gmp_mul::declared(),
            gmp_div_qr::declared(),
            gmp_div_q::declared(),
            gmp_div_r::declared(),
            gmp_div::declared(),
            gmp_mod::declared(),
            gmp_divexact::declared(),
            gmp_neg::declared(),
            gmp_abs::declared(),
            gmp_fact::declared(),
            gmp_sqrt::declared(),
            gmp_sqrtrem::declared(),
            gmp_root::declared(),
            gmp_rootrem::declared(),
            gmp_pow::declared(),
            gmp_powm::declared(),
            gmp_perfect_square::declared(),
            gmp_perfect_power::declared(),
            gmp_prob_prime::declared(),
            gmp_gcd::declared(),
            gmp_gcdext::declared(),
            gmp_lcm::declared(),
            gmp_invert::declared(),
            gmp_jacobi::declared(),
            gmp_legendre::declared(),
            gmp_kronecker::declared(),
            gmp_cmp::declared(),
            gmp_sign::declared(),
            gmp_random_seed::declared(),
            gmp_random_bits::declared(),
            gmp_random_range::declared(),
            gmp_and::declared(),
            gmp_or::declared(),
            gmp_com::declared(),
            gmp_xor::declared(),
            gmp_setbit::declared(),
            gmp_clrbit::declared(),
            gmp_testbit::declared(),
            gmp_scan0::declared(),
            gmp_scan1::declared(),
            gmp_popcount::declared(),
            gmp_hamdist::declared(),
            gmp_nextprime::declared(),
            gmp_binomial::declared(),
        ])
        .class::<GMP>()
        .request_state::<State>()
        .engine_state(throwables::Throwables::start)
        .info(gmp_info)
        .build("gmp")
}

/// `ZEND_MODULE_INFO_D(gmp)`: the support row and the library's
/// version.
///
/// ```php
/// ob_start(); (new ReflectionExtension('gmp'))->info(); $info = ob_get_clean();
/// echo str_replace(GMP_VERSION, 'X', $info);
/// ```
/// ```expect
/// gmp
///
/// gmp support => enabled
/// GMP version => X
/// ```
fn gmp_info(req: &mut Request) {
    let version = ffi::version();
    req.extensions_mut()
        .info_table(&[&["gmp support", "enabled"], &["GMP version", &version]]);
}

/// A `GMP|string|int` parameter, read by hand as
/// `GMP_Z_PARAM_INTO_MPZ_PTR` reads it.
type Number = Raw<Union<Instance<GMP>, Union<PhpString, i64>>>;

/// `convert_zstr_to_gmp`: leading whitespace skipped, a `0x`, `0o` or
/// `0b` prefix read where `base` allows it, then `mpz_set_str`; a
/// string that is no integer is php's `ValueError`, naming the argument
/// at `position` or, at zero, the number.
pub(crate) fn from_text(
    req: &mut Request,
    text: &[u8],
    base: i64,
    position: u32,
) -> Result<Mpz, Thrown> {
    let start = text
        .iter()
        .position(|b| !is_space(*b))
        .unwrap_or(text.len());
    let text = text.get(start..).unwrap_or_default();
    let (mut base, mut skip) = (base, false);
    if let [b'0', prefix, ..] = text {
        let found = match prefix {
            b'x' | b'X' => 16,
            b'o' | b'O' => 8,
            b'b' | b'B' => 2,
            _ => 0,
        };
        if found != 0 && (base == 0 || base == found) {
            (base, skip) = (found, true);
        }
    }
    let digits = if skip {
        text.get(2..).unwrap_or_default()
    } else {
        text
    };
    let mut n = Mpz::default();
    if n.set_str(digits, i32::try_from(base).unwrap_or(0)) {
        return Ok(n);
    }
    Err(if position == 0 {
        php_ext::raising::value_error(req, "Number is not an integer string")
    } else {
        let ce = Throwables::of(req)?;
        argument_error(req, ce.value_error, position, "is not an integer string")
    })
}

/// `zend_parse_arg_long_weak` over what is neither a string nor an
/// int: a float that fits, with php's deprecation where it loses a
/// fraction, `null` with its deprecation, and a bool; `None` for the
/// rest. The `Err` is a deprecation an error handler threw.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::float_cmp,
    reason = "`ZEND_DOUBLE_FITS_LONG`, `zend_dval_to_lval` over a float in range and `zend_is_long_compatible`'s exact `(double) l == d`"
)]
fn long_weak(req: &mut Request, value: &Value, position: u32) -> Result<Option<i64>, Thrown> {
    Ok(match value.as_ref() {
        ValueRef::Float(f) => {
            if f.is_nan()
                || !(-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&f)
            {
                return Ok(None);
            }
            let long = f as i64;
            if long as f64 != f {
                let text = float_format(f, -1, Exponent::Upper);
                deprecated(
                    req,
                    &format!("Implicit conversion from float {text} to int loses precision"),
                );
                pending(req)?;
            }
            Some(long)
        }
        ValueRef::Undef | ValueRef::Null => {
            arg_null_deprecated(req, position, "GMP|string|int");
            pending(req)?;
            Some(0)
        }
        ValueRef::Bool(b) => Some(i64::from(b)),
        _ => None,
    })
}

/// `gmp_zend_parse_arg_into_mpz_ex`: a `GMP` as it is, a string read by
/// [`from_text`] in base 0, an int, and under the weak rule what
/// [`long_weak`] takes, `null` refused under an operator. `Ok(None)` is
/// php's `false` with nothing thrown.
pub(crate) fn operand(
    req: &mut Request,
    value: &Value,
    position: u32,
    operator: bool,
) -> Result<Option<Mpz>, Thrown> {
    let value = value.deref();
    Ok(Some(match value.as_ref() {
        ValueRef::Object(o) => match o.native::<Mpz>(req) {
            Some(n) => n.clone(),
            None => return Ok(None),
        },
        ValueRef::Str(text) => {
            let text = text.as_bytes().to_vec();
            from_text(req, &text, 0, if operator { 0 } else { position })?
        }
        ValueRef::Int(n) => Mpz::from_long(n),
        _ if strict_types(req) => return Ok(None),
        ValueRef::Undef | ValueRef::Null if operator => return Ok(None),
        _ => match long_weak(req, &value, position)? {
            Some(n) => Mpz::from_long(n),
            None => return Ok(None),
        },
    }))
}

/// `GMP_Z_PARAM_INTO_MPZ_PTR`: the argument at `position` read by
/// [`operand`], or php's `TypeError` naming the union.
fn number(req: &mut Request, raw: &Number, position: u32) -> Result<Mpz, Thrown> {
    let value = raw.value();
    if let Some(n) = operand(req, &value, position, false)? {
        return Ok(n);
    }
    let given = value_name(req, &value);
    let tail = format!("must be of type GMP|string|int, {given} given");
    let ce = Throwables::of(req)?;
    Err(argument_error(req, ce.type_error, position, &tail))
}

/// The two operands of a binary function, the first read first.
fn numbers(req: &mut Request, a: &Number, b: &Number) -> Result<(Mpz, Mpz), Thrown> {
    let a = number(req, a, 1)?;
    Ok((a, number(req, b, 2)?))
}

/// An `int` parameter held as `Z_PARAM_LONG` holds it, after the ones
/// before it, as php's parser reads them in order.
fn long(req: &mut Request, raw: &Raw<i64>) -> Result<i64, Thrown> {
    raw.hold(req)
}

/// `zend_argument_value_error`.
fn value_error(req: &mut Request, position: u32, tail: &str) -> Thrown {
    let ce = match Throwables::of(req) {
        Ok(ce) => ce,
        Err(fatal) => return fatal,
    };
    argument_error(req, ce.value_error, position, tail)
}

/// `zend_new_pair` of two fresh numbers.
fn pair(req: &mut Request, a: Mpz, b: Mpz) -> Result<Owner<PhpArray>, Thrown> {
    let a = made(req, a)?;
    let b = made(req, b)?;
    let mut array = PhpArray::new(req)?;
    array.set(req, Owner::index(0), a.into_value())?;
    array.set(req, Owner::index(1), b.into_value())?;
    Ok(array)
}

/// `gmp_verify_base`.
pub(crate) fn verify_base(req: &mut Request, base: i64, position: u32) -> Result<(), Thrown> {
    if base != 0 && !(2..=MAX_BASE).contains(&base) {
        let tail = format!("must be 0 or between 2 and {MAX_BASE}");
        return Err(value_error(req, position, &tail));
    }
    Ok(())
}

/// `gmp_initialize_number`: a string in `base`, or an int.
pub(crate) fn initialized(
    req: &mut Request,
    num: Union<PhpString, i64>,
    base: i64,
) -> Result<Mpz, Thrown> {
    match num {
        Union::A(text) => {
            let text = text.as_bytes().to_vec();
            from_text(req, &text, base, 1)
        }
        Union::B(n) => Ok(Mpz::from_long(n)),
    }
}

/// `gmp_init(int|string $num, int $base = 0): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_init('0x1F')), gmp_strval(gmp_init(' 0b101')), gmp_strval(gmp_init('z', 36)), gmp_strval(gmp_init(-7)));
/// try { gmp_init('12a'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { gmp_init(1, 63); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(2) "31"
/// string(1) "5"
/// string(2) "35"
/// string(2) "-7"
/// gmp_init(): Argument #1 ($num) is not an integer string
/// gmp_init(): Argument #2 ($base) must be 0 or between 2 and 62
/// ```
#[php_function]
fn gmp_init(
    req: &mut Request,
    num: Union<PhpString, i64>,
    #[php(default = 0)] base: i64,
) -> Result<Owner<Instance<GMP>>, Thrown> {
    verify_base(req, base, 2)?;
    let n = initialized(req, num, base)?;
    made(req, n)
}

/// `gmp_import_export_validate`: the word order and the endianness the
/// flags name, `mpz_import`'s `order` and `endian`.
fn order_and_endian(req: &mut Request, size: i64, flags: i64) -> Result<(i32, i32), Thrown> {
    if size < 1 {
        return Err(value_error(req, 2, "must be greater than or equal to 1"));
    }
    let order = match flags & (GMP_LSW_FIRST | GMP_MSW_FIRST) {
        GMP_LSW_FIRST => -1,
        GMP_MSW_FIRST | 0 => 1,
        _ => {
            return Err(value_error(
                req,
                3,
                "cannot use multiple word order options",
            ));
        }
    };
    let endian = match flags & (GMP_LITTLE_ENDIAN | GMP_BIG_ENDIAN | GMP_NATIVE_ENDIAN) {
        GMP_LITTLE_ENDIAN => -1,
        GMP_BIG_ENDIAN => 1,
        GMP_NATIVE_ENDIAN | 0 => 0,
        _ => return Err(value_error(req, 3, "cannot use multiple endian options")),
    };
    Ok((order, endian))
}

/// `gmp_import(string $data, int $word_size = 1, int $flags =
/// GMP_MSW_FIRST | GMP_NATIVE_ENDIAN): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_import("\x01\x00")), gmp_strval(gmp_import("\x01\x00", 2, GMP_LSW_FIRST | GMP_LITTLE_ENDIAN)), gmp_strval(gmp_import('')));
/// try { gmp_import('abc', 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { gmp_import('ab', 1, GMP_LSW_FIRST | GMP_MSW_FIRST); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "256"
/// string(1) "1"
/// string(1) "0"
/// gmp_import(): Argument #1 ($data) must be a multiple of argument #2 ($word_size)
/// gmp_import(): Argument #3 ($flags) cannot use multiple word order options
/// ```
#[php_function]
fn gmp_import(
    req: &mut Request,
    data: PhpString,
    #[php(default = 1)] word_size: i64,
    #[php(default = GMP_MSW_FIRST | GMP_NATIVE_ENDIAN)] flags: i64,
) -> Result<Owner<Instance<GMP>>, Thrown> {
    let (order, endian) = order_and_endian(req, word_size, flags)?;
    let size = usize::try_from(word_size).unwrap_or(usize::MAX);
    if !data.len().is_multiple_of(size) {
        let tail = "must be a multiple of argument #2 ($word_size)";
        return Err(value_error(req, 1, tail));
    }
    let n = Mpz::import(data.as_bytes(), size, order, endian);
    made(req, n)
}

/// `gmp_export(GMP|int|string $num, int $word_size = 1, int $flags =
/// GMP_MSW_FIRST | GMP_NATIVE_ENDIAN): string`.
///
/// ```php
/// var_dump(bin2hex(gmp_export(258)), bin2hex(gmp_export(258, 4, GMP_BIG_ENDIAN)), bin2hex(gmp_export(258, 2, GMP_LSW_FIRST | GMP_LITTLE_ENDIAN)), gmp_export(0));
/// try { gmp_export(1, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(4) "0102"
/// string(8) "00000102"
/// string(4) "0201"
/// string(0) ""
/// gmp_export(): Argument #2 ($word_size) must be greater than or equal to 1
/// ```
#[php_function]
fn gmp_export(
    req: &mut Request,
    num: Number,
    #[php(default = 1)] word_size: Raw<i64>,
    #[php(default = GMP_MSW_FIRST | GMP_NATIVE_ENDIAN)] flags: Raw<i64>,
) -> Result<Owner<PhpString>, Thrown> {
    let n = number(req, &num, 1)?;
    let size = long(req, &word_size)?;
    let flags = long(req, &flags)?;
    let (order, endian) = order_and_endian(req, size, flags)?;
    if n.sign() == 0 {
        return PhpString::new(req, b"");
    }
    let bits = n.size_in_base(2);
    let size = usize::try_from(size).unwrap_or(usize::MAX);
    if size > usize::MAX / 32 || bits > usize::MAX - size * 8 + 1 {
        return Err(value_error(req, 2, "is too large for argument #1 ($num)"));
    }
    let count = bits.div_ceil(size * 8);
    let out = n.export(count, size, order, endian);
    PhpString::new(req, &out)
}

/// `gmp_intval(GMP|int|string $num): int`: `mpz_get_si`, the low bits
/// of a number past a long's range.
///
/// ```php
/// var_dump(gmp_intval(gmp_init('-42')), gmp_intval('0x10'), gmp_intval(gmp_pow(2, 64) + 5));
/// ```
/// ```expect
/// int(-42)
/// int(16)
/// int(5)
/// ```
#[php_function]
fn gmp_intval(req: &mut Request, num: Number) -> Result<i64, Thrown> {
    Ok(number(req, &num, 1)?.to_long())
}

/// `gmp_strval(GMP|int|string $num, int $base = 10): string`.
///
/// ```php
/// var_dump(gmp_strval(255, 16), gmp_strval(255, -16), gmp_strval(-255, 2), gmp_strval(61, 62));
/// try { gmp_strval(1, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { gmp_strval(1, -37); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(2) "ff"
/// string(2) "FF"
/// string(9) "-11111111"
/// string(1) "z"
/// gmp_strval(): Argument #2 ($base) must be between 2 and 62, or -2 and -36
/// gmp_strval(): Argument #2 ($base) must be between 2 and 62, or -2 and -36
/// ```
#[php_function]
fn gmp_strval(
    req: &mut Request,
    num: Number,
    #[php(default = 10)] base: Raw<i64>,
) -> Result<Owner<PhpString>, Thrown> {
    let n = number(req, &num, 1)?;
    let base = long(req, &base)?;
    if (-1..=1).contains(&base) || !(-36..=MAX_BASE).contains(&base) {
        let tail = format!("must be between 2 and {MAX_BASE}, or -2 and -36");
        return Err(value_error(req, 2, &tail));
    }
    PhpString::new(req, &n.to_text(i32::try_from(base).unwrap_or(10)))
}

/// `gmp_add(GMP|int|string $num1, GMP|int|string $num2): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_add('123456789012345678901234567890', 1)), gmp_strval(gmp_add(-5, gmp_init(3))));
/// try { gmp_add([], 1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { gmp_add(1, 'x'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(30) "123456789012345678901234567891"
/// string(2) "-2"
/// gmp_add(): Argument #1 ($num1) must be of type GMP|string|int, array given
/// gmp_add(): Argument #2 ($num2) is not an integer string
/// ```
#[php_function]
fn gmp_add(req: &mut Request, num1: Number, num2: Number) -> Result<Owner<Instance<GMP>>, Thrown> {
    let (a, b) = numbers(req, &num1, &num2)?;
    made(req, a.add(&b))
}

/// `gmp_sub(GMP|int|string $num1, GMP|int|string $num2): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_sub(5, 7)), gmp_strval(gmp_sub('0x100', '1')));
/// ```
/// ```expect
/// string(2) "-2"
/// string(3) "255"
/// ```
#[php_function]
fn gmp_sub(req: &mut Request, num1: Number, num2: Number) -> Result<Owner<Instance<GMP>>, Thrown> {
    let (a, b) = numbers(req, &num1, &num2)?;
    made(req, a.sub(&b))
}

/// `gmp_mul(GMP|int|string $num1, GMP|int|string $num2): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_mul(PHP_INT_MAX, PHP_INT_MAX)), gmp_strval(gmp_mul(-3, 4)));
/// ```
/// ```expect
/// string(38) "85070591730234615847396907784232501249"
/// string(3) "-12"
/// ```
#[php_function]
fn gmp_mul(req: &mut Request, num1: Number, num2: Number) -> Result<Owner<Instance<GMP>>, Thrown> {
    let (a, b) = numbers(req, &num1, &num2)?;
    made(req, a.mul(&b))
}

/// The rounding a `gmp_div*` call names, or php's `ValueError`.
fn rounding(req: &mut Request, round: i64) -> Result<Round, Thrown> {
    Ok(match round {
        GMP_ROUND_ZERO => Round::Zero,
        GMP_ROUND_PLUSINF => Round::PlusInf,
        GMP_ROUND_MINUSINF => Round::MinusInf,
        _ => {
            let tail = "must be one of GMP_ROUND_ZERO, GMP_ROUND_PLUSINF, or GMP_ROUND_MINUSINF";
            return Err(value_error(req, 3, tail));
        }
    })
}

/// The two operands of a division, then its rounding, the divisor held
/// to not zero with php's `DivisionByZeroError` naming it.
fn division(
    req: &mut Request,
    num1: &Number,
    num2: &Number,
    rounding_mode: Option<&Raw<i64>>,
    what: &str,
) -> Result<(Mpz, Mpz, i64), Thrown> {
    let (a, b) = numbers(req, num1, num2)?;
    let round = match rounding_mode {
        Some(raw) => long(req, raw)?,
        None => GMP_ROUND_ZERO,
    };
    if b.sign() == 0 {
        let ce = Throwables::of(req)?;
        return Err(argument_error(req, ce.division_by_zero_error, 2, what));
    }
    Ok((a, b, round))
}

/// `gmp_div_qr(GMP|int|string $num1, GMP|int|string $num2, int $rounding_mode = GMP_ROUND_ZERO): array`.
///
/// ```php
/// var_dump(array_map('gmp_strval', gmp_div_qr(-7, 2)), array_map('gmp_strval', gmp_div_qr(-7, 2, GMP_ROUND_MINUSINF)));
/// try { gmp_div_qr(1, 0); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
/// try { gmp_div_qr(1, 2, 5); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(2) "-3"
///   [1]=>
///   string(2) "-1"
/// }
/// array(2) {
///   [0]=>
///   string(2) "-4"
///   [1]=>
///   string(1) "1"
/// }
/// gmp_div_qr(): Argument #2 ($num2) Division by zero
/// gmp_div_qr(): Argument #3 ($rounding_mode) must be one of GMP_ROUND_ZERO, GMP_ROUND_PLUSINF, or GMP_ROUND_MINUSINF
/// ```
#[php_function]
fn gmp_div_qr(
    req: &mut Request,
    num1: Number,
    num2: Number,
    #[php(default = GMP_ROUND_ZERO)] rounding_mode: Raw<i64>,
) -> Result<Owner<PhpArray>, Thrown> {
    let (a, b, round) = division(req, &num1, &num2, Some(&rounding_mode), "Division by zero")?;
    let round = rounding(req, round)?;
    let (q, r) = a.div_qr(&b, round);
    pair(req, q, r)
}

/// `gmp_div_q(GMP|int|string $num1, GMP|int|string $num2, int $rounding_mode = GMP_ROUND_ZERO): GMP`, and its alias `gmp_div`.
///
/// ```php
/// var_dump(gmp_strval(gmp_div_q(7, 2)), gmp_strval(gmp_div_q(7, 2, GMP_ROUND_PLUSINF)), gmp_strval(gmp_div(-7, 2, GMP_ROUND_MINUSINF)));
/// try { gmp_div_q(1, 0); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "3"
/// string(1) "4"
/// string(2) "-4"
/// gmp_div_q(): Argument #2 ($num2) Division by zero
/// ```
#[php_function(aliases(gmp_div))]
fn gmp_div_q(
    req: &mut Request,
    num1: Number,
    num2: Number,
    #[php(default = GMP_ROUND_ZERO)] rounding_mode: Raw<i64>,
) -> Result<Owner<Instance<GMP>>, Thrown> {
    let (a, b, round) = division(req, &num1, &num2, Some(&rounding_mode), "Division by zero")?;
    let q = match rounding(req, round)? {
        Round::Zero => a.tdiv_q(&b),
        Round::PlusInf => a.cdiv_q(&b),
        Round::MinusInf => a.fdiv_q(&b),
    };
    made(req, q)
}

/// `gmp_div_r(GMP|int|string $num1, GMP|int|string $num2, int $rounding_mode = GMP_ROUND_ZERO): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_div_r(-7, 2)), gmp_strval(gmp_div_r(7, 2, GMP_ROUND_PLUSINF)));
/// try { gmp_div_r(1, 2, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(2) "-1"
/// string(2) "-1"
/// gmp_div_r(): Argument #3 ($rounding_mode) must be one of GMP_ROUND_ZERO, GMP_ROUND_PLUSINF, or GMP_ROUND_MINUSINF
/// ```
#[php_function]
fn gmp_div_r(
    req: &mut Request,
    num1: Number,
    num2: Number,
    #[php(default = GMP_ROUND_ZERO)] rounding_mode: Raw<i64>,
) -> Result<Owner<Instance<GMP>>, Thrown> {
    let (a, b, round) = division(req, &num1, &num2, Some(&rounding_mode), "Division by zero")?;
    let r = match rounding(req, round)? {
        Round::Zero => a.tdiv_r(&b),
        Round::PlusInf => a.cdiv_r(&b),
        Round::MinusInf => a.fdiv_r(&b),
    };
    made(req, r)
}

/// `gmp_mod(GMP|int|string $num1, GMP|int|string $num2): GMP`: never
/// negative.
///
/// ```php
/// var_dump(gmp_strval(gmp_mod(-7, 3)), gmp_strval(gmp_mod(7, -3)));
/// try { gmp_mod(1, 0); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "2"
/// string(1) "1"
/// gmp_mod(): Argument #2 ($num2) Modulo by zero
/// ```
#[php_function]
fn gmp_mod(req: &mut Request, num1: Number, num2: Number) -> Result<Owner<Instance<GMP>>, Thrown> {
    let (a, b, _) = division(req, &num1, &num2, None, "Modulo by zero")?;
    made(req, a.modulo(&b))
}

/// `gmp_divexact(GMP|int|string $num1, GMP|int|string $num2): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_divexact('10000000000000000000', 4)));
/// try { gmp_divexact(1, 0); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(19) "2500000000000000000"
/// gmp_divexact(): Argument #2 ($num2) Division by zero
/// ```
#[php_function]
fn gmp_divexact(
    req: &mut Request,
    num1: Number,
    num2: Number,
) -> Result<Owner<Instance<GMP>>, Thrown> {
    let (a, b, _) = division(req, &num1, &num2, None, "Division by zero")?;
    made(req, a.divexact(&b))
}

/// `gmp_neg(GMP|int|string $num): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_neg(5)), gmp_strval(gmp_neg('-0x10')));
/// ```
/// ```expect
/// string(2) "-5"
/// string(2) "16"
/// ```
#[php_function]
fn gmp_neg(req: &mut Request, num: Number) -> Result<Owner<Instance<GMP>>, Thrown> {
    let n = number(req, &num, 1)?;
    made(req, n.neg())
}

/// `gmp_abs(GMP|int|string $num): GMP`, the argument read under the
/// caller's rule.
///
/// ```php
/// var_dump(gmp_strval(gmp_abs(-5)), gmp_strval(gmp_abs(1.0)), gmp_strval(gmp_abs(true)));
/// var_dump(gmp_strval(gmp_abs(-2.5)));
/// var_dump(gmp_strval(gmp_abs(null)));
/// try { gmp_abs(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "5"
/// string(1) "1"
/// string(1) "1"
///
/// Deprecated: Implicit conversion from float -2.5 to int loses precision in %s on line 3
/// string(1) "2"
///
/// Deprecated: gmp_abs(): Passing null to parameter #1 ($num) of type GMP|string|int is deprecated in %s on line 4
/// string(1) "0"
/// gmp_abs(): Argument #1 ($num) must be of type GMP|string|int, stdClass given
/// ```
#[php_function]
fn gmp_abs(req: &mut Request, num: Number) -> Result<Owner<Instance<GMP>>, Thrown> {
    let n = number(req, &num, 1)?;
    made(req, n.abs())
}

/// `gmp_fact(GMP|int|string $num): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_fact(20)), gmp_strval(gmp_fact(0)));
/// try { gmp_fact(-1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(19) "2432902008176640000"
/// string(1) "1"
/// gmp_fact(): Argument #1 ($num) must be greater than or equal to 0
/// ```
#[php_function]
fn gmp_fact(req: &mut Request, num: Number) -> Result<Owner<Instance<GMP>>, Thrown> {
    let n = not_negative(req, &num)?;
    made(req, Mpz::factorial(n.to_ulong()))
}

/// The operand of a root or a factorial, held to not negative.
fn not_negative(req: &mut Request, num: &Number) -> Result<Mpz, Thrown> {
    let n = number(req, num, 1)?;
    if n.sign() < 0 {
        return Err(value_error(req, 1, "must be greater than or equal to 0"));
    }
    Ok(n)
}

/// `gmp_sqrt(GMP|int|string $num): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_sqrt(17)), gmp_strval(gmp_sqrt('1000000000000000000000000')));
/// try { gmp_sqrt(-4); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "4"
/// string(13) "1000000000000"
/// gmp_sqrt(): Argument #1 ($num) must be greater than or equal to 0
/// ```
#[php_function]
fn gmp_sqrt(req: &mut Request, num: Number) -> Result<Owner<Instance<GMP>>, Thrown> {
    let n = not_negative(req, &num)?;
    made(req, n.sqrt())
}

/// `gmp_sqrtrem(GMP|int|string $num): array`.
///
/// ```php
/// var_dump(array_map('gmp_strval', gmp_sqrtrem(17)));
/// try { gmp_sqrtrem(-1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(1) "4"
///   [1]=>
///   string(1) "1"
/// }
/// gmp_sqrtrem(): Argument #1 ($num) must be greater than or equal to 0
/// ```
#[php_function]
fn gmp_sqrtrem(req: &mut Request, num: Number) -> Result<Owner<PhpArray>, Thrown> {
    let n = not_negative(req, &num)?;
    let (s, r) = n.sqrtrem();
    pair(req, s, r)
}

/// The operand and the degree of a root, held as `gmp_root` and
/// `gmp_rootrem` hold them, `least` worded for each.
fn root_of(
    req: &mut Request,
    num: &Number,
    nth: &Raw<i64>,
    least: &str,
) -> Result<(Mpz, u64), Thrown> {
    let n = number(req, num, 1)?;
    let nth = long(req, nth)?;
    if nth <= 0 {
        return Err(value_error(req, 2, least));
    }
    if nth % 2 == 0 && n.sign() < 0 {
        let tail = "must be odd if argument #1 ($a) is negative";
        return Err(value_error(req, 2, tail));
    }
    Ok((n, nth.unsigned_abs()))
}

/// `gmp_root(GMP|int|string $num, int $nth): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_root(28, 3)), gmp_strval(gmp_root(-27, 3)));
/// try { gmp_root(4, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { gmp_root(-4, 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "3"
/// string(2) "-3"
/// gmp_root(): Argument #2 ($nth) must be greater than 0
/// gmp_root(): Argument #2 ($nth) must be odd if argument #1 ($a) is negative
/// ```
#[php_function]
fn gmp_root(req: &mut Request, num: Number, nth: Raw<i64>) -> Result<Owner<Instance<GMP>>, Thrown> {
    let (n, nth) = root_of(req, &num, &nth, "must be greater than 0")?;
    made(req, n.root(nth))
}

/// `gmp_rootrem(GMP|int|string $num, int $nth): array`.
///
/// ```php
/// var_dump(array_map('gmp_strval', gmp_rootrem(-28, 3)));
/// try { gmp_rootrem(4, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(2) "-3"
///   [1]=>
///   string(2) "-1"
/// }
/// gmp_rootrem(): Argument #2 ($nth) must be greater than or equal to 1
/// ```
#[php_function]
fn gmp_rootrem(req: &mut Request, num: Number, nth: Raw<i64>) -> Result<Owner<PhpArray>, Thrown> {
    let (n, nth) = root_of(req, &num, &nth, "must be greater than or equal to 1")?;
    let (s, r) = n.rootrem(nth);
    pair(req, s, r)
}

/// `gmp_pow(GMP|int|string $num, int $exponent): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_pow(2, 100)), gmp_strval(gmp_pow(-3, 3)), gmp_strval(gmp_pow(0, 0)));
/// try { gmp_pow(2, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(31) "1267650600228229401496703205376"
/// string(3) "-27"
/// string(1) "1"
/// gmp_pow(): Argument #2 ($exponent) must be greater than or equal to 0
/// ```
#[php_function]
fn gmp_pow(
    req: &mut Request,
    num: Number,
    exponent: Raw<i64>,
) -> Result<Owner<Instance<GMP>>, Thrown> {
    let n = number(req, &num, 1)?;
    let exponent = long(req, &exponent)?;
    let Ok(exponent) = u64::try_from(exponent) else {
        return Err(value_error(req, 2, "must be greater than or equal to 0"));
    };
    made(req, n.pow_ui(exponent))
}

/// `gmp_powm(GMP|int|string $num, GMP|int|string $exponent,
/// GMP|int|string $modulus): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_powm(4, 13, 497)), gmp_strval(gmp_powm(-2, 3, 5)));
/// try { gmp_powm(2, -1, 5); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { gmp_powm(2, 1, 0); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "445"
/// string(1) "2"
/// gmp_powm(): Argument #2 ($exponent) must be greater than or equal to 0
/// Modulo by zero
/// ```
#[php_function]
fn gmp_powm(
    req: &mut Request,
    num: Number,
    exponent: Number,
    modulus: Number,
) -> Result<Owner<Instance<GMP>>, Thrown> {
    let base = number(req, &num, 1)?;
    let exp = number(req, &exponent, 2)?;
    let modulus = number(req, &modulus, 3)?;
    if exp.sign() < 0 {
        return Err(value_error(req, 2, "must be greater than or equal to 0"));
    }
    if modulus.sign() == 0 {
        let ce = Throwables::of(req)?;
        return Err(throw_error(
            req,
            ce.division_by_zero_error,
            "Modulo by zero",
        ));
    }
    made(req, base.powm(&exp, &modulus))
}

/// `gmp_perfect_square(GMP|int|string $num): bool`.
///
/// ```php
/// var_dump(gmp_perfect_square(49), gmp_perfect_square(50), gmp_perfect_square(0));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn gmp_perfect_square(req: &mut Request, num: Number) -> Result<bool, Thrown> {
    Ok(number(req, &num, 1)?.perfect_square_p() != 0)
}

/// `gmp_perfect_power(GMP|int|string $num): bool`.
///
/// ```php
/// var_dump(gmp_perfect_power(8), gmp_perfect_power(-8), gmp_perfect_power(10));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn gmp_perfect_power(req: &mut Request, num: Number) -> Result<bool, Thrown> {
    Ok(number(req, &num, 1)?.perfect_power_p() != 0)
}

/// `gmp_prob_prime(GMP|int|string $num, int $repetitions = 10): int`:
/// 2 for certainly prime, 1 for probably, 0 for not.
///
/// ```php
/// var_dump(gmp_prob_prime(7), gmp_prob_prime(8), gmp_prob_prime('170141183460469231731687303715884105727', 25));
/// ```
/// ```expect
/// int(2)
/// int(0)
/// int(1)
/// ```
#[php_function]
fn gmp_prob_prime(
    req: &mut Request,
    num: Number,
    #[php(default = 10)] repetitions: Raw<i64>,
) -> Result<i64, Thrown> {
    let n = number(req, &num, 1)?;
    let reps = long(req, &repetitions)?;
    #[expect(clippy::cast_possible_truncation, reason = "php's `(int)reps`")]
    Ok(n.probab_prime(reps as i32))
}

/// `gmp_gcd(GMP|int|string $num1, GMP|int|string $num2): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_gcd(12, -18)), gmp_strval(gmp_gcd(0, 0)));
/// ```
/// ```expect
/// string(1) "6"
/// string(1) "0"
/// ```
#[php_function]
fn gmp_gcd(req: &mut Request, num1: Number, num2: Number) -> Result<Owner<Instance<GMP>>, Thrown> {
    let (a, b) = numbers(req, &num1, &num2)?;
    made(req, a.gcd(&b))
}

/// `gmp_gcdext(GMP|int|string $num1, GMP|int|string $num2): array`:
/// `g`, `s` and `t` with `a*s + b*t = g`.
///
/// ```php
/// var_dump(array_map('gmp_strval', gmp_gcdext(12, 21)));
/// ```
/// ```expect
/// array(3) {
///   ["g"]=>
///   string(1) "3"
///   ["s"]=>
///   string(1) "2"
///   ["t"]=>
///   string(2) "-1"
/// }
/// ```
#[php_function]
fn gmp_gcdext(req: &mut Request, num1: Number, num2: Number) -> Result<Owner<PhpArray>, Thrown> {
    let (a, b) = numbers(req, &num1, &num2)?;
    let (gcd, s, t) = a.gcdext(&b);
    let mut array = PhpArray::new(req)?;
    for (name, n) in [(b"g", gcd), (b"s", s), (b"t", t)] {
        let value = made(req, n)?.into_value();
        array.set_str(req, name, value)?;
    }
    Ok(array)
}

/// `gmp_lcm(GMP|int|string $num1, GMP|int|string $num2): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_lcm(4, -6)));
/// ```
/// ```expect
/// string(2) "12"
/// ```
#[php_function]
fn gmp_lcm(req: &mut Request, num1: Number, num2: Number) -> Result<Owner<Instance<GMP>>, Thrown> {
    let (a, b) = numbers(req, &num1, &num2)?;
    made(req, a.lcm(&b))
}

/// `gmp_invert(GMP|int|string $num1, GMP|int|string $num2): GMP|false`.
///
/// ```php
/// var_dump(gmp_strval(gmp_invert(3, 11)), gmp_invert(2, 4));
/// try { gmp_invert(3, 0); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "4"
/// bool(false)
/// Division by zero
/// ```
#[php_function]
fn gmp_invert(
    req: &mut Request,
    num1: Number,
    num2: Number,
) -> Result<OrFalse<Owner<Instance<GMP>>>, Thrown> {
    let (a, b) = numbers(req, &num1, &num2)?;
    if b.sign() == 0 {
        let ce = Throwables::of(req)?;
        return Err(throw_error(
            req,
            ce.division_by_zero_error,
            "Division by zero",
        ));
    }
    match a.invert(&b) {
        Some(r) => Ok(OrFalse::Value(made(req, r)?)),
        None => Ok(OrFalse::False),
    }
}

/// `gmp_jacobi(GMP|int|string $num1, GMP|int|string $num2): int`.
///
/// ```php
/// var_dump(gmp_jacobi(3, 7), gmp_jacobi(2, 7), gmp_jacobi(7, 21));
/// ```
/// ```expect
/// int(-1)
/// int(1)
/// int(0)
/// ```
#[php_function]
fn gmp_jacobi(req: &mut Request, num1: Number, num2: Number) -> Result<i64, Thrown> {
    let (a, b) = numbers(req, &num1, &num2)?;
    Ok(a.jacobi(&b))
}

/// `gmp_legendre(GMP|int|string $num1, GMP|int|string $num2): int`:
/// `mpz_jacobi`, as `gmp.h` defines `mpz_legendre`.
///
/// ```php
/// var_dump(gmp_legendre(4, 7), gmp_legendre(3, 7));
/// ```
/// ```expect
/// int(1)
/// int(-1)
/// ```
#[php_function]
fn gmp_legendre(req: &mut Request, num1: Number, num2: Number) -> Result<i64, Thrown> {
    let (a, b) = numbers(req, &num1, &num2)?;
    Ok(a.jacobi(&b))
}

/// `gmp_kronecker(GMP|int|string $num1, GMP|int|string $num2): int`:
/// `mpz_jacobi`, as `gmp.h` defines `mpz_kronecker`.
///
/// ```php
/// var_dump(gmp_kronecker(3, 8), gmp_kronecker(5, 10));
/// ```
/// ```expect
/// int(-1)
/// int(0)
/// ```
#[php_function]
fn gmp_kronecker(req: &mut Request, num1: Number, num2: Number) -> Result<i64, Thrown> {
    let (a, b) = numbers(req, &num1, &num2)?;
    Ok(a.jacobi(&b))
}

/// `gmp_cmp(GMP|int|string $num1, GMP|int|string $num2): int`.
///
/// ```php
/// var_dump(gmp_cmp(1, 2), gmp_cmp('100000000000000000000', 5), gmp_cmp(-3, -3));
/// ```
/// ```expect
/// int(-1)
/// int(1)
/// int(0)
/// ```
#[php_function]
fn gmp_cmp(req: &mut Request, num1: Number, num2: Number) -> Result<i64, Thrown> {
    let (a, b) = numbers(req, &num1, &num2)?;
    Ok(a.cmp(&b) as i64)
}

/// `gmp_sign(GMP|int|string $num): int`.
///
/// ```php
/// var_dump(gmp_sign(-5), gmp_sign(0), gmp_sign('99'));
/// ```
/// ```expect
/// int(-1)
/// int(0)
/// int(1)
/// ```
#[php_function]
fn gmp_sign(req: &mut Request, num: Number) -> Result<i64, Thrown> {
    Ok(number(req, &num, 1)?.sign())
}

/// `gmp_init_random`: the request's Mersenne Twister, seeded from the
/// platform's entropy on first use, as php seeds it from
/// `php_random_bytes_silent`.
fn random(req: &mut Request) -> &mut RandState {
    req.state::<State>().rand.get_or_insert_with(|| {
        let seed = ffi::entropy().unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() ^ u64::from(d.subsec_nanos()))
        });
        RandState::mersenne_twister(seed)
    })
}

/// `gmp_random_seed(GMP|int|string $seed): void`: what follows is
/// libgmp's Mersenne Twister from that seed.
///
/// ```php
/// gmp_random_seed(100);
/// $a = gmp_strval(gmp_random_bits(64));
/// gmp_random_seed(gmp_init(100));
/// var_dump($a === gmp_strval(gmp_random_bits(64)), $a, gmp_strval(gmp_random_range(1, 100)));
/// ```
/// ```expect
/// bool(true)
/// string(19) "7842303329126688544"
/// string(2) "98"
/// ```
#[php_function]
fn gmp_random_seed(req: &mut Request, seed: Number) -> Result<(), Thrown> {
    let seed = number(req, &seed, 1)?;
    random(req).seed(&seed);
    Ok(())
}

/// `gmp_random_bits(int $bits): GMP`: a number below `2 ** $bits`.
///
/// ```php
/// gmp_random_seed(42);
/// var_dump(gmp_cmp(gmp_random_bits(10), 1024) < 0, gmp_strval(gmp_random_bits(1)) <= 1);
/// try { gmp_random_bits(0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// gmp_random_bits(): Argument #1 ($bits) must be between 1 and 2147483647
/// ```
#[php_function]
fn gmp_random_bits(req: &mut Request, bits: i64) -> Result<Owner<Instance<GMP>>, Thrown> {
    if !(1..=INT_MAX).contains(&bits) {
        let tail = format!("must be between 1 and {INT_MAX}");
        return Err(value_error(req, 1, &tail));
    }
    let n = random(req).bits(bits.unsigned_abs());
    made(req, n)
}

/// `gmp_random_range(GMP|int|string $min, GMP|int|string $max): GMP`:
/// a number in `[$min, $max]`.
///
/// ```php
/// gmp_random_seed(7);
/// $n = gmp_random_range(-5, 5);
/// var_dump($n >= -5 && $n <= 5);
/// try { gmp_random_range(5, 5); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// gmp_random_range(): Argument #1 ($min) must be less than argument #2 ($maximum)
/// ```
#[php_function]
fn gmp_random_range(
    req: &mut Request,
    min: Number,
    max: Number,
) -> Result<Owner<Instance<GMP>>, Thrown> {
    let (min, max) = numbers(req, &min, &max)?;
    random(req);
    if max.cmp(&min).is_le() {
        let tail = "must be less than argument #2 ($maximum)";
        return Err(value_error(req, 1, tail));
    }
    let range = max.sub(&min).add(&Mpz::from_long(1));
    let n = random(req).below(&range).add(&min);
    made(req, n)
}

/// `gmp_and(GMP|int|string $num1, GMP|int|string $num2): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_and(0b1100, 0b1010)), gmp_strval(gmp_and(-1, 255)));
/// ```
/// ```expect
/// string(1) "8"
/// string(3) "255"
/// ```
#[php_function]
fn gmp_and(req: &mut Request, num1: Number, num2: Number) -> Result<Owner<Instance<GMP>>, Thrown> {
    let (a, b) = numbers(req, &num1, &num2)?;
    made(req, a.and(&b))
}

/// `gmp_or(GMP|int|string $num1, GMP|int|string $num2): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_or(0b1100, 0b1010)), gmp_strval(gmp_or(-8, 1)));
/// ```
/// ```expect
/// string(2) "14"
/// string(2) "-7"
/// ```
#[php_function]
fn gmp_or(req: &mut Request, num1: Number, num2: Number) -> Result<Owner<Instance<GMP>>, Thrown> {
    let (a, b) = numbers(req, &num1, &num2)?;
    made(req, a.ior(&b))
}

/// `gmp_com(GMP|int|string $num): GMP`: `-$num - 1`.
///
/// ```php
/// var_dump(gmp_strval(gmp_com(5)), gmp_strval(gmp_com(-1)));
/// ```
/// ```expect
/// string(2) "-6"
/// string(1) "0"
/// ```
#[php_function]
fn gmp_com(req: &mut Request, num: Number) -> Result<Owner<Instance<GMP>>, Thrown> {
    let n = number(req, &num, 1)?;
    made(req, n.com())
}

/// `gmp_xor(GMP|int|string $num1, GMP|int|string $num2): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_xor(0b1100, 0b1010)), gmp_strval(gmp_xor(-1, 1)));
/// ```
/// ```expect
/// string(1) "6"
/// string(2) "-2"
/// ```
#[php_function]
fn gmp_xor(req: &mut Request, num1: Number, num2: Number) -> Result<Owner<Instance<GMP>>, Thrown> {
    let (a, b) = numbers(req, &num1, &num2)?;
    made(req, a.xor(&b))
}

/// `gmp_is_bit_index_valid`, or php's `ValueError` naming the bound.
fn bit_index(req: &mut Request, index: i64) -> Result<u64, Thrown> {
    match u64::try_from(index) {
        Ok(bit) if index / NUMB_BITS < INT_MAX => Ok(bit),
        _ => {
            let tail = format!("must be between 0 and {INT_MAX} * {NUMB_BITS}");
            Err(value_error(req, 2, &tail))
        }
    }
}

/// `gmp_setbit(GMP $num, int $index, bool $value = true): void`: the
/// number changed in place, the one mutation a `GMP` takes.
///
/// ```php
/// $n = gmp_init(0); $m = $n;
/// gmp_setbit($n, 70); gmp_setbit($n, 0); gmp_setbit($n, 0, false);
/// var_dump(gmp_strval($m));
/// try { gmp_setbit($n, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(22) "1180591620717411303424"
/// gmp_setbit(): Argument #2 ($index) must be between 0 and 2147483647 * 64
/// ```
#[php_function]
fn gmp_setbit(
    req: &mut Request,
    num: Instance<GMP>,
    index: i64,
    #[php(default = true)] value: bool,
) -> Result<(), Thrown> {
    let bit = bit_index(req, index)?;
    let n = num.payload_mut(req)?;
    if value {
        n.set_bit(bit);
    } else {
        n.clear_bit(bit);
    }
    Ok(())
}

/// `gmp_clrbit(GMP $num, int $index): void`.
///
/// ```php
/// $n = gmp_init(0b111);
/// gmp_clrbit($n, 1);
/// var_dump(gmp_strval($n));
/// try { gmp_clrbit($n, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "5"
/// gmp_clrbit(): Argument #2 ($index) must be between 0 and 2147483647 * 64
/// ```
#[php_function]
fn gmp_clrbit(req: &mut Request, num: Instance<GMP>, index: i64) -> Result<(), Thrown> {
    let bit = bit_index(req, index)?;
    num.payload_mut(req)?.clear_bit(bit);
    Ok(())
}

/// `gmp_testbit(GMP|int|string $num, int $index): bool`.
///
/// ```php
/// var_dump(gmp_testbit(5, 0), gmp_testbit(5, 1), gmp_testbit(-1, 1000));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn gmp_testbit(req: &mut Request, num: Number, index: Raw<i64>) -> Result<bool, Thrown> {
    let n = number(req, &num, 1)?;
    let index = long(req, &index)?;
    let bit = bit_index(req, index)?;
    Ok(n.test_bit(bit))
}

/// `gmp_scan0(GMP|int|string $num1, int $start): int`.
///
/// ```php
/// var_dump(gmp_scan0(0b1011, 0), gmp_scan0(-1, 0));
/// ```
/// ```expect
/// int(2)
/// int(-1)
/// ```
#[php_function]
fn gmp_scan0(req: &mut Request, num1: Number, start: Raw<i64>) -> Result<i64, Thrown> {
    let n = number(req, &num1, 1)?;
    let start = long(req, &start)?;
    let bit = bit_index(req, start)?;
    Ok(n.scan0(bit))
}

/// `gmp_scan1(GMP|int|string $num1, int $start): int`.
///
/// ```php
/// var_dump(gmp_scan1(0b1000, 0), gmp_scan1(0, 0));
/// try { gmp_scan1(1, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// int(-1)
/// gmp_scan1(): Argument #2 ($start) must be between 0 and 2147483647 * 64
/// ```
#[php_function]
fn gmp_scan1(req: &mut Request, num1: Number, start: Raw<i64>) -> Result<i64, Thrown> {
    let n = number(req, &num1, 1)?;
    let start = long(req, &start)?;
    let bit = bit_index(req, start)?;
    Ok(n.scan1(bit))
}

/// `gmp_popcount(GMP|int|string $num): int`.
///
/// ```php
/// var_dump(gmp_popcount(255), gmp_popcount(-1));
/// ```
/// ```expect
/// int(8)
/// int(-1)
/// ```
#[php_function]
fn gmp_popcount(req: &mut Request, num: Number) -> Result<i64, Thrown> {
    Ok(number(req, &num, 1)?.popcount())
}

/// `gmp_hamdist(GMP|int|string $num1, GMP|int|string $num2): int`.
///
/// ```php
/// var_dump(gmp_hamdist(0b1011, 0b0110), gmp_hamdist(-1, 1));
/// ```
/// ```expect
/// int(3)
/// int(-1)
/// ```
#[php_function]
fn gmp_hamdist(req: &mut Request, num1: Number, num2: Number) -> Result<i64, Thrown> {
    let (a, b) = numbers(req, &num1, &num2)?;
    Ok(a.hamdist(&b))
}

/// `gmp_nextprime(GMP|int|string $num): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_nextprime(100)), gmp_strval(gmp_nextprime(-10)));
/// ```
/// ```expect
/// string(3) "101"
/// string(1) "2"
/// ```
#[php_function]
fn gmp_nextprime(req: &mut Request, num: Number) -> Result<Owner<Instance<GMP>>, Thrown> {
    let n = number(req, &num, 1)?;
    made(req, n.nextprime())
}

/// `gmp_binomial(GMP|int|string $n, int $k): GMP`.
///
/// ```php
/// var_dump(gmp_strval(gmp_binomial(10, 3)), gmp_strval(gmp_binomial(-5, 2)));
/// try { gmp_binomial(5, -1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "120"
/// string(2) "15"
/// gmp_binomial(): Argument #2 ($k) must be greater than or equal to 0
/// ```
#[php_function]
fn gmp_binomial(req: &mut Request, n: Number, k: Raw<i64>) -> Result<Owner<Instance<GMP>>, Thrown> {
    let n = number(req, &n, 1)?;
    let k = long(req, &k)?;
    let Ok(k) = u64::try_from(k) else {
        return Err(value_error(req, 2, "must be greater than or equal to 0"));
    };
    made(req, n.bin_ui(k))
}

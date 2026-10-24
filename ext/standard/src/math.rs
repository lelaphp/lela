//! The natives of `math.c`, `enum RoundingMode`, the constants the stub
//! declares beside them, and the rounding under them that other files
//! call: `_php_math_round` and `php_round_helper`, copied
//! (`Z/ext/standard/math.c:62-252`), which `round`, `number_format` and
//! `range` round with.

use crate::string::Decimal;
use php_ext::abi::{Passed, Raw, Union};
use php_ext::attr::{php_class, php_const, php_function};
use php_ext::coerce::{BinaryOp, binary_op, coerce_param, is_space};
use php_ext::decl::Type;
use php_ext::raising::{Level, Thrown, arg_value_error, deprecated, docref, throw, value_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject, PhpString, Value, ValueRef};

pub(crate) use constants::{
    PHP_ROUND_HALF_DOWN, PHP_ROUND_HALF_EVEN, PHP_ROUND_HALF_ODD, PHP_ROUND_HALF_UP,
};

/// An int or a float, `Z_PARAM_NUMBER`'s two.
type Number = Union<i64, f64>;

/// The modes of `Z/ext/standard/php_math_modes.h` beside the four the
/// stub declares as `PHP_ROUND_*`.
pub(crate) const PHP_ROUND_CEILING: i32 = 0x05;
pub(crate) const PHP_ROUND_FLOOR: i32 = 0x06;
pub(crate) const PHP_ROUND_TOWARD_ZERO: i32 = 0x07;
pub(crate) const PHP_ROUND_AWAY_FROM_ZERO: i32 = 0x08;

/// The constants of `math.c`'s part of the stub: the seventeen `M_*`
/// and the four `PHP_ROUND_*` modes.
///
/// ```php
/// var_dump(M_E, M_LOG2E, M_LOG10E, M_LN2, M_LN10, M_PI, M_PI_2, M_PI_4, M_1_PI, M_2_PI, M_SQRTPI, M_2_SQRTPI, M_LNPI, M_EULER, M_SQRT2, M_SQRT1_2, M_SQRT3, INF, NAN, PHP_ROUND_HALF_UP, PHP_ROUND_HALF_DOWN, PHP_ROUND_HALF_EVEN, PHP_ROUND_HALF_ODD);
/// ```
/// ```expect
/// float(2.718281828459045)
/// float(1.4426950408889634)
/// float(0.4342944819032518)
/// float(0.6931471805599453)
/// float(2.302585092994046)
/// float(3.141592653589793)
/// float(1.5707963267948966)
/// float(0.7853981633974483)
/// float(0.3183098861837907)
/// float(0.6366197723675814)
/// float(1.772453850905516)
/// float(1.1283791670955126)
/// float(1.1447298858494002)
/// float(0.5772156649015329)
/// float(1.4142135623730951)
/// float(0.7071067811865476)
/// float(1.7320508075688772)
/// float(INF)
/// float(NAN)
/// int(1)
/// int(2)
/// int(3)
/// int(4)
/// ```
#[php_const]
pub(crate) mod constants {
    pub(crate) const M_E: f64 = std::f64::consts::E;
    pub(crate) const M_LOG2E: f64 = std::f64::consts::LOG2_E;
    pub(crate) const M_LOG10E: f64 = std::f64::consts::LOG10_E;
    pub(crate) const M_LN2: f64 = std::f64::consts::LN_2;
    pub(crate) const M_LN10: f64 = std::f64::consts::LN_10;
    pub(crate) const M_PI: f64 = std::f64::consts::PI;
    pub(crate) const M_PI_2: f64 = std::f64::consts::FRAC_PI_2;
    pub(crate) const M_PI_4: f64 = std::f64::consts::FRAC_PI_4;
    pub(crate) const M_1_PI: f64 = std::f64::consts::FRAC_1_PI;
    pub(crate) const M_2_PI: f64 = std::f64::consts::FRAC_2_PI;
    pub(crate) const M_SQRTPI: f64 = 1.772_453_850_905_516_f64;
    pub(crate) const M_2_SQRTPI: f64 = std::f64::consts::FRAC_2_SQRT_PI;
    pub(crate) const M_LNPI: f64 = 1.144_729_885_849_400_2_f64;
    pub(crate) const M_EULER: f64 = 0.577_215_664_901_532_9_f64;
    pub(crate) const M_SQRT2: f64 = std::f64::consts::SQRT_2;
    pub(crate) const M_SQRT1_2: f64 = std::f64::consts::FRAC_1_SQRT_2;
    pub(crate) const M_SQRT3: f64 = 1.732_050_807_568_877_2_f64;
    pub(crate) const INF: f64 = f64::INFINITY;
    pub(crate) const NAN: f64 = f64::NAN;
    pub(crate) const PHP_ROUND_HALF_UP: i32 = 0x01;
    pub(crate) const PHP_ROUND_HALF_DOWN: i32 = 0x02;
    pub(crate) const PHP_ROUND_HALF_EVEN: i32 = 0x03;
    pub(crate) const PHP_ROUND_HALF_ODD: i32 = 0x04;
}

/// `enum RoundingMode` as `basic_functions.stub.php` declares it, its
/// eight cases in order; `cases()` is the engine's, which gives it to
/// every enum it links as `zend_enum_register_funcs` does.
///
/// ```php
/// var_dump(RoundingMode::HalfEven, RoundingMode::cases()[7], count(RoundingMode::cases()), RoundingMode::TowardsZero->name, RoundingMode::HalfOdd instanceof UnitEnum);
/// ```
/// ```expect
/// enum(RoundingMode::HalfEven)
/// enum(RoundingMode::PositiveInfinity)
/// int(8)
/// string(11) "TowardsZero"
/// bool(true)
/// ```
#[php_class]
pub(crate) enum RoundingMode {
    HalfAwayFromZero,
    HalfTowardsZero,
    HalfEven,
    HalfOdd,
    TowardsZero,
    AwayFromZero,
    NegativeInfinity,
    PositiveInfinity,
}

#[php_class]
impl RoundingMode {}

impl RoundingMode {
    /// The rounding `php_math_round_mode_from_enum` reads a case as.
    fn mode(self) -> i32 {
        match self {
            RoundingMode::HalfAwayFromZero => PHP_ROUND_HALF_UP,
            RoundingMode::HalfTowardsZero => PHP_ROUND_HALF_DOWN,
            RoundingMode::HalfEven => PHP_ROUND_HALF_EVEN,
            RoundingMode::HalfOdd => PHP_ROUND_HALF_ODD,
            RoundingMode::TowardsZero => PHP_ROUND_TOWARD_ZERO,
            RoundingMode::AwayFromZero => PHP_ROUND_AWAY_FROM_ZERO,
            RoundingMode::NegativeInfinity => PHP_ROUND_FLOOR,
            RoundingMode::PositiveInfinity => PHP_ROUND_CEILING,
        }
    }
}

/// `abs(int|float $num): int|float`, php's `PHP_FUNCTION(abs)`
/// (`Z/ext/standard/math.c:249`): the magnitude, an int for an int
/// except `PHP_INT_MIN`, whose magnitude is a float.
///
/// ```php
/// var_dump(abs(-5), abs(-5.5), abs(PHP_INT_MIN), abs("-3"), abs(-0.0), abs(true));
/// try { abs("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(5)
/// float(5.5)
/// float(9.223372036854776E+18)
/// int(3)
/// float(0)
/// int(1)
/// abs(): Argument #1 ($num) must be of type int|float, string given
/// ```
///
/// The declarations of `math.c` Reflection is asked for most, as
/// `basic_functions.stub.php` has them.
///
/// ```php
/// foreach (['abs', 'log', 'pow', 'intdiv', 'number_format', 'base_convert', 'dechex', 'pi'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function abs ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> int|float $num ]
///   }
///   - Return [ int|float ]
/// }
/// Function [ <internal:standard> function log ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> float $num ]
///     Parameter #1 [ <optional> float $base = M_E ]
///   }
///   - Return [ float ]
/// }
/// Function [ <internal:standard> function pow ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> mixed $num ]
///     Parameter #1 [ <required> mixed $exponent ]
///   }
///   - Return [ object|int|float ]
/// }
/// Function [ <internal:standard> function intdiv ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> int $num1 ]
///     Parameter #1 [ <required> int $num2 ]
///   }
///   - Return [ int ]
/// }
/// Function [ <internal:standard> function number_format ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> float $num ]
///     Parameter #1 [ <optional> int $decimals = 0 ]
///     Parameter #2 [ <optional> ?string $decimal_separator = "." ]
///     Parameter #3 [ <optional> ?string $thousands_separator = "," ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function base_convert ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> string $num ]
///     Parameter #1 [ <required> int $from_base ]
///     Parameter #2 [ <required> int $to_base ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function dechex ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> int $num ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function pi ] {
///
///   - Parameters [0] {
///   }
///   - Return [ float ]
/// }
/// ```
#[php_function]
fn abs(num: Number) -> Number {
    match num {
        #[allow(clippy::cast_precision_loss)]
        Union::A(i64::MIN) => Union::B(-(i64::MIN as f64)),
        Union::A(n) => Union::A(n.abs()),
        Union::B(f) => Union::B(f.abs()),
    }
}

/// `ceil(int|float $num): float`, php's `PHP_FUNCTION(ceil)`: the least
/// whole float at or above, an int as the float it is.
///
/// ```php
/// var_dump(ceil(4.1), ceil(-4.1), ceil(5), ceil("2.5"), ceil(-0.5));
/// ```
/// ```expect
/// float(5)
/// float(-4)
/// float(5)
/// float(3)
/// float(-0)
/// ```
#[php_function]
fn ceil(num: Number) -> f64 {
    whole(num, f64::ceil)
}

/// `floor(int|float $num): float`, php's `PHP_FUNCTION(floor)`: the
/// greatest whole float at or below.
///
/// ```php
/// var_dump(floor(4.9), floor(-4.1), floor(5), floor("3"), floor(0.5));
/// ```
/// ```expect
/// float(4)
/// float(-5)
/// float(5)
/// float(3)
/// float(0)
/// ```
#[php_function]
fn floor(num: Number) -> f64 {
    whole(num, f64::floor)
}

fn whole(num: Number, apply: fn(f64) -> f64) -> f64 {
    match num {
        #[allow(clippy::cast_precision_loss)]
        Union::A(n) => n as f64,
        Union::B(f) => apply(f),
    }
}

/// `round(int|float $num, int $precision = 0, int|RoundingMode $mode = RoundingMode::HalfAwayFromZero): float`,
/// php's `PHP_FUNCTION(round)` (`Z/ext/standard/math.c:345`):
/// `_php_math_round` in the rounding named by
/// a `PHP_ROUND_*` constant or a `RoundingMode` case; an int to places at
/// or past zero is the float it is.
///
/// ```php
/// var_dump(round(1.955, 2), round(5.045, 2), round(5.055, 2), round(-0.5), round(2.5, 0, PHP_ROUND_HALF_EVEN), round(1234.5678, -2), round(5, -1), round(15, -1), round("3.7"), round(3.3, 0, PHP_ROUND_HALF_ODD), round(0.285, 2), round(1.45, 1, PHP_ROUND_HALF_DOWN));
/// ```
/// ```expect
/// float(1.96)
/// float(5.05)
/// float(5.06)
/// float(-1)
/// float(2)
/// float(1200)
/// float(10)
/// float(20)
/// float(4)
/// float(3)
/// float(0.29)
/// float(1.4)
/// ```
///
/// ```php
/// var_dump(round(1.45, 1, RoundingMode::HalfTowardsZero), round(-1.5, 0, RoundingMode::TowardsZero), round(1.1, 0, RoundingMode::PositiveInfinity), round(-1.1, 0, RoundingMode::NegativeInfinity), round(1.1, 0, RoundingMode::AwayFromZero), round(2.5, 0, RoundingMode::HalfEven), round(2.5, 0, RoundingMode::HalfOdd), round(-2.5, 0, RoundingMode::HalfAwayFromZero));
/// try { round(1, 0, 99); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { round(1, 0, "x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// float(1.4)
/// float(-1)
/// float(2)
/// float(-2)
/// float(2)
/// float(2)
/// float(3)
/// float(-3)
/// round(): Argument #3 ($mode) must be a valid rounding mode (RoundingMode::*)
/// round(): Argument #3 ($mode) must be of type RoundingMode|int, string given
/// ```
///
/// The declaration, `$mode`'s default the enum case php renders.
///
/// ```php
/// echo new ReflectionFunction('round');
/// ```
/// ```expect
/// Function [ <internal:standard> function round ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> int|float $num ]
///     Parameter #1 [ <optional> int $precision = 0 ]
///     Parameter #2 [ <optional> RoundingMode|int $mode = RoundingMode::HalfAwayFromZero ]
///   }
///   - Return [ float ]
/// }
/// ```
#[php_function]
fn round(
    req: &mut Request,
    num: Number,
    #[php(default = 0)] precision: i64,
    #[php(default = RoundingMode::HalfAwayFromZero)] mode: Union<i64, RoundingMode>,
) -> Result<f64, Thrown> {
    let places =
        i32::try_from(precision).unwrap_or(if precision < 0 { i32::MIN } else { i32::MAX });
    let mode = match mode {
        Union::A(mode) => i32::try_from(mode).unwrap_or(0),
        Union::B(case) => case.mode(),
    };
    if !(PHP_ROUND_HALF_UP..=PHP_ROUND_AWAY_FROM_ZERO).contains(&mode) {
        return Err(arg_value_error(
            req,
            3,
            "be a valid rounding mode (RoundingMode::*)",
        ));
    }
    Ok(match num {
        #[allow(clippy::cast_precision_loss)]
        Union::A(n) if places >= 0 => n as f64,
        #[allow(clippy::cast_precision_loss)]
        Union::A(n) => math_round(n as f64, places, mode),
        Union::B(f) => math_round(f, places, mode),
    })
}

/// `sin(float $num): float`.
///
/// ```php
/// var_dump(sin(0), sin(M_PI_2), sin("1"));
/// ```
/// ```expect
/// float(0)
/// float(1)
/// float(0.8414709848078965)
/// ```
#[php_function]
fn sin(num: f64) -> f64 {
    num.sin()
}

/// `cos(float $num): float`.
///
/// ```php
/// var_dump(cos(0), cos(M_PI));
/// ```
/// ```expect
/// float(1)
/// float(-1)
/// ```
#[php_function]
fn cos(num: f64) -> f64 {
    num.cos()
}

/// `tan(float $num): float`.
///
/// ```php
/// var_dump(tan(0), tan(M_PI_4));
/// ```
/// ```expect
/// float(0)
/// float(0.9999999999999999)
/// ```
#[php_function]
fn tan(num: f64) -> f64 {
    num.tan()
}

/// `asin(float $num): float`.
///
/// ```php
/// var_dump(asin(1), asin(2));
/// ```
/// ```expect
/// float(1.5707963267948966)
/// float(NAN)
/// ```
#[php_function]
fn asin(num: f64) -> f64 {
    num.asin()
}

/// `acos(float $num): float`.
///
/// ```php
/// var_dump(acos(1), acos(-1));
/// ```
/// ```expect
/// float(0)
/// float(3.141592653589793)
/// ```
#[php_function]
fn acos(num: f64) -> f64 {
    num.acos()
}

/// `atan(float $num): float`.
///
/// ```php
/// var_dump(atan(1), atan(INF));
/// ```
/// ```expect
/// float(0.7853981633974483)
/// float(1.5707963267948966)
/// ```
#[php_function]
fn atan(num: f64) -> f64 {
    num.atan()
}

/// `atan2(float $y, float $x): float`.
///
/// ```php
/// var_dump(atan2(1, 1), atan2(0, -1), atan2(-0.0, -1));
/// ```
/// ```expect
/// float(0.7853981633974483)
/// float(3.141592653589793)
/// float(-3.141592653589793)
/// ```
#[php_function]
fn atan2(y: f64, x: f64) -> f64 {
    y.atan2(x)
}

/// `sinh(float $num): float`.
///
/// ```php
/// var_dump(sinh(0), sinh(1));
/// ```
/// ```expect
/// float(0)
/// float(1.1752011936438014)
/// ```
#[php_function]
fn sinh(num: f64) -> f64 {
    num.sinh()
}

/// `cosh(float $num): float`.
///
/// ```php
/// var_dump(cosh(0), cosh(1));
/// ```
/// ```expect
/// float(1)
/// float(1.5430806348152437)
/// ```
#[php_function]
fn cosh(num: f64) -> f64 {
    num.cosh()
}

/// `tanh(float $num): float`.
///
/// ```php
/// var_dump(tanh(0), tanh(1));
/// ```
/// ```expect
/// float(0)
/// float(0.7615941559557649)
/// ```
#[php_function]
fn tanh(num: f64) -> f64 {
    num.tanh()
}

/// `asinh(float $num): float`.
///
/// ```php
/// var_dump(asinh(0), asinh(1));
/// ```
/// ```expect
/// float(0)
/// float(0.881373587019543)
/// ```
#[php_function]
fn asinh(num: f64) -> f64 {
    num.asinh()
}

/// `acosh(float $num): float`.
///
/// ```php
/// var_dump(acosh(1), acosh(2), acosh(0));
/// ```
/// ```expect
/// float(0)
/// float(1.3169578969248166)
/// float(NAN)
/// ```
#[php_function]
fn acosh(num: f64) -> f64 {
    num.acosh()
}

/// `atanh(float $num): float`.
///
/// ```php
/// var_dump(atanh(0), atanh(0.5), atanh(1));
/// ```
/// ```expect
/// float(0)
/// float(0.5493061443340549)
/// float(INF)
/// ```
#[php_function]
fn atanh(num: f64) -> f64 {
    num.atanh()
}

/// `expm1(float $num): float`, `exp(num) - 1` without the cancellation.
///
/// ```php
/// var_dump(expm1(0), expm1(1e-10), expm1(1));
/// ```
/// ```expect
/// float(0)
/// float(1.00000000005E-10)
/// float(1.7182818284590453)
/// ```
#[php_function]
fn expm1(num: f64) -> f64 {
    num.exp_m1()
}

/// `log1p(float $num): float`, `log(1 + num)` without the cancellation.
///
/// ```php
/// var_dump(log1p(0), log1p(1e-10), log1p(-1));
/// ```
/// ```expect
/// float(0)
/// float(9.999999999500001E-11)
/// float(-INF)
/// ```
#[php_function]
fn log1p(num: f64) -> f64 {
    num.ln_1p()
}

/// `pi(): float`, `M_PI`.
///
/// ```php
/// var_dump(pi(), pi() === M_PI);
/// ```
/// ```expect
/// float(3.141592653589793)
/// bool(true)
/// ```
#[php_function]
fn pi() -> f64 {
    std::f64::consts::PI
}

/// `is_finite(float $num): bool`.
///
/// ```php
/// var_dump(is_finite(1.0), is_finite(INF), is_finite(NAN), is_finite(PHP_INT_MAX));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn is_finite(num: f64) -> bool {
    num.is_finite()
}

/// `is_nan(float $num): bool`.
///
/// ```php
/// var_dump(is_nan(NAN), is_nan(1.0), is_nan(sqrt(-1)));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn is_nan(num: f64) -> bool {
    num.is_nan()
}

/// `is_infinite(float $num): bool`.
///
/// ```php
/// var_dump(is_infinite(-INF), is_infinite(1e308 * 10), is_infinite(NAN));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn is_infinite(num: f64) -> bool {
    num.is_infinite()
}

/// `intdiv(int $num1, int $num2): int`, php's `PHP_FUNCTION(intdiv)`
/// (`Z/ext/standard/math.c:1471`): the quotient toward zero, and php's
/// `DivisionByZeroError` and `ArithmeticError`.
///
/// ```php
/// var_dump(intdiv(7, 2), intdiv(-7, 2), intdiv(PHP_INT_MIN, 1));
/// try { intdiv(1, 0); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; }
/// try { intdiv(PHP_INT_MIN, -1); } catch (ArithmeticError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// int(-3)
/// int(-9223372036854775808)
/// Division by zero
/// ArithmeticError: Division of PHP_INT_MIN by -1 is not an integer
/// ```
#[php_function]
fn intdiv(req: &mut Request, num1: i64, num2: i64) -> Result<i64, Thrown> {
    let (dividend, divisor) = (num1, num2);
    if divisor == 0 {
        return Err(throw(req, "DivisionByZeroError", "Division by zero"));
    }
    if divisor == -1 && dividend == i64::MIN {
        return Err(throw(
            req,
            "ArithmeticError",
            "Division of PHP_INT_MIN by -1 is not an integer",
        ));
    }
    Ok(dividend / divisor)
}

/// `pow(mixed $num, mixed $exponent): int|float|object`, php's
/// `pow_function` (`Z/Zend/zend_operators.c:1415`): the `**` operator's
/// own kernel through [`binary_op`], two ints to an int by squaring where
/// nothing overflows, a float otherwise, the operands made numbers first,
/// with php's warning for a leading-numeric string and its `TypeError`
/// for anything that is no number.
///
/// ```php
/// var_dump(pow(2, 62), pow(2, 63), pow(2, -1), pow(0, 0), pow("3", 2), pow(2.5, 2), pow(-2, 63), pow(null, 2), pow(true, 3), pow(-8, 1/3));
/// try { pow([1], 2); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { pow("abc", 2); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(pow("5x", 2));
/// ```
/// ```expect
/// int(4611686018427387904)
/// float(9.223372036854776E+18)
/// float(0.5)
/// int(1)
/// int(9)
/// float(6.25)
/// int(-9223372036854775808)
/// int(0)
/// int(1)
/// float(NAN)
/// Unsupported operand types: array ** int
/// Unsupported operand types: string ** int
///
/// Warning: A non-numeric value encountered in %s on line 5
/// int(25)
/// ```
///
/// A zero base under a negative exponent is `safe_pow`'s deprecation
/// before `INF`, whichever of the two is a float, as it is for `**`.
///
/// ```php
/// var_dump(pow(0, -1), pow(0.0, -0.5), pow(0, -0), pow(0, -0.0));
/// ```
/// ```expect
///
/// Deprecated: Power of base 0 and negative exponent is deprecated in %s on line 2
///
/// Deprecated: Power of base 0 and negative exponent is deprecated in %s on line 2
/// float(INF)
/// float(INF)
/// int(1)
/// float(1)
/// ```
#[php_function]
fn pow(
    req: &mut Request,
    num: Value,
    exponent: Value,
) -> Result<Owner<Raw<Union<Number, PhpObject>>>, Thrown> {
    let power = binary_op(req, BinaryOp::Pow, &num, &exponent)?;
    Ok(power.into_raw())
}

/// `exp(float $num): float`.
///
/// ```php
/// var_dump(exp(1), exp(0), exp(710));
/// ```
/// ```expect
/// float(2.718281828459045)
/// float(1)
/// float(INF)
/// ```
#[php_function]
fn exp(num: f64) -> f64 {
    num.exp()
}

/// `log(float $num, float $base = M_E): float`, php's `PHP_FUNCTION(log)`
/// (`Z/ext/standard/math.c:643`): the natural logarithm, or with a base
/// `log2`, `log10`, `NAN` for base 1, php's `ValueError` for a base at or
/// below zero, and `log(num) / log(base)` for the rest.
///
/// ```php
/// var_dump(log(M_E), log(8, 2), log(100, 10), log(1, 1), log(9, 3), log(0), log(-1), log(27, 3));
/// try { log(1, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// float(1)
/// float(3)
/// float(2)
/// float(NAN)
/// float(2)
/// float(-INF)
/// float(NAN)
/// float(3)
/// log(): Argument #2 ($base) must be greater than 0
/// ```
// php compares the base exactly.
#[allow(clippy::float_cmp)]
#[php_function]
fn log(
    req: &mut Request,
    num: f64,
    #[php(default = M_E)] base: Passed<f64>,
) -> Result<f64, Thrown> {
    let Passed::Given(base) = base else {
        return Ok(num.ln());
    };
    Ok(if base == 2.0 {
        num.log2()
    } else if base == 10.0 {
        num.log10()
    } else if base == 1.0 {
        f64::NAN
    } else if base <= 0.0 {
        return Err(arg_value_error(req, 2, "be greater than 0"));
    } else {
        num.ln() / base.ln()
    })
}

/// `log10(float $num): float`.
///
/// ```php
/// var_dump(log10(1000), log10(0.001), log10(-1));
/// ```
/// ```expect
/// float(3)
/// float(-3)
/// float(NAN)
/// ```
#[php_function]
fn log10(num: f64) -> f64 {
    num.log10()
}

/// `sqrt(float $num): float`.
///
/// ```php
/// var_dump(sqrt(16), sqrt(2), sqrt(-1), sqrt(INF));
/// ```
/// ```expect
/// float(4)
/// float(1.4142135623730951)
/// float(NAN)
/// float(INF)
/// ```
#[php_function]
fn sqrt(num: f64) -> f64 {
    num.sqrt()
}

/// `hypot(float $x, float $y): float`.
///
/// ```php
/// var_dump(hypot(3, 4), hypot(-5, 12), hypot(INF, NAN));
/// ```
/// ```expect
/// float(5)
/// float(13)
/// float(INF)
/// ```
#[php_function]
fn hypot(x: f64, y: f64) -> f64 {
    x.hypot(y)
}

/// `deg2rad(float $num): float`, `(num / 180) * M_PI` in php's order.
///
/// ```php
/// var_dump(deg2rad(180), deg2rad(45), deg2rad(-90));
/// ```
/// ```expect
/// float(3.141592653589793)
/// float(0.7853981633974483)
/// float(-1.5707963267948966)
/// ```
#[php_function]
fn deg2rad(num: f64) -> f64 {
    (num / 180.0) * std::f64::consts::PI
}

/// `rad2deg(float $num): float`, `(num / M_PI) * 180` in php's order.
///
/// ```php
/// var_dump(rad2deg(M_PI), rad2deg(1), rad2deg(M_PI_4));
/// ```
/// ```expect
/// float(180)
/// float(57.29577951308232)
/// float(45)
/// ```
#[php_function]
fn rad2deg(num: f64) -> f64 {
    (num / std::f64::consts::PI) * 180.0
}

/// `_php_math_basetozval` (`Z/ext/standard/math.c:791`): the digits of
/// `base` in the string, whitespace at either end and a `0x`, `0o` or
/// `0b` prefix of that base skipped, every other byte skipped behind the
/// 7.4 deprecation, an int until it would overflow and a float from then
/// on.
fn base_to_value(req: &mut Request, text: &[u8], base: u32) -> Number {
    let mut s = 0;
    let mut e = text.len();
    while s < e && is_space(text[s]) {
        s += 1;
    }
    while s < e && is_space(text[e - 1]) {
        e -= 1;
    }
    if e - s >= 2 && text[s] == b'0' {
        let prefix = text[s + 1].to_ascii_lowercase();
        if (base == 16 && prefix == b'x')
            || (base == 8 && prefix == b'o')
            || (base == 2 && prefix == b'b')
        {
            s += 2;
        }
    }
    let wide = i64::from(base);
    let cutoff = i64::MAX / wide;
    let cutlim = i64::MAX % wide;
    let mut num: i64 = 0;
    let mut fnum: f64 = 0.0;
    let mut float = false;
    let mut invalid = false;
    for &c in &text[s..e] {
        let digit = match c {
            b'0'..=b'9' => i64::from(c - b'0'),
            b'A'..=b'Z' => i64::from(c - b'A') + 10,
            b'a'..=b'z' => i64::from(c - b'a') + 10,
            _ => {
                invalid = true;
                continue;
            }
        };
        if digit >= wide {
            invalid = true;
            continue;
        }
        if !float {
            if num < cutoff || (num == cutoff && digit <= cutlim) {
                num = num * wide + digit;
                continue;
            }
            #[allow(clippy::cast_precision_loss)]
            {
                fnum = num as f64;
            }
            float = true;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            fnum = fnum * f64::from(base) + digit as f64;
        }
    }
    if invalid {
        deprecated(
            req,
            "Invalid characters passed for attempted conversion, these have been ignored",
        );
    }
    if float { Union::B(fnum) } else { Union::A(num) }
}

/// `bindec(string $binary_string): int|float`, php's
/// `PHP_FUNCTION(bindec)` over `_php_math_basetozval`.
///
/// ```php
/// var_dump(bindec("111"), bindec("0b101"), bindec(" 11 "), bindec(str_repeat("1", 64)));
/// var_dump(bindec("12"));
/// ```
/// ```expect
/// int(7)
/// int(5)
/// int(3)
/// float(1.8446744073709552E+19)
///
/// Deprecated: Invalid characters passed for attempted conversion, these have been ignored in %s on line 3
/// int(1)
/// ```
///
/// The white space at either end is C's `isspace` under the request's
/// `LC_CTYPE`, `\v` and `\f` among it, and under `C` not 0xA0.
///
/// ```php
/// var_dump(bindec("\v101\f"));
/// setlocale(LC_CTYPE, "C"); var_dump(bindec("\xa0101"));
/// ```
/// ```expect
/// int(5)
///
/// Deprecated: Invalid characters passed for attempted conversion, these have been ignored in %s on line 3
/// int(5)
/// ```
#[php_function]
fn bindec(req: &mut Request, binary_string: PhpString) -> Number {
    base_to_value(req, binary_string.as_bytes(), 2)
}

/// `hexdec(string $hex_string): int|float`, php's `PHP_FUNCTION(hexdec)`.
///
/// ```php
/// var_dump(hexdec("ff"), hexdec("0xFF"), hexdec("7fffffffffffffff"), hexdec("fffffffffffffffff"));
/// var_dump(hexdec("xyz1"));
/// ```
/// ```expect
/// int(255)
/// int(255)
/// int(9223372036854775807)
/// float(2.9514790517935283E+20)
///
/// Deprecated: Invalid characters passed for attempted conversion, these have been ignored in %s on line 3
/// int(1)
/// ```
#[php_function]
fn hexdec(req: &mut Request, hex_string: PhpString) -> Number {
    base_to_value(req, hex_string.as_bytes(), 16)
}

/// `octdec(string $octal_string): int|float`, php's `PHP_FUNCTION(octdec)`.
///
/// ```php
/// var_dump(octdec("777"), octdec("0o17"), octdec("0O17"), octdec("-7"));
/// ```
/// ```expect
/// Deprecated: Invalid characters passed for attempted conversion, these have been ignored in %s on line 2
/// int(511)
/// int(15)
/// int(15)
/// int(7)
/// ```
#[php_function]
fn octdec(req: &mut Request, octal_string: PhpString) -> Number {
    base_to_value(req, octal_string.as_bytes(), 8)
}

/// `_php_math_longtobase_pwr2`: the long's bits, unsigned, in the digits
/// of `2^bits`.
/// `_php_math_longtobase_pwr2` as the string it builds.
fn to_pwr2(req: &mut Request, num: i64, bits: u32) -> Result<Owner<PhpString>, Thrown> {
    PhpString::new(req, &long_to_pwr2(num, bits))
}

fn long_to_pwr2(value: i64, bits: u32) -> Vec<u8> {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut value = value.cast_unsigned();
    let mask = (1u64 << bits) - 1;
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[usize::try_from(value & mask).unwrap_or(0)]);
        value >>= bits;
        if value == 0 {
            break;
        }
    }
    out.reverse();
    out
}

/// `decbin(int $num): string`, php's `PHP_FUNCTION(decbin)`: the bits of
/// the long as unsigned, so a negative one is its two's complement.
///
/// ```php
/// var_dump(decbin(5), decbin(0), decbin(-1), decbin("12"));
/// ```
/// ```expect
/// string(3) "101"
/// string(1) "0"
/// string(64) "1111111111111111111111111111111111111111111111111111111111111111"
/// string(4) "1100"
/// ```
#[php_function]
fn decbin(req: &mut Request, num: i64) -> Result<Owner<PhpString>, Thrown> {
    to_pwr2(req, num, 1)
}

/// `decoct(int $num): string`, php's `PHP_FUNCTION(decoct)`.
///
/// ```php
/// var_dump(decoct(8), decoct(-1), decoct(0));
/// ```
/// ```expect
/// string(2) "10"
/// string(22) "1777777777777777777777"
/// string(1) "0"
/// ```
#[php_function]
fn decoct(req: &mut Request, num: i64) -> Result<Owner<PhpString>, Thrown> {
    to_pwr2(req, num, 3)
}

/// `dechex(int $num): string`, php's `PHP_FUNCTION(dechex)` and its
/// frameless arity 1.
///
/// ```php
/// var_dump(dechex(255), dechex(-1), dechex(PHP_INT_MAX), dechex(0));
/// ```
/// ```expect
/// string(2) "ff"
/// string(16) "ffffffffffffffff"
/// string(16) "7fffffffffffffff"
/// string(1) "0"
/// ```
#[php_function(frameless(1))]
fn dechex(req: &mut Request, num: i64) -> Result<Owner<PhpString>, Thrown> {
    to_pwr2(req, num, 4)
}

/// `_php_math_zvaltobase`: an int in the digits of `base`, and a float
/// floored and taken apart digit by digit with `fmod`, php's
/// `ValueError` for an infinite one.
fn value_to_base(req: &mut Request, value: Number, base: u32) -> Result<Vec<u8>, Thrown> {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    if let Union::B(f) = value {
        let mut fvalue = f.floor();
        if fvalue.is_infinite() {
            return Err(value_error(
                req,
                &format!("An infinite value cannot be converted to base {base}"),
            ));
        }
        let mut out = Vec::new();
        loop {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let digit = (fvalue % f64::from(base)) as usize;
            out.push(DIGITS[digit.min(35)]);
            fvalue /= f64::from(base);
            if out.len() >= 400 || fvalue.abs() < 1.0 {
                break;
            }
        }
        out.reverse();
        return Ok(out);
    }
    let Union::A(value) = value else {
        return Ok(Vec::new());
    };
    let mut value = value.cast_unsigned();
    let base = u64::from(base);
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[usize::try_from(value % base).unwrap_or(0)]);
        value /= base;
        if value == 0 {
            break;
        }
    }
    out.reverse();
    Ok(out)
}

/// `base_convert(string $num, int $from_base, int $to_base): string`,
/// php's `PHP_FUNCTION(base_convert)` (`Z/ext/standard/math.c:1066`): the
/// digits read in the one base, a float past the long's range, and
/// written in the other; each base held between 2 and 36.
///
/// ```php
/// var_dump(base_convert("ff", 16, 2), base_convert("zz", 36, 10), base_convert("-255", 10, 16), base_convert("ffffffffffffffffffff", 16, 36));
/// var_dump(base_convert("1e10", 10, 16));
/// try { base_convert("1", 1, 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { base_convert("1", 2, 37); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Deprecated: Invalid characters passed for attempted conversion, these have been ignored in %s on line 2
/// string(8) "11111111"
/// string(4) "1295"
/// string(2) "ff"
/// string(16) "5gv2rma270w8w404"
///
/// Deprecated: Invalid characters passed for attempted conversion, these have been ignored in %s on line 3
/// string(2) "6e"
/// base_convert(): Argument #2 ($from_base) must be between 2 and 36 (inclusive)
/// base_convert(): Argument #3 ($to_base) must be between 2 and 36 (inclusive)
/// ```
#[php_function]
fn base_convert(
    req: &mut Request,
    num: PhpString,
    from_base: i64,
    to_base: i64,
) -> Result<Owner<PhpString>, Thrown> {
    let number = num.as_bytes().to_vec();
    let (from, to) = (from_base, to_base);
    if !(2..=36).contains(&from) {
        return Err(arg_value_error(req, 2, "be between 2 and 36 (inclusive)"));
    }
    if !(2..=36).contains(&to) {
        return Err(arg_value_error(req, 3, "be between 2 and 36 (inclusive)"));
    }
    let from = u32::try_from(from).unwrap_or(10);
    let to = u32::try_from(to).unwrap_or(10);
    let value = base_to_value(req, &number, from);
    let digits = value_to_base(req, value, to)?;
    PhpString::new(req, &digits)
}

/// `number_format(float $num, int $decimals = 0, ?string $decimal_separator = ".", ?string $thousands_separator = ","): string`, php's `PHP_FUNCTION(number_format)` (`Z/ext/standard/math.c:1394`):
/// an int, or a float too large to carry a fraction, through
/// `_php_math_number_format_long`, and any other float rounded half up by
/// `_php_math_round` and printed `%.*F`, then grouped by threes.
///
/// ```php
/// var_dump(number_format(1234.5678), number_format(1234.5678, 2), number_format(1234.5678, 2, ',', '.'), number_format(-0.4), number_format(-1234.567, 2, '', ' '), number_format(0.5), number_format(1e15, 2), number_format(123456789, -3), number_format(-5, 2), number_format(1.005, 2), number_format(12345678.9, 1, '.', ''), number_format(1e80), number_format(INF), number_format(-1234.5, 1, '', ''), number_format(0.285, 2), number_format(1234, 2, null, null));
/// ```
/// ```expect
/// string(5) "1,235"
/// string(8) "1,234.57"
/// string(8) "1.234,57"
/// string(1) "0"
/// string(8) "-1 23457"
/// string(1) "1"
/// string(24) "1,000,000,000,000,000.00"
/// string(11) "123,457,000"
/// string(5) "-5.00"
/// string(4) "1.01"
/// string(10) "12345678.9"
/// string(107) "100,000,000,000,000,000,026,609,864,708,367,276,537,402,401,181,200,809,098,131,977,453,489,758,916,313,088"
/// string(3) "inf"
/// string(6) "-12345"
/// string(4) "0.29"
/// string(8) "1,234.00"
/// ```
#[php_function]
fn number_format(
    req: &mut Request,
    num: Raw<f64>,
    #[php(default = 0)] decimals: Raw<i64>,
    #[php(default = ".")] decimal_separator: Raw<Option<PhpString>>,
    #[php(default = ",")] thousands_separator: Raw<Option<PhpString>>,
) -> Result<Owner<PhpString>, Thrown> {
    // `Z_PARAM_NUMBER` under the stub's `float`, then the rest in order.
    let num = coerce_param(
        req,
        &num.value(),
        &Type::Union(vec![Type::Int, Type::Float]),
        1,
    )?;
    let decimals = decimals.hold(req)?;
    let separator = |req: &mut Request,
                     given: Raw<Option<PhpString>>,
                     default: &[u8]|
     -> Result<Vec<u8>, Thrown> {
        Ok(match given.hold(req)? {
            Some(bytes) => bytes.as_bytes().to_vec(),
            None => default.to_vec(),
        })
    };
    let point = separator(req, decimal_separator, b".")?;
    let thousands = separator(req, thousands_separator, b",")?;
    let text = match num.as_ref() {
        ValueRef::Int(n) => format_long(n, decimals, &point, &thousands),
        ValueRef::Float(f)
            if (f >= 4_503_599_627_370_496.0 || f <= -4_503_599_627_370_496.0) && fits_long(f) =>
        {
            #[allow(clippy::cast_possible_truncation)]
            format_long(f as i64, decimals, &point, &thousands)
        }
        ValueRef::Float(f) => {
            let dec =
                i32::try_from(decimals).unwrap_or(if decimals < 0 { i32::MIN } else { i32::MAX });
            format_double(f, dec, &point, &thousands)
        }
        _ => Vec::new(),
    };
    PhpString::new(req, &text)
}

/// `ZEND_DOUBLE_FITS_LONG`.
fn fits_long(d: f64) -> bool {
    (-9_223_372_036_854_775_808.0..9_223_372_036_854_775_808.0).contains(&d)
}

/// The digits before the point grouped by threes from the right.
fn grouped(digits: &[u8], thousands: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(digits.len() + digits.len() / 3 * thousands.len());
    for (at, digit) in digits.iter().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.extend_from_slice(thousands);
        }
        out.push(*digit);
    }
    out
}

/// `_php_math_number_format_long`: the magnitude rounded half up to the
/// negative places asked for, grouped, and the zeros of positive places.
fn format_long(num: i64, dec: i64, point: &[u8], thousands: &[u8]) -> Vec<u8> {
    let mut negative = num < 0;
    let mut magnitude = num.unsigned_abs();
    if dec < 0 {
        if dec < -19 {
            magnitude = 0;
        } else {
            let power = 10u64.pow(u32::try_from(-dec).unwrap_or(0));
            let rest = magnitude % power;
            magnitude /= power;
            magnitude = if rest >= power / 2 {
                magnitude.wrapping_mul(power).wrapping_add(power)
            } else {
                magnitude.wrapping_mul(power)
            };
        }
        if magnitude == 0 {
            negative = false;
        }
    }
    let digits = Decimal::unsigned(magnitude);
    let mut out = Vec::new();
    if negative {
        out.push(b'-');
    }
    out.extend(grouped(digits.as_bytes(), thousands));
    if dec > 0 {
        out.extend_from_slice(point);
        out.extend(std::iter::repeat_n(b'0', usize::try_from(dec).unwrap_or(0)));
    }
    out
}

/// `_php_math_number_format_ex`: the magnitude rounded half up to `dec`
/// places, printed `%.*F`, the text as it is where it is no number
/// (`INF`, `NAN`), the sign dropped where the rounding made it zero, and
/// the digits grouped before the point.
// php compares the rounded value with zero exactly.
#[allow(clippy::float_cmp)]
fn format_double(num: f64, dec: i32, point: &[u8], thousands: &[u8]) -> Vec<u8> {
    let mut negative = num < 0.0;
    let d = math_round(num.abs(), dec, PHP_ROUND_HALF_UP);
    let dec = usize::try_from(dec.max(0)).unwrap_or(0);
    let text = if d.is_finite() {
        format!("{d:.dec$}")
    } else if d.is_nan() {
        String::from("nan")
    } else {
        String::from("inf")
    };
    let bytes = text.into_bytes();
    if !bytes.first().is_some_and(u8::is_ascii_digit) {
        return bytes;
    }
    if negative && d == 0.0 {
        negative = false;
    }
    let (integral, fraction) = match bytes.iter().position(|b| *b == b'.') {
        Some(at) if dec > 0 => (&bytes[..at], &bytes[at + 1..]),
        _ => (&bytes[..], &[][..]),
    };
    let mut out = Vec::new();
    if negative {
        out.push(b'-');
    }
    out.extend(grouped(integral, thousands));
    if dec > 0 {
        out.extend_from_slice(point);
        out.extend_from_slice(fraction);
        out.extend(std::iter::repeat_n(
            b'0',
            dec.saturating_sub(fraction.len()),
        ));
    }
    out
}

/// `fmod(float $num1, float $num2): float`, C's `fmod`.
///
/// ```php
/// var_dump(fmod(10, 3), fmod(-10, 3), fmod(5.5, 1.5), fmod(1, 0));
/// ```
/// ```expect
/// float(1)
/// float(-1)
/// float(1)
/// float(NAN)
/// ```
#[php_function]
fn fmod(num1: f64, num2: f64) -> f64 {
    num1 % num2
}

/// `fdiv(float $num1, float $num2): float`, IEEE 754 division, a zero
/// divisor answering an infinity or NAN.
///
/// ```php
/// var_dump(fdiv(1, 0), fdiv(-1, 0), fdiv(0, 0), fdiv(10, 4));
/// ```
/// ```expect
/// float(INF)
/// float(-INF)
/// float(NAN)
/// float(2.5)
/// ```
#[php_function]
fn fdiv(num1: f64, num2: f64) -> f64 {
    num1 / num2
}

/// `fpow(float $num, float $exponent): float`, C's `pow`.
///
/// ```php
/// var_dump(fpow(2, 0.5), fpow(-8, 1/3), fpow(0, -1), fpow(2, 10));
/// ```
/// ```expect
/// float(1.4142135623730951)
/// float(NAN)
/// float(INF)
/// float(1024)
/// ```
#[php_function]
fn fpow(num: f64, exponent: f64) -> f64 {
    num.powf(exponent)
}

#[allow(dead_code)]
fn unused(req: &mut Request) {
    docref(req, Level::Warning, "");
}

/// `php_intpow10`: an exact power of ten from the table where it has
/// one, `pow` otherwise.
fn intpow10(power: i32) -> f64 {
    const POWERS: [f64; 23] = [
        1e0, 1e1, 1e2, 1e3, 1e4, 1e5, 1e6, 1e7, 1e8, 1e9, 1e10, 1e11, 1e12, 1e13, 1e14, 1e15, 1e16,
        1e17, 1e18, 1e19, 1e20, 1e21, 1e22,
    ];
    match usize::try_from(power) {
        Ok(at) if at < POWERS.len() => POWERS[at],
        _ => 10f64.powi(power),
    }
}

fn basic_edge(integral: f64, exponent: f64, places: i32) -> f64 {
    let half = integral + 0.5f64.copysign(integral);
    if places > 0 {
        (half / exponent).abs()
    } else {
        (half * exponent).abs()
    }
}

fn zero_edge(integral: f64, exponent: f64, places: i32) -> f64 {
    if places > 0 {
        (integral / exponent).abs()
    } else {
        (integral * exponent).abs()
    }
}

/// `php_round_helper`: `integral` moved one away from zero, or toward
/// the side `mode` names, where `value` passes its edge.
// php compares exactly, not within a margin.
#[allow(clippy::float_cmp)]
fn round_helper(integral: f64, value: f64, exponent: f64, places: i32, mode: i32) -> f64 {
    let away = integral + 1f64.copysign(integral);
    let value_abs = value.abs();
    match mode {
        PHP_ROUND_HALF_DOWN => {
            if value_abs > basic_edge(integral, exponent, places) {
                away
            } else {
                integral
            }
        }
        PHP_ROUND_CEILING => {
            if value > 0.0 && value_abs > zero_edge(integral, exponent, places) {
                integral + 1.0
            } else {
                integral
            }
        }
        PHP_ROUND_FLOOR => {
            if value < 0.0 && value_abs > zero_edge(integral, exponent, places) {
                integral - 1.0
            } else {
                integral
            }
        }
        PHP_ROUND_TOWARD_ZERO => integral,
        PHP_ROUND_AWAY_FROM_ZERO => {
            if value_abs > zero_edge(integral, exponent, places) {
                away
            } else {
                integral
            }
        }
        PHP_ROUND_HALF_EVEN | PHP_ROUND_HALF_ODD => {
            let edge = basic_edge(integral, exponent, places);
            if value_abs > edge {
                away
            } else if value_abs == edge {
                let even = integral % 2.0 == 0.0;
                if even == (mode == PHP_ROUND_HALF_ODD) {
                    away
                } else {
                    integral
                }
            } else {
                integral
            }
        }
        _ => {
            if value_abs >= basic_edge(integral, exponent, places) {
                away
            } else {
                integral
            }
        }
    }
}

/// `_php_math_round` (`Z/ext/standard/math.c:161`): `value` to `places`
/// decimal places in `mode`, the integral part corrected where the
/// scaling lost the last unit, a value past sixteen digits handed back
/// as it is, and the scaled value divided back, or for more than 22
/// places printed and read back with `zend_strtod`. What `range` sizes a
/// float range with, rounding the count of steps half up, so a count
/// that division leaves a hair under or over a whole number is that
/// number.
///
/// ```php
/// var_dump(count(range(0, 1, 0.1)), count(range(0.1, 0.5, 0.1)), count(range(0, 0.3, 0.1)), range(0, 1, 0.3));
/// ```
/// ```expect
/// int(11)
/// int(5)
/// int(3)
/// array(4) {
///   [0]=>
///   float(0)
///   [1]=>
///   float(0.3)
///   [2]=>
///   float(0.6)
///   [3]=>
///   float(0.8999999999999999)
/// }
/// ```
// php compares exactly, not within a margin.
#[allow(clippy::float_cmp)]
pub(crate) fn math_round(value: f64, places: i32, mode: i32) -> f64 {
    if !value.is_finite() || value == 0.0 {
        return value;
    }
    let places = places.max(i32::MIN + 1);
    let exponent = intpow10(places.abs());
    let scaled = if places > 0 {
        value * exponent
    } else {
        value / exponent
    };
    let (mut tmp, tmp2) = if value >= 0.0 {
        let tmp = scaled.floor();
        (tmp, tmp + 1.0)
    } else {
        let tmp = scaled.ceil();
        (tmp, tmp - 1.0)
    };
    let back = if places > 0 {
        tmp2 / exponent
    } else {
        tmp2 * exponent
    };
    if back == value {
        tmp = tmp2;
    }
    if tmp.abs() >= 1e16 {
        return value;
    }
    tmp = round_helper(tmp, value, exponent, places, mode);
    if places.abs() < 23 {
        if places > 0 {
            tmp / exponent
        } else {
            tmp * exponent
        }
    } else {
        // `snprintf(buf, 39, "%15fe%d", tmp_value, -places)` read back.
        let text = format!("{tmp:15.6}e{}", -i64::from(places));
        match text.trim().parse::<f64>() {
            Ok(read) if read.is_finite() => read,
            _ => value,
        }
    }
}

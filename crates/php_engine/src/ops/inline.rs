//! The pairs php's handlers take inline before they call a kernel
//! (design §5.8): two operands that are numbers as they stand, `IS_LONG`
//! or `IS_DOUBLE`, which `ZEND_ADD` and its kin compute in the handler
//! (`Z/Zend/zend_vm_def.h:50-91`, `fast_long_add_function`), and a
//! condition that is a scalar, which `ZEND_JMPZ` tests there. Each rule
//! has its one home here: the kernel in [`crate::ops`] takes the same
//! arithmetic for the numbers it converted, so a handler's inline path
//! and the kernel cannot disagree. Nothing here warns, throws or reads
//! the request; whatever would is `None`, and the kernel serves it.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::cmp::Ordering;

use php_heap::{Arr, Key, Number, Scalar, Zval, ZvalType};
use php_unit::Opcode;

use crate::api::values::Value;

/// The three operators php computes inline over any two numbers, which
/// [`computed`] answers for every pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arith {
    Add,
    Sub,
    Mul,
}

/// The two shifts, `ZEND_SL` and `ZEND_SR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Shift {
    Left,
    Right,
}

/// The three bitwise operators, `ZEND_BW_OR`, `ZEND_BW_XOR` and
/// `ZEND_BW_AND`: the one thing `bitwise_or_function`,
/// `bitwise_xor_function` and `bitwise_and_function` differ by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bitwise {
    Or,
    Xor,
    And,
}

/// Every binary operator with an inline pair, as the handler of its
/// opcode and a compound assignment over it take it: [`Arith`]'s three
/// over any two numbers, `/` over two numbers it can divide
/// ([`quotient`]), `%` over two longs with a divisor that is not zero
/// ([`remainder`]), a shift over two longs with a count in `0..64`
/// ([`shifted`]), and a bitwise operator over two longs
/// ([`Bitwise::long`]). What a rule refuses is the kernel's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Binary {
    Arith(Arith),
    Div,
    Mod,
    Shift(Shift),
    Bitwise(Bitwise),
}

/// The operator of a compound assignment, `ZEND_ASSIGN_OP`'s and
/// `ZEND_ASSIGN_DIM_OP`'s `extended_value`, where it is one the binary
/// opcodes compute inline ([`Binary`]); None for every other operator,
/// `.`, `**` and `??` among them, which is the kernel's. Every compound
/// assignment's inline path takes what this names.
///
/// ```php
/// $a = [3, 3, 3, 3, 3, 6, 6, 6, 6, 6]; $a[0] += 2; $a[1] -= 2; $a[2] *= 2; $a[3] /= 2; $a[4] %= 2;
/// $a[5] <<= 2; $a[6] >>= 1; $a[7] |= 1; $a[8] ^= 3; $a[9] &= 3;
/// echo implode(",", $a), "\n";
/// $x = 7; $x /= 7; $y = PHP_INT_MAX; $y += 1; $z = 2.5; $z *= 2; $w = -7; $w %= -1; $v = 5; $v .= 1; $u = 2; $u **= 3;
/// var_dump($x, $y, $z, $w, $v, $u);
/// ```
/// ```expect
/// 5,1,6,1.5,1,24,3,7,5,2
/// int(1)
/// float(9.223372036854776E+18)
/// float(5)
/// int(0)
/// string(2) "51"
/// int(8)
/// ```
///
/// A variable bound to a reference a typed property holds is the
/// kernel's, which holds what the operator came to to the property's
/// type under the frame's regime:
///
/// ```php
/// declare(strict_types=1);
/// class T { public int $i = 1; }
/// $t = new T; $r = &$t->i; $r += 2; $r <<= 1;
/// try { $r += 1.5; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { $r /= 4; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($t->i);
/// ```
/// ```expect
/// Cannot assign float to reference held by property T::$i of type int
/// Cannot assign float to reference held by property T::$i of type int
/// int(6)
/// ```
#[inline]
pub(crate) fn compound(extended_value: u32) -> Option<Binary> {
    // php's numbers for the opcodes, matched as the field holds them.
    const DIV: u32 = Opcode::Div as u32;
    const MOD: u32 = Opcode::Mod as u32;
    const SL: u32 = Opcode::Sl as u32;
    const SR: u32 = Opcode::Sr as u32;
    const BW_OR: u32 = Opcode::BwOr as u32;
    const BW_XOR: u32 = Opcode::BwXor as u32;
    const BW_AND: u32 = Opcode::BwAnd as u32;
    Some(match extended_value {
        ADD => Binary::Arith(Arith::Add),
        SUB => Binary::Arith(Arith::Sub),
        MUL => Binary::Arith(Arith::Mul),
        DIV => Binary::Div,
        MOD => Binary::Mod,
        SL => Binary::Shift(Shift::Left),
        SR => Binary::Shift(Shift::Right),
        BW_OR => Binary::Bitwise(Bitwise::Or),
        BW_XOR => Binary::Bitwise(Bitwise::Xor),
        BW_AND => Binary::Bitwise(Bitwise::And),
        _ => return None,
    })
}

/// `ZEND_ADD`'s, `ZEND_SUB`'s and `ZEND_MUL`'s numbers, as a compound
/// assignment's `extended_value` holds them.
const ADD: u32 = Opcode::Add as u32;
const SUB: u32 = Opcode::Sub as u32;
const MUL: u32 = Opcode::Mul as u32;

/// [`binary`] under the operator of a compound assignment as its
/// `extended_value` holds it: [`Arith`]'s three matched on the field
/// itself, the operator then known where [`binary`] branches on it, and
/// every other operator through [`compound`], so `$s += $i` makes one
/// compare of the field where [`compound`] builds a [`Binary`] the pair
/// then branches on again. [`binary`] and not [`arith`], whose tests
/// put two longs and two doubles first: an accumulation is the mixed
/// pair, a double and a long, which [`binary`] reaches in fewer (#3273).
/// None where [`compound`] names no operator or the rule leaves the
/// pair to the kernel.
///
/// ```php
/// function c($a, $b) { $x = $a; $x += $b; $y = $a; $y -= $b; $z = $a; $z *= $b; $w = $a; $w /= $b; $t = $a; $t .= $b; echo implode(" ", [$x, $y, $z, $w, $t]), "\n"; }
/// c(7, 2); c(1.5, 2); c(PHP_INT_MAX, 1); c(-3, 0.5);
/// ```
/// ```expect
/// 9 5 14 3.5 72
/// 3.5 -0.5 3 0.75 1.52
/// 9.2233720368548E+18 9223372036854775806 9223372036854775807 9223372036854775807 92233720368547758071
/// -2.5 -3.5 -1.5 -6 -30.5
/// ```
#[inline(always)] // as `computed`
pub(crate) fn compounded(extended_value: u32, a: Zval, b: Zval) -> Option<Zval> {
    match extended_value {
        ADD => binary(Binary::Arith(Arith::Add), a, b),
        SUB => binary(Binary::Arith(Arith::Sub), a, b),
        MUL => binary(Binary::Arith(Arith::Mul), a, b),
        other => binary(compound(other)?, a, b),
    }
}

/// A zval that is a number as it stands, nothing converted.
#[inline(always)] // a test of the type byte, on every inline path
fn number(z: Zval) -> Option<Number> {
    match z.kind() {
        ZvalType::Long => z.as_long().map(Number::Long),
        ZvalType::Double => z.as_double().map(Number::Double),
        _ => None,
    }
}

/// The number a value is as it stands, nothing converted and a
/// reference not followed.
#[inline(always)] // as `number`
fn value_number(v: &Value) -> Option<Number> {
    v.as_int()
        .map(Number::Long)
        .or_else(|| v.as_float().map(Number::Double))
}

/// The value a number is, which a kernel hands back for what a rule
/// computed over the numbers it converted.
///
/// ```php
/// $six = "6"; $seven = "7.0";
/// var_dump($six / 3, $seven / 2, $six / "4", 9 / $six);
/// ```
/// ```expect
/// int(2)
/// float(3.5)
/// float(1.5)
/// float(1.5)
/// ```
#[inline(always)] // as `number`
pub(crate) fn number_value(n: Number) -> Value {
    match n {
        Number::Long(n) => Value::int(n),
        Number::Double(d) => Value::float(d),
    }
}

impl Arith {
    /// The operator over two longs, none where it overflowed.
    #[inline]
    fn longs(self, x: i64, y: i64) -> Option<i64> {
        match self {
            Arith::Add => x.checked_add(y),
            Arith::Sub => x.checked_sub(y),
            Arith::Mul => x.checked_mul(y),
        }
    }

    /// The operator over two doubles.
    #[inline]
    fn doubles(self, x: f64, y: f64) -> f64 {
        match self {
            Arith::Add => x + y,
            Arith::Sub => x - y,
            Arith::Mul => x * y,
        }
    }
}

/// Two longs whose sum, difference or product overflowed, as doubles: the
/// rare arm, as php's `UNEXPECTED` marks it, kept out of line so the
/// long pair branches to it rather than computing a double beside every
/// long and selecting it away.
#[cold]
#[inline(never)]
fn overflowed(op: Arith, x: i64, y: i64) -> Number {
    Number::Double(op.doubles(Number::Long(x).double(), Number::Long(y).double()))
}

/// `add_function`'s, `sub_function`'s and `mul_function`'s numeric type
/// pairs as one match, in the order `ZEND_ADD` and `ZEND_MUL` test them:
/// two longs as longs, with the double `fast_long_add_function`,
/// `fast_long_sub_function` and `ZEND_SIGNED_MULTIPLY_LONG` compute
/// where the long overflowed, two doubles as they are, and a mixed pair
/// with its long converted.
#[inline(always)] // the arithmetic of an inline path
fn computed(op: Arith, a: Number, b: Number) -> Number {
    if let (Number::Long(x), Number::Long(y)) = (a, b) {
        return match op.longs(x, y) {
            Some(n) => Number::Long(n),
            None => overflowed(op, x, y),
        };
    }
    if let (Number::Double(x), Number::Double(y)) = (a, b) {
        return Number::Double(op.doubles(x, y));
    }
    Number::Double(op.doubles(a.double(), b.double()))
}

/// `div_function_base` (`Z/Zend/zend_operators.c:1454`) over two
/// numbers: two longs as a long where the division comes out whole and
/// a double where it does not, and any pair with a double as doubles.
/// None for a divisor of zero, `0`, `0.0` and `-0.0` alike, and for
/// `PHP_INT_MIN / -1`, which php computes as a double to keep the long
/// from overflowing; [`crate::ops::div`] serves both.
///
/// ```php
/// function d($a, $b) { try { var_dump($a / $b); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; } }
/// d(6, 3); d(7, 2); d(-7, 2); d(0, -1); d(7.5, 2); d(1, 0.5); d(INF, 2); d("9", 3);
/// d(1, 0); d(1.5, 0.0); d(1, -0.0); d(PHP_INT_MIN, -1); d(PHP_INT_MIN, 1);
/// ```
/// ```expect
/// int(2)
/// float(3.5)
/// float(-3.5)
/// int(0)
/// float(3.75)
/// float(2)
/// float(INF)
/// int(3)
/// Division by zero
/// Division by zero
/// Division by zero
/// float(9.223372036854776E+18)
/// int(-9223372036854775808)
/// ```
#[inline(always)] // the arithmetic of an inline path
pub(crate) fn quotient(a: Number, b: Number) -> Option<Number> {
    match (a, b) {
        (Number::Long(_), Number::Long(0)) | (Number::Long(i64::MIN), Number::Long(-1)) => None,
        (Number::Long(x), Number::Long(y)) if x % y == 0 => Some(Number::Long(x / y)),
        _ => {
            let y = b.double();
            (y != 0.0).then(|| Number::Double(a.double() / y))
        }
    }
}

/// `ZEND_MOD`'s inline pair (`Z/Zend/zend_vm_def.h:251`) over two longs:
/// the remainder, and zero for a divisor of `-1`, which keeps
/// `PHP_INT_MIN % -1` from overflowing as php's own case does. None for
/// a divisor of zero, which php hands to `zend_mod_by_zero_helper` and
/// [`crate::ops::r#mod`] serves.
///
/// ```php
/// function m($a, $b) { try { var_dump($a % $b); } catch (DivisionByZeroError $e) { echo $e->getMessage(), "\n"; } }
/// m(7, 3); m(-7, 3); m(7, -3); m(5, -1); m(PHP_INT_MIN, -1); m(PHP_INT_MIN, 3); m(5, 0); m(7.5, 2);
/// ```
/// ```expect
/// int(1)
/// int(-1)
/// int(1)
/// int(0)
/// int(0)
/// int(-2)
/// Modulo by zero
///
/// Deprecated: Implicit conversion from float 7.5 to int loses precision in %s on line 2
/// int(1)
/// ```
#[inline(always)] // the arithmetic of an inline path
pub(crate) fn remainder(a: i64, b: i64) -> Option<i64> {
    match b {
        0 => None,
        -1 => Some(0),
        _ => Some(a % b),
    }
}

/// `ZEND_SL`'s and `ZEND_SR`'s inline pair (`Z/Zend/zend_vm_def.h:271-311`)
/// over two longs: the shift by a count in `0..64`, as
/// [`php_heap::shift_left`] and [`php_heap::shift_right`] compute it.
/// None for a negative count, php's `ArithmeticError`, and for a count
/// of 64 or more, which php's handler hands to `shift_left_function`;
/// [`crate::ops::shift_left`] and [`crate::ops::shift_right`] serve both.
///
/// ```php
/// function s($a, $b) { try { echo $a << $b, " ", $a >> $b, "\n"; } catch (ArithmeticError $e) { echo $e->getMessage(), "\n"; } }
/// s(1, 0); s(1, 63); s(-8, 1); s(PHP_INT_MAX, 1); s(1, 64); s(-1, 64); s(-1, 65); s(1, -1); s(-1, PHP_INT_MIN);
/// ```
/// ```expect
/// 1 1
/// -9223372036854775808 0
/// -16 -4
/// -2 4611686018427387903
/// 0 0
/// 0 -1
/// 0 -1
/// Bit shift by negative number
/// Bit shift by negative number
/// ```
#[inline(always)] // the arithmetic of an inline path
pub(crate) fn shifted(op: Shift, a: i64, b: i64) -> Option<i64> {
    let by = u64::try_from(b).ok().filter(|by| *by < 64)?;
    Some(match op {
        Shift::Left => php_heap::shift_left(a, by),
        Shift::Right => php_heap::shift_right(a, by),
    })
}

impl Bitwise {
    /// `ZEND_BW_OR`'s, `ZEND_BW_XOR`'s and `ZEND_BW_AND`'s inline pair
    /// (`Z/Zend/zend_vm_def.h:824-940`), the operator over two longs,
    /// which is also what [`crate::ops::bitwise_or`] and its kin come to
    /// over the longs they converted.
    ///
    /// ```php
    /// function b($a, $b) { echo $a | $b, " ", $a ^ $b, " ", $a & $b, "\n"; }
    /// b(6, 3); b(-1, PHP_INT_MIN); b(0, 0); b(6, 3.0); b("6", 3);
    /// ```
    /// ```expect
    /// 7 5 2
    /// -1 9223372036854775807 -9223372036854775808
    /// 0 0 0
    /// 7 5 2
    /// 7 5 2
    /// ```
    #[inline(always)] // the arithmetic of an inline path
    pub(crate) const fn long(self, left: i64, right: i64) -> i64 {
        match self {
            Bitwise::Or => left | right,
            Bitwise::Xor => left ^ right,
            Bitwise::And => left & right,
        }
    }
}

/// What `op` comes to over two numbers, by its rule, or None where the
/// rule leaves the pair to the kernel.
#[inline(always)] // the arithmetic of an inline path
fn binary_numbers(op: Binary, a: Number, b: Number) -> Option<Number> {
    // The type pair first, then the operator, as php's handlers test.
    if let (Number::Long(x), Number::Long(y)) = (a, b) {
        return match op {
            Binary::Arith(op) => Some(computed(op, a, b)),
            Binary::Div => quotient(a, b),
            Binary::Mod => remainder(x, y).map(Number::Long),
            Binary::Shift(op) => shifted(op, x, y).map(Number::Long),
            Binary::Bitwise(op) => Some(Number::Long(op.long(x, y))),
        };
    }
    match op {
        Binary::Arith(op) => Some(computed(op, a, b)),
        Binary::Div => quotient(a, b),
        Binary::Mod | Binary::Shift(_) | Binary::Bitwise(_) => None,
    }
}

/// A binary opcode's inline pair over two zvals as they stand, and a
/// compound assignment's: what `op` comes to over two numbers, by its
/// rule ([`Binary`]), and None for everything else.
///
/// ```php
/// $max = PHP_INT_MAX; $one = 1; $half = 1.5; $two = -2;
/// var_dump($max + $one, $max / $two, $half / $two, $max % $two, $two << $one, $two >> $one, $two | $one, $two ^ $one, $two & $one);
/// ```
/// ```expect
/// float(9.223372036854776E+18)
/// float(-4.611686018427388E+18)
/// float(-0.75)
/// int(1)
/// int(-4)
/// int(-1)
/// int(-1)
/// int(-1)
/// int(0)
/// ```
#[inline(always)] // as `computed`
pub(crate) fn binary(op: Binary, a: Zval, b: Zval) -> Option<Zval> {
    binary_numbers(op, number(a)?, number(b)?).map(number_zval)
}

/// [`binary`] for [`Arith`]'s three, which every pair of numbers meets:
/// the inline pair `ZEND_ADD`, `ZEND_SUB` and `ZEND_MUL` are
/// monomorphised over (design §5.8), with the operator known where it
/// is written.
///
/// ```php
/// $max = PHP_INT_MAX; $one = 1; $half = 1.5; $two = -2;
/// var_dump($max + $one, $max - $two, $max * $two, $half + $one, $one - $half, $half * $half, $one + $two);
/// ```
/// ```expect
/// float(9.223372036854776E+18)
/// float(9.223372036854776E+18)
/// float(-1.8446744073709552E+19)
/// float(2.5)
/// float(-0.5)
/// float(2.25)
/// int(-1)
/// ```
///
/// Two doubles are computed as they stand and a mixed pair with its long
/// converted, so a signed zero, an infinity and a NAN come out as php's:
///
/// ```php
/// $nz = -0.0; $one = 1.0; $inf = INF; $zero = 0.0; $nan = NAN; $i = 1; $half = 0.5; $max = PHP_INT_MAX;
/// var_dump($nz * $one, $inf * $zero, $nan + $i, $i * $half, $max + $i);
/// ```
/// ```expect
/// float(-0)
/// float(NAN)
/// float(NAN)
/// float(0.5)
/// float(9.223372036854776E+18)
/// ```
#[inline(always)] // as `computed`
pub(crate) fn arith(op: Arith, a: Zval, b: Zval) -> Option<Zval> {
    numbers(
        a,
        b,
        |x, y| match op.longs(x, y) {
            Some(n) => Zval::long(n),
            None => number_zval(overflowed(op, x, y)),
        },
        |x, y| Zval::double(op.doubles(x, y)),
    )
}

/// Two zvals that are numbers as they stand, each pair served where its
/// test lands, as php's handlers test them: two longs first, as
/// `ZEND_ADD`'s `EXPECTED` pair is, then two doubles, then a mixed pair
/// with its long converted as [`Number::double`] converts it. Each pair
/// is one test of both type bytes (`&`, not `&&`, so the two compares
/// meet in one branch), and no [`Number`] is built for a pair to be
/// matched on again. `longs` serves two longs and `doubles` every other
/// pair; None where either is no number.
#[cfg_attr(not(debug_assertions), inline(always))] // the type tests of an inline path
#[cfg_attr(debug_assertions, inline(never))] // a debug `serve_frame` holds every inlined arm's locals
#[allow(clippy::needless_bitwise_bool)] // the two compares meet in one branch, measured (#3273)
fn numbers<R>(
    a: Zval,
    b: Zval,
    longs: impl FnOnce(i64, i64) -> R,
    doubles: impl FnOnce(f64, f64) -> R,
) -> Option<R> {
    let (ka, kb) = (a.kind(), b.kind());
    if (ka == ZvalType::Long) & (kb == ZvalType::Long) {
        return Some(longs(a.as_long()?, b.as_long()?));
    }
    if (ka == ZvalType::Double) & (kb == ZvalType::Double) {
        return Some(doubles(a.as_double()?, b.as_double()?));
    }
    Some(doubles(number(a)?.double(), number(b)?.double()))
}

/// The step of `ZEND_PRE_INC`, `ZEND_PRE_DEC` and their `POST_` pair over
/// a zval as it stands: `+ 1` or `- 1` as [`arith`] computes it, with the
/// same overflow (`fast_long_increment_function`), and a long tested
/// before anything else and expected, as php's handler tests `IS_LONG`
/// first under `EXPECTED` (`Z/Zend/zend_vm_def.h:1490`), so the common
/// step takes one compare.
///
/// ```php
/// $max = PHP_INT_MAX;
/// $i = $max; $i++; $j = -$max - 1; --$j; $d = 0.5; ++$d;
/// var_dump($i, $j, $d);
/// ```
/// ```expect
/// float(9.223372036854776E+18)
/// float(-9.223372036854776E+18)
/// float(1.5)
/// ```
#[inline(always)] // as `computed`
pub(crate) fn step(op: Arith, z: Zval) -> Option<Zval> {
    if let Some(n) = z.as_long() {
        return Some(number_zval(computed(op, Number::Long(n), Number::Long(1))));
    }
    // php's `EXPECTED(Z_TYPE_INFO_P(var_ptr) == IS_LONG)`.
    std::hint::cold_path();
    arith(op, z, Zval::long(1))
}

/// The zval a number is, which an inline path writes.
#[inline(always)] // as `number`
fn number_zval(n: Number) -> Zval {
    match n {
        Number::Long(n) => Zval::long(n),
        Number::Double(d) => Zval::double(d),
    }
}

/// An operator a handler's inline path computes over two zvals as they
/// stand: [`Arith`] by [`arith`], where `ZEND_ADD` and its two kin name
/// it as they are written, and [`Binary`] by [`binary`], where a
/// handler or a compound assignment reads it off the opline.
pub(crate) trait Operator: Copy {
    /// The operator's inline pair over `a` and `b`, None where its rule
    /// leaves them to the kernel.
    fn apply(self, a: Zval, b: Zval) -> Option<Zval>;
}

impl Operator for Arith {
    #[inline(always)] // as `computed`
    fn apply(self, a: Zval, b: Zval) -> Option<Zval> {
        arith(self, a, b)
    }
}

impl Operator for Binary {
    #[inline(always)] // as `computed`
    fn apply(self, a: Zval, b: Zval) -> Option<Zval> {
        binary(self, a, b)
    }
}

/// The same pair over two values as a kernel is handed them, which it
/// tests before any other: two numbers as they stand meet neither the
/// array pair nor a class's `do_operation`, so testing them first moves
/// nothing php observes. A reference, a numeric string and every pair
/// the rule refuses go on to the kernel's conversions.
///
/// ```php
/// $a = 6; $r = &$a; $s = "3";
/// var_dump($r + 1, $r / 4, $s * 2, $a % $s, $r << 1, $s | 4, [1] + [2, 3], 1.5 % 1);
/// ```
/// ```expect
///
/// Deprecated: Implicit conversion from float 1.5 to int loses precision in %s on line 3
/// int(7)
/// float(1.5)
/// int(6)
/// int(0)
/// int(12)
/// int(7)
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(3)
/// }
/// int(0)
/// ```
#[inline(always)] // the kernel's first test
pub(crate) fn binary_values(op: Binary, a: &Value, b: &Value) -> Option<Value> {
    binary_numbers(op, value_number(a)?, value_number(b)?).map(number_value)
}

/// The arithmetic of [`computed`] over the numbers a kernel converted
/// its operands to, a numeric string's among them, as the value it
/// hands back.
///
/// ```php
/// $s = "9223372036854775807"; $t = " 2"; $u = "1.5";
/// var_dump($s + 1, $t - 3, $u * $t, "3" * "4");
/// ```
/// ```expect
/// float(9.223372036854776E+18)
/// int(-1)
/// float(3)
/// int(12)
/// ```
pub(crate) fn arith_numbers(op: Arith, a: Number, b: Number) -> Value {
    number_value(computed(op, a, b))
}

/// `ZEND_TYPE_CHECK`'s test (`Z/Zend/zend_vm_def.h:8828`): whether the
/// type `kind`, in php's `IS_*` numbering, is one of the `MAY_BE_*` bits
/// in `mask`, `(extended_value >> Z_TYPE_P(value)) & 1`. What
/// [`crate::ops::type_check`] reads of the value it dereferenced.
///
/// ```php
/// $s = "x"; $r = &$s; $i = 1; $q = &$i; $a = [1]; $p = &$a;
/// var_dump(is_string($r), is_scalar($r), is_int($q), is_float($q), is_array($p), is_null($p), is_bool($q));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
#[inline(always)] // a shift and a test, on an inline path
pub(crate) fn has_type(mask: u32, kind: ZvalType) -> bool {
    (mask >> (kind as u32)) & 1 != 0
}

/// `ZEND_TYPE_CHECK` over a zval as it stands ([`has_type`]): a value
/// that is no reference and no resource. None for a variable nothing
/// wrote, which warns, a reference, which is followed, a resource, which
/// `is_resource` asks whether it is closed, and anything that is no
/// value; the kernel serves each.
///
/// ```php
/// $f = fopen("php://memory", "r"); $c = fopen("php://memory", "r"); fclose($c);
/// $i = 1; $r = &$i; $n = null; $s = "1"; $o = new stdClass;
/// var_dump(is_int($i), is_int($s), is_int($r), is_null($n), is_scalar($s), is_scalar($n), is_object($o), is_array($o));
/// var_dump(is_resource($f), is_resource($c), is_int($u), is_null($v));
/// $p = [explode(",", "a,b")]; var_dump($p[0] === null, is_array($p[0]), explode(",", "a") === null);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
///
/// Warning: Undefined variable $u in %s on line 5
///
/// Warning: Undefined variable $v in %s on line 5
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
#[inline(always)] // as `has_type`
pub(crate) fn type_check(z: Zval, mask: u32) -> Option<bool> {
    match z.kind() {
        ZvalType::Null
        | ZvalType::False
        | ZvalType::True
        | ZvalType::Long
        | ZvalType::Double
        | ZvalType::String
        | ZvalType::Array
        | ZvalType::Object => Some(has_type(mask, z.kind())),
        _ => None,
    }
}

/// `ZEND_STRLEN`'s inline arm (`Z/Zend/zend_vm_def.h:8788`): the length
/// of a zval that is a string as it stands. None for everything else,
/// a reference among it, which [`crate::ops::strlen`] follows, converts
/// or refuses under the frame's regime.
///
/// ```php
/// $s = str_repeat("ab", 3); $r = &$s; $i = 12; $n = null; $e = "";
/// var_dump(strlen($s), strlen($r), strlen($e), strlen($s . "c"), strlen($i), strlen(1.5));
/// var_dump(strlen($n));
/// ```
/// ```expect
/// int(6)
/// int(6)
/// int(0)
/// int(7)
/// int(2)
/// int(3)
///
/// Deprecated: strlen(): Passing null to parameter #1 ($string) of type string is deprecated in %s on line 4
/// int(0)
/// ```
#[inline(always)] // a test of the type byte, on an inline path
pub(crate) fn string_length(z: Zval) -> Option<i64> {
    z.as_str()
        .map(|s| i64::try_from(s.len()).unwrap_or(i64::MAX))
}

/// `ZEND_IS_SMALLER`'s, `ZEND_IS_SMALLER_OR_EQUAL`'s and `ZEND_IS_EQUAL`'s
/// inline pairs: what `holds` reads off [`Number::compare`]'s order over
/// two zvals that are numbers as they stand, read in the pair's own
/// branch so no order is built for the answer to be read off again. A
/// NAN is greater whichever side it is on, so
/// it is smaller than nothing, equal to nothing and nothing is smaller
/// than it.
///
/// ```php
/// $nan = NAN; $i = 1; $f = 1.0; $max = PHP_INT_MAX; $g = (float) PHP_INT_MAX;
/// var_dump($i < $f, $i == $f, $i <= $f, $nan < $i, $i < $nan, $nan == $nan, $nan <= $nan, $max < $g, $max == $g);
/// for ($k = 0; $k < 3; $k++) echo $k;
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// 012
/// ```
#[inline(always)] // as `computed`
pub(crate) fn order(a: Zval, b: Zval, holds: impl Fn(Ordering) -> bool + Copy) -> Option<bool> {
    numbers(
        a,
        b,
        |x, y| holds(x.cmp(&y)),
        |x, y| holds(Number::Double(x).compare(Number::Double(y))),
    )
}

/// `ZEND_JMPZ`'s and `ZEND_JMPNZ`'s inline test
/// (`Z/Zend/zend_vm_def.h:3011-3017`): the truth of `null`, a bool, a
/// long or a double, as [`crate::ops::is_true`] reads them. A NAN, which
/// 8.5 warns of as it is coerced, and everything else is the kernel's.
///
/// ```php
/// $z = 0; $f = 0.0; $n = null; $t = true; $one = -1; $nan = NAN;
/// if ($z) echo "z"; if ($f) echo "f"; if ($n) echo "n"; if ($t) echo "t"; if ($one) echo "1";
/// if ($nan) echo "nan\n";
/// ```
/// ```expect
/// t1
/// Warning: unexpected NAN value was coerced to bool in %s on line 4
/// nan
/// ```
#[inline(always)] // as `number`
pub(crate) fn truth(z: Zval) -> Option<bool> {
    match Scalar::of(&z)? {
        Scalar::Double(d) if d.is_nan() => None,
        Scalar::Str(_) => None,
        scalar => Some(scalar.is_true()),
    }
}

/// The key `zend_fetch_dimension_address_inner` reads out of a
/// dimension it takes inline (`Z/Zend/zend_execute.c:2600-2640`): a long
/// as itself, and a string as `ZEND_HANDLE_NUMERIC_STR` reads it, a
/// canonical decimal integer as that integer and any other string as
/// itself; a constant string is itself, since the compiler wrote the
/// integer in its place where it was one (`zend_handle_numeric_dim`).
/// Everything else, `null`, a bool, a float, a resource, an array or an
/// object, is the kernel's to convert or refuse ([`crate::ops::array_offset`]).
///
/// ```php
/// $a = [5 => "five", "05" => "zero five", "x" => "ex", -3 => "minus"];
/// $i = 5; $s = "5"; $t = "05"; $m = "-3"; $x = "x";
/// var_dump($a[$i], $a[$s], $a[$t], $a[$m], $a[$x], $a["5"], isset($a[$t]), count($a));
/// ```
/// ```expect
/// string(4) "five"
/// string(4) "five"
/// string(9) "zero five"
/// string(5) "minus"
/// string(2) "ex"
/// string(4) "five"
/// bool(true)
/// int(4)
/// ```
#[inline(always)] // a test of the type byte, on an inline path
pub(crate) fn dim_key(dim: Zval, constant: bool) -> Option<Key> {
    match dim.kind() {
        ZvalType::Long => dim.as_long().map(Key::Long),
        ZvalType::String => {
            let text = dim.as_str()?;
            if constant {
                return Some(Key::Str(text));
            }
            Some(php_heap::canonical_int(text.bytes()).map_or(Key::Str(text), Key::Long))
        }
        _ => None,
    }
}

/// The element `key` names in `array` as a read takes it inline, a
/// reference followed (`ZVAL_COPY_DEREF`), no owner taken. None for a
/// key the array has not got, whose read warns, and for a slot of a
/// symbol table, an `IS_INDIRECT` php follows into a CV, which the
/// kernel reads.
///
/// ```php
/// $a = [1, [2], "k" => "v"]; $r = &$a[0]; $r = 7;
/// var_dump($a[0], $a[1][0], $a["k"], $a[9] ?? "none", isset($a[0]), empty($a[1]));
/// ```
/// ```expect
/// int(7)
/// int(2)
/// string(1) "v"
/// string(4) "none"
/// bool(true)
/// bool(false)
/// ```
///
/// A packed table's hole, a string key over a packed table and an
/// uninitialised table are each a key the table has not got:
///
/// ```php
/// $p = [1.5, 2.5, 3.5]; unset($p[1]); $h = ["k" => 1, 7 => 2]; $u = []; $r = [1, 2]; $x = &$r[1]; $x = 9;
/// $k = "k"; $one = 1; $s = "0"; $nine = 9;
/// var_dump($p[0], $p[$s], $p[2], $h[$k], $h[7], $r[$one], isset($p[1]), isset($p["k"]), array_key_exists(-1, $p), isset($h[8]), isset($u[0]), isset($u["k"]));
/// echo $p[$one], $p[$k], $u[0], $u[$k], $h[$nine];
/// ```
/// ```expect
/// float(1.5)
/// float(1.5)
/// float(3.5)
/// int(1)
/// int(2)
/// int(9)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
///
/// Warning: Undefined array key 1 in %s on line 5
///
/// Warning: Undefined array key "k" in %s on line 5
///
/// Warning: Undefined array key 0 in %s on line 5
///
/// Warning: Undefined array key "k" in %s on line 5
///
/// Warning: Undefined array key 9 in %s on line 5
/// ```
#[inline(always)] // as `dim_key`
pub(crate) fn element(array: Arr, key: Key) -> Option<Zval> {
    let found = array.value_ref(key)?.read();
    match found.kind() {
        ZvalType::Reference => found.as_reference().map(|cell| cell.val()),
        ZvalType::Indirect | ZvalType::Undef => None,
        _ => Some(found),
    }
}

/// `zend_is_identical` (`Z/Zend/zend_operators.c:2445`) over two zvals
/// a reference was followed for, where it reads no table: two of
/// different types are not identical, two scalars or two strings are
/// [`php_heap::identical`], two objects are one object, and an array
/// is identical to itself. Two different arrays, which
/// `zend_hash_compare` walks, and an operand nothing wrote are the
/// kernel's.
///
/// ```php
/// $o = new stdClass; $p = new stdClass; $a = [1]; $b = [1]; $n = NAN; $s = "1"; $r = &$s;
/// var_dump(1 === 1.0, "1" === $s, $r === "1", $o === $o, $o === $p, $a === $a, $a === $b, $n === $n, null === false, 0.5 === 0.5);
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// ```
#[inline(always)] // as `number`
pub(crate) fn identical(a: Zval, b: Zval) -> Option<bool> {
    if a.is_undef() || b.is_undef() {
        return None;
    }
    if a.kind() != b.kind() {
        return Some(false);
    }
    match a.kind() {
        ZvalType::Object => Some(a.as_obj()?.is_same(b.as_obj()?)),
        ZvalType::Array => a.as_arr()?.is_same(b.as_arr()?).then_some(true),
        _ => Some(php_heap::identical(Scalar::of(&a)?, Scalar::of(&b)?)),
    }
}

//! `zend_compare` over the scalars (design §5.2): the ordering every
//! comparison operator reads, `Z/Zend/zend_operators.c:2312`, for the
//! five types a constant table holds and an operand may be, ported
//! once. The compiler folds `<` over two constants through this very
//! ordering (`zend_try_ct_eval_binary_op` over `is_smaller_function`),
//! and the kernel reads it for the same pairs while serving, so one copy
//! is what keeps the folded constant and the opcode's value in step.
//! The pairs an array, an object or a resource makes are the kernel's,
//! since they need the request.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::cmp::Ordering;

use crate::numeric::{Number, double_to_string, numeric_read};

/// One scalar as `zend_compare` switches on it: php's `IS_NULL`,
/// `IS_FALSE` and `IS_TRUE`, `IS_LONG`, `IS_DOUBLE` and `IS_STRING`, in
/// php's order of type numbers, which the default arm reads as an order.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Scalar<'a> {
    Null,
    Bool(bool),
    Long(i64),
    Double(f64),
    Str(&'a [u8]),
}

impl Scalar<'_> {
    /// `zval_is_true` over a scalar, which the default arm reads a null
    /// or a bool against. No NAN reaches it: `zend_compare` orders a NAN
    /// against a null or a bool before it asks, so the warning `zend_is_true`
    /// raises for one is never raised here.
    fn is_true(self) -> bool {
        match self {
            Scalar::Null => false,
            Scalar::Bool(b) => b,
            Scalar::Long(n) => n != 0,
            Scalar::Double(d) => d != 0.0,
            Scalar::Str(s) => !(s.is_empty() || s == b"0"),
        }
    }

    /// php's `Z_TYPE_P(op) < IS_TRUE`: a null or a false.
    fn is_falsy_type(self) -> bool {
        matches!(self, Scalar::Null | Scalar::Bool(false))
    }
}

/// `zend_compare` (`Z/Zend/zend_operators.c:2312`) over two scalars:
/// two longs by value; a long and a double, or two doubles, as
/// `ZEND_THREEWAY_COMPARE` orders them, which puts a NAN after
/// everything; a null and a bool as their truth; two strings as
/// `zendi_smart_strcmp` orders them; a string against a number as
/// `compare_long_to_string` and `compare_double_to_string` read it, the
/// number's text compared byte by byte where the string is not
/// numeric, a double rendered at php's `precision`; and the default arm,
/// where a null or a bool on either side compares as the other side's
/// truth, with php's own NAN cases before it. Nothing here warns or
/// throws: every conversion `zend_compare` makes of a scalar is the
/// silent one.
///
/// ```php
/// $one = extension_loaded('Core');
/// var_dump(1 < $one + 1, 2 < $one, 1.5 < $one + 1, $one + 1 < 1.5, "10" < $one + 9, "abc" < $one, 9223372036854775807 < $one + 9223372036854775807);
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
///
/// Two strings compare as numbers where both are numeric and as bytes
/// where either is not; a null or a bool compares as truth. Each side is
/// a CV, which php's compiler folds nothing over.
///
/// ```php
/// $ten = "10"; $nine = "9"; $abc = "abc"; $abd = "abd"; $e3 = "1e3"; $n999 = "999"; $empty = ""; $one = "1"; $null = null; $f = false; $t = true; $zero = "0";
/// var_dump($ten < $nine, $abc < $abd, $e3 < $n999, $empty < $one, $null < $one, $f < $one, $t < $one, $one < $null, $zero < $f);
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[must_use]
pub fn compare(left: Scalar, right: Scalar, precision: i32) -> Ordering {
    match (left, right) {
        (Scalar::Long(a), Scalar::Long(b)) => a.cmp(&b),
        (Scalar::Long(a), Scalar::Double(b)) => three_way(Number::Long(a).double(), b),
        (Scalar::Double(a), Scalar::Long(b)) => three_way(a, Number::Long(b).double()),
        (Scalar::Double(a), Scalar::Double(b)) => three_way(a, b),
        (Scalar::Null | Scalar::Bool(false), Scalar::Null | Scalar::Bool(false))
        | (Scalar::Bool(true), Scalar::Bool(true)) => Ordering::Equal,
        (Scalar::Null, Scalar::Bool(true)) => Ordering::Less,
        (Scalar::Bool(true), Scalar::Null) => Ordering::Greater,
        (Scalar::Str(a), Scalar::Str(b)) => smart_strcmp(a, b),
        (Scalar::Null, Scalar::Str(s)) => {
            if s.is_empty() {
                Ordering::Equal
            } else {
                Ordering::Less
            }
        }
        (Scalar::Str(s), Scalar::Null) => {
            if s.is_empty() {
                Ordering::Equal
            } else {
                Ordering::Greater
            }
        }
        (Scalar::Long(n), Scalar::Str(s)) => long_to_string(n, s),
        (Scalar::Str(s), Scalar::Long(n)) => long_to_string(n, s).reverse(),
        (Scalar::Double(d), Scalar::Str(s)) => {
            if d.is_nan() {
                Ordering::Greater
            } else {
                double_to_string_cmp(d, s, precision)
            }
        }
        (Scalar::Str(s), Scalar::Double(d)) => {
            if d.is_nan() {
                Ordering::Greater
            } else {
                double_to_string_cmp(d, s, precision).reverse()
            }
        }
        // php's default arm, over the pairs a null or a bool makes with a
        // number or a string: its NAN cases first, "NAN used be cast to
        // TRUE so handle this manually", then the truth of the other side.
        (a, b) => {
            let nan = matches!(a, Scalar::Double(d) if d.is_nan())
                || matches!(b, Scalar::Double(d) if d.is_nan());
            if nan {
                if a.is_falsy_type() {
                    Ordering::Less
                } else if a == Scalar::Bool(true) || b == Scalar::Bool(true) {
                    Ordering::Equal
                } else {
                    Ordering::Greater
                }
            } else if a.is_falsy_type() {
                if b.is_true() {
                    Ordering::Less
                } else {
                    Ordering::Equal
                }
            } else if a == Scalar::Bool(true) {
                if b.is_true() {
                    Ordering::Equal
                } else {
                    Ordering::Greater
                }
            } else if b.is_falsy_type() {
                if a.is_true() {
                    Ordering::Greater
                } else {
                    Ordering::Equal
                }
            } else if a.is_true() {
                // `b` is `true`: every other pair of these five types
                // has its own arm above.
                Ordering::Equal
            } else {
                Ordering::Less
            }
        }
    }
}

/// `ZEND_THREEWAY_COMPARE` over two doubles: equal, less, else greater,
/// which is where a NAN lands whichever side it is on.
#[expect(
    clippy::float_cmp,
    reason = "php's `(a) == (b)`, the exact equality being copied"
)]
fn three_way(a: f64, b: f64) -> Ordering {
    if a == b {
        Ordering::Equal
    } else if a < b {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

/// `zendi_smart_strcmp` (`Z/Zend/zend_operators.c:3421`): two numeric
/// strings as the numbers they hold, and any other pair as bytes
/// (`zend_binary_strcmp`), with php's two guards against a lost digit:
/// two integers that overflowed a long on the same side and read as one
/// double are compared as bytes, and so are two doubles that are equal
/// and not finite; an integer that overflowed against a double is
/// ordered by the side it overflowed on.
fn smart_strcmp(a: &[u8], b: &[u8]) -> Ordering {
    let Some(first) = numeric_read(a).filter(|r| !r.trailing) else {
        return a.cmp(b);
    };
    let Some(second) = numeric_read(b).filter(|r| !r.trailing) else {
        return a.cmp(b);
    };
    let (d1, d2) = (first.number.double(), second.number.double());
    if first.overflow != 0 && first.overflow == second.overflow && d1 - d2 == 0.0 {
        return a.cmp(b);
    }
    match (first.number, second.number) {
        (Number::Long(l1), Number::Long(l2)) => l1.cmp(&l2),
        (Number::Long(_), Number::Double(_)) if second.overflow != 0 => {
            if second.overflow < 0 {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (Number::Double(_), Number::Long(_)) if first.overflow != 0 => {
            if first.overflow < 0 {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Number::Double(_), Number::Double(_)) if same(d1, d2) && !d1.is_finite() => a.cmp(b),
        // `ZEND_NORMALIZE_BOOL(dval1 - dval2)`, a NAN difference being
        // nonzero and not negative.
        _ => {
            let difference = d1 - d2;
            if difference == 0.0 {
                Ordering::Equal
            } else if difference < 0.0 {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
    }
}

/// php's `dval1 == dval2`, which two infinities of one sign meet.
#[expect(
    clippy::float_cmp,
    reason = "php's `dval1 == dval2`, the exact equality being copied"
)]
fn same(a: f64, b: f64) -> bool {
    a == b
}

/// `compare_long_to_string` (`Z/Zend/zend_operators.c:2260`): the string
/// as the number it holds where it is numeric, and the long's digits
/// against its bytes where it is not.
fn long_to_string(n: i64, s: &[u8]) -> Ordering {
    match numeric_read(s).filter(|r| !r.trailing).map(|r| r.number) {
        Some(Number::Long(l)) => n.cmp(&l),
        Some(Number::Double(d)) => three_way(Number::Long(n).double(), d),
        None => n.to_string().as_bytes().cmp(s),
    }
}

/// `compare_double_to_string` (`Z/Zend/zend_operators.c:2282`): the same
/// for a double that is not a NAN, its text `zend_double_to_str`'s at
/// php's `precision`.
fn double_to_string_cmp(d: f64, s: &[u8], precision: i32) -> Ordering {
    match numeric_read(s).filter(|r| !r.trailing).map(|r| r.number) {
        Some(Number::Long(l)) => three_way(d, Number::Long(l).double()),
        Some(Number::Double(sd)) => three_way(d, sd),
        None => double_to_string(d, precision).as_bytes().cmp(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row is php's own: `var_dump($a <=> $b)` under the pin, read
    /// off the oracle when the rows were written.
    #[test]
    fn two_scalars_compare_as_zend_compare_orders_them() {
        use Ordering::{Equal, Greater, Less};
        let s = Scalar::Str;
        for (a, b, ordering) in [
            (Scalar::Long(1), Scalar::Long(2), Less),
            (Scalar::Long(2), Scalar::Long(2), Equal),
            (Scalar::Long(2), Scalar::Double(1.5), Greater),
            (Scalar::Double(1.5), Scalar::Long(2), Less),
            (Scalar::Double(f64::NAN), Scalar::Long(2), Greater),
            (Scalar::Long(2), Scalar::Double(f64::NAN), Greater),
            (
                Scalar::Long(i64::MAX),
                Scalar::Double(9_223_372_036_854_775_808.0),
                Equal,
            ),
            (Scalar::Null, Scalar::Bool(false), Equal),
            (Scalar::Null, Scalar::Bool(true), Less),
            (Scalar::Bool(true), Scalar::Null, Greater),
            (s(b"10"), s(b"9"), Greater),
            (s(b"abc"), s(b"abd"), Less),
            (s(b"abc"), s(b"abc"), Equal),
            (s(b"1e3"), s(b"999"), Greater),
            (s(b"10"), s(b"1e1"), Equal),
            (s(b"9223372036854775808"), s(b"9223372036854775809"), Less),
            (s(b"1e1000"), s(b"2e1000"), Less),
            (s(b"9223372036854775808"), s(b"1.5"), Greater),
            (s(b"1.5"), s(b"-9223372036854775809"), Greater),
            (s(b""), s(b"0"), Less),
            (Scalar::Null, s(b""), Equal),
            (Scalar::Null, s(b"a"), Less),
            (s(b"a"), Scalar::Null, Greater),
            (Scalar::Long(10), s(b"9"), Greater),
            (Scalar::Long(10), s(b"1e1"), Equal),
            (Scalar::Long(10), s(b"abc"), Less),
            (s(b"abc"), Scalar::Long(10), Greater),
            (Scalar::Long(10), s(b"10abc"), Less),
            (Scalar::Double(1.5), s(b"1.5"), Equal),
            (Scalar::Double(1.5), s(b"2"), Less),
            (Scalar::Double(1.5), s(b"abc"), Less),
            (Scalar::Double(f64::NAN), s(b"abc"), Greater),
            (s(b"abc"), Scalar::Double(f64::NAN), Greater),
            (Scalar::Double(0.1 + 0.2), s(b"0.3"), Greater),
            (Scalar::Bool(true), Scalar::Long(2), Equal),
            (Scalar::Bool(true), Scalar::Long(0), Greater),
            (Scalar::Bool(false), Scalar::Long(0), Equal),
            (Scalar::Bool(false), Scalar::Long(2), Less),
            (Scalar::Null, Scalar::Long(0), Equal),
            (Scalar::Null, Scalar::Long(-1), Less),
            (Scalar::Long(-1), Scalar::Null, Greater),
            (Scalar::Long(2), Scalar::Bool(true), Equal),
            (Scalar::Long(0), Scalar::Bool(true), Less),
            (s(b"0"), Scalar::Bool(false), Equal),
            (s(b"a"), Scalar::Bool(false), Greater),
            (Scalar::Double(f64::NAN), Scalar::Null, Greater),
            (Scalar::Null, Scalar::Double(f64::NAN), Less),
            (Scalar::Double(f64::NAN), Scalar::Bool(true), Equal),
            (Scalar::Bool(false), Scalar::Double(f64::NAN), Less),
        ] {
            assert_eq!(compare(a, b, 14), ordering, "{a:?} <=> {b:?}");
        }
    }

    /// The double's text is rendered at `precision`, so the same pair
    /// orders differently under `precision=14` and `precision=17`, as
    /// php's does with `ini_set('precision', 17)` between the two.
    #[test]
    fn a_double_against_a_string_that_is_not_numeric_reads_at_precision() {
        let against = Scalar::Str(b"0.30000000000000003x");
        assert_eq!(
            compare(Scalar::Double(0.1 + 0.2), against, 14),
            Ordering::Less,
            "\"0.3\" against it"
        );
        assert_eq!(
            compare(Scalar::Double(0.1 + 0.2), against, 17),
            Ordering::Greater,
            "\"0.30000000000000004\" against it"
        );
    }
}

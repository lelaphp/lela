//! `zend_compare` over the scalars (design §5.2): the ordering every
//! comparison operator reads, `Z/Zend/zend_operators.c:2312`, for the
//! five types a constant table holds and an operand may be, ported
//! once, and `zend_is_identical` over the same, which `===` reads
//! instead. The compiler folds a comparison over two constants through
//! this very ordering (`zend_try_ct_eval_binary_op` over
//! `is_smaller_function` and its kin), and the kernel reads it for the
//! same pairs while serving, so one copy is what keeps the folded
//! constant and the opcode's value in step. The pairs an array, an
//! object or a resource makes are the kernel's, since they need the
//! request.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::cmp::Ordering;

use crate::array::Arr;
use crate::numeric::{Exponent, Number, double_to_string, numeric_read};
use crate::zval::{Zval, ZvalType};

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

impl<'a> Scalar<'a> {
    /// The scalar a zval is, read off its type byte once: a null, a
    /// bool, a long, a double, or a string's bytes, borrowed for as long
    /// as the zval is ([`Zval::str_bytes`]). `None` for an array, an
    /// object, a resource, a reference, and a slot nothing wrote, which
    /// each reader decides for itself: a constant table holds none but
    /// the array ([`constant_truth`], [`compare_constants`]), and the
    /// engine's inline tests leave each to the kernel.
    #[must_use]
    #[inline]
    pub fn of(value: &'a Zval) -> Option<Scalar<'a>> {
        Some(match value.kind() {
            ZvalType::Null => Scalar::Null,
            ZvalType::False => Scalar::Bool(false),
            ZvalType::True => Scalar::Bool(true),
            ZvalType::Long => Scalar::Long(value.as_long()?),
            ZvalType::Double => Scalar::Double(value.as_double()?),
            ZvalType::String => Scalar::Str(value.str_bytes()?),
            _ => return None,
        })
    }
}

impl Scalar<'_> {
    /// `zend_is_true`'s value over a scalar (`Z/Zend/zend_operators.h:409`):
    /// a null and a false are false, a long and a double are true where
    /// they are not zero, a NAN among them, and a string is true unless
    /// it is empty or `"0"`. The warning 8.5 raises as a NAN is coerced
    /// is the caller's to raise where php raises it: `zend_compare`'s
    /// default arm orders a NAN against a null or a bool before it asks,
    /// and the engine's inline test leaves a NAN to the kernel.
    #[must_use]
    pub fn is_true(self) -> bool {
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
        (Scalar::Long(a), Scalar::Long(b)) => Number::Long(a).compare(Number::Long(b)),
        (Scalar::Long(a), Scalar::Double(b)) => Number::Long(a).compare(Number::Double(b)),
        (Scalar::Double(a), Scalar::Long(b)) => Number::Double(a).compare(Number::Long(b)),
        (Scalar::Double(a), Scalar::Double(b)) => Number::Double(a).compare(Number::Double(b)),
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

/// `ZEND_THREEWAY_COMPARE` over two doubles, which [`Number::compare`]
/// holds.
fn three_way(a: f64, b: f64) -> Ordering {
    Number::Double(a).compare(Number::Double(b))
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
        None => double_to_string(d, precision, Exponent::Upper)
            .as_bytes()
            .cmp(s),
    }
}

/// `zend_is_identical` (`Z/Zend/zend_operators.c:2479`) over two
/// scalars: the same type, and then the same value, a string by its
/// bytes (`zend_string_equals`) and a double by `==`, so a NAN is
/// identical to nothing, itself included. A null, a false and a true
/// are three types, so `null === false` is not. The compiler folds
/// `===` and `!==` over two constants through this
/// (`zend_try_ct_eval_binary_op` over `is_identical_function`) and the
/// kernel reads it for the same pairs; an array, an object and a
/// resource are the kernel's.
///
/// ```php
/// $t = extension_loaded('Core'); $i = $t + 0; $s = "1"; $d = 1.0; $nan = NAN; $n = null;
/// var_dump($i === 1, $t === 1, $i === $d, $s === "1", $s === "01", $d === 1.0, $d === $s, $nan === $nan, $n === false, $n === $n, "abc" === "abc");
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// ```
#[must_use]
#[expect(
    clippy::float_cmp,
    reason = "php's `Z_DVAL_P(op1) == Z_DVAL_P(op2)`, the exact equality being copied"
)]
pub fn identical(left: Scalar, right: Scalar) -> bool {
    match (left, right) {
        (Scalar::Null, Scalar::Null) => true,
        (Scalar::Bool(a), Scalar::Bool(b)) => a == b,
        (Scalar::Long(a), Scalar::Long(b)) => a == b,
        (Scalar::Double(a), Scalar::Double(b)) => a == b,
        (Scalar::Str(a), Scalar::Str(b)) => a == b,
        _ => false,
    }
}

/// `zend_compare` over two values of a constant table, the scalars and
/// the immutable arrays of them a compiler folds, which is what
/// `zend_try_ct_eval_binary_op` reads for `[1] == [2]`, `[1] < [2]` and
/// `[1, 2, 3] <=> []`: two scalars as [`compare`] orders them; two
/// arrays as `zend_compare_arrays`, one table against itself equal and
/// otherwise [`compare_tables`]; and an array against a scalar through
/// `zend_compare`'s default arm, a null or a bool as the array's truth
/// and the array greater than anything else. No constant holds an
/// object, a resource or a reference, and an immutable array cannot
/// hold itself, so nothing here needs the request or throws; the
/// kernel's own comparison takes every other pair.
#[must_use]
pub fn compare_constants(left: Zval, right: Zval, precision: i32) -> Ordering {
    match (left.as_arr(), right.as_arr()) {
        (Some(a), Some(b)) => compare_tables(a, b, precision),
        (Some(a), None) => against_array(a, right.kind()).reverse(),
        (None, Some(b)) => against_array(b, left.kind()),
        (None, None) => compare(scalar(&left), scalar(&right), precision),
    }
}

/// `zend_is_identical` over two values of a constant table: the scalars
/// as [`identical`] has them, and two arrays when `zend_hash_compare`
/// with `ordered` on finds them the same, the same count and, walked
/// together, the same key at each step and identical values.
#[must_use]
pub fn identical_constants(left: Zval, right: Zval) -> bool {
    match (left.as_arr(), right.as_arr()) {
        (Some(a), Some(b)) => {
            a.is_same(b)
                || (a.len() == b.len()
                    && a.iter()
                        .zip(b.iter())
                        .all(|((k, v), (l, w))| k == l && identical_constants(v, w)))
        }
        (None, None) => identical(scalar(&left), scalar(&right)),
        _ => false,
    }
}

/// `zend_hash_compare_impl` with `ordered` off
/// (`Z/Zend/zend_hash.c:3104`) over two constant arrays: the counts
/// first, then each key of the left array looked up in the right one, a
/// key the right one lacks making the left the greater, and the values
/// compared by [`compare_constants`] until one pair differs.
fn compare_tables(left: Arr, right: Arr, precision: i32) -> Ordering {
    if left.is_same(right) {
        return Ordering::Equal;
    }
    if left.len() != right.len() {
        return left.len().cmp(&right.len());
    }
    for (key, value) in &left {
        let Some(other) = right.get(key) else {
            return Ordering::Greater;
        };
        let ordering = compare_constants(value, other, precision);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// `zend_compare`'s default arm with a scalar of type `other` on the
/// left and an array on the right: a null or a false less than a
/// non-empty array and equal to an empty one, a true equal to a
/// non-empty array and greater than an empty one, and anything else
/// less than the array.
fn against_array(array: Arr, other: ZvalType) -> Ordering {
    let full = !array.is_empty();
    match other {
        ZvalType::Null | ZvalType::False if full => Ordering::Less,
        ZvalType::Null | ZvalType::False => Ordering::Equal,
        ZvalType::True if full => Ordering::Equal,
        ZvalType::True => Ordering::Greater,
        _ => Ordering::Less,
    }
}

/// `zend_is_true`'s value over a value of a constant table
/// (`i_zend_is_true`, `Z/Zend/zend_operators.h:409`): a scalar as
/// [`Scalar::is_true`] reads it, a NAN true, and an array true where it
/// has an element. `None` for an object, a resource, a reference or a
/// slot nothing wrote, none of which a constant table holds and whose
/// truth needs the request. The compiler's folds over a constant read
/// it, and so does the compiled tier where it reads pass 1's fold of a
/// jump on a constant (`Z/Zend/Optimizer/pass1.c:315-333`).
///
/// ```php
/// var_dump(!"0", !"00", ![], ![0], (bool)"0", (bool)[], (bool)[0]);
/// if (NAN) echo "NAN is true\n";
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
///
/// Warning: unexpected NAN value was coerced to bool in %s on line 3
/// NAN is true
/// ```
#[must_use]
pub fn constant_truth(value: Zval) -> Option<bool> {
    match value.as_arr() {
        Some(array) => Some(!array.is_empty()),
        None => Scalar::of(&value).map(Scalar::is_true),
    }
}

/// The scalar a constant table's value is, past the arrays its callers
/// took first; no constant is anything else, and a slot nothing wrote
/// reads as null.
fn scalar(value: &Zval) -> Scalar<'_> {
    Scalar::of(value).unwrap_or(Scalar::Null)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A NAN is true, an array is true where it has an element, and the
    /// string `"0"` is false where `"0.0"` is true, as `zend_is_true`
    /// reads each; an object, an `IS_PTR` or a slot nothing wrote has no
    /// truth a constant gives.
    #[test]
    fn a_constant_is_true_as_zend_is_true_reads_it() {
        let _alone = crate::serial::alone();
        let mut heap = crate::Heap::new();
        let mut strings = crate::Strings::new();
        let mut string = |bytes: &[u8]| {
            crate::Intern::intern(&mut strings, bytes)
                .map(Zval::string)
                .expect("an interned string")
        };
        let (zero, double_zero, one, empty) =
            (string(b"0"), string(b"00"), string(b"1"), string(b""));
        let zero_point_zero = string(b"0.0");
        let object = heap.object(1, 0, 0).expect("an object");
        let full = heap.array().expect("an array");
        assert!(matches!(heap.array_append(full, Zval::long(0)), Ok(true)));
        for (value, truth) in [
            (Zval::double(f64::NAN), Some(true)),
            (Zval::double(0.0), Some(false)),
            (Zval::double(-0.0), Some(false)),
            (Zval::array(Arr::empty()), Some(false)),
            (Zval::array(full), Some(true)),
            (zero, Some(false)),
            (double_zero, Some(true)),
            (one, Some(true)),
            (empty, Some(false)),
            (zero_point_zero, Some(true)),
            (Zval::null(), Some(false)),
            (Zval::bool(true), Some(true)),
            (Zval::long(0), Some(false)),
            (Zval::long(-1), Some(true)),
            (Zval::object(object), None),
            (Zval::ptr(0), None),
            (Zval::UNDEF, None),
        ] {
            assert_eq!(constant_truth(value), truth, "{:?}", value.kind());
        }
        assert_eq!(Scalar::of(&zero), Some(Scalar::Str(b"0")));
        assert_eq!(Scalar::of(&Zval::array(full)), None);
    }

    #[test]
    fn two_scalars_are_identical_by_type_and_then_by_value() {
        let s = Scalar::Str;
        for (a, b, same) in [
            (Scalar::Long(1), Scalar::Long(1), true),
            (Scalar::Long(1), Scalar::Double(1.0), false),
            (Scalar::Double(1.0), Scalar::Double(1.0), true),
            (Scalar::Double(0.0), Scalar::Double(-0.0), true),
            (Scalar::Double(f64::NAN), Scalar::Double(f64::NAN), false),
            (s(b"1"), s(b"1"), true),
            (s(b"1"), s(b"01"), false),
            (s(b"1"), Scalar::Long(1), false),
            (Scalar::Null, Scalar::Null, true),
            (Scalar::Null, Scalar::Bool(false), false),
            (Scalar::Bool(true), Scalar::Bool(true), true),
            (Scalar::Bool(true), Scalar::Bool(false), false),
        ] {
            assert_eq!(identical(a, b), same, "{a:?} === {b:?}");
        }
    }

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

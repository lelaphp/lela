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

use php_heap::{Arr, Key, Number, Zval, ZvalType};

use crate::api::values::Value;

/// The three operators php computes inline over two numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Arith {
    Add,
    Sub,
    Mul,
}

/// A zval that is a number as it stands, nothing converted.
#[inline(always)] // a test of the type byte, on every inline path
#[allow(clippy::inline_always)]
fn number(z: Zval) -> Option<Number> {
    match z.kind() {
        ZvalType::Long => z.as_long().map(Number::Long),
        ZvalType::Double => z.as_double().map(Number::Double),
        _ => None,
    }
}

/// `add_function`'s, `sub_function`'s and `mul_function`'s two numeric
/// type pairs: two longs as longs, with the double
/// `fast_long_add_function`, `fast_long_sub_function` and
/// `ZEND_SIGNED_MULTIPLY_LONG` compute where the long overflowed, and
/// any other pair as doubles.
#[inline(always)] // the arithmetic of an inline path
#[allow(clippy::inline_always)]
fn computed(op: Arith, a: Number, b: Number) -> Number {
    if let (Number::Long(x), Number::Long(y)) = (a, b) {
        let stayed = match op {
            Arith::Add => x.checked_add(y),
            Arith::Sub => x.checked_sub(y),
            Arith::Mul => x.checked_mul(y),
        };
        if let Some(n) = stayed {
            return Number::Long(n);
        }
    }
    let (x, y) = (a.double(), b.double());
    Number::Double(match op {
        Arith::Add => x + y,
        Arith::Sub => x - y,
        Arith::Mul => x * y,
    })
}

/// `ZEND_ADD`'s, `ZEND_SUB`'s and `ZEND_MUL`'s inline pairs over two
/// zvals as they stand, and `ZEND_PRE_INC`'s and `ZEND_PRE_DEC`'s, which
/// are `+ 1` and `- 1` with the same overflow (`fast_long_increment_function`).
///
/// ```php
/// $max = PHP_INT_MAX; $one = 1; $half = 1.5; $two = -2;
/// var_dump($max + $one, $max - $two, $max * $two, $half + $one, $one - $half, $half * $half, $one + $two);
/// $i = $max; $i++; $j = -$max - 1; --$j; $d = 0.5; ++$d;
/// var_dump($i, $j, $d);
/// ```
/// ```expect
/// float(9.223372036854776E+18)
/// float(9.223372036854776E+18)
/// float(-1.8446744073709552E+19)
/// float(2.5)
/// float(-0.5)
/// float(2.25)
/// int(-1)
/// float(9.223372036854776E+18)
/// float(-9.223372036854776E+18)
/// float(1.5)
/// ```
#[inline(always)] // as `computed`
#[allow(clippy::inline_always)]
pub(crate) fn arith(op: Arith, a: Zval, b: Zval) -> Option<Zval> {
    Some(match computed(op, number(a)?, number(b)?) {
        Number::Long(n) => Zval::long(n),
        Number::Double(d) => Zval::double(d),
    })
}

/// The same arithmetic over the numbers a kernel converted its operands
/// to, a numeric string's among them, as the value it hands back.
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
    match computed(op, a, b) {
        Number::Long(n) => Value::int(n),
        Number::Double(d) => Value::float(d),
    }
}

/// `ZEND_IS_SMALLER`'s, `ZEND_IS_SMALLER_OR_EQUAL`'s and `ZEND_IS_EQUAL`'s
/// inline pairs, [`Number::compare`]'s order over two zvals that are
/// numbers as they stand: a NAN is greater whichever side it is on, so
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
#[allow(clippy::inline_always)]
pub(crate) fn order(a: Zval, b: Zval) -> Option<Ordering> {
    Some(number(a)?.compare(number(b)?))
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
#[allow(clippy::inline_always)]
pub(crate) fn truth(z: Zval) -> Option<bool> {
    match z.kind() {
        ZvalType::Null | ZvalType::False => Some(false),
        ZvalType::True => Some(true),
        ZvalType::Long => z.as_long().map(|n| n != 0),
        ZvalType::Double => z.as_double().filter(|d| !d.is_nan()).map(|d| d != 0.0),
        _ => None,
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
#[allow(clippy::inline_always)]
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
#[allow(clippy::inline_always)]
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
#[allow(clippy::inline_always)]
pub(crate) fn identical(a: Zval, b: Zval) -> Option<bool> {
    let scalar = |z: Zval| match z.kind() {
        ZvalType::Null => Some(php_heap::Scalar::Null),
        ZvalType::False => Some(php_heap::Scalar::Bool(false)),
        ZvalType::True => Some(php_heap::Scalar::Bool(true)),
        ZvalType::Long => z.as_long().map(php_heap::Scalar::Long),
        ZvalType::Double => z.as_double().map(php_heap::Scalar::Double),
        _ => None,
    };
    if a.is_undef() || b.is_undef() {
        return None;
    }
    if a.kind() != b.kind() {
        return Some(false);
    }
    match a.kind() {
        ZvalType::String => Some(a.as_str()?.bytes() == b.as_str()?.bytes()),
        ZvalType::Object => Some(a.as_obj()?.is_same(b.as_obj()?)),
        ZvalType::Array => a.as_arr()?.is_same(b.as_arr()?).then_some(true),
        _ => Some(php_heap::identical(scalar(a)?, scalar(b)?)),
    }
}

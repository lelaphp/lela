//! The natives of `array.c` declared so far, and what they sort with:
//! `array_unique` over php's `zend_sort` (`Z/Zend/zend_sort.c`), copied
//! because the order it leaves a stretch of equal elements in is what decides
//! which of them survives a comparison that is not transitive, and
//! `strnatcmp_ex` (`Z/ext/standard/strnatcmp.c`) for `SORT_NATURAL`.

use std::cmp::Ordering;

use php_ext::coerce::{coerce_param, compare, to_double, to_string};
use php_ext::decl::{ConstDecl, ConstExpr, FunctionDecl, Parameter, Type};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, Value};

use crate::output::Entry;
use crate::{argument_count_between, constant, declared, long_argument, parameter};

/// `PHP_SORT_*` (`Z/ext/standard/php_array.h`), what a sort native's
/// `$flags` is made of.
const SORT_REGULAR: i64 = 0;
const SORT_NUMERIC: i64 = 1;
const SORT_STRING: i64 = 2;
const SORT_DESC: i64 = 3;
const SORT_ASC: i64 = 4;
const SORT_LOCALE_STRING: i64 = 5;
const SORT_NATURAL: i64 = 6;
const SORT_FLAG_CASE: i64 = 8;

/// The `SORT_*` constants, in the order `basic_functions.stub.php`
/// declares them.
///
/// ```php
/// var_dump(SORT_ASC, SORT_DESC, SORT_REGULAR, SORT_NUMERIC, SORT_STRING, SORT_LOCALE_STRING, SORT_NATURAL, SORT_FLAG_CASE);
/// ```
/// ```expect
/// int(4)
/// int(3)
/// int(0)
/// int(1)
/// int(2)
/// int(5)
/// int(6)
/// int(8)
/// ```
pub(crate) fn constants() -> Vec<ConstDecl> {
    vec![
        constant(b"SORT_ASC", SORT_ASC),
        constant(b"SORT_DESC", SORT_DESC),
        constant(b"SORT_REGULAR", SORT_REGULAR),
        constant(b"SORT_NUMERIC", SORT_NUMERIC),
        constant(b"SORT_STRING", SORT_STRING),
        constant(b"SORT_LOCALE_STRING", SORT_LOCALE_STRING),
        constant(b"SORT_NATURAL", SORT_NATURAL),
        constant(b"SORT_FLAG_CASE", SORT_FLAG_CASE),
    ]
}

/// The natives declared here, paired with their entries.
///
/// ```php
/// var_dump(array_unique([1, 1]));
/// ```
/// ```expect
/// array(1) {
///   [0]=>
///   int(1)
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![(
        declared(
            b"array_unique",
            &[
                parameter(b"array", Type::Array),
                Parameter {
                    default: Some(ConstExpr::Constant(php_ext::decl::Name(
                        b"SORT_STRING".as_slice().into(),
                    ))),
                    ..parameter(b"flags", Type::Int)
                },
            ],
            Type::Array,
        ),
        array_unique,
    )]
}

/// `array_unique(array $array, int $flags = SORT_STRING): array`, php's
/// `PHP_FUNCTION(array_unique)` (`Z/ext/standard/array.c:4964`). An
/// array of one element or none comes back as it is, one more owner.
/// Under `SORT_STRING` each value is read as a string and the first
/// element to spell each string is kept, in the array's order and under
/// its key, a new array. Under any other `$flags` the array is copied, the
/// elements sorted by the comparison it names with `zend_sort`, and of
/// each stretch of equal elements the one that came first is kept and the
/// rest deleted from the copy, so what survives a comparison that is
/// not transitive, `SORT_REGULAR` over mixed types, is what php's sort
/// leaves beside what.
///
/// ```php
/// var_dump(array_unique([3 => "b", 1 => "a", "x" => "b", 2 => "a", "1", 1, true]), array_unique([7]), array_unique([]));
/// ```
/// ```expect
/// array(3) {
///   [3]=>
///   string(1) "b"
///   [1]=>
///   string(1) "a"
///   [4]=>
///   string(1) "1"
/// }
/// array(1) {
///   [0]=>
///   int(7)
/// }
/// array(0) {
/// }
/// ```
///
/// ```php
/// var_dump(array_unique([1, "1", 2, 2.0, "a", "A", null, "", false, true], SORT_REGULAR), array_unique(["10", "9", "1e1", 10, "x"], SORT_NUMERIC), array_unique(["a", "A", "b"], 10));
/// ```
/// ```expect
/// array(5) {
///   [0]=>
///   int(1)
///   [2]=>
///   int(2)
///   [4]=>
///   string(1) "a"
///   [5]=>
///   string(1) "A"
///   [6]=>
///   NULL
/// }
/// array(3) {
///   [0]=>
///   string(2) "10"
///   [1]=>
///   string(1) "9"
///   [4]=>
///   string(1) "x"
/// }
/// array(2) {
///   [0]=>
///   string(1) "a"
///   [2]=>
///   string(1) "b"
/// }
/// ```
///
/// ```php
/// var_dump(array_unique(["img12", "IMG10", "img10", "img2", "img012", "img 12"], 14), array_unique(["img12", "IMG10", "img10", "img2", "img012"], SORT_NATURAL), array_unique(["b", "a", "b"], SORT_LOCALE_STRING));
/// ```
/// ```expect
/// array(4) {
///   [0]=>
///   string(5) "img12"
///   [1]=>
///   string(5) "IMG10"
///   [3]=>
///   string(4) "img2"
///   [4]=>
///   string(6) "img012"
/// }
/// array(5) {
///   [0]=>
///   string(5) "img12"
///   [1]=>
///   string(5) "IMG10"
///   [2]=>
///   string(5) "img10"
///   [3]=>
///   string(4) "img2"
///   [4]=>
///   string(6) "img012"
/// }
/// array(2) {
///   [0]=>
///   string(1) "b"
///   [1]=>
///   string(1) "a"
/// }
/// ```
///
/// An array past sixteen elements takes `zend_sort`'s partition, not its
/// insertion sort, and the keys keep the array's order.
///
/// ```php
/// var_dump(array_unique(["a" => 3, "b" => 1, "c" => 4, "d" => 1, "e" => 5, "f" => 9, "g" => 2, "h" => 6, "i" => 5, "j" => 3, "k" => 5, "l" => 8, "m" => 9, "n" => 7, "o" => 9, "p" => 3, "q" => 2, "r" => 3, "s" => 8, "t" => 4, "u" => 6], SORT_NUMERIC));
/// ```
/// ```expect
/// array(9) {
///   ["a"]=>
///   int(3)
///   ["b"]=>
///   int(1)
///   ["c"]=>
///   int(4)
///   ["e"]=>
///   int(5)
///   ["f"]=>
///   int(9)
///   ["g"]=>
///   int(2)
///   ["h"]=>
///   int(6)
///   ["l"]=>
///   int(8)
///   ["n"]=>
///   int(7)
/// }
/// ```
///
/// ```php
/// var_dump(array_unique("x"));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: array_unique(): Argument #1 ($array) must be of type array, string given in %s:2
/// Stack trace:
/// #0 %s(2): array_unique('x')
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn array_unique(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let given = req.argument(1).unwrap_or_default();
    let coerced = coerce_param(req, &given, &Type::Array, 1)?;
    let Some(array) = coerced.deref().as_array().copied() else {
        return Ok(Value::array(PhpArray::new(req)?));
    };
    let flags = if req.argument_count() == 2 {
        long_argument(req, 2)?
    } else {
        SORT_STRING
    };
    if array.len() <= 1 {
        let value = Value::array(array);
        req.addref(&value);
        return Ok(value);
    }
    if flags == SORT_STRING {
        return unique_by_string(req, array).map(Value::array);
    }
    let entries = array.snapshot_refs();
    let mut copy = array.copy(req)?;
    let mut order: Vec<usize> = (0..entries.len()).collect();
    let mut pending: Option<Thrown> = None;
    zend_sort(&mut order, &mut |a, b| {
        if pending.is_some() {
            return Ordering::Equal;
        }
        match compared(req, flags, &entries[*a].1, &entries[*b].1) {
            Ok(ordering) => ordering,
            Err(thrown) => {
                pending = Some(thrown);
                Ordering::Equal
            }
        }
    });
    if let Some(thrown) = pending {
        req.release(Value::array(copy));
        return Err(thrown);
    }
    let mut kept = 0;
    for at in 1..order.len() {
        let (last, this) = (order[kept], order[at]);
        if compared(req, flags, &entries[last].1, &entries[this].1)? != Ordering::Equal {
            kept = at;
            continue;
        }
        let gone = if last > this {
            kept = at;
            last
        } else {
            this
        };
        copy.unset(req, &entries[gone].0)?;
    }
    Ok(Value::array(copy))
}

/// The `SORT_STRING` arm: a set of the strings seen, and a new array of
/// the first element to spell each, under its own key. A reference
/// with one owner is followed to its value, as `ZVAL_DEREF` follows it
/// there; every other value is written as it is, one more owner.
fn unique_by_string(req: &mut Request, array: PhpArray) -> Result<PhpArray, Thrown> {
    let mut seen = std::collections::HashSet::new();
    let mut out = PhpArray::new(req)?;
    for (key, value) in array.snapshot_refs() {
        let text = to_string(req, &value)?;
        let bytes = text.as_bytes().to_vec();
        req.release(Value::string(text));
        if !seen.insert(bytes) {
            continue;
        }
        let value = match value.as_php_ref() {
            Some(reference) if value.refcount() == 1 => reference.get(),
            _ => value,
        };
        req.addref(&value);
        if let Key::Str(s) = &key {
            req.addref(&Value::string(*s));
        }
        out.set(req, key, value)?;
    }
    Ok(out)
}

/// `php_get_data_compare_func_unstable` (`Z/ext/standard/array.c`),
/// applied: the comparison `$flags` names, `SORT_FLAG_CASE` read off
/// it first, and `zend_compare` for `SORT_REGULAR` and any value the
/// switch has no arm for. The enum grouping php adds beside
/// `zend_compare` is not here: two enum cases compare as objects do.
fn compared(req: &mut Request, flags: i64, a: &Value, b: &Value) -> Result<Ordering, Thrown> {
    let fold = flags & SORT_FLAG_CASE != 0;
    match flags & !SORT_FLAG_CASE {
        SORT_NUMERIC => {
            let x = to_double(req, a)?;
            let y = to_double(req, b)?;
            Ok(if x < y {
                Ordering::Less
            } else if x > y {
                Ordering::Greater
            } else {
                Ordering::Equal
            })
        }
        SORT_STRING => as_strings(req, a, b, |x, y| {
            if fold {
                x.iter()
                    .map(u8::to_ascii_lowercase)
                    .cmp(y.iter().map(u8::to_ascii_lowercase))
            } else {
                x.cmp(y)
            }
        }),
        SORT_NATURAL => as_strings(req, a, b, |x, y| strnatcmp(x, y, fold)),
        SORT_LOCALE_STRING => as_strings(req, a, b, <[u8]>::cmp),
        _ => compare(req, a, b),
    }
}

/// Both values as strings (`zval_get_tmp_string`), compared by `by`,
/// the strings given up after.
fn as_strings(
    req: &mut Request,
    a: &Value,
    b: &Value,
    by: impl Fn(&[u8], &[u8]) -> Ordering,
) -> Result<Ordering, Thrown> {
    let x = to_string(req, a)?;
    let y = match to_string(req, b) {
        Ok(y) => y,
        Err(thrown) => {
            req.release(Value::string(x));
            return Err(thrown);
        }
    };
    let ordering = by(x.as_bytes(), y.as_bytes());
    req.release(Value::string(x));
    req.release(Value::string(y));
    Ok(ordering)
}

// -------------------------------------------------- zend_sort

/// `zend_sort` (`Z/Zend/zend_sort.c:248`), libc++'s hybrid as php
/// carries it: insertion sort at sixteen elements or fewer, else a
/// pivot chosen by a three-element network, five past a thousand,
/// swapped to the second slot and partitioned, the smaller side
/// recursed and the larger looped. The element order this leaves is
/// php's for any comparison, transitive or not.
fn zend_sort<T>(v: &mut [T], cmp: &mut dyn FnMut(&T, &T) -> Ordering) {
    let mut base = 0;
    let mut n = v.len();
    loop {
        if n <= 16 {
            insert_sort(&mut v[base..base + n], cmp);
            return;
        }
        let start = base;
        let end = base + n;
        let offset = n >> 1;
        let mut pivot = start + offset;
        if n >> 10 != 0 {
            let delta = offset >> 1;
            sort5(
                v,
                [start, start + delta, pivot, pivot + delta, end - 1],
                cmp,
            );
        } else {
            sort3(v, [start, pivot, end - 1], cmp);
        }
        v.swap(start + 1, pivot);
        pivot = start + 1;
        let mut i = pivot + 1;
        let mut j = end - 1;
        'partition: loop {
            while greater(v, pivot, i, cmp) {
                i += 1;
                if i == j {
                    break 'partition;
                }
            }
            j -= 1;
            if j == i {
                break 'partition;
            }
            while greater(v, j, pivot, cmp) {
                j -= 1;
                if j == i {
                    break 'partition;
                }
            }
            v.swap(i, j);
            i += 1;
            if i == j {
                break 'partition;
            }
        }
        v.swap(pivot, i - 1);
        if (i - 1) - start < end - i {
            zend_sort(&mut v[start..i - 1], cmp);
            base = i;
            n = end - i;
        } else {
            zend_sort(&mut v[i..end], cmp);
            n = i - start - 1;
        }
    }
}

/// `cmp(a, b) > 0`.
fn greater<T>(v: &[T], a: usize, b: usize, cmp: &mut dyn FnMut(&T, &T) -> Ordering) -> bool {
    cmp(&v[a], &v[b]) == Ordering::Greater
}

/// `zend_insert_sort` (`Z/Zend/zend_sort.c:84`): the networks up to
/// five elements, and past them an insertion sort that walks the first
/// six back one slot at a time and the rest two at a time.
fn insert_sort<T>(v: &mut [T], cmp: &mut dyn FnMut(&T, &T) -> Ordering) {
    match v.len() {
        0 | 1 => {}
        2 => sort2(v, 0, 1, cmp),
        3 => sort3(v, [0, 1, 2], cmp),
        4 => sort4(v, [0, 1, 2, 3], cmp),
        5 => sort5(v, [0, 1, 2, 3, 4], cmp),
        len => {
            let sentry = 6;
            for i in 1..sentry {
                let mut j = i - 1;
                if !greater(v, j, i, cmp) {
                    continue;
                }
                while j != 0 {
                    j -= 1;
                    if !greater(v, j, i, cmp) {
                        j += 1;
                        break;
                    }
                }
                slide(v, i, j);
            }
            for i in sentry..len {
                let mut j = i - 1;
                if !greater(v, j, i, cmp) {
                    continue;
                }
                loop {
                    j -= 2;
                    if !greater(v, j, i, cmp) {
                        j += 1;
                        if !greater(v, j, i, cmp) {
                            j += 1;
                        }
                        break;
                    }
                    if j == 0 {
                        break;
                    }
                    if j == 1 {
                        j -= 1;
                        if greater(v, i, j, cmp) {
                            j += 1;
                        }
                        break;
                    }
                }
                slide(v, i, j);
            }
        }
    }
}

/// The swap loop that moves the element at `from` down to `to`.
fn slide<T>(v: &mut [T], from: usize, to: usize) {
    let mut k = from;
    while k > to {
        v.swap(k, k - 1);
        k -= 1;
    }
}

/// `zend_sort_2`.
fn sort2<T>(v: &mut [T], a: usize, b: usize, cmp: &mut dyn FnMut(&T, &T) -> Ordering) {
    if greater(v, a, b, cmp) {
        v.swap(a, b);
    }
}

/// `zend_sort_3`.
fn sort3<T>(v: &mut [T], [a, b, c]: [usize; 3], cmp: &mut dyn FnMut(&T, &T) -> Ordering) {
    if !greater(v, a, b, cmp) {
        if !greater(v, b, c, cmp) {
            return;
        }
        v.swap(b, c);
        if greater(v, a, b, cmp) {
            v.swap(a, b);
        }
        return;
    }
    if !greater(v, c, b, cmp) {
        v.swap(a, c);
        return;
    }
    v.swap(a, b);
    if greater(v, b, c, cmp) {
        v.swap(b, c);
    }
}

/// `zend_sort_4`, over the four positions `at`.
fn sort4<T>(v: &mut [T], at: [usize; 4], cmp: &mut dyn FnMut(&T, &T) -> Ordering) {
    sort3(v, [at[0], at[1], at[2]], cmp);
    if greater(v, at[2], at[3], cmp) {
        v.swap(at[2], at[3]);
        if greater(v, at[1], at[2], cmp) {
            v.swap(at[1], at[2]);
            if greater(v, at[0], at[1], cmp) {
                v.swap(at[0], at[1]);
            }
        }
    }
}

/// `zend_sort_5`, over the five positions `at`.
fn sort5<T>(v: &mut [T], at: [usize; 5], cmp: &mut dyn FnMut(&T, &T) -> Ordering) {
    sort4(v, [at[0], at[1], at[2], at[3]], cmp);
    if greater(v, at[3], at[4], cmp) {
        v.swap(at[3], at[4]);
        if greater(v, at[2], at[3], cmp) {
            v.swap(at[2], at[3]);
            if greater(v, at[1], at[2], cmp) {
                v.swap(at[1], at[2]);
                if greater(v, at[0], at[1], cmp) {
                    v.swap(at[0], at[1]);
                }
            }
        }
    }
}

// -------------------------------------------------- strnatcmp_ex

/// The byte at `at`, or the terminator php's strings carry past their
/// end, which is what `*++ap` reads there.
fn at(s: &[u8], at: usize) -> u8 {
    s.get(at).copied().unwrap_or(0)
}

/// `strnatcmp_ex` (`Z/ext/standard/strnatcmp.c:88`): Martin Pool's
/// natural order as php carries it. Leading zeros are skipped once,
/// stretches of whitespace are skipped, a stretch of digits on both sides is
/// compared as a number, fractionally when either starts with a zero,
/// and everything else byte by byte, upper-cased under `fold`.
fn strnatcmp(a: &[u8], b: &[u8], fold: bool) -> Ordering {
    if a.is_empty() || b.is_empty() {
        return a.len().cmp(&b.len());
    }
    let (mut ap, mut bp) = (0, 0);
    let (mut ca, mut cb) = (a[0], b[0]);
    while ca == b'0' && ap + 1 < a.len() && a[ap + 1].is_ascii_digit() {
        ap += 1;
        ca = a[ap];
    }
    while cb == b'0' && bp + 1 < b.len() && b[bp + 1].is_ascii_digit() {
        bp += 1;
        cb = b[bp];
    }
    loop {
        while is_space(ca) {
            ap += 1;
            ca = at(a, ap);
        }
        while is_space(cb) {
            bp += 1;
            cb = at(b, bp);
        }
        if ca.is_ascii_digit() && cb.is_ascii_digit() {
            let fractional = ca == b'0' || cb == b'0';
            let result = if fractional {
                compare_left(a, &mut ap, b, &mut bp)
            } else {
                compare_right(a, &mut ap, b, &mut bp)
            };
            if result != Ordering::Equal {
                return result;
            } else if ap == a.len() && bp == b.len() {
                return Ordering::Equal;
            } else if ap == a.len() {
                return Ordering::Less;
            } else if bp == b.len() {
                return Ordering::Greater;
            }
            ca = a[ap];
            cb = b[bp];
        }
        if fold {
            ca = ca.to_ascii_uppercase();
            cb = cb.to_ascii_uppercase();
        }
        if ca != cb {
            return ca.cmp(&cb);
        }
        ap += 1;
        bp += 1;
        if ap >= a.len() && bp >= b.len() {
            return Ordering::Equal;
        } else if ap >= a.len() {
            return Ordering::Less;
        } else if bp >= b.len() {
            return Ordering::Greater;
        }
        ca = a[ap];
        cb = b[bp];
    }
}

/// `isspace` in the C locale.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | 0x0B | 0x0C | b'\r')
}

/// Whether the position is past the string or off a digit.
fn digit_ended(s: &[u8], p: usize) -> bool {
    p == s.len() || !s[p].is_ascii_digit()
}

/// `compare_right`: the longer stretch of digits wins, and at equal length
/// the first difference, remembered as the bias while the stretches go on.
fn compare_right(a: &[u8], ap: &mut usize, b: &[u8], bp: &mut usize) -> Ordering {
    let mut bias = Ordering::Equal;
    loop {
        let (a_done, b_done) = (digit_ended(a, *ap), digit_ended(b, *bp));
        if a_done && b_done {
            return bias;
        } else if a_done {
            return Ordering::Less;
        } else if b_done {
            return Ordering::Greater;
        }
        if bias == Ordering::Equal {
            bias = a[*ap].cmp(&b[*bp]);
        }
        *ap += 1;
        *bp += 1;
    }
}

/// `compare_left`: two left-aligned numbers, the first to differ wins.
fn compare_left(a: &[u8], ap: &mut usize, b: &[u8], bp: &mut usize) -> Ordering {
    loop {
        let (a_done, b_done) = (digit_ended(a, *ap), digit_ended(b, *bp));
        if a_done && b_done {
            return Ordering::Equal;
        } else if a_done {
            return Ordering::Less;
        } else if b_done {
            return Ordering::Greater;
        }
        let ordering = a[*ap].cmp(&b[*bp]);
        if ordering != Ordering::Equal {
            return ordering;
        }
        *ap += 1;
        *bp += 1;
    }
}

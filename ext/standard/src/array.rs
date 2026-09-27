//! The natives of `array.c` declared so far, and what they sort with:
//! `sort` and `usort` over the face's `PhpArray::sort_by`, `array_unique`
//! over `zend_sort` itself on an index table of its own, both the one
//! statement of php's hybrid in `php_heap`, because the order it leaves
//! a stretch of equal elements in is what decides which of them survives
//! a comparison that is not transitive; and `strnatcmp_ex`
//! (`Z/ext/standard/strnatcmp.c`) for `SORT_NATURAL`.

use std::cmp::Ordering;

use php_ext::abi::{Callable, FromArgument, Instance, Raw, Ref, Union, Variadic};
use php_ext::attr::{php_class, php_const, php_function};
use php_ext::call::call;
use php_ext::coerce::{compare, to_double, to_long, to_string};
use php_ext::raising::{Level, Thrown, arg_value_error, docref};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, Value, ValueRef, zend_sort};

use constants::{
    COUNT_NORMAL, COUNT_RECURSIVE, SORT_FLAG_CASE, SORT_LOCALE_STRING, SORT_NATURAL, SORT_NUMERIC,
    SORT_STRING,
};

pub(crate) mod keys;
pub(crate) mod make;
pub(crate) mod pointer;
pub(crate) mod set;
pub(crate) mod sort;
pub(crate) mod stack;
pub(crate) mod symbols;
pub(crate) mod walk;

/// The constants of `array.c`'s part of the stub, in its order: the
/// `EXTR_*` modes `extract` reads, the `SORT_*` flags, the `CASE_*` pair
/// `array_change_key_case` reads, `count`'s two modes and
/// `array_filter`'s two.
///
/// ```php
/// var_dump(SORT_ASC, SORT_DESC, SORT_REGULAR, SORT_NUMERIC, SORT_STRING, SORT_LOCALE_STRING, SORT_NATURAL, SORT_FLAG_CASE);
/// var_dump(EXTR_OVERWRITE, EXTR_REFS, CASE_LOWER, CASE_UPPER, COUNT_NORMAL, COUNT_RECURSIVE, ARRAY_FILTER_USE_BOTH, ARRAY_FILTER_USE_KEY);
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
/// int(0)
/// int(256)
/// int(0)
/// int(1)
/// int(0)
/// int(1)
/// int(1)
/// int(2)
/// ```
#[php_const]
pub(crate) mod constants {
    pub(crate) const EXTR_OVERWRITE: i64 = 0;
    pub(crate) const EXTR_SKIP: i64 = 1;
    pub(crate) const EXTR_PREFIX_SAME: i64 = 2;
    pub(crate) const EXTR_PREFIX_ALL: i64 = 3;
    pub(crate) const EXTR_PREFIX_INVALID: i64 = 4;
    pub(crate) const EXTR_PREFIX_IF_EXISTS: i64 = 5;
    pub(crate) const EXTR_IF_EXISTS: i64 = 6;
    pub(crate) const EXTR_REFS: i64 = 0x100;
    /// `PHP_SORT_*` (`Z/ext/standard/php_array.h`), what a sort native's
    /// `$flags` is made of.
    pub(crate) const SORT_ASC: i64 = 4;
    pub(crate) const SORT_DESC: i64 = 3;
    pub(crate) const SORT_REGULAR: i64 = 0;
    pub(crate) const SORT_NUMERIC: i64 = 1;
    pub(crate) const SORT_STRING: i64 = 2;
    pub(crate) const SORT_LOCALE_STRING: i64 = 5;
    pub(crate) const SORT_NATURAL: i64 = 6;
    pub(crate) const SORT_FLAG_CASE: i64 = 8;
    pub(crate) const CASE_LOWER: i64 = 0;
    pub(crate) const CASE_UPPER: i64 = 1;
    /// `PHP_COUNT_NORMAL` and `PHP_COUNT_RECURSIVE`, what `count`'s
    /// `$mode` is one of.
    pub(crate) const COUNT_NORMAL: i64 = 0;
    pub(crate) const COUNT_RECURSIVE: i64 = 1;
    pub(crate) const ARRAY_FILTER_USE_BOTH: i64 = 1;
    pub(crate) const ARRAY_FILTER_USE_KEY: i64 = 2;
}

/// Core's `Countable`, what `count` takes beside an array.
#[php_class(extern)]
pub(crate) struct Countable;

/// The value a body built, returned under `T`'s declaration as it is:
/// php verifies an internal return only in debug builds.
fn returned<T: FromArgument>(req: &mut Request, value: Value) -> Result<Raw<T>, Thrown> {
    <Raw<T> as FromArgument>::from_value(req, value)
}

/// `zval_add_ref` on an element being copied into a new array: a
/// reference with no other owner is followed to its value, which is
/// what the copy holds, and anything else is one more owner.
fn copied(req: &mut Request, value: Value) -> Value {
    let value = match value.as_ref() {
        ValueRef::Reference(reference) if value.refcount() == 1 => reference.get(),
        _ => value,
    };
    req.addref(&value);
    value
}

/// A key written into another array: a string key one more owner, as
/// `zend_hash_add_new` takes it.
fn key_copy(req: &mut Request, key: &Key) -> Key {
    if let Key::Str(s) = key {
        req.addref(&Value::string(*s));
    }
    key.clone()
}

/// `array_merge(array ...$arrays): array`, php's `PHP_FUNCTION(array_merge)`
/// over `php_array_merge_wrapper` (`Z/ext/standard/array.c:4268`) and
/// `php_array_merge` (`Z/ext/standard/array.c:4113`): `Z_PARAM_VARIADIC`
/// and then each argument held to being an array in turn, the
/// `TypeError` naming its number and no parameter name, since the
/// variadic's slots have none; no argument is the empty array; of two
/// arguments one of which is empty, the other is handed back as it is
/// with one more owner where nothing would be renumbered, which is a
/// packed array without holes or one keyed by strings alone; and
/// otherwise a new array the first argument's elements are laid into,
/// a string key kept and an integer key given the next index, and each
/// later argument's the same way over it, a string key writing in
/// place where it is already there (`zend_hash_update`). A reference
/// with no other owner is followed into the copy (`Z_ISREF_P` with
/// `Z_REFCOUNT_P` one) and any other is kept, one more owner.
///
/// ```php
/// var_dump(array_merge());
/// var_dump(array_merge([3 => 'a', 'k' => 'b'], ['k' => 'c', 9 => 'd'], [], [true]));
/// var_dump(array_merge([5 => 'x'], []));
/// $a = 1; $r = []; $r[0] = &$a; $m = array_merge($r, [2]); $a = 3; var_dump($m);
/// $b = memory_get_usage(); $e = array_merge([1, 2], []); var_dump(memory_get_usage() - $b);
/// ```
/// ```expect
/// array(0) {
/// }
/// array(4) {
///   [0]=>
///   string(1) "a"
///   ["k"]=>
///   string(1) "c"
///   [1]=>
///   string(1) "d"
///   [2]=>
///   bool(true)
/// }
/// array(1) {
///   [0]=>
///   string(1) "x"
/// }
/// array(2) {
///   [0]=>
///   &int(3)
///   [1]=>
///   int(2)
/// }
/// int(0)
/// ```
///
/// The declaration, as `basic_functions.stub.php` has it: one
/// variadic, so a named argument it does not declare rides on the
/// frame and `Z_PARAM_VARIADIC` refuses it, which
/// `call_user_func_array`'s witness pins.
///
/// ```php
/// echo new ReflectionFunction('array_merge');
/// ```
/// ```expect
/// Function [ <internal:standard> function array_merge ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <optional> array ...$arrays ]
///   }
///   - Return [ array ]
/// }
/// ```
///
/// ```php
/// var_dump(array_merge(['a' => 1], [1], 'x'));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: array_merge(): Argument #3 must be of type array, string given in %s:2
/// Stack trace:
/// #0 %s(2): array_merge(Array, Array, 'x')
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function]
fn array_merge(req: &mut Request, arrays: Variadic<PhpArray>) -> Result<Raw<PhpArray>, Thrown> {
    let arrays = arrays.0;
    if arrays.is_empty() {
        let empty = Value::array(PhpArray::new(req)?);
        return returned(req, empty);
    }
    if let [first, second] = arrays[..] {
        let kept = if first.is_empty() {
            Some(second)
        } else if second.is_empty() {
            Some(first)
        } else {
            None
        };
        // `HT_IS_PACKED` without holes, or every key a string: what a
        // merge would lay down as it stands.
        if let Some(kept) = kept
            && (kept.is_list() || kept.iter().all(|(key, _)| matches!(key, Key::Str(_))))
        {
            let value = Value::array(kept);
            req.addref(&value);
            return returned(req, value);
        }
    }
    let mut merged = PhpArray::new(req)?;
    for array in arrays {
        for (key, value) in array.snapshot_refs() {
            // `Z_ISREF_P(src_entry) && Z_REFCOUNT_P(src_entry) == 1`.
            let value = match value.as_ref() {
                ValueRef::Reference(_) if value.refcount() == 1 => value.deref(),
                _ => value,
            };
            req.addref(&value);
            match key {
                Key::Str(name) => {
                    req.addref(&Value::string(name));
                    merged.set(req, Key::Str(name), value)?;
                }
                Key::Int(_) => merged.append(req, value)?,
            }
        }
    }
    returned(req, Value::array(merged))
}

/// `php_array_user_compare_unstable` (`Z/ext/standard/array.c:826`):
/// the callback over the two values, its answer read as `php_get_long`
/// reads it and normalised to a sign, `ZEND_NORMALIZE_BOOL`. A bool is
/// the deprecation, raised once per sort as `ARRAYG(compare_deprecation_thrown)`
/// holds it, and `false` is the callback asked again with the operands
/// swapped and that answer's sign reversed, since `$a > $b` returning
/// `false` says nothing about which is less.
fn user_compare(
    req: &mut Request,
    callback: &Callable,
    deprecated: &mut bool,
    a: &Value,
    b: &Value,
) -> Result<Ordering, Thrown> {
    let answer = call(req, callback, &[*a, *b])?;
    if let ValueRef::Bool(truth) = answer.as_ref() {
        if !*deprecated {
            docref(
                req,
                Level::Deprecated,
                "Returning bool from comparison function is deprecated, return an integer less than, equal to, or greater than zero",
            );
            *deprecated = true;
        }
        if !truth {
            let swapped = call(req, callback, &[*b, *a])?;
            let sign = answer_sign(req, swapped)?;
            return Ok(sign.reverse());
        }
    }
    answer_sign(req, answer)
}

/// `ZEND_NORMALIZE_BOOL(php_get_long(&retval))`, the answer given up.
fn answer_sign(req: &mut Request, answer: Value) -> Result<Ordering, Thrown> {
    let long = to_long(req, &answer);
    req.release(answer);
    Ok(long?.cmp(&0))
}

/// What `Z_PARAM_ARRAY_EX(array, 0, 1)` on a by-reference parameter
/// separated: the array in the caller's reference, written in place, or
/// a copy of an argument that came by value, `call_user_func('sort', $a)`
/// past its warning, which is sorted and given up, as php's sort of
/// the argument slot is read by nobody.
struct SortTarget {
    array: PhpArray,
    copied: bool,
}

impl SortTarget {
    /// The copy given up; the caller's array left where the caller
    /// reads it.
    fn finish(self, req: &mut Request) {
        if self.copied {
            req.release(self.array.to_value());
        }
    }
}

/// `Z_PARAM_ARRAY_EX(array, 0, 1)` at `position`: php's `TypeError` for
/// anything but an array, `ZVAL_DEREF` and `SEPARATE_ZVAL_NOREF` on the
/// reference the argument is, so that what the native sorts is what the
/// caller holds. An argument that is no reference is copied, which is
/// what php's separation of the argument slot comes to for every such
/// argument but a temporary with one owner.
fn array_by_reference(req: &mut Request, given: &Ref<PhpArray>) -> Result<SortTarget, Thrown> {
    if let Some(array) = given.separated(req)? {
        return Ok(SortTarget {
            array,
            copied: false,
        });
    }
    let array = match given.get() {
        Some(array) => array.copy(req)?,
        None => PhpArray::new(req)?,
    };
    Ok(SortTarget {
        array,
        copied: true,
    })
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
///
/// ```php
/// echo new ReflectionFunction('array_unique');
/// ```
/// ```expect
/// Function [ <internal:standard> function array_unique ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array $array ]
///     Parameter #1 [ <optional> int $flags = SORT_STRING ]
///   }
///   - Return [ array ]
/// }
/// ```
#[php_function]
fn array_unique(
    req: &mut Request,
    array: PhpArray,
    #[php(default = SORT_STRING)] flags: i64,
) -> Result<PhpArray, Thrown> {
    if array.len() <= 1 {
        req.addref(&Value::array(array));
        return Ok(array);
    }
    if flags == SORT_STRING {
        return unique_by_string(req, array);
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
    Ok(copy)
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
/// switch has no arm for, with the enum grouping php adds beside it
/// (`php_array_data_compare_unstable_i`): where `zend_compare` finds
/// an enum case on the right uncomparable, two cases are ordered by
/// identity and anything else goes before the case. php orders two
/// cases by their addresses and this engine by their object ids,
/// which groups them the same way.
fn compared(req: &mut Request, flags: i64, a: &Value, b: &Value) -> Result<Ordering, Thrown> {
    let fold = flags & SORT_FLAG_CASE != 0;
    match flags & !SORT_FLAG_CASE {
        SORT_NUMERIC => {
            // `numeric_compare_function`: `ZEND_THREEWAY_COMPARE` over
            // `zval_get_double` of each.
            let x = to_double(req, a)?;
            let y = to_double(req, b)?;
            Ok(sort::threeway(x, y))
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
        SORT_LOCALE_STRING => {
            as_strings(req, a, b, |x, y| sort::c_string(x).cmp(sort::c_string(y)))
        }
        _ => {
            let ordering = compare(req, a, b)?;
            // `result == ZEND_UNCOMPARABLE` with an enum case on the
            // right: two cases grouped by identity, anything else put
            // before the case.
            if ordering == Ordering::Greater
                && let Some(right) = enum_case(req, b)
            {
                return Ok(match enum_case(req, a) {
                    Some(left) => left.cmp(&right),
                    None => Ordering::Less,
                });
            }
            Ok(ordering)
        }
    }
}

/// The identity of the enum case `value` holds, a reference followed;
/// none for anything else.
fn enum_case(req: &Request, value: &Value) -> Option<u32> {
    let value = value.deref();
    let object = value.as_object()?;
    let decl = req.class_decl(object.class())?;
    matches!(decl.kind, php_ext::decl::ClassKind::Enum).then(|| object.index())
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

/// `count(Countable|array $value, int $mode = COUNT_NORMAL): int` and
/// its alias `sizeof`, php's `PHP_FUNCTION(count)`
/// (`Z/ext/standard/array.c:648`): the one-argument call is `ZEND_COUNT`
/// over the engine's kernel and never reaches here, so what does is the
/// call with a `$mode`. One that is neither constant is php's
/// `ValueError`, raised before the value is looked at; then the kernel's
/// arm through the face, `php_ext::coerce::count`, which is the element
/// count or the `TypeError`, and under `COUNT_RECURSIVE` the count of
/// every nested array added in (`php_count_recursive`), a reference
/// followed to what it holds. The `TypeError` names the function as it
/// was called, `sizeof` for the alias, since php reads the name off the
/// active function.
///
/// ```php
/// var_dump(count(token_get_all("<?php 1;"), COUNT_RECURSIVE), sizeof(token_get_all("<?php 1;"), 1), count(token_get_all("<?php 1;"), 0), count(token_get_all("<?php 1;"), true), count(token_get_all("<?php 1;"), "1"), count(token_get_all("<?php 1;"), null));
/// ```
/// ```expect
/// Deprecated: count(): Passing null to parameter #2 ($mode) of type int is deprecated in %s on line 2
/// int(9)
/// int(9)
/// int(3)
/// int(9)
/// int(9)
/// int(3)
/// ```
///
/// ```php
/// var_dump(count("x", 2));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ValueError: count(): Argument #2 ($mode) must be either COUNT_NORMAL or COUNT_RECURSIVE in %s:2
/// Stack trace:
/// #0 %s(2): count('x', 2)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// var_dump(sizeof(false, 0));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: sizeof(): Argument #1 ($value) must be of type Countable|array, false given in %s:2
/// Stack trace:
/// #0 %s(2): sizeof(false, 0)
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function(aliases(sizeof))]
fn count(
    req: &mut Request,
    value: Raw<Union<Instance<Countable>, PhpArray>>,
    #[php(default = COUNT_NORMAL)] mode: i64,
) -> Result<i64, Thrown> {
    // `Z_PARAM_ZVAL`: the value's type is the body's to refuse, after
    // the mode.
    let value = value.value();
    if mode != COUNT_NORMAL && mode != COUNT_RECURSIVE {
        return Err(arg_value_error(
            req,
            2,
            "be either COUNT_NORMAL or COUNT_RECURSIVE",
        ));
    }
    match value.deref().as_array() {
        Some(array) if mode == COUNT_RECURSIVE => Ok(count_recursive(req, *array)),
        _ => php_ext::coerce::count(req, &value),
    }
}

/// `php_count_recursive`: the elements, and every nested array's
/// elements counted the same way, a reference followed to what it
/// holds, and the walk guarded as php guards it, so an array that
/// holds itself is the warning `Recursion detected` and a zero where
/// the walk meets itself again.
fn count_recursive(req: &mut Request, array: PhpArray) -> i64 {
    if array.is_recursive() {
        docref(req, Level::Warning, "Recursion detected");
        return 0;
    }
    array.protect();
    let mut count = i64::try_from(array.len()).unwrap_or(i64::MAX);
    for (_, element) in &array {
        if let Some(nested) = element.deref().as_array() {
            count = count.saturating_add(count_recursive(req, *nested));
        }
    }
    array.unprotect();
    count
}

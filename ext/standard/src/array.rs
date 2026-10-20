//! The natives of `array.c` declared so far, and what they sort with:
//! `sort` and `usort` over the face's `PhpArray::sort_by`, `array_unique`
//! over `zend_sort` itself on an index table of its own, both the one
//! statement of php's hybrid in `php_heap`, because the order it leaves
//! a stretch of equal elements in is what decides which of them survives
//! a comparison that is not transitive; and `strnatcmp_ex`
//! (`Z/ext/standard/strnatcmp.c`) for `SORT_NATURAL`.

use std::cmp::Ordering;
use std::ops::ControlFlow;

use php_ext::abi::{Callable, FromArgument, Instance, Raw, Ref, Union, Variadic};
use php_ext::attr::{php_class, php_const, php_function};
use php_ext::call::call;
use php_ext::coerce::{compare, to_double, to_long, to_string, with_string};
use php_ext::raising::{Level, Thrown, arg_value_error, call_stack_end, docref};
use php_ext::request::Request;
use php_ext::values::walk::{self as rows, Hold};
use php_ext::values::{Key, Owner, PhpArray, Value, ValueRef, zend_sort};

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

/// `HT_MAX_SIZE` on a 64-bit build (`Z/Zend/zend_types.h`): the most
/// elements a table may be sized for.
const HT_MAX_SIZE: u64 = 0x4000_0000;

/// `php_array_merge_wrapper`'s count (`Z/ext/standard/array.c:4268`):
/// the elements of every array added up before anything is merged, and
/// php's `Error` where they reach `HT_MAX_SIZE`.
fn merge_total(req: &mut Request, arrays: &[PhpArray]) -> Result<(), Thrown> {
    let count: u64 = arrays.iter().map(|array| array.len() as u64).sum();
    if count >= HT_MAX_SIZE {
        return Err(php_ext::raising::throw_error(
            req,
            "Error",
            &format!("The total number of elements must be lower than {HT_MAX_SIZE}"),
        ));
    }
    Ok(())
}

/// The value a body built, returned under `T`'s declaration as it is:
/// php verifies an internal return only in debug builds.
fn returned<T: FromArgument>(value: Owner<Value>) -> Owner<Raw<T>> {
    value.into_raw()
}

/// `zval_add_ref` on an element being copied into a new array: a
/// reference with no other owner is followed to its value, which is
/// what the copy holds, and anything else is one more owner.
fn copied(req: &Request, value: Value) -> Owner<Value> {
    let value = match value.as_ref() {
        ValueRef::Reference(reference) if value.refcount() == 1 => reference.get(),
        _ => value,
    };
    Owner::take(req, value)
}

/// Rows taken with [`copied`] that no write took, given up: the way out
/// of a write that failed while they were held.
fn give_up_rows(req: &mut Request, rows: impl Iterator<Item = (Key, Owner<Value>)>) {
    for (_, value) in rows {
        value.give_up(req);
    }
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
///
/// The elements of every array are counted before any is merged, and
/// reaching `HT_MAX_SIZE` is php's `Error` (GHSA-h96m-rvf9-jgm2), for
/// `array_merge_recursive` too.
///
/// ```php
/// $r = range(0, 2 ** 20); $all = array_fill(0, 2 ** 10, $r);
/// try { array_merge(...$all); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { array_merge_recursive(...$all); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// The total number of elements must be lower than 1073741824
/// The total number of elements must be lower than 1073741824
/// ```
#[php_function]
fn array_merge(
    req: &mut Request,
    arrays: Variadic<PhpArray>,
) -> Result<Owner<Raw<PhpArray>>, Thrown> {
    let arrays = arrays.0;
    if arrays.is_empty() {
        return Ok(returned(Owner::take(req, Value::array(PhpArray::empty()))));
    }
    merge_total(req, &arrays)?;
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
            && (kept.is_list() || rows::rows(req, kept).all(|(key, _)| matches!(key, Key::Str(_))))
        {
            return Ok(returned(Owner::take(req, Value::array(kept))));
        }
    }
    let mut merged = PhpArray::new(req)?;
    for array in arrays {
        // `php_array_merge`'s `ZEND_HASH_FOREACH_STR_KEY_VAL(src, string_key,
        // src_entry)` (`Z/ext/standard/array.c:4131`), no owner of the table
        // taken: the argument holds it while an update's release serves a
        // destructor.
        rows::foreach(req, array, Hold::None, |req, row| -> rows::Step {
            // `Z_ISREF_P(src_entry) && Z_REFCOUNT_P(src_entry) == 1`.
            let value = copied(req, *row.value());
            match row.key() {
                Key::Str(_) => {
                    let key = Owner::take(req, row.key().clone());
                    merged.set(req, key, value)?;
                }
                Key::Int(_) => merged.append(req, value)?,
            }
            Ok(ControlFlow::Continue(()))
        })?;
    }
    Ok(returned(merged.into_value()))
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
            answer.give_up(req);
            let swapped = call(req, callback, &[*b, *a])?;
            let sign = answer_sign(req, swapped)?;
            return Ok(sign.reverse());
        }
    }
    answer_sign(req, answer)
}

/// `ZEND_NORMALIZE_BOOL(php_get_long(&retval))`, the answer given up.
fn answer_sign(req: &mut Request, answer: Owner<Value>) -> Result<Ordering, Thrown> {
    let long = to_long(req, &answer);
    answer.give_up(req);
    Ok(long?.cmp(&0))
}

/// What `Z_PARAM_ARRAY_EX(array, 0, 1)` on a by-reference parameter
/// separated: the array in the caller's reference, written in place, or
/// a copy of an argument that came by value, `call_user_func('sort', $a)`
/// past its warning, which is sorted and given up, as php's sort of
/// the argument slot is read by nobody.
struct SortTarget {
    array: PhpArray,
    /// The copy's one owner, where the argument came by value: a copy
    /// nobody else holds, which the sort writes in place.
    copy: Option<Owner<PhpArray>>,
}

impl SortTarget {
    /// The copy given up; the caller's array left where the caller
    /// reads it.
    fn finish(self, req: &mut Request) {
        if let Some(copy) = self.copy {
            copy.give_up(req);
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
        return Ok(SortTarget { array, copy: None });
    }
    let copy = match given.get() {
        Some(array) => array.copy(req)?,
        None => PhpArray::new(req)?,
    };
    Ok(SortTarget {
        array: *copy,
        copy: Some(copy),
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
) -> Result<Owner<PhpArray>, Thrown> {
    if array.len() <= 1 {
        return Ok(Owner::take(req, array));
    }
    if flags == SORT_STRING {
        return unique_by_string(req, array);
    }
    // php copies the buckets into `arTmp` (`Z/ext/standard/array.c:5038`),
    // taking no owner, and sorts them under the comparison: the rows read
    // under the borrowed request, views the argument keeps.
    let entries: Vec<(Key, Value)> = rows::rows(req, array).collect();
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
        copy.give_up(req);
        return Err(thrown);
    }
    let mut kept = 0;
    for at in 1..order.len() {
        let (last, this) = (order[kept], order[at]);
        let ordering = match compared(req, flags, &entries[last].1, &entries[this].1) {
            Ok(ordering) => ordering,
            Err(thrown) => {
                copy.give_up(req);
                return Err(thrown);
            }
        };
        if ordering != Ordering::Equal {
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
fn unique_by_string(req: &mut Request, array: PhpArray) -> Result<Owner<PhpArray>, Thrown> {
    let mut seen = std::collections::HashSet::new();
    let mut out = PhpArray::new(req)?;
    // `ZEND_HASH_FOREACH_KEY_VAL(Z_ARRVAL_P(array), num_key, str_key, val)`
    // (`Z/ext/standard/array.c:4993`), no owner of the table taken: the
    // argument holds it while a value's `__toString` runs. A reference is
    // read as held by one owner as php reads its count.
    let walked = rows::foreach(req, array, Hold::None, |req, row| -> rows::Step {
        let bytes = with_string(req, row.value(), <[u8]>::to_vec)?;
        if !seen.insert(bytes) {
            return Ok(ControlFlow::Continue(()));
        }
        let value = copied(req, *row.value());
        let key = row.key().clone();
        out.set(req, Owner::take(req, key), value)?;
        Ok(ControlFlow::Continue(()))
    });
    if let Err(thrown) = walked {
        out.give_up(req);
        return Err(thrown);
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
        SORT_LOCALE_STRING => as_strings(req, a, b, crate::string::locale::collate),
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
    let decl = req.symbols().class_decl(object.class())?;
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
            x.give_up(req);
            return Err(thrown);
        }
    };
    let ordering = by(x.as_bytes(), y.as_bytes());
    x.give_up(req);
    y.give_up(req);
    Ok(ordering)
}

/// `strnatcmp_ex` (`Z/ext/standard/strnatcmp.c:88`) as an ordering, the
/// one in `strnatcmp` behind `strnatcmp()` and `strnatcasecmp()`.
fn strnatcmp(a: &[u8], b: &[u8], fold: bool) -> Ordering {
    crate::strnatcmp::strnatcmp_ex(a, b, fold).cmp(&0)
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
        Some(array) if mode == COUNT_RECURSIVE => count_recursive(req, array),
        _ => php_ext::coerce::count(req, &value),
    }
}

/// `php_count_recursive`: the elements, and every nested array's
/// elements counted the same way, a reference followed to what it
/// holds, and the walk guarded as php guards it, so an array that
/// holds itself is the warning `Recursion detected` and a zero where
/// the walk meets itself again.
///
/// The walk is held to the thread's floor, where php's runs to its end.
fn count_recursive(req: &mut Request, array: PhpArray) -> Result<i64, Thrown> {
    if array.is_recursive() {
        docref(req, Level::Warning, "Recursion detected");
        return Ok(0);
    }
    call_stack_end(req)?;
    array.protect();
    let mut count = i64::try_from(array.len()).unwrap_or(i64::MAX);
    // `ZEND_HASH_FOREACH_VAL` under `GC_PROTECT_RECURSION`, holding
    // nothing: a nested warning can reach a handler.
    let walked = rows::foreach(req, array, Hold::None, |req, row| -> rows::Step {
        if let Some(nested) = row.value().deref().as_array() {
            count = count.saturating_add(count_recursive(req, nested)?);
        }
        Ok(ControlFlow::Continue(()))
    });
    array.unprotect();
    walked.map(|_| count)
}

//! The natives of `array.c` declared so far, and what they sort with:
//! `sort` and `usort` over the face's `PhpArray::sort_by`, `array_unique`
//! over `zend_sort` itself on an index table of its own, both the one
//! statement of php's hybrid in `php_heap`, because the order it leaves
//! a stretch of equal elements in is what decides which of them survives
//! a comparison that is not transitive; and `strnatcmp_ex`
//! (`Z/ext/standard/strnatcmp.c`) for `SORT_NATURAL`.

use std::cmp::Ordering;

use php_ext::abi::Callable;
use php_ext::call::call;
use php_ext::coerce::{coerce_param, compare, to_double, to_long, to_string};
use php_ext::decl::{ConstDecl, ConstExpr, FunctionDecl, Parameter, Type};
use php_ext::raising::{Level, Thrown, arg_type_error, docref};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, Value, ValueRef, zend_sort};

use crate::output::Entry;
use crate::{
    argument_count_between, callable_argument, constant, declared, long_argument, parameter,
};

mod sort;

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

/// The natives declared here, paired with their entries, each with
/// php's arity and parameter names, so `ReflectionFunction::__toString`
/// renders it as the oracle does.
///
/// ```php
/// echo new ReflectionFunction('array_unique'), new ReflectionFunction('sort'), new ReflectionFunction('usort');
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
/// Function [ <internal:standard> function sort ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array &$array ]
///     Parameter #1 [ <optional> int $flags = SORT_REGULAR ]
///   }
///   - Return [ true ]
/// }
/// Function [ <internal:standard> function usort ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array &$array ]
///     Parameter #1 [ <required> callable $callback ]
///   }
///   - Return [ true ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    let flags = |default: &[u8]| Parameter {
        default: Some(ConstExpr::Constant(php_ext::decl::Name(default.into()))),
        ..parameter(b"flags", Type::Int)
    };
    let by_ref_array = || Parameter {
        by_ref: true,
        ..parameter(b"array", Type::Array)
    };
    let mut natives: Vec<(FunctionDecl, Entry)> = vec![
        (
            declared(
                b"sort",
                &[by_ref_array(), flags(b"SORT_REGULAR")],
                Type::True,
            ),
            sort,
        ),
        (
            declared(
                b"usort",
                &[by_ref_array(), parameter(b"callback", Type::Callable)],
                Type::True,
            ),
            usort,
        ),
        (
            declared(
                b"array_unique",
                &[parameter(b"array", Type::Array), flags(b"SORT_STRING")],
                Type::Array,
            ),
            array_unique,
        ),
        (
            declared(
                b"array_merge",
                &[Parameter {
                    variadic: true,
                    ..parameter(b"arrays", Type::Array)
                }],
                Type::Array,
            ),
            array_merge,
        ),
    ];
    natives.extend(sort::natives());
    natives
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
fn array_merge(req: &mut Request) -> Result<Value, Thrown> {
    let arguments = req.variadic_arguments(1)?;
    let mut arrays = Vec::with_capacity(arguments.len());
    for (at, argument) in arguments.iter().enumerate() {
        let Some(array) = argument.deref().as_array().copied() else {
            let position = u32::try_from(at.saturating_add(1)).unwrap_or(u32::MAX);
            let given = php_ext::coerce::value_name(req, argument);
            return Err(arg_type_error(
                req,
                position,
                &format!("be of type array, {given} given"),
            ));
        };
        arrays.push(array);
    }
    if arrays.is_empty() {
        return Ok(Value::array(PhpArray::new(req)?));
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
            return Ok(value);
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
    Ok(Value::array(merged))
}

/// `sort(array &$array, int $flags = SORT_REGULAR): true`, php's
/// `PHP_FUNCTION(sort)` (`Z/ext/standard/array.c:780`): the array the
/// reference holds, separated in the cell, sorted in place by the
/// comparison `$flags` names with the bucket-order fallback
/// (`php_get_data_compare_func`) and renumbered, `zend_hash_sort` with
/// `renumber`. Under `SORT_REGULAR` the comparison is `zend_compare`,
/// which over mixed types is not transitive, and the order it leaves is
/// `zend_sort`'s walk over those answers, php's to the element.
///
/// ```php
/// $a = [1, "1", 2, 2.0, "a", "A", null, "", false, true]; var_dump(sort($a), $a);
/// ```
/// ```expect
/// bool(true)
/// array(10) {
///   [0]=>
///   NULL
///   [1]=>
///   string(0) ""
///   [2]=>
///   bool(false)
///   [3]=>
///   int(1)
///   [4]=>
///   string(1) "1"
///   [5]=>
///   int(2)
///   [6]=>
///   float(2)
///   [7]=>
///   string(1) "A"
///   [8]=>
///   string(1) "a"
///   [9]=>
///   bool(true)
/// }
/// ```
///
/// The other comparisons `$flags` names, and `SORT_FLAG_CASE` read off
/// them.
///
/// ```php
/// $a = [3, "10", 9, "9a", 1.5]; $b = $a; sort($a, SORT_STRING); sort($b, SORT_NUMERIC); $c = ["img12", "img10", "IMG2"]; sort($c, SORT_NATURAL | SORT_FLAG_CASE); echo json_encode($a), json_encode($b), json_encode($c), "\n";
/// ```
/// ```expect
/// [1.5,"10",3,9,"9a"][1.5,3,9,"9a","10"]["IMG2","img10","img12"]
/// ```
///
/// A shared array is copied into the variable before it is sorted, so
/// the other holder keeps its order, `SEPARATE_ZVAL_NOREF`; anything
/// but an array in the variable is php's `TypeError`, and an array
/// written in the call cannot be passed by reference at all.
///
/// ```php
/// $a = [3, 1, 2]; $shared = $a; sort($a); $s = "x"; try { sort($s); } catch (TypeError $e) { echo $e->getMessage(), "\n"; } echo json_encode($shared), json_encode($a), "\n";
/// ```
/// ```expect
/// sort(): Argument #1 ($array) must be of type array, string given
/// [3,1,2][1,2,3]
/// ```
///
/// ```php
/// sort([2, 1]);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: sort(): Argument #1 ($array) could not be passed by reference in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
fn sort(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let mut target = array_by_reference(req, 1)?;
    let flags = if req.argument_count() == 2 {
        long_argument(req, 2)?
    } else {
        SORT_REGULAR
    };
    let sorted = target
        .array
        .sort_by(req, true, |req, (_, a), (_, b)| compared(req, flags, a, b));
    target.finish(req);
    sorted?;
    Ok(Value::bool(true))
}

/// `usort(array &$array, callable $callback): true`, php's
/// `PHP_FUNCTION(usort)` over `php_usort` (`Z/ext/standard/array.c:878`):
/// the array copied first, `zend_array_dup`, so the callback sees the
/// caller's array unchanged while the copy is sorted with
/// `php_array_user_compare`, and the copy written into the caller's
/// variable after, whatever the sort raised. An empty array is left
/// alone, not copied. The callback's answer is read as `php_get_long`
/// reads it and normalised to a sign, so a constant answer of `1` or
/// `-1` never falls back to the bucket order and what is left is
/// `zend_sort`'s walk alone; `0` falls back every time and leaves the
/// array as it was.
///
/// ```php
/// $seven = [1, 2, 3, 4, 5, 6, 7]; usort($seven, fn($a, $b) => 1);
/// $twenty = []; for ($i = 1; $i <= 20; $i++) $twenty[] = $i; $less = $twenty; usort($twenty, fn($a, $b) => 1); usort($less, fn($a, $b) => -1);
/// $zero = [3, 1, 2]; usort($zero, fn($a, $b) => 0);
/// echo json_encode($seven), "\n", json_encode($twenty), "\n", json_encode($less), "\n", json_encode($zero), "\n";
/// ```
/// ```expect
/// [6,7,5,4,3,2,1]
/// [16,14,15,12,13,19,2,8,9,6,7,5,4,3,17,10,18,11,20,1]
/// [1,12,19,18,17,16,15,14,13,11,2,10,9,8,7,6,5,4,3,20]
/// [3,1,2]
/// ```
///
/// A bool answer is the 8.0 deprecation, once per call, and `false` is
/// asked again with the operands swapped; a string answer is read as a
/// long.
///
/// ```php
/// $a = [3, 1, 2]; usort($a, fn($x, $y) => $x > $y); $b = [3, 1, 2]; usort($b, fn($x, $y) => $x < $y); $c = [3, 1, 2]; var_dump(usort($c, fn($x, $y) => "2abc")); echo json_encode($a), json_encode($b), json_encode($c), "\n";
/// ```
/// ```expect
///
/// Deprecated: usort(): Returning bool from comparison function is deprecated, return an integer less than, equal to, or greater than zero in %s on line 2
///
/// Deprecated: usort(): Returning bool from comparison function is deprecated, return an integer less than, equal to, or greater than zero in %s on line 2
/// bool(true)
/// [1,2,3][3,2,1][1,2,3]
/// ```
///
/// A callback that throws ends the calls but not the sort: every
/// comparison after it answers by bucket order, the copy is written
/// into the variable as it stands, and what was thrown is raised from the
/// native.
///
/// ```php
/// $a = [3, 1, 2, 5, 4]; $seen = 0; try { usort($a, function ($x, $y) use (&$seen) { if (++$seen == 3) throw new Exception("stop"); return $x <=> $y; }); } catch (Exception $e) { echo $e->getMessage(), "\n"; } echo json_encode($a), " ", $seen, "\n";
/// ```
/// ```expect
/// stop
/// [1,3,2,5,4] 3
/// ```
///
/// ```php
/// $a = [1]; usort($a, "no_such_function");
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: usort(): Argument #2 ($callback) must be a valid callback, function "no_such_function" not found or invalid function name in %s:2
/// Stack trace:
/// #0 %s(2): usort(Array, 'no_such_functio...')
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn usort(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let given = req.argument(1).unwrap_or_default();
    let coerced = coerce_param(req, &given, &Type::Array, 1)?;
    let callback = callable_argument(req, 2)?;
    let Some(array) = coerced.as_array().copied() else {
        return Ok(Value::bool(true));
    };
    if array.is_empty() {
        return Ok(Value::bool(true));
    }
    let mut copy = array.copy(req)?;
    let mut deprecated = false;
    let sorted = copy.sort_by(req, true, |req, (_, a), (_, b)| {
        user_compare(req, &callback, &mut deprecated, a, b)
    });
    match given.as_ref() {
        ValueRef::Reference(reference) => reference.set(req, copy.to_value()),
        _ => req.release(copy.to_value()),
    }
    sorted?;
    Ok(Value::bool(true))
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
fn array_by_reference(req: &mut Request, position: u32) -> Result<SortTarget, Thrown> {
    let given = req.argument(position).unwrap_or_default();
    let coerced = coerce_param(req, &given, &Type::Array, position)?;
    if let ValueRef::Reference(reference) = given.as_ref()
        && let Some(array) = reference.separate_array(req)?
    {
        return Ok(SortTarget {
            array,
            copied: false,
        });
    }
    let array = match coerced.as_array() {
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

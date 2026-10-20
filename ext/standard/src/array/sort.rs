//! The sort family of `array.c` beside `sort` and `usort`: the value
//! sorts that keep their keys or reverse, the key sorts, the natural
//! sorts and the two user sorts that keep keys, each one
//! `zend_hash_sort` over the face's `PhpArray::sort_by` with the
//! comparison `php_get_data_compare_func` or `php_get_key_compare_func`
//! picks, stable since 8.0 by the position fallback `sort_by` adds, and
//! reversed as php reverses it, the answer negated and the fallback
//! left ascending (`DEFINE_SORT_VARIANTS`).

use std::cmp::Ordering;

use php_ext::abi::{Callable, Ref, True, Untyped, Variadic};
use php_ext::attr::php_function;
use php_ext::coerce::compare;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Key, Owner, PhpArray, Value, walk};

use super::constants::{
    SORT_ASC, SORT_DESC, SORT_FLAG_CASE, SORT_LOCALE_STRING, SORT_NATURAL, SORT_NUMERIC,
    SORT_REGULAR, SORT_STRING,
};
use super::{array_by_reference, compared, strnatcmp, user_compare};
use crate::string::KeyBytes;

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
///
/// ```php
/// echo new ReflectionFunction('sort');
/// ```
/// ```expect
/// Function [ <internal:standard> function sort ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array &$array ]
///     Parameter #1 [ <optional> int $flags = SORT_REGULAR ]
///   }
///   - Return [ true ]
/// }
/// ```
#[php_function]
fn sort(
    req: &mut Request,
    array: Ref<PhpArray>,
    #[php(default = SORT_REGULAR)] flags: i64,
) -> Result<True, Thrown> {
    let mut target = array_by_reference(req, &array)?;
    let sorted = target
        .array
        .sort_by(req, true, |req, (_, a), (_, b)| compared(req, flags, a, b));
    target.finish(req);
    sorted?;
    Ok(True)
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
///
/// ```php
/// echo new ReflectionFunction('usort');
/// ```
/// ```expect
/// Function [ <internal:standard> function usort ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array &$array ]
///     Parameter #1 [ <required> callable $callback ]
///   }
///   - Return [ true ]
/// }
/// ```
#[php_function]
fn usort(req: &mut Request, array: Ref<PhpArray>, callback: Callable) -> Result<True, Thrown> {
    let mut deprecated = false;
    walk::detached(req, &array, |req, copy| {
        copy.sort_by(req, true, |req, (_, a), (_, b)| {
            user_compare(req, &callback, &mut deprecated, a, b)
        })
    })?;
    Ok(True)
}

/// `rsort(array &$array, int $flags = SORT_REGULAR): true`, php's
/// `PHP_FUNCTION(rsort)` (`Z/ext/standard/array.c:801`): `sort` with
/// the reversed comparison, the keys dropped. Two elements that compare
/// equal keep the order they had, since the fallback is not reversed.
///
/// ```php
/// $a = [3 => "c", 1 => "a", 2 => "b"]; var_dump(rsort($a), $a);
/// $b = [1, "1", 2, 2.0, "10", 9]; rsort($b); var_dump($b);
/// $c = ["b10", "B2", "a"]; rsort($c, SORT_STRING | SORT_FLAG_CASE); echo json_encode($c), "\n";
/// ```
/// ```expect
/// bool(true)
/// array(3) {
///   [0]=>
///   string(1) "c"
///   [1]=>
///   string(1) "b"
///   [2]=>
///   string(1) "a"
/// }
/// array(6) {
///   [0]=>
///   string(2) "10"
///   [1]=>
///   int(9)
///   [2]=>
///   int(2)
///   [3]=>
///   float(2)
///   [4]=>
///   int(1)
///   [5]=>
///   string(1) "1"
/// }
/// ["B2","b10","a"]
/// ```
#[php_function]
fn rsort(
    req: &mut Request,
    array: Ref<PhpArray>,
    #[php(default = SORT_REGULAR)] flags: i64,
) -> Result<True, Thrown> {
    data_sort(req, &array, flags, true, true)
}

/// `asort(array &$array, int $flags = SORT_REGULAR): true`, php's
/// `PHP_FUNCTION(asort)` (`Z/ext/standard/array.c:738`): the values
/// ordered by the comparison `$flags` names, each under the key it had,
/// equal values in the order they stood.
///
/// ```php
/// $g = ["x" => 3, "y" => 1, "z" => 2, "w" => 1]; var_dump(asort($g)); echo json_encode($g), "\n";
/// $n = ["a" => "10", "b" => "9", "c" => "9a"]; asort($n, SORT_NUMERIC); echo json_encode($n), "\n"; asort($n, SORT_STRING); echo json_encode($n), "\n";
/// ```
/// ```expect
/// bool(true)
/// {"y":1,"w":1,"z":2,"x":3}
/// {"b":"9","c":"9a","a":"10"}
/// {"a":"10","b":"9","c":"9a"}
/// ```
///
/// `SORT_LOCALE_STRING` orders by `strcoll` under the request's
/// `LC_COLLATE`, byte order under `C`:
///
/// ```php
/// $t = ["NS" => "Nouvelle-\xC9cosse", "ON" => "Ontario", "PE" => "\xCEle-du-Prince-\xC9douard", "QC" => "Qu\xE9bec", "QB" => "Quebed", "MB" => "Manitoba", "AB" => "alberta"];
/// $c = $t; asort($c, SORT_LOCALE_STRING); echo implode(" ", array_keys($c)), "\n";
/// setlocale(LC_ALL, "fr_FR.ISO8859-1", "fr_FR"); asort($t, SORT_LOCALE_STRING); echo implode(" ", array_keys($t)), "\n";
/// ```
/// ```expect locale=fr_FR.ISO8859-1
/// MB NS ON QB QC AB PE
/// AB PE MB NS ON QC QB
/// ```
#[php_function]
fn asort(
    req: &mut Request,
    array: Ref<PhpArray>,
    #[php(default = SORT_REGULAR)] flags: i64,
) -> Result<True, Thrown> {
    data_sort(req, &array, flags, false, false)
}

/// `arsort(array &$array, int $flags = SORT_REGULAR): true`, php's
/// `PHP_FUNCTION(arsort)` (`Z/ext/standard/array.c:759`): `asort`
/// reversed, equal values still in the order they stood.
///
/// ```php
/// $g = ["x" => 3, "y" => 1, "z" => 2, "w" => 1]; var_dump(arsort($g)); echo json_encode($g), "\n";
/// $s = "x"; try { arsort($s); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// {"x":3,"z":2,"y":1,"w":1}
/// arsort(): Argument #1 ($array) must be of type array, string given
/// ```
#[php_function]
fn arsort(
    req: &mut Request,
    array: Ref<PhpArray>,
    #[php(default = SORT_REGULAR)] flags: i64,
) -> Result<True, Thrown> {
    data_sort(req, &array, flags, true, false)
}

/// `php_get_data_compare_func` applied through `zend_hash_sort`: the
/// array the reference holds, separated, sorted by the comparison
/// `$flags` names, reversed where `reverse` says, and renumbered where
/// `renumber` does.
fn data_sort(
    req: &mut Request,
    array: &Ref<PhpArray>,
    flags: i64,
    reverse: bool,
    renumber: bool,
) -> Result<True, Thrown> {
    let mut target = array_by_reference(req, array)?;
    let sorted = target.array.sort_by(req, renumber, |req, (_, a), (_, b)| {
        compared(req, flags, a, b).map(|o| if reverse { o.reverse() } else { o })
    });
    target.finish(req);
    sorted?;
    Ok(True)
}

/// `ksort(array &$array, int $flags = SORT_REGULAR): true`, php's
/// `PHP_FUNCTION(ksort)` (`Z/ext/standard/array.c:589`): the elements
/// ordered by key with `php_get_key_compare_func`. Under `SORT_REGULAR`
/// two integer keys compare as integers, two string keys as
/// `zendi_smart_strcmp` compares them, numerically where both are
/// numeric, and an integer beside a string as `zend_compare` does, so
/// `"a"` sorts after `10` as 8.0 compares a number with a non-numeric
/// string.
///
/// ```php
/// $a = ["b" => 2, "a" => 1, 10 => "x", "9" => "y", "c" => 3, "1e1" => "z"]; var_dump(ksort($a), $a);
/// ```
/// ```expect
/// bool(true)
/// array(6) {
///   [9]=>
///   string(1) "y"
///   [10]=>
///   string(1) "x"
///   ["1e1"]=>
///   string(1) "z"
///   ["a"]=>
///   int(1)
///   ["b"]=>
///   int(2)
///   ["c"]=>
///   int(3)
/// }
/// ```
///
/// Under `SORT_NUMERIC` a string key is read by `zend_strtod`, the
/// longest prefix that is a decimal number after leading whitespace
/// and zero where there is none, so `"0x1A"` and `"inf"` are both zero;
/// `SORT_STRING` compares the keys' bytes, an integer key printed in
/// decimal, and `SORT_FLAG_CASE` lowers ASCII first; `SORT_NATURAL`
/// is `strnatcmp_ex` over the same text.
///
/// ```php
/// $c = ["10" => 1, "9a" => 2, "1e1" => 3, "x" => 4, 5 => 5, "0x1A" => 6, " 7" => 7, "inf" => 8]; ksort($c, SORT_NUMERIC); echo json_encode($c), "\n";
/// $d = ["b" => 1, "B" => 2, "a" => 3, 10 => 4, 9 => 5]; ksort($d, SORT_STRING); echo json_encode($d), "\n"; ksort($d, SORT_STRING | SORT_FLAG_CASE); echo json_encode($d), "\n";
/// $e = ["img12" => 1, "img10" => 2, "IMG2" => 3]; ksort($e, SORT_NATURAL); echo json_encode($e), "\n"; ksort($e, SORT_NATURAL | SORT_FLAG_CASE); echo json_encode($e), "\n";
/// $l = ["b" => 1, "a\0z" => 2, "a" => 3]; ksort($l, SORT_LOCALE_STRING); foreach ($l as $v) echo $v; echo "\n";
/// ```
/// ```expect
/// {"x":4,"0x1A":6,"inf":8,"5":5," 7":7,"9a":2,"10":1,"1e1":3}
/// {"10":4,"9":5,"B":2,"a":3,"b":1}
/// {"10":4,"9":5,"a":3,"B":2,"b":1}
/// {"IMG2":3,"img10":2,"img12":1}
/// {"IMG2":3,"img10":2,"img12":1}
/// 231
/// ```
///
/// The declarations of the sort family.
///
/// ```php
/// foreach (['rsort', 'asort', 'arsort', 'ksort', 'krsort', 'natsort', 'natcasesort', 'uasort', 'uksort'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function rsort ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array &$array ]
///     Parameter #1 [ <optional> int $flags = SORT_REGULAR ]
///   }
///   - Return [ true ]
/// }
/// Function [ <internal:standard> function asort ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array &$array ]
///     Parameter #1 [ <optional> int $flags = SORT_REGULAR ]
///   }
///   - Return [ true ]
/// }
/// Function [ <internal:standard> function arsort ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array &$array ]
///     Parameter #1 [ <optional> int $flags = SORT_REGULAR ]
///   }
///   - Return [ true ]
/// }
/// Function [ <internal:standard> function ksort ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array &$array ]
///     Parameter #1 [ <optional> int $flags = SORT_REGULAR ]
///   }
///   - Return [ true ]
/// }
/// Function [ <internal:standard> function krsort ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array &$array ]
///     Parameter #1 [ <optional> int $flags = SORT_REGULAR ]
///   }
///   - Return [ true ]
/// }
/// Function [ <internal:standard> function natsort ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> array &$array ]
///   }
///   - Return [ true ]
/// }
/// Function [ <internal:standard> function natcasesort ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> array &$array ]
///   }
///   - Return [ true ]
/// }
/// Function [ <internal:standard> function uasort ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array &$array ]
///     Parameter #1 [ <required> callable $callback ]
///   }
///   - Return [ true ]
/// }
/// Function [ <internal:standard> function uksort ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array &$array ]
///     Parameter #1 [ <required> callable $callback ]
///   }
///   - Return [ true ]
/// }
/// ```
#[php_function]
fn ksort(
    req: &mut Request,
    array: Ref<PhpArray>,
    #[php(default = SORT_REGULAR)] flags: i64,
) -> Result<True, Thrown> {
    key_sort(req, &array, flags, false)
}

/// `krsort(array &$array, int $flags = SORT_REGULAR): true`, php's
/// `PHP_FUNCTION(krsort)` (`Z/ext/standard/array.c:568`): `ksort`
/// reversed.
///
/// ```php
/// $a = ["b" => 2, "a" => 1, 10 => "x", "9" => "y", "c" => 3]; var_dump(krsort($a)); echo json_encode($a), "\n";
/// $e = ["img12" => 1, "img10" => 2, "IMG2" => 3]; krsort($e, SORT_NATURAL | SORT_FLAG_CASE); echo json_encode($e), "\n";
/// ```
/// ```expect
/// bool(true)
/// {"c":3,"b":2,"a":1,"10":"x","9":"y"}
/// {"img12":1,"img10":2,"IMG2":3}
/// ```
#[php_function]
fn krsort(
    req: &mut Request,
    array: Ref<PhpArray>,
    #[php(default = SORT_REGULAR)] flags: i64,
) -> Result<True, Thrown> {
    key_sort(req, &array, flags, true)
}

/// `php_get_key_compare_func` through `zend_hash_sort`, the keys kept.
fn key_sort(
    req: &mut Request,
    array: &Ref<PhpArray>,
    flags: i64,
    reverse: bool,
) -> Result<True, Thrown> {
    let mut target = array_by_reference(req, array)?;
    let sorted = target.array.sort_by(req, false, |req, (a, _), (b, _)| {
        key_compare(req, flags, a, b, reverse)
    });
    target.finish(req);
    sorted?;
    Ok(True)
}

/// `natsort(array &$array): true`, php's `php_natsort`
/// (`Z/ext/standard/array.c:705`): the values ordered by `strnatcmp_ex`
/// over their string forms, each under the key it had.
///
/// ```php
/// $h = ["img12.png", "img10.png", "IMG2.png", "img1.png", 3, "03", "x 1", "x  1"]; var_dump(natsort($h)); echo json_encode($h), "\n";
/// ```
/// ```expect
/// bool(true)
/// {"4":3,"5":"03","2":"IMG2.png","3":"img1.png","1":"img10.png","0":"img12.png","6":"x 1","7":"x  1"}
/// ```
#[php_function]
fn natsort(req: &mut Request, array: Ref<PhpArray>) -> Result<True, Thrown> {
    natural_sort(req, &array, false)
}

/// `natcasesort(array &$array): true`, php's `php_natsort` with
/// `fold_case`: `natsort` with ASCII letters compared upper-cased.
///
/// ```php
/// $h = ["img12.png", "img10.png", "IMG2.png", "img1.png"]; var_dump(natcasesort($h)); echo json_encode($h), "\n";
/// ```
/// ```expect
/// bool(true)
/// {"3":"img1.png","2":"IMG2.png","1":"img10.png","0":"img12.png"}
/// ```
///
/// The fold is `toupper(3)`'s under the request's `LC_CTYPE`.
///
/// ```php
/// foreach (['C', 'fr_FR.ISO8859-1'] as $l) { setlocale(LC_CTYPE, $l); $a = ["\xe9a", "\xc9b", "Ea"]; natcasesort($a); echo implode(' ', array_map('bin2hex', $a)), "\n"; }
/// ```
/// ```expect locale=fr_FR.ISO8859-1
/// 4561 c962 e961
/// 4561 e961 c962
/// ```
#[php_function]
fn natcasesort(req: &mut Request, array: Ref<PhpArray>) -> Result<True, Thrown> {
    natural_sort(req, &array, true)
}

fn natural_sort(req: &mut Request, array: &Ref<PhpArray>, fold: bool) -> Result<True, Thrown> {
    let mut target = array_by_reference(req, array)?;
    let flags = SORT_NATURAL | if fold { SORT_FLAG_CASE } else { 0 };
    let sorted = target
        .array
        .sort_by(req, false, |req, (_, a), (_, b)| compared(req, flags, a, b));
    target.finish(req);
    sorted?;
    Ok(True)
}

/// `uasort(array &$array, callable $callback): true`, php's `php_usort`
/// with `php_array_user_compare` and no renumbering
/// (`Z/ext/standard/array.c:920`): `usort` with the keys kept.
///
/// ```php
/// $i = ["x" => 3, "y" => 1, "z" => 2]; var_dump(uasort($i, fn($p, $q) => $p <=> $q)); echo json_encode($i), "\n";
/// $j = ["a" => 1, "b" => 1, "c" => 0]; uasort($j, fn($p, $q) => $p > $q); echo json_encode($j), "\n";
/// ```
/// ```expect
/// bool(true)
/// {"y":1,"z":2,"x":3}
///
/// Deprecated: uasort(): Returning bool from comparison function is deprecated, return an integer less than, equal to, or greater than zero in %s on line 3
/// {"c":0,"a":1,"b":1}
/// ```
#[php_function]
fn uasort(req: &mut Request, array: Ref<PhpArray>, callback: Callable) -> Result<True, Thrown> {
    user_sort(req, &array, &callback, false)
}

/// `uksort(array &$array, callable $callback): true`, php's `php_usort`
/// with `php_array_user_key_compare` (`Z/ext/standard/array.c:987`): the
/// callback handed the two keys, an integer key as an int and a string
/// key as the string, its answer read as `usort` reads it.
///
/// ```php
/// $j = ["b" => 1, "a" => 2, 3 => 3, "10" => 4]; var_dump(uksort($j, function ($p, $q) { echo is_int($p) ? "int " : "string "; return (string)$p <=> (string)$q; })); echo "\n", json_encode($j), "\n";
/// $k = [2 => "a", 1 => "b"]; uksort($k, fn($p, $q) => $p > $q); echo json_encode($k), "\n";
/// ```
/// ```expect
/// string int string string int bool(true)
///
/// {"3":3,"10":4,"a":2,"b":1}
///
/// Deprecated: uksort(): Returning bool from comparison function is deprecated, return an integer less than, equal to, or greater than zero in %s on line 3
/// {"1":"b","2":"a"}
/// ```
#[php_function]
fn uksort(req: &mut Request, array: Ref<PhpArray>, callback: Callable) -> Result<True, Thrown> {
    user_sort(req, &array, &callback, true)
}

/// `php_usort` keeping keys, over the Detached walk
/// ([`walk::detached`]): the callback sees the caller's array
/// unchanged, and the sorted copy is written into the caller's variable
/// after, whatever the sort raised.
fn user_sort(
    req: &mut Request,
    array: &Ref<PhpArray>,
    callback: &Callable,
    by_key: bool,
) -> Result<True, Thrown> {
    let mut deprecated = false;
    walk::detached(req, array, |req, copy| {
        copy.sort_by(req, false, |req, (ka, a), (kb, b)| {
            if by_key {
                let (x, y) = (key_value(ka), key_value(kb));
                // `php_array_user_key_compare_unstable`: the keys, read
                // back as the value comparison reads its answer.
                user_compare(req, callback, &mut deprecated, &x, &y)
            } else {
                user_compare(req, callback, &mut deprecated, a, b)
            }
        })
    })?;
    Ok(True)
}

/// A key as the value php hands a callback or `zend_compare`:
/// `ZVAL_LONG(h)` or `ZVAL_STR(key)`, no owner taken.
pub(super) fn key_value(key: &Key) -> Value {
    match key {
        Key::Int(n) => Value::int(*n),
        Key::Str(s) => Value::string(*s),
    }
}

/// `php_get_key_compare_func` applied, reversed where `reverse` says as
/// `DEFINE_SORT_VARIANTS` reverses: the answer negated, or for the
/// natural comparisons the operands swapped.
pub(super) fn key_compare(
    req: &mut Request,
    flags: i64,
    a: &Key,
    b: &Key,
    reverse: bool,
) -> Result<Ordering, Thrown> {
    let fold = flags & SORT_FLAG_CASE != 0;
    let ordering = match flags & !SORT_FLAG_CASE {
        SORT_NUMERIC => {
            if let (Key::Int(x), Key::Int(y)) = (a, b) {
                long_keys(*x, *y)
            } else {
                threeway(key_double(a), key_double(b))
            }
        }
        SORT_STRING => {
            let (x, y) = (KeyBytes::of(a), KeyBytes::of(b));
            if fold {
                x.iter()
                    .map(u8::to_ascii_lowercase)
                    .cmp(y.iter().map(u8::to_ascii_lowercase))
            } else {
                x.cmp(&y)
            }
        }
        SORT_NATURAL => {
            let (x, y) = (KeyBytes::of(a), KeyBytes::of(b));
            return Ok(if reverse {
                strnatcmp(&y, &x, fold)
            } else {
                strnatcmp(&x, &y, fold)
            });
        }
        SORT_LOCALE_STRING => {
            let (x, y) = (KeyBytes::of(a), KeyBytes::of(b));
            crate::string::locale::collate(&x, &y)
        }
        _ => match (a, b) {
            (Key::Int(x), Key::Int(y)) => long_keys(*x, *y),
            _ => compare(req, &key_value(a), &key_value(b))?,
        },
    };
    Ok(if reverse {
        ordering.reverse()
    } else {
        ordering
    })
}

/// `(zend_long)f->h > (zend_long)s->h ? 1 : -1`: two integer keys are
/// never equal in one table, so php answers less for equal ones.
fn long_keys(x: i64, y: i64) -> Ordering {
    if x > y {
        Ordering::Greater
    } else {
        Ordering::Less
    }
}

/// `ZEND_THREEWAY_COMPARE(a, b)`: equal, less, and greater for
/// everything else, a NaN on either side included.
// php's macro is exact equality, not a margin.
#[allow(clippy::float_cmp)]
pub(super) fn threeway(a: f64, b: f64) -> Ordering {
    if a == b {
        Ordering::Equal
    } else if a < b {
        Ordering::Less
    } else {
        Ordering::Greater
    }
}

/// A key as `SORT_NUMERIC` reads it: an integer key as a double, a string
/// key through `zend_strtod`.
fn key_double(key: &Key) -> f64 {
    match key {
        #[allow(clippy::cast_precision_loss)]
        Key::Int(n) => *n as f64,
        Key::Str(s) => strtod(s.as_bytes()),
    }
}

/// `zend_strtod(s, NULL)` (`Z/Zend/zend_strtod.c:2517`) as php builds it:
/// leading whitespace skipped, a sign, then the longest decimal number
/// with an optional fraction and exponent, correctly rounded, and zero
/// where no digit stands; neither a hexadecimal float nor `inf` or
/// `nan` is read.
pub(super) fn strtod(bytes: &[u8]) -> f64 {
    let mut at = 0;
    while at < bytes.len() && matches!(bytes[at], b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ') {
        at += 1;
    }
    let start = at;
    if at < bytes.len() && matches!(bytes[at], b'+' | b'-') {
        at += 1;
    }
    let digits = at;
    while at < bytes.len() && bytes[at].is_ascii_digit() {
        at += 1;
    }
    let mut seen = at > digits;
    if at < bytes.len() && bytes[at] == b'.' {
        let fraction = at + 1;
        let mut end = fraction;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if seen || end > fraction {
            seen = true;
            at = end;
        }
    }
    if !seen {
        return 0.0;
    }
    if at < bytes.len() && matches!(bytes[at], b'e' | b'E') {
        let mut end = at + 1;
        if end < bytes.len() && matches!(bytes[end], b'+' | b'-') {
            end += 1;
        }
        let exponent = end;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end > exponent {
            at = end;
        }
    }
    std::str::from_utf8(&bytes[start..at])
        .ok()
        .and_then(|text| text.parse::<f64>().ok())
        .unwrap_or(0.0)
}

/// `array_multisort(&$array, &...$rest): true`, php's
/// `PHP_FUNCTION(array_multisort)` (`Z/ext/standard/array.c:6058`): the
/// arrays sorted together as SQL's `ORDER BY` sorts columns, the first
/// array's values deciding and each later one breaking the ties the
/// earlier left, each followed by at most one order and one comparison
/// constant; then every array rewritten in the order found, string keys kept
/// and integer keys renumbered. The rows are sorted by `zend_sort` with
/// php's comparison, the position a row held its last word.
///
/// ```php
/// $data = [3, 1, 2]; $other = ["c", "a", "b"]; var_dump(array_multisort($data, SORT_DESC, $other), $data, $other);
/// $a = [10, 100, 100, 0]; $b = [1, 3, 2, 4]; array_multisort($a, $b); echo json_encode([$a, $b]), "\n";
/// $s = ["10", 9, "1e1", "a" => 1]; array_multisort($s, SORT_STRING); echo json_encode($s), "\n";
/// $e = []; var_dump(array_multisort($e));
/// ```
/// ```expect
/// bool(true)
/// array(3) {
///   [0]=>
///   int(3)
///   [1]=>
///   int(2)
///   [2]=>
///   int(1)
/// }
/// array(3) {
///   [0]=>
///   string(1) "c"
///   [1]=>
///   string(1) "b"
///   [2]=>
///   string(1) "a"
/// }
/// [[0,10,100,100],[4,1,2,3]]
/// {"a":1,"0":"10","1":"1e1","2":9}
/// bool(true)
/// ```
///
/// ```php
/// $a = [1, 2]; $b = [1];
/// try { array_multisort($a, $b); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { array_multisort($a, SORT_ASC, SORT_DESC); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { array_multisort($a, 99); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { array_multisort($a, "x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Array sizes are inconsistent
/// array_multisort(): Argument #3 must be an array or a sort flag that has not already been specified
/// array_multisort(): Argument #2 must be a valid sort flag
/// array_multisort(): Argument #2 must be an array or a sort flag
/// ```
///
/// The sort holds each table while it compares (GH-22678): a comparison
/// that writes the variable separates it, and the variable keeps what
/// the program wrote.
///
/// ```php
/// class E { function __toString(): string { $GLOBALS["a"] = ["gone"]; return "x"; } }
/// $a = [new E, new E, "b"]; var_dump(array_multisort($a, SORT_STRING), $a);
/// ```
/// ```expect
/// bool(true)
/// array(1) {
///   [0]=>
///   string(4) "gone"
/// }
/// ```
// php's body is one function; this one is the parse, and `multisort`
// the sort and the rewrite.
#[allow(clippy::too_many_lines)]
#[php_function]
fn array_multisort(
    req: &mut Request,
    #[php(prefer_ref)] array: Ref<Untyped<Value>>,
    #[php(prefer_ref)] rest: Variadic<Ref<Untyped<Value>>>,
) -> Result<True, Thrown> {
    use php_ext::raising::{arg_type_error, throw_error};
    let given: Vec<Ref<Untyped<Value>>> = std::iter::once(array).chain(rest.0).collect();
    // Each array with the cell it came in, and the comparison each array
    // is sorted by, `(flags, descending)`.
    let mut arrays: Vec<(PhpArray, Ref<Untyped<Value>>)> = Vec::new();
    let mut sorts: Vec<(i64, bool)> = Vec::new();
    let (mut order, mut kind) = (SORT_ASC, SORT_REGULAR);
    let (mut order_allowed, mut kind_allowed) = (false, false);
    for (at, argument) in given.iter().enumerate() {
        let position = u32::try_from(at + 1).unwrap_or(u32::MAX);
        let value = argument.get().map_or_else(Value::null, |held| held.0);
        if let Some(array) = value.as_array() {
            if at > 0 {
                sorts.push((kind, order != SORT_ASC));
                order = SORT_ASC;
                kind = SORT_REGULAR;
            }
            let separated = argument.separated(req)?.unwrap_or(array);
            arrays.push((separated, *argument));
            order_allowed = true;
            kind_allowed = true;
        } else if let Some(flag) = value.as_int() {
            match flag & !SORT_FLAG_CASE {
                SORT_ASC | SORT_DESC => {
                    if !order_allowed {
                        return Err(arg_type_error(
                            req,
                            position,
                            "be an array or a sort flag that has not already been specified",
                        ));
                    }
                    order = if flag == SORT_DESC {
                        SORT_DESC
                    } else {
                        SORT_ASC
                    };
                    order_allowed = false;
                }
                SORT_REGULAR | SORT_NUMERIC | SORT_STRING | SORT_NATURAL | SORT_LOCALE_STRING => {
                    if !kind_allowed {
                        return Err(arg_type_error(
                            req,
                            position,
                            "be an array or a sort flag that has not already been specified",
                        ));
                    }
                    kind = flag;
                    kind_allowed = false;
                }
                _ => {
                    return Err(php_ext::raising::arg_value_error(
                        req,
                        position,
                        "be a valid sort flag",
                    ));
                }
            }
        } else {
            return Err(arg_type_error(req, position, "be an array or a sort flag"));
        }
    }
    let Some((first, _)) = arrays.first() else {
        return Ok(True);
    };
    let size = first.len();
    if arrays.iter().any(|(array, _)| array.len() != size) {
        return Err(throw_error(
            req,
            "ValueError",
            "Array sizes are inconsistent",
        ));
    }
    if size < 1 {
        return Ok(True);
    }
    sorts.push((kind, order != SORT_ASC));
    // `GC_ADDREF(hashes[i])` (GH-22678): the sort holds each table while
    // it compares, so a comparison that writes a variable separates it
    // and every element the columns name stays alive.
    let held: Vec<Owner<PhpArray>> = arrays
        .iter()
        .map(|(array, _)| Owner::take(req, *array))
        .collect();
    let sorted = multisort(req, &arrays, &sorts, size);
    for owner in held {
        owner.give_up(req);
    }
    sorted.map(|()| True)
}

/// `array_multisort`'s sort over the tables it holds, and each table
/// rebuilt in the order found and written where its variable still holds
/// it: php restructures the table it holds, so a variable the program
/// wrote over while the sort ran keeps what the program wrote.
fn multisort(
    req: &mut Request,
    arrays: &[(PhpArray, Ref<Untyped<Value>>)],
    sorts: &[(i64, bool)],
    size: usize,
) -> Result<(), Thrown> {
    use php_ext::values::zend_sort;
    // php copies each array's buckets into `indirect`, `indirect[k][i] = *p`
    // (`Z/ext/standard/array.c:6205`), taking no owner, and sorts the
    // rows under the comparison: the rows read under the borrowed request,
    // views the arrays keep.
    let columns: Vec<Vec<(Key, Value)>> = arrays
        .iter()
        .map(|(array, _)| walk::rows(req, *array).collect())
        .collect();
    let mut rows: Vec<usize> = (0..size).collect();
    let mut pending: Option<Thrown> = None;
    zend_sort(&mut rows, &mut |a, b| {
        if pending.is_some() {
            return Ordering::Equal;
        }
        for (column, (flags, reverse)) in columns.iter().zip(sorts) {
            match compared(req, *flags, &column[*a].1, &column[*b].1) {
                Ok(Ordering::Equal) => {}
                Ok(ordering) => {
                    return if *reverse {
                        ordering.reverse()
                    } else {
                        ordering
                    };
                }
                Err(thrown) => {
                    pending = Some(thrown);
                    return Ordering::Equal;
                }
            }
        }
        a.cmp(b)
    });
    if let Some(thrown) = pending {
        return Err(thrown);
    }
    let count = i64::try_from(size).unwrap_or(i64::MAX);
    for ((array, cell), column) in arrays.iter().zip(&columns) {
        let holds = cell.reference().is_none_or(|reference| {
            reference
                .get()
                .as_array()
                .is_some_and(|now| now.is_same(*array))
        });
        if !holds {
            continue;
        }
        let mut rebuilt = PhpArray::new(req)?;
        for row in &rows {
            let (key, value) = &column[*row];
            let value = Owner::take(req, *value);
            match key {
                Key::Str(_) => {
                    let key = Owner::take(req, key.clone());
                    rebuilt.set(req, key, value)?;
                }
                Key::Int(_) => rebuilt.append(req, value)?,
            }
        }
        rebuilt.set_next_free(req, count)?;
        cell.set(req, Untyped(rebuilt.into_value()))?;
    }
    Ok(())
}

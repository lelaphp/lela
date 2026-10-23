//! The natives of `array.c` that make an array out of others or out of
//! a range: `array_merge_recursive`, `array_replace` and
//! `array_replace_recursive` beside `array_merge`, `range`, and the two
//! that draw from the random engine, `shuffle` and `array_rand`.

use std::ops::ControlFlow;

use php_ext::abi::{Passed, Raw, Ref, True, Union, Variadic};
use php_ext::attr::php_function;
use php_ext::coerce::{numeric_string, to_array};
use php_ext::raising::{Level, Thrown, argument_error, call_stack_end, docref, throw_error};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpString, Value, ValueRef};
use php_ext_random::Rng;

use super::sort::key_value;
use super::stack::Stack;
use super::{HT_MAX_SIZE, copied, give_up_rows, returned};
use crate::math::math_round;

/// `string|int|float`, a bound of `range`.
type RangeBound = Union<PhpString, Union<i64, f64>>;

/// `int|string|array`, what `array_rand` answers: one key, or a list.
type Picked = Raw<Union<i64, Union<PhpString, PhpArray>>>;

/// `array_merge_recursive(array ...$arrays): array`, php's
/// `php_array_merge_wrapper` with `php_array_merge_recursive`
/// (`Z/ext/standard/array.c:4036`): `array_merge`, except that a string
/// key both arrays have is merged, the two values made arrays and the
/// second's elements appended to the first's, recursively where both
/// are arrays.
///
/// ```php
/// var_dump(array_merge_recursive(["a" => 1, "b" => ["x"]], ["a" => 2, "b" => ["y"], 5 => "n"], ["a" => [3]]), array_merge_recursive(["a" => null], ["a" => 1]), array_merge_recursive());
/// $a = [1]; $a[] = &$a; try { array_merge_recursive($a, $a); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(3) {
///   ["a"]=>
///   array(3) {
///     [0]=>
///     int(1)
///     [1]=>
///     int(2)
///     [2]=>
///     int(3)
///   }
///   ["b"]=>
///   array(2) {
///     [0]=>
///     string(1) "x"
///     [1]=>
///     string(1) "y"
///   }
///   [0]=>
///   string(1) "n"
/// }
/// array(1) {
///   ["a"]=>
///   array(2) {
///     [0]=>
///     NULL
///     [1]=>
///     int(1)
///   }
/// }
/// array(0) {
/// }
/// ```
///
/// Two rows holding one reference are merged, as php's recursion test
/// asks whether the rows are one slot; a table that holds itself is
/// still the `Error`.
///
/// ```php
/// $r = [1]; $m = ["a" => &$r]; var_dump(array_merge_recursive($m, $m));
/// $a = ['x' => ['k' => 1]]; $a['x']['k'] = &$a['x'];
/// try { array_merge_recursive($a, $a); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(1) {
///   ["a"]=>
///   array(2) {
///     [0]=>
///     int(1)
///     [1]=>
///     int(1)
///   }
/// }
/// Recursion detected
/// ```
///
/// The declarations of the array builders.
///
/// ```php
/// foreach (['array_merge_recursive', 'array_replace', 'array_replace_recursive', 'range', 'shuffle', 'array_rand'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function array_merge_recursive ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <optional> array ...$arrays ]
///   }
///   - Return [ array ]
/// }
/// Function [ <internal:standard> function array_replace ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array $array ]
///     Parameter #1 [ <optional> array ...$replacements ]
///   }
///   - Return [ array ]
/// }
/// Function [ <internal:standard> function array_replace_recursive ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array $array ]
///     Parameter #1 [ <optional> array ...$replacements ]
///   }
///   - Return [ array ]
/// }
/// Function [ <internal:standard> function range ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> string|int|float $start ]
///     Parameter #1 [ <required> string|int|float $end ]
///     Parameter #2 [ <optional> int|float $step = 1 ]
///   }
///   - Return [ array ]
/// }
/// Function [ <internal:standard> function shuffle ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> array &$array ]
///   }
///   - Return [ true ]
/// }
/// Function [ <internal:standard> function array_rand ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array $array ]
///     Parameter #1 [ <optional> int $num = 1 ]
///   }
///   - Return [ array|string|int ]
/// }
/// ```
#[php_function]
fn array_merge_recursive(
    req: &mut Request,
    arrays: Variadic<PhpArray>,
) -> Result<Owner<PhpArray>, Thrown> {
    let Some((first, rest)) = arrays.split_first() else {
        return Ok(Owner::take(req, PhpArray::empty()));
    };
    super::merge_total(req, &arrays)?;
    let mut dest = PhpArray::new(req)?;
    // `php_array_merge_wrapper`'s copy of the first array
    // (`ZEND_HASH_MAP_FOREACH_STR_KEY_VAL(src, string_key, src_entry)`,
    // `Z/ext/standard/array.c:4364`), which no code of the program reaches:
    // the rows read under the borrowed request, each taken as php takes
    // it, a reference with one owner followed, then written.
    let rows: Vec<(Key, Owner<Value>)> = walk::rows(req, *first)
        .map(|(key, value)| (key, copied(req, value)))
        .collect();
    let mut rows = rows.into_iter();
    while let Some((key, value)) = rows.next() {
        let written = match key {
            Key::Str(_) => {
                let key = Owner::take(req, key);
                dest.set(req, key, value)
            }
            Key::Int(_) => dest.append(req, value),
        };
        if let Err(thrown) = written {
            give_up_rows(req, rows);
            dest.give_up(req);
            return Err(thrown);
        }
    }
    for src in rest {
        if let Err(thrown) = merge_recursive(req, &mut dest, *src) {
            dest.give_up(req);
            return Err(thrown);
        }
    }
    Ok(dest)
}

/// `php_array_merge_recursive`: `src`'s elements into `dest`.
fn merge_recursive(
    req: &mut Request,
    dest: &mut Owner<PhpArray>,
    src: PhpArray,
) -> Result<(), Thrown> {
    call_stack_end(req)?;
    // `ZEND_HASH_FOREACH_STR_KEY_VAL(src, string_key, src_entry)`
    // (`Z/ext/standard/array.c:4041`), no owner of the table taken: the
    // argument holds it while an object's cast or a destructor an
    // overwrite serves runs.
    walk::foreach(req, src, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let src_entry = *row.value();
        let Key::Str(_) = key else {
            let value = copied(req, *row.value());
            dest.append(req, value)?;
            return Ok(ControlFlow::Continue(()));
        };
        let Some(dest_entry) = dest.get(&key) else {
            let value = copied(req, *row.value());
            let key = Owner::take(req, key.clone());
            dest.set(req, key, value)?;
            return Ok(ControlFlow::Continue(()));
        };
        let src_value = src_entry.deref();
        let dest_value = dest_entry.deref();
        let thash = dest_value.as_array();
        // `src_entry == dest_entry && Z_ISREF_P(dest_entry) &&
        // (Z_REFCOUNT_P(dest_entry) % 2)`: the two rows are one slot, which
        // they are where the tables are one, as a key finds one bucket in
        // a table; two slots holding one reference are merged.
        let same_slot = dest.is_same(src)
            && dest_entry.as_php_ref().is_some()
            && dest_entry.refcount() % 2 == 1;
        if thash.is_some_and(|thash| thash.is_recursive()) || same_slot {
            return Err(throw_error(req, "Error", "Recursion detected"));
        }
        // `convert_to_array(dest_zval)` then `SEPARATE_ZVAL`: a `null` is
        // `[null]`, a scalar `[scalar]`, and an array a copy of its own.
        let mut nested = match dest_value.as_ref() {
            ValueRef::Array(array) => array.copy(req)?,
            ValueRef::Null => {
                let mut array = PhpArray::new(req)?;
                array.append(req, Owner::null())?;
                array
            }
            _ => to_array(req, &dest_value)?,
        };
        let (src_array, temporary) = match src_value.as_ref() {
            ValueRef::Array(array) => (Some(array), None),
            ValueRef::Object(_) => match to_array(req, &src_value) {
                Ok(temporary) => (Some(*temporary), Some(temporary)),
                Err(thrown) => {
                    nested.give_up(req);
                    return Err(thrown);
                }
            },
            _ => (None, None),
        };
        let merged = if let Some(array) = src_array {
            if let Some(thash) = thash {
                thash.protect();
            }
            let merged = merge_recursive(req, &mut nested, array);
            if let Some(thash) = thash {
                thash.unprotect();
            }
            if let Some(temporary) = temporary {
                temporary.give_up(req);
            }
            merged
        } else {
            nested.append(req, Owner::take(req, src_value))
        };
        let written = Owner::take(req, key.clone());
        dest.set(req, written, nested.into_value())?;
        merged?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(())
}

/// `array_replace(array $array, array ...$replacements): array`, php's
/// `php_array_replace_wrapper` (`Z/ext/standard/array.c:4218`): a copy of
/// the first array, and each replacement's elements written over it
/// under their own keys, `zend_hash_merge` with overwrite.
///
/// ```php
/// var_dump(array_replace([1, 2, 3], [1 => "b"], [3 => "d", "k" => "v"]), array_replace([1]));
/// try { array_replace([1], 2); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(5) {
///   [0]=>
///   int(1)
///   [1]=>
///   string(1) "b"
///   [2]=>
///   int(3)
///   [3]=>
///   string(1) "d"
///   ["k"]=>
///   string(1) "v"
/// }
/// array(1) {
///   [0]=>
///   int(1)
/// }
/// array_replace(): Argument #2 must be of type array, int given
/// ```
#[php_function]
fn array_replace(
    req: &mut Request,
    array: PhpArray,
    replacements: Variadic<PhpArray>,
) -> Result<Owner<PhpArray>, Thrown> {
    replace(req, array, &replacements, false)
}

/// `array_replace_recursive(array $array, array ...$replacements):
/// array`, php's `php_array_replace_recursive`
/// (`Z/ext/standard/array.c:4148`): `array_replace`, except that where
/// both arrays hold an array under a key, the second is written over the
/// first recursively.
///
/// ```php
/// var_dump(array_replace_recursive(["a" => [1, 2], "b" => 1, "c" => [1]], ["a" => [1 => "x"], "b" => [9], "c" => 5]));
/// ```
/// ```expect
/// array(3) {
///   ["a"]=>
///   array(2) {
///     [0]=>
///     int(1)
///     [1]=>
///     string(1) "x"
///   }
///   ["b"]=>
///   array(1) {
///     [0]=>
///     int(9)
///   }
///   ["c"]=>
///   int(5)
/// }
/// ```
#[php_function]
fn array_replace_recursive(
    req: &mut Request,
    array: PhpArray,
    replacements: Variadic<PhpArray>,
) -> Result<Owner<PhpArray>, Thrown> {
    replace(req, array, &replacements, true)
}

fn replace(
    req: &mut Request,
    array: PhpArray,
    replacements: &[PhpArray],
    recursive: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let mut dest = array.copy(req)?;
    for src in replacements {
        let replaced = if recursive {
            replace_recursive(req, &mut dest, *src)
        } else {
            replace_flat(req, &mut dest, *src)
        };
        if let Err(thrown) = replaced {
            dest.give_up(req);
            return Err(thrown);
        }
    }
    Ok(dest)
}

/// `zend_hash_merge(dest, src, zval_add_ref, 1)`.
fn replace_flat(
    req: &mut Request,
    dest: &mut Owner<PhpArray>,
    src: PhpArray,
) -> Result<(), Thrown> {
    // `zend_hash_merge(dest, src, zval_add_ref, 1)`'s walk over `src`
    // (`Z/Zend/zend_hash.c:2556`), no owner of it taken: the argument holds
    // it while an overwrite's release serves a destructor.
    walk::foreach(req, src, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let value = copied(req, *row.value());
        let key = Owner::take(req, key.clone());
        dest.set(req, key, value)?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(())
}

/// `php_array_replace_recursive`: `src` written over `dest`.
fn replace_recursive(
    req: &mut Request,
    dest: &mut Owner<PhpArray>,
    src: PhpArray,
) -> Result<(), Thrown> {
    call_stack_end(req)?;
    // `ZEND_HASH_FOREACH_KEY_VAL(src, num_key, string_key, src_entry)`
    // (`Z/ext/standard/array.c:4148`), no owner of the table taken: the
    // argument holds it while an overwrite's release serves a destructor.
    walk::foreach(req, src, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let src_entry = *row.value();
        let src_value = src_entry.deref();
        let dest_array = dest.get(&key).and_then(|entry| entry.deref().as_array());
        let (Some(src_array), Some(dest_array)) = (src_value.as_array(), dest_array) else {
            let value = copied(req, *row.value());
            let key = Owner::take(req, key.clone());
            dest.set(req, key, value)?;
            return Ok(ControlFlow::Continue(()));
        };
        if dest_array.is_recursive() || src_array.is_recursive() {
            return Err(throw_error(req, "Error", "Recursion detected"));
        }
        let mut nested = dest_array.copy(req)?;
        dest_array.protect();
        src_array.protect();
        let replaced = replace_recursive(req, &mut nested, src_array);
        dest_array.unprotect();
        src_array.unprotect();
        let written = Owner::take(req, key.clone());
        dest.set(req, written, nested.into_value())?;
        replaced?;
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(())
}

/// What `php_range_process_input` reads one bound as.
#[derive(Clone, Copy, PartialEq)]
enum Bound {
    Long(i64),
    Double(f64),
    /// A string of one byte that is a digit: a number or a character.
    Digit(i64),
    /// A string that is no number: its first byte.
    Byte(u8),
}

impl Bound {
    fn long(self) -> i64 {
        match self {
            Bound::Long(n) | Bound::Digit(n) => n,
            #[allow(clippy::cast_possible_truncation)]
            Bound::Double(d) => d as i64,
            Bound::Byte(_) => 0,
        }
    }

    fn double(self) -> f64 {
        match self {
            #[allow(clippy::cast_precision_loss)]
            Bound::Long(n) | Bound::Digit(n) => n as f64,
            Bound::Double(d) => d,
            Bound::Byte(_) => 0.0,
        }
    }

    /// `IS_STRING` or the ambiguous `IS_ARRAY` php ranks above it.
    fn stringy(self) -> bool {
        matches!(self, Bound::Digit(_) | Bound::Byte(_))
    }
}

/// `php_range_process_input` (`Z/ext/standard/array.c:2883`): one bound
/// read, with php's warnings for an empty string and for one of more than
/// a byte, and its `ValueError` for a non-finite float.
fn range_input(
    req: &mut Request,
    given: RangeBound,
    position: u32,
    name: &str,
) -> Result<(Bound, u8), Thrown> {
    match given {
        Union::B(Union::A(n)) => Ok((Bound::Long(n), 0)),
        Union::B(Union::B(d)) => finite(req, position, d).map(|d| (Bound::Double(d), 0)),
        Union::A(s) => {
            let bytes = s.as_bytes();
            if bytes.is_empty() {
                docref(
                    req,
                    Level::Warning,
                    &format!("Argument #{position} (${name}) must not be empty, casted to 0"),
                );
                return Ok((Bound::Long(0), 0));
            }
            match numeric_string(bytes) {
                Some(number) if number.as_float().is_some() => {
                    let d = number.as_float().unwrap_or(0.0);
                    finite(req, position, d).map(|d| (Bound::Double(d), bytes[0]))
                }
                Some(number) => {
                    let n = number.as_int().unwrap_or(0);
                    if bytes.len() == 1 {
                        Ok((Bound::Digit(n), bytes[0]))
                    } else {
                        Ok((Bound::Long(n), bytes[0]))
                    }
                }
                None => {
                    if bytes.len() != 1 {
                        docref(
                            req,
                            Level::Warning,
                            &format!(
                                "Argument #{position} (${name}) must be a single byte, subsequent bytes are ignored"
                            ),
                        );
                    }
                    Ok((Bound::Byte(bytes[0]), bytes[0]))
                }
            }
        }
    }
}

fn finite(req: &mut Request, position: u32, d: f64) -> Result<f64, Thrown> {
    if d.is_infinite() {
        return Err(argument_error(
            req,
            "ValueError",
            position,
            "must be a finite number, INF provided",
        ));
    }
    if d.is_nan() {
        return Err(argument_error(
            req,
            "ValueError",
            position,
            "must be a finite number, NAN provided",
        ));
    }
    Ok(d)
}

/// `range(string|int|float $start, string|int|float $end, int|float $step
/// = 1): array`, php's `PHP_FUNCTION(range)` (`Z/ext/standard/array.c:2950`):
/// the numbers from `$start` to `$end` by `$step`, ints where every input
/// is one and floats where any is, or the bytes between two single-byte
/// strings, each way, with php's warnings where a string bound is
/// ambiguous and its `ValueError`s for a step of zero, a step the range
/// cannot hold, and a negative step on an increasing range.
///
/// ```php
/// echo json_encode([range(1, 5), range(5, 1, 2), range('a', 'e', 2), range('A', 'z', 20), range(0, 1, 0.25), range(1.5, 0), range("1", "3"), range("5", "b"), range(2, 1, -1), range(3, 3)]), "\n";
/// ```
/// ```expect
/// [[1,2,3,4,5],[5,3,1],["a","c","e"],["A","U","i"],[0,0.25,0.5,0.75,1],[1.5,0.5],["1","2","3"],["5","6","7","8","9",":",";","<","=",">","?","@","A","B","C","D","E","F","G","H","I","J","K","L","M","N","O","P","Q","R","S","T","U","V","W","X","Y","Z","[","\\","]","^","_","`","a","b"],[2,1],[3]]
/// ```
///
/// ```php
/// try { range(1, 5, 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { range(1, 3, 5); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { range(1, 5, -2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { range(INF, 5); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(range("", 2), range("ab", "c"), range("a", 1.5), range(1, "b"), range('a', 'c', 1.5));
/// ```
/// ```expect
/// range(): Argument #3 ($step) cannot be 0
/// range(): Argument #3 ($step) must be less than the range spanned by argument #1 ($start) and argument #2 ($end)
/// range(): Argument #3 ($step) must be greater than 0 for increasing ranges
/// range(): Argument #1 ($start) must be a finite number, INF provided
///
/// Warning: range(): Argument #1 ($start) must not be empty, casted to 0 in %s on line 6
///
/// Warning: range(): Argument #1 ($start) must be a single byte, subsequent bytes are ignored in %s on line 6
///
/// Warning: range(): Argument #2 ($end) must be a single byte string if argument #1 ($start) is a single byte string, argument #1 ($start) converted to 0 in %s on line 6
///
/// Warning: range(): Argument #1 ($start) must be a single byte string if argument #2 ($end) is a single byte string, argument #2 ($end) converted to 0 in %s on line 6
///
/// Warning: range(): Argument #3 ($step) must be of type int when generating an array of characters, inputs converted to 0 in %s on line 6
/// array(3) {
///   [0]=>
///   int(0)
///   [1]=>
///   int(1)
///   [2]=>
///   int(2)
/// }
/// array(3) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "b"
///   [2]=>
///   string(1) "c"
/// }
/// array(2) {
///   [0]=>
///   float(0)
///   [1]=>
///   float(1)
/// }
/// array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(0)
/// }
/// array(1) {
///   [0]=>
///   float(0)
/// }
/// ```
// php's body is one function with its gotos; the arms stay together.
#[allow(clippy::too_many_lines)]
#[php_function]
fn range(
    req: &mut Request,
    start: RangeBound,
    end: RangeBound,
    #[php(default = 1)] step: Passed<Union<i64, f64>>,
) -> Result<Owner<PhpArray>, Thrown> {
    let (user_start, user_end, user_step) = (start, end, step);
    let mut step_double = 1.0f64;
    let mut step: i64 = 1;
    let mut step_is_double = false;
    let mut negative = false;
    if let Passed::Given(given) = user_step {
        match given {
            Union::B(d) => {
                if d.is_infinite() {
                    return Err(argument_error(
                        req,
                        "ValueError",
                        3,
                        "must be a finite number, INF provided",
                    ));
                }
                if d.is_nan() {
                    return Err(argument_error(
                        req,
                        "ValueError",
                        3,
                        "must be a finite number, NAN provided",
                    ));
                }
                step_double = d;
                if step_double < 0.0 {
                    negative = true;
                    step_double = -step_double;
                }
                step = php_ext::coerce::float_to_long(step_double);
                #[allow(clippy::cast_precision_loss, clippy::float_cmp)]
                if step as f64 != step_double {
                    step_is_double = true;
                }
            }
            Union::A(n) => {
                step = n;
                if step < 0 {
                    if step == i64::MIN {
                        return Err(argument_error(
                            req,
                            "ValueError",
                            3,
                            &format!("must be greater than {}", i64::MIN),
                        ));
                    }
                    negative = true;
                    step = -step;
                }
                #[allow(clippy::cast_precision_loss)]
                {
                    step_double = step as f64;
                }
            }
        }
        #[allow(clippy::float_cmp)]
        if step_double == 0.0 {
            return Err(argument_error(req, "ValueError", 3, "cannot be 0"));
        }
    }
    let (mut start, low) = range_input(req, user_start, 1, "start")?;
    let (mut end, high) = range_input(req, user_end, 2, "end")?;
    if start.stringy() || end.stringy() {
        let mut numeric = false;
        if !start.stringy() {
            if !matches!(end, Bound::Digit(_)) {
                docref(
                    req,
                    Level::Warning,
                    "Argument #1 ($start) must be a single byte string if argument #2 ($end) is a single byte string, argument #2 ($end) converted to 0",
                );
            }
            end = Bound::Long(end.long());
            numeric = true;
        } else if !end.stringy() {
            if !matches!(start, Bound::Digit(_)) {
                docref(
                    req,
                    Level::Warning,
                    "Argument #2 ($end) must be a single byte string if argument #1 ($start) is a single byte string, argument #1 ($start) converted to 0",
                );
            }
            start = Bound::Long(start.long());
            numeric = true;
        } else if step_is_double {
            if matches!(start, Bound::Byte(_)) || matches!(end, Bound::Byte(_)) {
                docref(
                    req,
                    Level::Warning,
                    "Argument #3 ($step) must be of type int when generating an array of characters, inputs converted to 0",
                );
            }
            start = Bound::Long(start.long());
            end = Bound::Long(end.long());
            numeric = true;
        }
        if !numeric {
            return character_range(req, low, high, step, negative);
        }
    }
    if matches!(start, Bound::Double(_)) || matches!(end, Bound::Double(_)) || step_is_double {
        return double_range(req, start.double(), end.double(), step_double, negative);
    }
    long_range(req, start.long(), end.long(), step.unsigned_abs(), negative)
}

fn boundary_error(req: &mut Request) -> Thrown {
    argument_error(
        req,
        "ValueError",
        3,
        "must be less than the range spanned by argument #1 ($start) and argument #2 ($end)",
    )
}

fn negative_step_error(req: &mut Request) -> Thrown {
    argument_error(
        req,
        "ValueError",
        3,
        "must be greater than 0 for increasing ranges",
    )
}

/// The array of characters between two bytes.
#[allow(clippy::comparison_chain)]
fn character_range(
    req: &mut Request,
    low: u8,
    high: u8,
    step: i64,
    negative: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    if low > high && i64::from(low - high) < step {
        return Err(boundary_error(req));
    }
    if high > low {
        if negative {
            return Err(negative_step_error(req));
        }
        if i64::from(high - low) < step {
            return Err(boundary_error(req));
        }
    }
    let mut out = PhpArray::new(req)?;
    let step_wide = i64::from(u8::MAX).min(step);
    if low > high {
        let mut at = i64::from(low);
        while at >= i64::from(high) {
            let byte = u8::try_from(at).unwrap_or(0);
            let text = PhpString::new(req, &[byte])?;
            out.append(req, text.into_value())?;
            if at - step < 0 {
                break;
            }
            at -= step;
        }
    } else if high > low {
        let mut at = i64::from(low);
        while at <= i64::from(high) {
            let byte = u8::try_from(at).unwrap_or(0);
            let text = PhpString::new(req, &[byte])?;
            out.append(req, text.into_value())?;
            if at + step_wide > 255 {
                break;
            }
            at += step;
        }
    } else {
        let text = PhpString::new(req, &[low])?;
        out.append(req, text.into_value())?;
    }
    Ok(out)
}

/// `RANGE_CHECK_DOUBLE_INIT_ARRAY` and the fill: the size rounded half
/// up by `_php_math_round`, and each element `start ± i * step` while it
/// stays inside the bound.
fn double_range(
    req: &mut Request,
    start: f64,
    end: f64,
    step: f64,
    negative: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    #[allow(clippy::cast_precision_loss)]
    let limit = HT_MAX_SIZE as f64;
    let size = |req: &mut Request, high: f64, low: f64| -> Result<u32, Thrown> {
        let calc = ((high - low) / step) + 1.0;
        if calc >= limit {
            return Err(throw_error(
                req,
                "ValueError",
                &format!(
                    "The supplied range exceeds the maximum array size by {:.1} elements: start={low:.1}, end={high:.1}, step={step:.1}. Max size: {limit:.0}",
                    calc - limit
                ),
            ));
        }
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        Ok(math_round(calc, 0, crate::math::PHP_ROUND_HALF_UP) as u32)
    };
    // `RANGE_CHECK_DOUBLE_INIT_ARRAY`: the checks, then the array.
    let (size, down) = if start > end {
        if start - end < step {
            return Err(boundary_error(req));
        }
        (size(req, start, end)?, true)
    } else if end > start {
        if negative {
            return Err(negative_step_error(req));
        }
        if end - start < step {
            return Err(boundary_error(req));
        }
        (size(req, end, start)?, false)
    } else {
        (1, false)
    };
    PhpArray::filled(req, |req, out| {
        let mut i = 0u32;
        let mut element = start;
        while i < size && if down { element >= end } else { element <= end } {
            out.append(req, Owner::float(element))?;
            i += 1;
            let distance = f64::from(i) * step;
            element = if down {
                start - distance
            } else {
                start + distance
            };
        }
        Ok(())
    })
}

/// `RANGE_CHECK_LONG_INIT_ARRAY` and the fill, over unsigned
/// arithmetic as php's is.
#[allow(clippy::comparison_chain)]
fn long_range(
    req: &mut Request,
    start: i64,
    end: i64,
    step: u64,
    negative: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let size = |req: &mut Request, high: i64, low: i64| -> Result<u64, Thrown> {
        let calc = high.wrapping_sub(low).cast_unsigned() / step;
        if calc >= HT_MAX_SIZE - 1 {
            return Err(throw_error(
                req,
                "ValueError",
                &format!(
                    "The supplied range exceeds the maximum array size by {} elements: start={low}, end={high}, step={step}. Calculated size: {calc}. Maximum size: {HT_MAX_SIZE}.",
                    calc - (HT_MAX_SIZE - 1)
                ),
            ));
        }
        Ok(calc + 1)
    };
    // `RANGE_CHECK_LONG_INIT_ARRAY`: the checks, then the array.
    let (size, down) = if start > end {
        if start.wrapping_sub(end).cast_unsigned() < step {
            return Err(boundary_error(req));
        }
        (size(req, start, end)?, true)
    } else if end > start {
        if negative {
            return Err(negative_step_error(req));
        }
        if end.wrapping_sub(start).cast_unsigned() < step {
            return Err(boundary_error(req));
        }
        (size(req, end, start)?, false)
    } else {
        (1, false)
    };
    PhpArray::filled(req, |req, out| {
        for i in 0..size {
            let distance = i.wrapping_mul(step).cast_signed();
            let element = if down {
                start.wrapping_sub(distance)
            } else {
                start.wrapping_add(distance)
            };
            out.append(req, Owner::int(element))?;
        }
        Ok(())
    })
}

/// `shuffle(array &$array): true`, php's `php_array_data_shuffle`
/// (`Z/ext/standard/array.c:3204`): the values of the array the
/// reference holds, keys dropped, put in a random order by the
/// Fisher-Yates walk php makes from the end, each swap drawn from the
/// request's default engine through the RNG ext/random exports.
///
/// ```php
/// $a = range(1, 20); var_dump(shuffle($a)); sort($a); var_dump($a == range(1, 20), array_keys($a) === range(0, 19));
/// $k = ["x" => 1, "y" => 2]; shuffle($k); var_dump(array_keys($k)); $e = []; var_dump(shuffle($e), $e);
/// mt_srand(42); $a = range(1, 10); shuffle($a); echo implode(",", $a), "\n";
/// mt_srand(42, MT_RAND_PHP); $a = range(1, 10); shuffle($a); echo implode(",", $a), "\n";
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// array(2) {
///   [0]=>
///   int(0)
///   [1]=>
///   int(1)
/// }
/// bool(true)
/// array(0) {
/// }
/// 2,4,10,8,7,1,9,5,6,3
///
/// Deprecated: Constant MT_RAND_PHP is deprecated since 8.3, as it uses a biased non-standard variant of Mt19937 in %s on line 5
///
/// Deprecated: The MT_RAND_PHP variant of Mt19937 is deprecated in %s on line 5
/// 3,4,2,9,10,1,8,5,6,7
/// ```
#[php_function]
fn shuffle(req: &mut Request, array: Ref<PhpArray>) -> Result<True, Thrown> {
    Stack::over(req, array, |req, stack| {
        // `php_array_data_shuffle` permutes the table's own rows with the
        // default engine, which runs no code of the program: the rows read
        // under the borrowed request, views the table keeps until the
        // shuffled copy replaces it.
        let mut values: Vec<Value> = walk::rows(req, stack.array).map(|(_, v)| v).collect();
        Rng::default_engine().shuffle(req, &mut values)?;
        let mut shuffled = PhpArray::new(req)?;
        for value in values {
            shuffled.append(req, Owner::take(req, value))?;
        }
        stack.replace(req, shuffled)?;
        Ok(True)
    })
}

/// `array_rand(array $array, int $num = 1): int|string|array`, php's
/// `php_array_pick_keys` (`Z/ext/standard/array.c:6269`) over the
/// request's default engine through the RNG ext/random exports: one key, sampled
/// by bucket, or a list of `$num` keys in the array's order.
///
/// ```php
/// $a = ["a" => 1, "b" => 2, "c" => 3]; var_dump(in_array(array_rand($a), ["a", "b", "c"], true), array_rand($a, 3), count(array_rand($a, 2)));
/// try { array_rand([]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { array_rand([1], 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// mt_srand(42); $h = range(0, 99); for ($i = 0; $i < 80; $i++) unset($h[$i]);
/// echo array_rand(range(0, 9)), " ", implode(",", array_rand(range(0, 9), 4)), " ", array_rand($h), " ", array_rand(["x" => 1, "y" => 2]), "\n";
/// ```
/// ```expect
/// bool(true)
/// array(3) {
///   [0]=>
///   string(1) "a"
///   [1]=>
///   string(1) "b"
///   [2]=>
///   string(1) "c"
/// }
/// int(2)
/// array_rand(): Argument #1 ($array) must not be empty
/// array_rand(): Argument #2 ($num) must be between 1 and the number of elements in argument #1 ($array)
/// 2 4,5,6,7 80 x
/// ```
#[php_function]
fn array_rand(
    req: &mut Request,
    array: PhpArray,
    #[php(default = 1)] num: i64,
) -> Result<Owner<Picked>, Thrown> {
    let want = num;
    let keys = Rng::default_engine().pick_keys(req, array, want)?;
    if want == 1 {
        let value = keys.first().map_or(Value::int(0), key_value);
        return Ok(returned(Owner::take(req, value)));
    }
    let mut out = PhpArray::new(req)?;
    for key in &keys {
        let value = key_value(key);
        out.append(req, Owner::take(req, value))?;
    }
    Ok(returned(out.into_value()))
}

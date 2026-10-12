//! The diff and intersect families of `array.c`: the sixteen natives
//! that keep the entries of their first array found, or not found, in
//! every other. Three shapes, as php has them. `array_diff` is its own
//! body, an exclusion set of the other arrays' values as strings.
//! `php_array_diff_key` and `php_array_intersect_key` look each key of the
//! first array up in the others, the value compared where the native
//! compares values. `php_array_diff` and `php_array_intersect` copy each
//! array into a list of its buckets, sort every list with `zend_sort`
//! under the native's comparison, and walk the lists together, deleting
//! from a copy of the first array; the order a user callback is called
//! in is that walk's, and so is what a comparison that is not a total
//! order leaves.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::ops::ControlFlow;

use php_ext::abi::{Callable, FromArgument, Raw, Untyped, Variadic};
use php_ext::attr::php_function;
use php_ext::call::call;
use php_ext::coerce::{to_long, with_string};
use php_ext::decl::Type;
use php_ext::raising::{Thrown, arity, fatal};
use php_ext::request::Request;
use php_ext::values::walk::Hold;
use php_ext::values::{Key, Owner, PhpArray, Value, ValueRef, walk, zend_sort};

use super::sort::key_value;
use super::{copied, user_compare};
use crate::string::KeyBytes;

/// What a diff or an intersection compares values with.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Data {
    /// Values are not compared (`*_KEY`).
    None,
    /// `string_compare_function`, both values as strings.
    Internal,
    /// The data callback.
    User,
}

/// What it compares keys with, in the sorted walk.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Keys {
    Internal,
    User,
}

/// The three behaviours `php_array_diff` and `php_array_intersect` take.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Behavior {
    /// Values alone (`DIFF_NORMAL`, `INTERSECT_NORMAL`).
    Normal,
    /// Keys alone (`DIFF_KEY`, `INTERSECT_KEY`).
    Key,
    /// Keys and then values (`DIFF_ASSOC`, `INTERSECT_ASSOC`).
    Assoc,
}

/// The arguments as `zend_parse_parameters` reads `"+"`, `"+f"` or
/// `"+ff"`: at least one array, then the callbacks at the end, each held
/// to being a callback before any array is held to being an array.
struct Parsed {
    arrays: Vec<Value>,
    callbacks: Vec<Callable>,
}

/// `Z_PARAM_VARIADIC('+', ..)` and `f` after it, as `"+f"` reads them:
/// the arrays held as they came, the body checking each where php does,
/// and the trailing callbacks resolved where they stand.
fn parse(req: &mut Request, values: &[Value], callbacks: usize) -> Result<Parsed, Thrown> {
    arity(req, callbacks + 1, None)?;
    let split = req.argument_count() - callbacks;
    let mut found = Vec::with_capacity(callbacks);
    for offset in 0..values.len() - split {
        let position = u32::try_from(split + offset + 1).unwrap_or(u32::MAX);
        match Callable::hold(req, values[split + offset], position)? {
            Some(callable) => found.push(callable),
            None => {
                return Err(fatal(
                    req,
                    "InternalError: a counted callback was not given",
                ));
            }
        }
    }
    Ok(Parsed {
        arrays: values[..split].to_vec(),
        callbacks: found,
    })
}

/// `zend_argument_type_error(i + 1, "must be of type array, %s given")`
/// over each array argument, in order.
fn arrays_of(req: &mut Request, values: &[Value]) -> Result<Vec<PhpArray>, Thrown> {
    let mut arrays = Vec::with_capacity(values.len());
    for (at, value) in values.iter().enumerate() {
        let position = u32::try_from(at + 1).unwrap_or(u32::MAX);
        arrays.push(array_at(req, position, value)?);
    }
    Ok(arrays)
}

/// `Z_PARAM_ARRAY` over one value of the variadic, `coerce_param`'s:
/// the declared `array ...$arrays` for the natives [`listed`] reads, and
/// the form php's `"+f"` reads past the untyped `...$rest` that ends in
/// the callbacks for the ones [`rested`] reads.
fn array_at(req: &mut Request, position: u32, value: &Value) -> Result<PhpArray, Thrown> {
    let held = php_ext::coerce::coerce_param(req, value, &Type::Array, position)?;
    match held.as_array() {
        Some(array) => Ok(*array),
        None => Ok(PhpArray::empty()),
    }
}

/// The comparisons one call carries, as php carries them: whether values
/// and keys are compared by a callback, the one callback slot both user
/// comparisons call (`BG(user_compare_fci)`), which the walk switches
/// between the data and the key callback where php switches it, the
/// deprecation raised once per call (`ARRAYG(compare_deprecation_thrown)`),
/// and the first throwable a callback raised, after which every
/// comparison answers equal without calling, as `zend_call_function`
/// does not call while `EG(exception)` stands.
struct Compare {
    data_user: bool,
    key_user: bool,
    data: Option<Callable>,
    key: Option<Callable>,
    slot: Option<Callable>,
    deprecated: bool,
    pending: Option<Thrown>,
}

impl Compare {
    fn new(data: Data, keys: Keys, callbacks: Vec<Callable>) -> Compare {
        let mut callbacks = callbacks.into_iter();
        let data_user = data == Data::User;
        let key_user = keys == Keys::User;
        let data_callback = if data_user { callbacks.next() } else { None };
        let key_callback = if key_user { callbacks.next() } else { None };
        Compare {
            data_user,
            key_user,
            data: data_callback,
            key: key_callback,
            slot: None,
            deprecated: false,
            pending: None,
        }
    }

    fn use_data(&mut self) {
        self.slot.clone_from(&self.data);
    }

    fn use_key(&mut self) {
        self.slot.clone_from(&self.key);
    }

    /// `php_array_data_compare_string_unstable`, or
    /// `php_array_user_compare_unstable` through the slot.
    fn values(&mut self, req: &mut Request, a: &Value, b: &Value) -> Ordering {
        if self.pending.is_some() {
            return Ordering::Equal;
        }
        let answered = if self.data_user {
            self.user(req, a, b)
        } else {
            string_compare(req, &a.deref(), &b.deref())
        };
        self.settle(answered)
    }

    /// `php_array_key_compare_string_unstable`, or
    /// `php_array_user_key_compare_unstable` through the slot.
    fn keys(&mut self, req: &mut Request, a: &Key, b: &Key) -> Ordering {
        if self.pending.is_some() {
            return Ordering::Equal;
        }
        let answered = if self.key_user {
            self.user(req, &key_value(a), &key_value(b))
        } else {
            Ok(KeyBytes::of(a).cmp(&KeyBytes::of(b)))
        };
        self.settle(answered)
    }

    fn user(&mut self, req: &mut Request, a: &Value, b: &Value) -> Result<Ordering, Thrown> {
        match self.slot.clone() {
            Some(callback) => user_compare(req, &callback, &mut self.deprecated, a, b),
            None => Ok(Ordering::Equal),
        }
    }

    fn settle(&mut self, answered: Result<Ordering, Thrown>) -> Ordering {
        match answered {
            Ok(ordering) => ordering,
            Err(thrown) => {
                self.pending = Some(thrown);
                Ordering::Equal
            }
        }
    }
}

/// `string_compare_function`: both values as strings, byte order, each
/// conversion given up after the comparison, as php's
/// `zend_tmp_string_release` gives up what `zval_get_tmp_string` made.
fn string_compare(req: &mut Request, a: &Value, b: &Value) -> Result<Ordering, Thrown> {
    super::as_strings(req, a, b, Ord::cmp)
}

/// `zval_user_compare`, the comparison of the key shape: the callback's
/// answer as `php_get_long` reads it, normalised to a sign, and no bool
/// deprecation, which only the sorted shape's comparison raises.
fn plain_user(
    req: &mut Request,
    callback: &Callable,
    a: &Value,
    b: &Value,
) -> Result<Ordering, Thrown> {
    let answer = call(req, callback, &[*a, *b])?;
    let long = to_long(req, &answer);
    answer.give_up(req);
    Ok(long?.cmp(&0))
}

// ------------------------------------------------------------ the key shape

/// `php_array_diff_key` and `php_array_intersect_key`: each element of
/// the first array whose key the other arrays hold (for an intersection,
/// every one of them; for a diff, none of them) with an equal value where
/// values are compared, in the first array's order, a reference with one
/// owner followed.
/// The arrays of an `array $array, array ...$arrays` native as they came.
fn listed(array: Raw<PhpArray>, arrays: &Variadic<Raw<PhpArray>>) -> Vec<Value> {
    std::iter::once(array.value())
        .chain(arrays.0.iter().map(Raw::value))
        .collect()
}

/// The arguments of an `array $array, ...$rest` native as they came,
/// `$rest` ending in its callbacks.
fn rested(array: Raw<PhpArray>, rest: Variadic<Untyped<Value>>) -> Vec<Value> {
    std::iter::once(array.value())
        .chain(rest.0.into_iter().map(|value| value.0))
        .collect()
}

fn by_key(
    req: &mut Request,
    values: &[Value],
    data: Data,
    diff: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let parsed = parse(req, values, usize::from(data == Data::User))?;
    let arrays = arrays_of(req, &parsed.arrays)?;
    let callback = parsed.callbacks.into_iter().next();
    let mut out = PhpArray::new(req)?;
    // `ZEND_HASH_FOREACH_KEY_VAL(Z_ARRVAL(args[0]), h, key, val)`
    // (`Z/ext/standard/array.c:5155`, `:5551`), no owner of the table taken: the
    // argument holds it while a comparison serves the callback or
    // `__toString`. A reference read as held by one owner is php's count.
    let walked = walk::foreach(req, arrays[0], Hold::None, |req, row| -> walk::Step {
        let key = row.key();
        let value = match row.value().as_ref() {
            ValueRef::Reference(reference) if row.value().refcount() == 1 => reference.get(),
            _ => *row.value(),
        };
        let mut keep = true;
        for other in &arrays[1..] {
            let matched = match other.get(key) {
                None => false,
                Some(_) if data == Data::None => true,
                Some(found) => {
                    let ordering = match &callback {
                        Some(callback) => plain_user(req, callback, &value, &found)?,
                        None => string_compare(req, &value.deref(), &found.deref())?,
                    };
                    ordering == Ordering::Equal
                }
            };
            if matched == diff {
                keep = false;
                break;
            }
        }
        if keep {
            let value = Owner::take(req, value);
            let key = Owner::take(req, key.clone());
            out.set(req, key, value)?;
        }
        Ok(ControlFlow::Continue(()))
    });
    if let Err(thrown) = walked {
        out.give_up(req);
        return Err(thrown);
    }
    Ok(out)
}

// ---------------------------------------------------------- the sorted shape

/// One bucket of a list: the key and the value, a view of each.
#[derive(Clone)]
struct Bucket {
    key: Key,
    val: Value,
}

/// The lists `php_array_diff` and `php_array_intersect` build: each
/// array's buckets in order, sorted by `zend_sort` under the behaviour's
/// comparison where it has more than one.
fn lists(
    req: &mut Request,
    arrays: &[PhpArray],
    behavior: Behavior,
    compare: &mut Compare,
) -> Vec<Vec<Bucket>> {
    let mut lists = Vec::with_capacity(arrays.len());
    for array in arrays {
        // php copies each table's buckets into its list, `*list++ = *p`,
        // taking no owner, and sorts the list under the comparison: the
        // rows read under the borrowed request, views the argument keeps.
        let mut list: Vec<Bucket> = walk::rows(req, *array)
            .map(|(key, val)| Bucket { key, val })
            .collect();
        if list.len() > 1 {
            zend_sort(&mut list, &mut |a, b| match behavior {
                Behavior::Normal => compare.values(req, &a.val, &b.val),
                Behavior::Key | Behavior::Assoc => compare.keys(req, &a.key, &b.key),
            });
        }
        lists.push(list);
    }
    lists
}

/// The arguments held to being arrays, the comparisons, and the lists.
type Sorted = (Vec<PhpArray>, Compare, Vec<Vec<Bucket>>);

/// What the sorted shape starts from: the arguments parsed and held to
/// being arrays, the comparisons, the slot set as php sets it before
/// the lists are sorted, and the sorted lists.
fn sorted(
    req: &mut Request,
    values: &[Value],
    behavior: Behavior,
    data: Data,
    keys: Keys,
) -> Result<Sorted, Thrown> {
    let callbacks = usize::from(data == Data::User) + usize::from(keys == Keys::User);
    let parsed = parse(req, values, callbacks)?;
    let arrays = arrays_of(req, &parsed.arrays)?;
    let mut compare = Compare::new(data, keys, parsed.callbacks);
    if behavior == Behavior::Normal && data == Data::User {
        compare.use_data();
    } else if behavior != Behavior::Normal && keys == Keys::User {
        compare.use_key();
    }
    let lists = lists(req, &arrays, behavior, &mut compare);
    Ok((arrays, compare, lists))
}

/// `php_array_intersect` (`Z/ext/standard/array.c:5194`), the walk over
/// the sorted lists: every entry of the first array whose value (or key,
/// and then value) is not in every other array is deleted from a copy
/// of the first.
fn intersect_sorted(
    req: &mut Request,
    values: &[Value],
    behavior: Behavior,
    data: Data,
    keys: Keys,
) -> Result<Owner<PhpArray>, Thrown> {
    let (arrays, mut compare, lists) = sorted(req, values, behavior, data, keys)?;
    let mut out = arrays[0].copy(req)?;
    let deleted = intersect_walk(req, behavior, &mut compare, &lists);
    for key in deleted {
        out.unset(req, &key)?;
    }
    if let Some(thrown) = compare.pending.take() {
        out.give_up(req);
        return Err(thrown);
    }
    Ok(out)
}

/// The keys `php_array_intersect`'s walk deletes, in the order it
/// deletes them. `c` outlives each pass as php's does.
fn intersect_walk(
    req: &mut Request,
    behavior: Behavior,
    compare: &mut Compare,
    lists: &[Vec<Bucket>],
) -> Vec<Key> {
    let mut deleted = Vec::new();
    let first = &lists[0];
    let mut ptrs = vec![0usize; lists.len()];
    let mut c = Ordering::Equal;
    while ptrs[0] < first.len() {
        if behavior != Behavior::Normal && compare.key_user {
            compare.use_key();
        }
        let mut at = lists.len();
        for i in 1..lists.len() {
            at = i;
            let list = &lists[i];
            if behavior == Behavior::Normal {
                while ptrs[i] < list.len() {
                    c = compare.values(req, &first[ptrs[0]].val, &list[ptrs[i]].val);
                    if c != Ordering::Greater {
                        break;
                    }
                    ptrs[i] += 1;
                }
            } else {
                while ptrs[i] < list.len() {
                    c = compare.keys(req, &first[ptrs[0]].key, &list[ptrs[i]].key);
                    if c != Ordering::Greater {
                        break;
                    }
                    ptrs[i] += 1;
                }
                if c == Ordering::Equal && ptrs[i] < list.len() && behavior == Behavior::Assoc {
                    if compare.data_user {
                        compare.use_data();
                    }
                    if compare.values(req, &first[ptrs[0]].val, &list[ptrs[i]].val)
                        != Ordering::Equal
                    {
                        c = Ordering::Greater;
                        if compare.key_user {
                            compare.use_key();
                        }
                    }
                }
            }
            if ptrs[i] >= list.len() {
                // What is left of the first list is in no other.
                deleted.extend(first[ptrs[0]..].iter().map(|b| b.key.clone()));
                return deleted;
            }
            if c != Ordering::Equal {
                break;
            }
            ptrs[i] += 1;
        }
        if c == Ordering::Equal {
            // In every other: skip what equals it.
            loop {
                ptrs[0] += 1;
                if ptrs[0] >= first.len() {
                    return deleted;
                }
                if behavior != Behavior::Normal
                    || compare.values(req, &first[ptrs[0] - 1].val, &first[ptrs[0]].val)
                        != Ordering::Equal
                {
                    break;
                }
            }
        } else {
            // Not in every other: delete it and what is below the
            // other list's entry.
            loop {
                deleted.push(first[ptrs[0]].key.clone());
                ptrs[0] += 1;
                if ptrs[0] >= first.len() {
                    return deleted;
                }
                if behavior != Behavior::Normal
                    || compare.values(req, &first[ptrs[0]].val, &lists[at][ptrs[at]].val)
                        != Ordering::Less
                {
                    break;
                }
            }
        }
    }
    deleted
}

/// `php_array_diff` (`Z/ext/standard/array.c:5590`), the walk over the
/// sorted lists: every entry of the first array whose value (or key, and
/// then value) is in some other array is deleted from a copy of the
/// first.
fn diff_sorted(
    req: &mut Request,
    values: &[Value],
    behavior: Behavior,
    data: Data,
    keys: Keys,
) -> Result<Owner<PhpArray>, Thrown> {
    let (arrays, mut compare, lists) = sorted(req, values, behavior, data, keys)?;
    let mut out = arrays[0].copy(req)?;
    let deleted = diff_walk(req, behavior, &mut compare, &lists);
    for key in deleted {
        out.unset(req, &key)?;
    }
    if let Some(thrown) = compare.pending.take() {
        out.give_up(req);
        return Err(thrown);
    }
    Ok(out)
}

/// The keys `php_array_diff`'s walk deletes, in the order it deletes
/// them.
fn diff_walk(
    req: &mut Request,
    behavior: Behavior,
    compare: &mut Compare,
    lists: &[Vec<Bucket>],
) -> Vec<Key> {
    let mut deleted = Vec::new();
    let first = &lists[0];
    let mut ptrs = vec![0usize; lists.len()];
    while ptrs[0] < first.len() {
        if behavior != Behavior::Normal && compare.key_user {
            compare.use_key();
        }
        let mut c = Ordering::Greater;
        for i in 1..lists.len() {
            let list = &lists[i];
            let mut ptr = ptrs[i];
            if behavior == Behavior::Normal {
                while ptrs[i] < list.len() {
                    c = compare.values(req, &first[ptrs[0]].val, &list[ptrs[i]].val);
                    if c != Ordering::Greater {
                        break;
                    }
                    ptrs[i] += 1;
                }
            } else {
                while ptr < list.len() {
                    c = compare.keys(req, &first[ptrs[0]].key, &list[ptr].key);
                    if c == Ordering::Equal {
                        break;
                    }
                    ptr += 1;
                }
            }
            if c == Ordering::Equal {
                match behavior {
                    Behavior::Normal => {
                        if ptrs[i] < list.len() {
                            ptrs[i] += 1;
                        }
                        break;
                    }
                    Behavior::Assoc => {
                        if ptr < list.len() {
                            if compare.data_user {
                                compare.use_data();
                            }
                            if compare.values(req, &first[ptrs[0]].val, &list[ptr].val)
                                == Ordering::Equal
                            {
                                break;
                            }
                            c = Ordering::Less;
                            if compare.key_user {
                                compare.use_key();
                            }
                        }
                    }
                    Behavior::Key => break,
                }
            }
        }
        let found = c == Ordering::Equal;
        loop {
            if found {
                deleted.push(first[ptrs[0]].key.clone());
            }
            ptrs[0] += 1;
            if ptrs[0] >= first.len() {
                return deleted;
            }
            if behavior != Behavior::Normal
                || compare.values(req, &first[ptrs[0] - 1].val, &first[ptrs[0]].val)
                    != Ordering::Equal
            {
                break;
            }
        }
    }
    deleted
}

// ------------------------------------------------------------- the natives

/// `array_diff(array $array, array ...$arrays): array`, php's
/// `PHP_FUNCTION(array_diff)` (`Z/ext/standard/array.c:5858`): the
/// entries of the first array whose value, as a string, no other array
/// holds, under their keys. A first array of one element is looked for
/// in the others directly; otherwise the others' values make an
/// exclusion set. No other array, or none with an element, hands the
/// first back as it is.
///
/// ```php
/// var_dump(array_diff(["a" => "green", "red", "blue", "red"], ["b" => "green", "yellow", "red"]));
/// var_dump(array_diff([1, "1", 1.0, true, null, ""], ["1"]), array_diff([7], [7.0]), array_diff([], 1 === 1 ? [] : 0));
/// ```
/// ```expect
/// array(1) {
///   [1]=>
///   string(4) "blue"
/// }
/// array(2) {
///   [4]=>
///   NULL
///   [5]=>
///   string(0) ""
/// }
/// array(0) {
/// }
/// array(0) {
/// }
/// ```
///
/// ```php
/// try { array_diff([1, 2], [1], 3); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { array_diff(1, [1]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array_diff(): Argument #3 must be of type array, int given
/// array_diff(): Argument #1 ($array) must be of type array, int given
/// ```
///
/// Each comparison string is made and given up within its comparison,
/// `zval_get_tmp_string` and `zend_tmp_string_release`, so a loop over
/// the four set functions keeps its memory flat whatever their values
/// convert from:
///
/// ```php
/// class S { function __toString(): string { return "s" . 1; } }
/// $a = [1, 2.5, "x", new S, 3]; $b = [2, "y", 1.5, new S]; $one = [7.5];
/// $f = function () use ($a, $b, $one) {
///     array_diff($a, $b); array_diff($one, $b); array_intersect($a, $b);
///     array_diff_assoc($a, $b); array_intersect_assoc($a, $b);
/// };
/// for ($i = 0; $i < 100; $i++) $f();
/// $before = memory_get_usage();
/// for ($i = 0; $i < 20000; $i++) $f();
/// var_dump(memory_get_usage() - $before);
/// ```
/// ```expect
/// int(0)
/// ```
///
/// The declarations of the diff and intersect families.
///
/// ```php
/// foreach (['array_diff', 'array_udiff_uassoc', 'array_intersect_key'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function array_diff ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array $array ]
///     Parameter #1 [ <optional> array ...$arrays ]
///   }
///   - Return [ array ]
/// }
/// Function [ <internal:standard> function array_udiff_uassoc ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array $array ]
///     Parameter #1 [ <optional> ...$rest ]
///   }
///   - Return [ array ]
/// }
/// Function [ <internal:standard> function array_intersect_key ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array $array ]
///     Parameter #1 [ <optional> array ...$arrays ]
///   }
///   - Return [ array ]
/// }
/// ```
#[php_function]
fn array_diff(
    req: &mut Request,
    array: Raw<PhpArray>,
    arrays: Variadic<Raw<PhpArray>>,
) -> Result<Owner<PhpArray>, Thrown> {
    let parsed = parse(req, &listed(array, &arrays), 0)?;
    let values = parsed.arrays;
    let first = array_at(req, 1, &values[0])?;
    let check_rest = |req: &mut Request| -> Result<Vec<PhpArray>, Thrown> {
        let mut rest = Vec::new();
        for (at, value) in values.iter().enumerate().skip(1) {
            let position = u32::try_from(at + 1).unwrap_or(u32::MAX);
            rest.push(array_at(req, position, value)?);
        }
        Ok(rest)
    };
    if first.is_empty() {
        check_rest(req)?;
        return Ok(Owner::take(req, PhpArray::empty()));
    }
    let only = if first.len() == 1 {
        walk::rows(req, first).next()
    } else {
        None
    };
    if let Some((_, value)) = only {
        let search = with_string(req, &value, <[u8]>::to_vec)?;
        let mut found = false;
        for (at, other) in values.iter().enumerate().skip(1) {
            let position = u32::try_from(at + 1).unwrap_or(u32::MAX);
            let other = array_at(req, position, other)?;
            if found {
                continue;
            }
            // Holding nothing: a conversion may serve `__toString`.
            let hit = walk::foreach(req, other, Hold::None, |req, row| -> walk::Step {
                if with_string(req, row.value(), |text| text == search.as_slice())? {
                    return Ok(ControlFlow::Break(()));
                }
                Ok(ControlFlow::Continue(()))
            })?;
            found = hit.is_some();
        }
        if found {
            return Ok(Owner::take(req, PhpArray::empty()));
        }
        return Ok(Owner::take(req, first));
    }
    let rest = check_rest(req)?;
    if rest.iter().all(PhpArray::is_empty) {
        return Ok(Owner::take(req, first));
    }
    let mut exclude: HashSet<Vec<u8>> = HashSet::new();
    for other in &rest {
        walk::foreach(req, *other, Hold::None, |req, row| -> walk::Step {
            exclude.insert(with_string(req, row.value(), <[u8]>::to_vec)?);
            Ok(ControlFlow::Continue(()))
        })?;
    }
    let mut out = PhpArray::new(req)?;
    // `ZEND_HASH_FOREACH_KEY_VAL(Z_ARRVAL(args[0]), idx, key, value)`
    // (`Z/ext/standard/array.c:5965`), no owner of the table taken: the
    // argument holds it while `zval_get_tmp_string` runs `__toString`.
    let walked = walk::foreach(req, first, Hold::None, |req, row| -> walk::Step {
        if with_string(req, row.value(), |text| exclude.contains(text))? {
            return Ok(ControlFlow::Continue(()));
        }
        let value = copied(req, *row.value());
        let key = Owner::take(req, row.key().clone());
        out.set(req, key, value)?;
        Ok(ControlFlow::Continue(()))
    });
    if let Err(thrown) = walked {
        out.give_up(req);
        return Err(thrown);
    }
    Ok(out)
}

/// `array_diff_key(array $array, array ...$arrays): array`, php's
/// `php_array_diff_key` with no data comparison
/// (`Z/ext/standard/array.c:5844`): the entries whose key no other array
/// holds.
///
/// ```php
/// var_dump(array_diff_key(["blue" => 1, "red" => 2, "green" => 3, "purple" => 4, 0 => 5, "1" => 6], ["green" => 5, "blue" => 6, "yellow" => 7, "cyan" => 8, 1 => 0]));
/// ```
/// ```expect
/// array(3) {
///   ["red"]=>
///   int(2)
///   ["purple"]=>
///   int(4)
///   [0]=>
///   int(5)
/// }
/// ```
#[php_function]
fn array_diff_key(
    req: &mut Request,
    array: Raw<PhpArray>,
    arrays: Variadic<Raw<PhpArray>>,
) -> Result<Owner<PhpArray>, Thrown> {
    by_key(req, &listed(array, &arrays), Data::None, true)
}

/// `array_diff_assoc(array $array, array ...$arrays): array`, php's
/// `php_array_diff_key` comparing values as strings
/// (`Z/ext/standard/array.c:5990`): the entries whose key and value, as
/// a string, no other array holds together.
///
/// ```php
/// var_dump(array_diff_assoc(["a" => "green", "b" => "brown", "c" => "blue", "red"], ["a" => "green", "yellow", "red"]), array_diff_assoc([0, 1, 2], ["00", "01", "2"]));
/// ```
/// ```expect
/// array(3) {
///   ["b"]=>
///   string(5) "brown"
///   ["c"]=>
///   string(4) "blue"
///   [0]=>
///   string(3) "red"
/// }
/// array(2) {
///   [0]=>
///   int(0)
///   [1]=>
///   int(1)
/// }
/// ```
#[php_function]
fn array_diff_assoc(
    req: &mut Request,
    array: Raw<PhpArray>,
    arrays: Variadic<Raw<PhpArray>>,
) -> Result<Owner<PhpArray>, Thrown> {
    by_key(req, &listed(array, &arrays), Data::Internal, true)
}

/// `array_udiff_assoc(array $array, ...$rest): array`, php's
/// `php_array_diff_key` with `zval_user_compare`
/// (`Z/ext/standard/array.c:6004`): `array_diff_assoc` with the values
/// compared by the callback, which is asked once per key the other
/// array shares, and whose bool answer raises no deprecation.
///
/// ```php
/// var_dump(array_udiff_assoc(["a" => 1, "b" => 2, "c" => 3], ["a" => "1", "b" => 5], function ($x, $y) { echo "$x:$y "; return $x == $y ? 0 : 1; }));
/// var_dump(array_udiff_assoc(["a" => 1], ["a" => 1], fn($x, $y) => false));
/// ```
/// ```expect
/// 1:1 2:5 array(2) {
///   ["b"]=>
///   int(2)
///   ["c"]=>
///   int(3)
/// }
/// array(0) {
/// }
/// ```
#[php_function(arity = manual)]
fn array_udiff_assoc(
    req: &mut Request,
    array: Raw<PhpArray>,
    rest: Variadic<Untyped<Value>>,
) -> Result<Owner<PhpArray>, Thrown> {
    by_key(req, &rested(array, rest), Data::User, true)
}

/// `array_intersect_key(array $array, array ...$arrays): array`, php's
/// `php_array_intersect_key` with no data comparison
/// (`Z/ext/standard/array.c:5460`): the entries whose key every other
/// array holds.
///
/// ```php
/// var_dump(array_intersect_key(["blue" => 1, "red" => 2, "green" => 3, "purple" => 4], ["green" => 5, "blue" => 6, "yellow" => 7, "cyan" => 8]), array_intersect_key([1, 2, 3]));
/// try { array_intersect_key([1], 2); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { array_intersect_key(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   ["blue"]=>
///   int(1)
///   ["green"]=>
///   int(3)
/// }
/// array(3) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
///   [2]=>
///   int(3)
/// }
/// array_intersect_key(): Argument #2 must be of type array, int given
/// array_intersect_key() expects at least 1 argument, 0 given
/// ```
#[php_function]
fn array_intersect_key(
    req: &mut Request,
    array: Raw<PhpArray>,
    arrays: Variadic<Raw<PhpArray>>,
) -> Result<Owner<PhpArray>, Thrown> {
    by_key(req, &listed(array, &arrays), Data::None, false)
}

/// `array_intersect_assoc(array $array, array ...$arrays): array`, php's
/// `php_array_intersect_key` comparing values as strings
/// (`Z/ext/standard/array.c:5488`).
///
/// ```php
/// var_dump(array_intersect_assoc(["a" => "green", "b" => "brown", "c" => "blue", "red"], ["a" => "green", "b" => "yellow", "blue", "red"]));
/// ```
/// ```expect
/// array(1) {
///   ["a"]=>
///   string(5) "green"
/// }
/// ```
#[php_function]
fn array_intersect_assoc(
    req: &mut Request,
    array: Raw<PhpArray>,
    arrays: Variadic<Raw<PhpArray>>,
) -> Result<Owner<PhpArray>, Thrown> {
    by_key(req, &listed(array, &arrays), Data::Internal, false)
}

/// `array_uintersect_assoc(array $array, ...$rest): array`, php's
/// `php_array_intersect_key` with `zval_user_compare`
/// (`Z/ext/standard/array.c:5502`).
///
/// ```php
/// var_dump(array_uintersect_assoc(["a" => "green", "b" => "brown", "c" => "blue", "red"], ["a" => "GREEN", "B" => "brown", "yellow", "red"], fn($x, $y) => strtolower($x) <=> strtolower($y)));
/// ```
/// ```expect
/// array(1) {
///   ["a"]=>
///   string(5) "green"
/// }
/// ```
#[php_function(arity = manual)]
fn array_uintersect_assoc(
    req: &mut Request,
    array: Raw<PhpArray>,
    rest: Variadic<Untyped<Value>>,
) -> Result<Owner<PhpArray>, Thrown> {
    by_key(req, &rested(array, rest), Data::User, false)
}

/// `array_udiff(array $array, ...$rest): array`, php's `php_array_diff`
/// with `DIFF_NORMAL` and the data callback
/// (`Z/ext/standard/array.c:5983`): each array sorted by the callback
/// with `zend_sort`, then the walk, the callback asked in the walk's
/// order.
///
/// ```php
/// var_dump(array_udiff([1, 5, 3], [3], function ($a, $b) { echo "$a-$b "; return $a <=> $b; }));
/// ```
/// ```expect
/// 1-5 5-3 1-3 1-3 1-3 3-3 3-5 array(2) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(5)
/// }
/// ```
///
/// The callbacks ride at the end of the arguments, held to being
/// callbacks before any array is held to being one.
///
/// ```php
/// try { array_udiff([1]); } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// try { array_udiff([1], [2], 'nope'); } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// try { array_udiff([1], 5, fn($a, $b) => 0); } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// ArgumentCountError: array_udiff() expects at least 2 arguments, 1 given
/// TypeError: array_udiff(): Argument #3 must be a valid callback, function "nope" not found or invalid function name
/// TypeError: array_udiff(): Argument #2 must be of type array, int given
/// ```
#[php_function(arity = manual)]
fn array_udiff(
    req: &mut Request,
    array: Raw<PhpArray>,
    rest: Variadic<Untyped<Value>>,
) -> Result<Owner<PhpArray>, Thrown> {
    diff_sorted(
        req,
        &rested(array, rest),
        Behavior::Normal,
        Data::User,
        Keys::Internal,
    )
}

/// `array_diff_ukey(array $array, ...$rest): array`, php's
/// `php_array_diff` with `DIFF_KEY` and the key callback
/// (`Z/ext/standard/array.c:5851`).
///
/// ```php
/// var_dump(array_diff_ukey(["a" => 1, "b" => 5, 3], ["a" => 3], function ($a, $b) { echo "$a-$b "; return $a <=> $b; }));
/// ```
/// ```expect
/// a-b b-0 a-0 0-a a-a b-a array(2) {
///   ["b"]=>
///   int(5)
///   [0]=>
///   int(3)
/// }
/// ```
#[php_function(arity = manual)]
fn array_diff_ukey(
    req: &mut Request,
    array: Raw<PhpArray>,
    rest: Variadic<Untyped<Value>>,
) -> Result<Owner<PhpArray>, Thrown> {
    diff_sorted(
        req,
        &rested(array, rest),
        Behavior::Key,
        Data::Internal,
        Keys::User,
    )
}

/// `array_diff_uassoc(array $array, ...$rest): array`, php's
/// `php_array_diff` with `DIFF_ASSOC`, the keys by the callback and the
/// values as strings (`Z/ext/standard/array.c:5997`).
///
/// ```php
/// var_dump(array_diff_uassoc(["a" => "green", "b" => "brown", "c" => "blue", "red"], ["a" => "green", "yellow", "red"], function ($a, $b) { echo "$a-$b "; return $a <=> $b; }));
/// ```
/// ```expect
/// a-b b-c c-0 b-0 a-0 a-0 1-0 a-1 0-0 a-0 a-1 a-a b-0 b-1 b-a c-0 c-1 c-a array(3) {
///   ["b"]=>
///   string(5) "brown"
///   ["c"]=>
///   string(4) "blue"
///   [0]=>
///   string(3) "red"
/// }
/// ```
#[php_function(arity = manual)]
fn array_diff_uassoc(
    req: &mut Request,
    array: Raw<PhpArray>,
    rest: Variadic<Untyped<Value>>,
) -> Result<Owner<PhpArray>, Thrown> {
    diff_sorted(
        req,
        &rested(array, rest),
        Behavior::Assoc,
        Data::Internal,
        Keys::User,
    )
}

/// `array_udiff_uassoc(array $array, ...$rest): array`, php's
/// `php_array_diff` with `DIFF_ASSOC` and both callbacks, the data one
/// first (`Z/ext/standard/array.c:6011`).
///
/// ```php
/// var_dump(array_udiff_uassoc(["a" => "green", "b" => "brown", "c" => "blue", "red"], ["a" => "GREEN", "B" => "brown", "yellow", "red"], fn($x, $y) => strtolower($x) <=> strtolower($y), fn($x, $y) => strtolower((string)$x) <=> strtolower((string)$y)));
/// ```
/// ```expect
/// array(2) {
///   ["c"]=>
///   string(4) "blue"
///   [0]=>
///   string(3) "red"
/// }
/// ```
#[php_function(arity = manual)]
fn array_udiff_uassoc(
    req: &mut Request,
    array: Raw<PhpArray>,
    rest: Variadic<Untyped<Value>>,
) -> Result<Owner<PhpArray>, Thrown> {
    diff_sorted(
        req,
        &rested(array, rest),
        Behavior::Assoc,
        Data::User,
        Keys::User,
    )
}

/// `array_intersect(array $array, array ...$arrays): array`, php's
/// `php_array_intersect` with `INTERSECT_NORMAL` and the values as
/// strings (`Z/ext/standard/array.c:5474`): the entries whose value
/// every other array holds, under their keys.
///
/// ```php
/// var_dump(array_intersect(["a" => "green", "red", "blue"], ["b" => "green", "yellow", "red"]), array_intersect([1, "1", 2, 1.0], ["1"], [true]));
/// try { array_intersect(1, [2]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   ["a"]=>
///   string(5) "green"
///   [0]=>
///   string(3) "red"
/// }
/// array(3) {
///   [0]=>
///   int(1)
///   [1]=>
///   string(1) "1"
///   [3]=>
///   float(1)
/// }
/// array_intersect(): Argument #1 ($array) must be of type array, int given
/// ```
#[php_function]
fn array_intersect(
    req: &mut Request,
    array: Raw<PhpArray>,
    arrays: Variadic<Raw<PhpArray>>,
) -> Result<Owner<PhpArray>, Thrown> {
    intersect_sorted(
        req,
        &listed(array, &arrays),
        Behavior::Normal,
        Data::Internal,
        Keys::Internal,
    )
}

/// `array_uintersect(array $array, ...$rest): array`, php's
/// `php_array_intersect` with `INTERSECT_NORMAL` and the data callback
/// (`Z/ext/standard/array.c:5481`).
///
/// ```php
/// var_dump(array_uintersect(["a" => 1, "b" => 5, 3], [3, 5], function ($a, $b) { echo "$a-$b "; return $a <=> $b; }));
/// ```
/// ```expect
/// 1-5 5-3 1-3 3-5 1-3 3-3 3-3 3-5 5-5 array(2) {
///   ["b"]=>
///   int(5)
///   [0]=>
///   int(3)
/// }
/// ```
#[php_function(arity = manual)]
fn array_uintersect(
    req: &mut Request,
    array: Raw<PhpArray>,
    rest: Variadic<Untyped<Value>>,
) -> Result<Owner<PhpArray>, Thrown> {
    intersect_sorted(
        req,
        &rested(array, rest),
        Behavior::Normal,
        Data::User,
        Keys::Internal,
    )
}

/// `array_intersect_ukey(array $array, ...$rest): array`, php's
/// `php_array_intersect` with `INTERSECT_KEY` and the key callback
/// (`Z/ext/standard/array.c:5467`).
///
/// ```php
/// var_dump(array_intersect_ukey(["blue" => 1, "red" => 2, "green" => 3, "purple" => 4], ["green" => 5, "blue" => 6, "yellow" => 7, "cyan" => 8], fn($a, $b) => $a <=> $b));
/// ```
/// ```expect
/// array(2) {
///   ["blue"]=>
///   int(1)
///   ["green"]=>
///   int(3)
/// }
/// ```
#[php_function(arity = manual)]
fn array_intersect_ukey(
    req: &mut Request,
    array: Raw<PhpArray>,
    rest: Variadic<Untyped<Value>>,
) -> Result<Owner<PhpArray>, Thrown> {
    intersect_sorted(
        req,
        &rested(array, rest),
        Behavior::Key,
        Data::Internal,
        Keys::User,
    )
}

/// `array_intersect_uassoc(array $array, ...$rest): array`, php's
/// `php_array_intersect` with `INTERSECT_ASSOC`, the keys by the
/// callback and the values as strings (`Z/ext/standard/array.c:5495`).
///
/// ```php
/// var_dump(array_intersect_uassoc(["a" => "green", "b" => "brown", "c" => "blue", "red"], ["a" => "GREEN", "B" => "brown", "yellow", "red"], fn($a, $b) => strtolower((string)$a) <=> strtolower((string)$b)));
/// ```
/// ```expect
/// array(1) {
///   ["b"]=>
///   string(5) "brown"
/// }
/// ```
#[php_function(arity = manual)]
fn array_intersect_uassoc(
    req: &mut Request,
    array: Raw<PhpArray>,
    rest: Variadic<Untyped<Value>>,
) -> Result<Owner<PhpArray>, Thrown> {
    intersect_sorted(
        req,
        &rested(array, rest),
        Behavior::Assoc,
        Data::Internal,
        Keys::User,
    )
}

/// `array_uintersect_uassoc(array $array, ...$rest): array`, php's
/// `php_array_intersect` with `INTERSECT_ASSOC` and both callbacks
/// (`Z/ext/standard/array.c:5509`). php's one callback slot is switched
/// to the data callback for a value comparison and back only where the
/// values differ, so after two equal values the next array's keys are
/// compared by the data callback, as here.
///
/// ```php
/// var_dump(array_uintersect_uassoc(["a" => "green", "b" => "brown", "c" => "blue", "red"], ["a" => "GREEN", "B" => "brown", "yellow", "red"], fn($x, $y) => strtolower($x) <=> strtolower($y), fn($x, $y) => strtolower((string)$x) <=> strtolower((string)$y)));
/// var_dump(array_uintersect_uassoc(["a" => 1, "b" => 2], ["a" => 1, "b" => 2], ["a" => 1, "b" => 2], function ($x, $y) { echo "d($x,$y) "; return $x <=> $y; }, function ($x, $y) { echo "k($x,$y) "; return $x <=> $y; }));
/// ```
/// ```expect
/// array(2) {
///   ["a"]=>
///   string(5) "green"
///   ["b"]=>
///   string(5) "brown"
/// }
/// k(a,b) k(a,b) k(a,b) k(a,a) d(1,1) d(a,a) d(1,1) k(b,b) d(2,2) d(b,b) d(2,2) array(2) {
///   ["a"]=>
///   int(1)
///   ["b"]=>
///   int(2)
/// }
/// ```
#[php_function(arity = manual)]
fn array_uintersect_uassoc(
    req: &mut Request,
    array: Raw<PhpArray>,
    rest: Variadic<Untyped<Value>>,
) -> Result<Owner<PhpArray>, Thrown> {
    intersect_sorted(
        req,
        &rested(array, rest),
        Behavior::Assoc,
        Data::User,
        Keys::User,
    )
}

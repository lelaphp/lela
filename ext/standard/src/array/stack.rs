//! The natives of `array.c` that treat an array as a stack or a queue,
//! and the two that cut one: `array_push` and `array_pop` at its end,
//! `array_shift` and `array_unshift` at its start, `array_splice`,
//! which cuts the array a reference holds, and `array_slice`, which cuts
//! a copy. The four that write take the array by reference and write
//! the one the reference holds, separated first; where php rebuilds the
//! table in place (`array_shift`, `array_unshift`, `php_splice`), the
//! rebuilt table is written into the reference.

use php_ext::abi::{Passed, Ref, Variadic};
use php_ext::attr::php_function;
use php_ext::coerce::to_array;
use php_ext::raising::{Thrown, throw_error};
use php_ext::request::Request;
use php_ext::values::walk::{self};
use php_ext::values::{Key, Owner, PhpArray, PhpRef, Value};

use super::copied;

/// The array a by-reference argument holds, as `Z_PARAM_ARRAY_EX(array,
/// 0, 1)` separates it, with the cell to write a rebuilt table into; an
/// argument that came by value, past its notice, is a cell of its own
/// nobody reads after.
pub(super) struct Stack {
    pub(super) array: PhpArray,
    /// A cell of the native's own over a copy, where the call sent a
    /// value for the reference: php separates the argument slot, which
    /// nobody reads after.
    owned: Option<Owner<PhpRef>>,
    given: Ref<PhpArray>,
}

impl Stack {
    /// `body` over the array `given` holds ([`Stack::of`]), then the
    /// stack finished, whichever way `body` ended, as php's frame frees
    /// the argument slot once the call is done.
    pub(super) fn over<T>(
        req: &mut Request,
        given: Ref<PhpArray>,
        body: impl FnOnce(&mut Request, &mut Stack) -> Result<T, Thrown>,
    ) -> Result<T, Thrown> {
        let mut stack = Stack::of(req, given)?;
        let answer = body(req, &mut stack);
        stack.finish(req);
        answer
    }

    /// The array `given` holds, separated where the caller's reference
    /// holds it, a copy in a cell of its own where the call sent a value.
    fn of(req: &mut Request, given: Ref<PhpArray>) -> Result<Stack, Thrown> {
        if let Some(array) = given.separated(req)? {
            return Ok(Stack {
                array,
                owned: None,
                given,
            });
        }
        let made = match given.get() {
            Some(array) => array.copy(req)?,
            None => PhpArray::new(req)?,
        };
        let array = *made;
        let cell = PhpRef::new(req, made.into_value())?;
        Ok(Stack {
            array,
            owned: Some(cell),
            given,
        })
    }

    /// A rebuilt table written where the array stood.
    pub(super) fn replace(
        &mut self,
        req: &mut Request,
        rebuilt: Owner<PhpArray>,
    ) -> Result<(), Thrown> {
        let view = *rebuilt;
        match self.owned.as_deref().copied() {
            Some(cell) => cell.set(req, rebuilt.into_value())?,
            None => self.given.set(req, rebuilt)?,
        }
        self.array = view;
        Ok(())
    }

    /// The cell of its own given up.
    fn finish(self, req: &mut Request) {
        if let Some(cell) = self.owned {
            cell.give_up(req);
        }
    }
}

/// `array_push(array &$array, mixed ...$values): int`, php's
/// `PHP_FUNCTION(array_push)` (`Z/ext/standard/array.c:3520`): each value
/// appended at `nNextFreeElement`, and the count after. An array whose
/// next index is taken is php's `Error`.
///
/// ```php
/// $f = [1]; var_dump(array_push($f, 2, [3]), $f, array_push($f));
/// $h = [PHP_INT_MAX => 1]; try { array_push($h, 2); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(3)
/// array(3) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
///   [2]=>
///   array(1) {
///     [0]=>
///     int(3)
///   }
/// }
/// int(3)
/// Cannot add element to the array as the next element is already occupied
/// ```
///
/// The declarations of the stack natives.
///
/// ```php
/// foreach (['array_push', 'array_pop', 'array_shift', 'array_unshift', 'array_splice', 'array_slice'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function array_push ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array &$array ]
///     Parameter #1 [ <optional> mixed ...$values ]
///   }
///   - Return [ int ]
/// }
/// Function [ <internal:standard> function array_pop ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> array &$array ]
///   }
///   - Return [ mixed ]
/// }
/// Function [ <internal:standard> function array_shift ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> array &$array ]
///   }
///   - Return [ mixed ]
/// }
/// Function [ <internal:standard> function array_unshift ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> array &$array ]
///     Parameter #1 [ <optional> mixed ...$values ]
///   }
///   - Return [ int ]
/// }
/// Function [ <internal:standard> function array_splice ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> array &$array ]
///     Parameter #1 [ <required> int $offset ]
///     Parameter #2 [ <optional> ?int $length = null ]
///     Parameter #3 [ <optional> mixed $replacement = [] ]
///   }
///   - Return [ array ]
/// }
/// Function [ <internal:standard> function array_slice ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> array $array ]
///     Parameter #1 [ <required> int $offset ]
///     Parameter #2 [ <optional> ?int $length = null ]
///     Parameter #3 [ <optional> bool $preserve_keys = false ]
///   }
///   - Return [ array ]
/// }
/// ```
#[php_function]
fn array_push(
    req: &mut Request,
    array: Ref<PhpArray>,
    values: Variadic<Value>,
) -> Result<i64, Thrown> {
    Stack::over(req, array, |req, stack| {
        for value in values {
            stack.array.append(req, Owner::take(req, value))?;
        }
        Ok(i64::try_from(stack.array.len()).unwrap_or(i64::MAX))
    })
}

/// `array_pop(array &$array): mixed`, php's `PHP_FUNCTION(array_pop)`
/// (`Z/ext/standard/array.c:3549`): the last element taken out and
/// handed back, a reference unwrapped, `null` for an empty array;
/// `nNextFreeElement` moved back one where the element's key was the
/// last index appended, and the pointer reset.
///
/// ```php
/// $a = [1, 2, 3]; var_dump(array_pop($a)); $a[] = 'x'; var_dump($a);
/// $b = [0, 1, 2, 3]; unset($b[3]); var_dump(array_pop($b)); $b[] = 'y'; var_dump(array_keys($b));
/// $e = []; var_dump(array_pop($e));
/// $g = [5 => 'a', 6 => 'b']; next($g); array_pop($g); var_dump(current($g));
/// $x = 1; $r = []; $r[0] = &$x; $v = array_pop($r); $v = 5; var_dump($x);
/// ```
/// ```expect
/// int(3)
/// array(3) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
///   [2]=>
///   string(1) "x"
/// }
/// int(2)
/// array(3) {
///   [0]=>
///   int(0)
///   [1]=>
///   int(1)
///   [2]=>
///   int(4)
/// }
/// NULL
/// string(1) "a"
/// int(1)
/// ```
#[php_function]
fn array_pop(req: &mut Request, array: Ref<PhpArray>) -> Result<Owner<Value>, Thrown> {
    Stack::over(req, array, |req, stack| {
        // The last element as `ZEND_HASH_REVERSE_FOREACH` finds it, from
        // `nNumUsed` back: the internal pointer set to it, and reset below as
        // php resets it.
        stack.array.cursor_end();
        let (Some(key), Some(value)) = (stack.array.cursor_key(), stack.array.cursor_current())
        else {
            return Ok(Owner::null());
        };
        let taken = Owner::take(req, value);
        let popped = stack.array.unset(req, &key);
        if let Key::Int(index) = key
            && index == stack.array.next_free().wrapping_sub(1)
            && let Err(thrown) = stack.array.set_next_free(req, index)
        {
            taken.give_up(req);
            return Err(thrown);
        }
        stack.array.cursor_reset();
        let value = unwrapped(req, taken);
        if let Err(thrown) = popped {
            value.give_up(req);
            return Err(thrown);
        }
        Ok(value)
    })
}

/// `zend_unwrap_reference` over the element `RETVAL_COPY_VALUE` took out,
/// made once the table is changed: a reference's value as it stands
/// after, with one more owner, in place of the reference, so an element
/// that referred to the array itself hands back the array as it is left.
/// `taken` is the owner the native took of the element.
fn unwrapped(req: &mut Request, taken: Owner<Value>) -> Owner<Value> {
    let Some(reference) = taken.as_php_ref() else {
        return taken;
    };
    let value = Owner::take(req, reference.get().deref());
    taken.give_up(req);
    value
}

/// `array_shift(array &$array): mixed`, php's `PHP_FUNCTION(array_shift)`
/// (`Z/ext/standard/array.c:3620`): the first element taken out and
/// handed back, the integer keys renumbered from zero and string keys
/// kept, `nNextFreeElement` the count of integer keys, and the pointer
/// reset.
///
/// ```php
/// $c = ["a" => 1, 5 => 2, "b" => 3, 9 => 4]; var_dump(array_shift($c), $c); $c[] = 'z'; var_dump(array_keys($c));
/// $e = []; var_dump(array_shift($e));
/// $q = [1, 2]; $copy = $q; array_shift($q); var_dump(count($copy));
/// ```
/// ```expect
/// int(1)
/// array(3) {
///   [0]=>
///   int(2)
///   ["b"]=>
///   int(3)
///   [1]=>
///   int(4)
/// }
/// array(4) {
///   [0]=>
///   int(0)
///   [1]=>
///   string(1) "b"
///   [2]=>
///   int(1)
///   [3]=>
///   int(2)
/// }
/// NULL
/// int(2)
/// ```
///
/// The element taken out is unwrapped once the table is left as it is,
/// `zend_unwrap_reference` after the delete, so an element that refers
/// to the array itself hands back the array without it (GH-16957,
/// GH-17447), as `array_pop` does.
///
/// ```php
/// $a = [&$a, 1, 2]; $s = array_shift($a); var_dump($s === $a, count($s));
/// $b = []; $b[] = &$b; var_dump(array_pop($b), $b);
/// ```
/// ```expect
/// bool(true)
/// int(2)
/// array(0) {
/// }
/// array(0) {
/// }
/// ```
///
/// A `foreach` by reference over the array goes on at the element it
/// stood before, the renumbering moving its iterator with it.
///
/// ```php
/// $a = [0, 1, 2, 3];
/// foreach ($a as &$v) { echo $v; if ($v == 1) { array_shift($a); } }
/// echo "\n";
/// ```
/// ```expect
/// 0123
/// ```
#[php_function]
fn array_shift(req: &mut Request, array: Ref<PhpArray>) -> Result<Owner<Value>, Thrown> {
    Stack::over(req, array, |req, stack| {
        // The rebuild runs no code of the program, as php's in-place one runs
        // none: the rows are read under the borrowed request, views the table
        // the stack holds keeps until it is replaced.
        let rows: Vec<(Key, Value)> = walk::rows(req, stack.array).collect();
        let Some((_, first)) = rows.first().cloned() else {
            return Ok(Owner::null());
        };
        let mut rebuilt = PhpArray::new(req)?;
        for (key, element) in rows.into_iter().skip(1) {
            let element = Owner::take(req, element);
            match key {
                Key::Str(_) => {
                    let key = Owner::take(req, key.clone());
                    rebuilt.set(req, key, element)?;
                }
                Key::Int(_) => rebuilt.append(req, element)?,
            }
        }
        if rebuilt.next_free() == i64::MIN {
            rebuilt.set_next_free(req, 0)?;
        }
        shifted_iterators(req, stack.array);
        stack.array.iterators_carry(req, &rebuilt);
        // The first element's owner, taken before the rebuilt table
        // replaces the one that held it.
        let first = Owner::take(req, first);
        if let Err(thrown) = stack.replace(req, rebuilt) {
            first.give_up(req);
            return Err(thrown);
        }
        Ok(unwrapped(req, first))
    })
}

/// The hash iterators on the table `array_shift` renumbers moved where
/// php's in-place renumbering leaves them: over a packed table, each
/// iterator on an element that moves goes with it to its index `k`, as
/// the arm walks them (`Z/ext/standard/array.c:3668`); over a hash,
/// `zend_hash_rehash` closes the holes, and every iterator at or before
/// an element goes to the index it takes, the end to the new end. The
/// rebuilt table has no holes, so an iterator php leaves on one reads
/// the element after it there as here.
fn shifted_iterators(req: &mut Request, table: PhpArray) {
    let Some((first, _, _)) = table.live_from(0) else {
        return;
    };
    let packed = table.is_packed();
    let mut iter_pos = table.iterators_lower_pos(req, 0);
    let mut idx = first.saturating_add(1);
    let mut k = 0;
    while let Some((at, _, _)) = table.live_from(idx) {
        idx = at.saturating_add(1);
        if packed {
            if at != k && at == iter_pos {
                table.iterators_update(req, at, k);
                iter_pos = table.iterators_lower_pos(req, iter_pos.saturating_add(1));
            }
        } else {
            while iter_pos <= at {
                if iter_pos != k {
                    table.iterators_update(req, iter_pos, k);
                }
                iter_pos = table.iterators_lower_pos(req, iter_pos.saturating_add(1));
            }
        }
        k += 1;
    }
    if !packed && idx != k {
        table.iterators_update(req, idx, k);
    }
}

/// `array_unshift(array &$array, mixed ...$values): int`, php's
/// `PHP_FUNCTION(array_unshift)` (`Z/ext/standard/array.c:3738`): a new
/// table of the values and then the array's elements, integer keys
/// renumbered and string keys kept, written in place of the old, and
/// the count after.
///
/// ```php
/// $d = [3 => 'p', 'k' => 'q']; var_dump(array_unshift($d, 'u', 'v'), $d, array_unshift($d));
/// ```
/// ```expect
/// int(4)
/// array(4) {
///   [0]=>
///   string(1) "u"
///   [1]=>
///   string(1) "v"
///   [2]=>
///   string(1) "p"
///   ["k"]=>
///   string(1) "q"
/// }
/// int(4)
/// ```
///
/// A `foreach` by reference over the array goes on past the values put
/// before it, php's `zend_hash_iterators_advance` over the table it
/// swaps the new data into (#1216).
///
/// ```php
/// $a = [1, 2, 3];
/// foreach ($a as &$v) { echo "$v\n"; if ($v == 2) { array_unshift($a, 0, 0, 0, 0, 0, 0, 0, 0); } }
/// var_dump(count($a));
/// ```
/// ```expect
/// 1
/// 2
/// 3
/// int(11)
/// ```
#[php_function]
fn array_unshift(
    req: &mut Request,
    array: Ref<PhpArray>,
    values: Variadic<Value>,
) -> Result<i64, Thrown> {
    Stack::over(req, array, |req, stack| {
        let mut rebuilt = PhpArray::new(req)?;
        let argc = values.len();
        for value in values {
            rebuilt.append(req, Owner::take(req, value))?;
        }
        // The rebuild runs no code of the program, as php's in-place one runs
        // none: the rows are read under the borrowed request, views the table
        // the stack holds keeps until it is replaced.
        let rows: Vec<(Key, Value)> = walk::rows(req, stack.array).collect();
        for (key, element) in rows {
            let element = Owner::take(req, element);
            match key {
                Key::Str(_) => {
                    let key = Owner::take(req, key.clone());
                    rebuilt.set(req, key, element)?;
                }
                Key::Int(_) => rebuilt.append(req, element)?,
            }
        }
        let count = i64::try_from(rebuilt.len()).unwrap_or(i64::MAX);
        // `zend_hash_iterators_advance(Z_ARRVAL_P(stack), argc)` and the
        // swap: a `foreach` by reference over the array goes on past the
        // values put before it.
        let argc = u32::try_from(argc).unwrap_or(u32::MAX);
        stack.array.iterators_advance(req, argc);
        stack.array.iterators_carry(req, &rebuilt);
        stack.replace(req, rebuilt)?;
        Ok(count)
    })
}

/// `array_splice(array &$array, int $offset, ?int $length = null, mixed
/// $replacement = []): array`, php's `PHP_FUNCTION(array_splice)` over
/// `php_splice` (`Z/ext/standard/array.c:3312`): the elements from
/// `$offset` for `$length` removed and handed back as a list, the
/// replacement's values put in their place, and the array rebuilt with
/// its integer keys renumbered and its string keys kept. A negative
/// offset counts from the end, a negative length stops that far from
/// it, and a replacement that is not an array is cast to one.
///
/// ```php
/// $a = ["a" => 1, 2, 3, 4, "b" => 5]; var_dump(array_splice($a, 1, 2, ["x", "y", "z"]), $a);
/// $b = [1, 2, 3, 4, 5]; var_dump(array_splice($b, -2), $b);
/// $c = [1, 2, 3, 4, 5]; array_splice($c, 1, -2, "only"); var_dump($c);
/// $d = [1, 2]; var_dump(array_splice($d, 5), array_splice($d, -9, 1));
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   int(2)
///   [1]=>
///   int(3)
/// }
/// array(6) {
///   ["a"]=>
///   int(1)
///   [0]=>
///   string(1) "x"
///   [1]=>
///   string(1) "y"
///   [2]=>
///   string(1) "z"
///   [3]=>
///   int(4)
///   ["b"]=>
///   int(5)
/// }
/// array(2) {
///   [0]=>
///   int(4)
///   [1]=>
///   int(5)
/// }
/// array(3) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
///   [2]=>
///   int(3)
/// }
/// array(4) {
///   [0]=>
///   int(1)
///   [1]=>
///   string(4) "only"
///   [2]=>
///   int(4)
///   [3]=>
///   int(5)
/// }
/// array(0) {
/// }
/// array(1) {
///   [0]=>
///   int(1)
/// }
/// ```
///
/// A removed element is released as it is deleted where the call's
/// value is not read (`USED_RET()`), with the splice holding the table;
/// a destructor that writes the array then separates it, and the splice
/// finds itself its table's only holder and throws, the array left as
/// the destructor wrote it. Where the value is read, the removed
/// elements live in it and nothing runs mid-splice (GH-16649).
///
/// ```php
/// class C { function __destruct() { global $a; echo "dtor ", count($a), "\n"; $a[] = 9; } }
/// $a = [1, new C, 2]; try { array_splice($a, 1, 1); } catch (Error $e) { echo $e->getMessage(), "\n"; } var_dump($a);
/// $a = [1, new C, 2]; $r = array_splice($a, 1, 1); echo "kept\n"; unset($r); var_dump($a);
/// ```
/// ```expect
/// dtor 2
/// Array was modified during array_splice operation
/// array(3) {
///   [0]=>
///   int(1)
///   [2]=>
///   int(2)
///   [3]=>
///   int(9)
/// }
/// kept
/// dtor 2
/// array(3) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
///   [2]=>
///   int(9)
/// }
/// ```
///
/// What the call hands back keeps a string key and renumbers an integer
/// one, `zend_hash_add_new(removed, p->key, entry)`.
///
/// ```php
/// $a = ["x" => 1, 5 => 2, "y" => 3, 4]; var_dump(array_splice($a, 0, 3), $a);
/// ```
/// ```expect
/// array(3) {
///   ["x"]=>
///   int(1)
///   [0]=>
///   int(2)
///   ["y"]=>
///   int(3)
/// }
/// array(1) {
///   [0]=>
///   int(4)
/// }
/// ```
///
/// A `foreach` by reference over the array goes on where php's
/// `php_splice` moves its iterator: past the elements removed before
/// it, and past the replacement where it stood on a removed one.
///
/// ```php
/// $a = [0, 1, 2, 3, 4];
/// foreach ($a as &$v) { echo $v; if ($v == 3) { array_splice($a, 1, 2); } }
/// echo "\n";
/// $a = [0, 1, 2, 3, 4];
/// foreach ($a as &$v) { echo $v; if ($v == 2) { array_splice($a, 1, 3, ['x', 'y', 'z']); } }
/// echo "\n";
/// ```
/// ```expect
/// 01234
/// 0124
/// ```
#[php_function]
fn array_splice(
    req: &mut Request,
    array: Ref<PhpArray>,
    offset: i64,
    #[php(default = null)] length: Option<i64>,
    #[php(default = [])] replacement: Passed<Value>,
) -> Result<Owner<PhpArray>, Thrown> {
    Stack::over(req, array, |req, stack| {
        let replacement = match replacement {
            Passed::Given(given) => Some(match given.as_array() {
                Some(array) => Owner::take(req, array),
                None => to_array(req, &given)?,
            }),
            Passed::Omitted => None,
        };
        let num_in = i64::try_from(stack.array.len()).unwrap_or(i64::MAX);
        let length = length.unwrap_or(num_in);
        let (start, taken) = clamp_splice(num_in, offset, length);
        // `USED_RET()`: the removed elements are gathered only where the
        // caller reads them; otherwise each is released as it is deleted.
        let mut removed = if req.frames().return_value_used() {
            Some(PhpArray::new(req)?)
        } else {
            None
        };
        let mut rebuilt = PhpArray::new(req)?;
        // php walks the table's buckets, `idx`, beside the position each
        // kept element takes in the new one, `pos`, and moves the hash
        // iterators a `foreach` by reference holds from the one to the other
        // as it passes them (`zend_hash_iterators_update`), so the loop goes
        // on in the rebuilt table at the element it stood before.
        let packed = stack.array.is_packed();
        let mut walk = Walk {
            iter_pos: stack.array.iterators_lower_pos(req, 0),
            idx: 0,
            pos: 0,
        };
        while walk.pos < start {
            let Some((at, key, element)) = stack.array.live_from(walk.idx) else {
                break;
            };
            walk.idx = at.saturating_add(1);
            keep(req, &mut rebuilt, &key, element)?;
            walk.moved(req, stack.array, at);
        }
        let (gone, alone) = remove_range(
            req,
            stack,
            &mut walk,
            removed.as_mut(),
            (start, taken),
            packed,
        )?;
        // The removed arm counts `pos` over what it takes, and the other
        // counts beside it; the hash arm looks for the lowest iterator again.
        if removed.is_some() {
            walk.pos += gone;
        }
        if !packed {
            walk.iter_pos = stack.array.iterators_lower_pos(req, walk.iter_pos);
        }
        if let Some(replacement) = &replacement {
            // The replacement's rows, which this native owns, appended; no code
            // of the program runs between.
            let values: Vec<Value> = walk::rows(req, **replacement).map(|(_, v)| v).collect();
            for value in values {
                rebuilt.append(req, Owner::take(req, value))?;
                walk.pos += 1;
            }
        }
        while let Some((at, key, element)) = stack.array.live_from(walk.idx) {
            walk.idx = at.saturating_add(1);
            keep(req, &mut rebuilt, &key, element)?;
            walk.moved(req, stack.array, at);
        }
        if let Some(replacement) = replacement {
            replacement.give_up(req);
        }
        if let Some(last) = alone {
            // `GC_DELREF(in_hash) == 0`: nothing but the splice holds the
            // table any more, so what the reference holds now stands.
            last.give_up(req);
            rebuilt.give_up(req);
            if let Some(removed) = removed {
                removed.give_up(req);
            }
            return Err(throw_error(
                req,
                "Error",
                "Array was modified during array_splice operation",
            ));
        }
        if rebuilt.next_free() == i64::MIN {
            rebuilt.set_next_free(req, 0)?;
        }
        stack.array.iterators_carry(req, &rebuilt);
        stack.replace(req, rebuilt)?;
        match removed {
            Some(removed) => Ok(removed),
            None => PhpArray::new(req),
        }
    })
}

/// `php_splice`'s middle loop: `taken` elements from the walk's bucket
/// deleted from the table, gathered into `removed` where the call's value
/// is read, and the iterators the packed arm bumps moved. Answers how
/// many it took, and the splice's owner of the table where it is the
/// last one, the program having written the reference over or
/// separated it while a destructor ran.
///
/// php holds one owner of the table across the loop (`GC_ADDREF(in_hash)`)
/// and deletes from it in place whatever its owners
/// (`HT_ALLOW_COW_VIOLATION`): a destructor the removal serves may take
/// a copy of the table, as a throwable's trace takes the call's
/// arguments, and that copy is the same table, not a reason to separate
/// it from under the reference that names it. The owner is let go after
/// the loop, `GC_DELREF(in_hash) == 0` read as nothing else holding it,
/// and is then handed back for the caller to give up.
///
/// ```php
/// class T { function __destruct() { throw new Exception("t"); } }
/// function f() {
///     $arr = [new T, new T, 3];
///     try { array_splice($arr, 0, 2); } catch (Exception $e) { echo $e->getMessage(), " ", $e->getPrevious()->getMessage(), "\n"; }
///     var_dump($arr);
/// }
/// f();
/// ```
/// ```expect
/// t t
/// array(1) {
///   [0]=>
///   int(3)
/// }
/// ```
fn remove_range(
    req: &mut Request,
    stack: &mut Stack,
    walk: &mut Walk,
    removed: Option<&mut Owner<PhpArray>>,
    range: (i64, i64),
    packed: bool,
) -> Result<(i64, Option<Owner<PhpArray>>), Thrown> {
    let held = Owner::take(req, stack.array);
    let gone = remove_held(req, stack, walk, &held, removed, range, packed);
    let alone = if Value::array(*held).refcount() == 1 {
        Some(held)
    } else {
        held.give_up(req);
        None
    };
    match gone {
        Ok(gone) => Ok((gone, alone)),
        Err(thrown) => {
            if let Some(last) = alone {
                last.give_up(req);
            }
            Err(thrown)
        }
    }
}

/// [`remove_range`]'s loop, under the owner it holds.
fn remove_held(
    req: &mut Request,
    stack: &mut Stack,
    walk: &mut Walk,
    held: &Owner<PhpArray>,
    mut removed: Option<&mut Owner<PhpArray>>,
    range: (i64, i64),
    packed: bool,
) -> Result<i64, Thrown> {
    let (start, taken) = range;
    let mut gone = 0;
    while gone < taken {
        let Some((at, key, element)) = stack.array.live_from(walk.idx) else {
            break;
        };
        walk.idx = at.saturating_add(1);
        gone += 1;
        // `zend_hash_packed_del_val` and `zend_hash_del_bucket` with
        // `ZVAL_PTR_DTOR`: a destructor served here runs after the
        // element left the table.
        // `zend_hash_add_new(removed, p->key, entry)`: a string key is
        // kept, copied before the delete gives the bucket's up.
        let kept = match (&removed, &key) {
            (Some(_), Key::Str(_)) => Some(Owner::take(req, key.clone())),
            _ => None,
        };
        let element = Owner::take(req, element);
        held.unset_in_place(req, &key);
        match (removed.as_deref_mut(), kept) {
            (Some(removed), Some(key)) => removed.set(req, key, element)?,
            (Some(removed), None) => removed.append(req, element)?,
            (None, _) => element.give_up(req),
        }
        // The packed arm bumps an iterator on a removed element to the
        // element after the replacement.
        if packed && at == walk.iter_pos {
            let after = u32::try_from(start + taken).unwrap_or(u32::MAX);
            stack.array.iterators_update(req, at, after);
            walk.iter_pos = stack
                .array
                .iterators_lower_pos(req, walk.iter_pos.saturating_add(1));
        }
    }
    Ok(gone)
}

/// Where `php_splice` stands in the table it rebuilds: the bucket it
/// looks at next, the position the next kept element takes in the new
/// table, and the lowest position a hash iterator on the old one stands
/// at from there (`zend_hash_iterators_lower_pos`).
struct Walk {
    iter_pos: u32,
    idx: u32,
    pos: i64,
}

impl Walk {
    /// A kept element at bucket `at` copied: the iterators standing at it
    /// moved to the position it takes (`zend_hash_iterators_update`).
    fn moved(&mut self, req: &mut Request, table: PhpArray, at: u32) {
        if at == self.iter_pos {
            if i64::from(at) != self.pos {
                let to = u32::try_from(self.pos).unwrap_or(u32::MAX);
                table.iterators_update(req, at, to);
            }
            self.iter_pos = table.iterators_lower_pos(req, self.iter_pos.saturating_add(1));
        }
        self.pos += 1;
    }
}

/// An element `php_splice` keeps, into the new table: a string key kept,
/// an integer one renumbered.
fn keep(
    req: &mut Request,
    rebuilt: &mut Owner<PhpArray>,
    key: &Key,
    element: Value,
) -> Result<(), Thrown> {
    let element = Owner::take(req, element);
    match key {
        Key::Str(_) => {
            let key = Owner::take(req, key.clone());
            rebuilt.set(req, key, element)
        }
        Key::Int(_) => rebuilt.append(req, element),
    }
}

/// `php_splice`'s clamps: the offset into `0..=num_in`, from the end
/// where it is negative, and the length to what is left, stopping short
/// of the end where it is negative.
fn clamp_splice(num_in: i64, offset: i64, length: i64) -> (i64, i64) {
    let offset = if offset > num_in {
        num_in
    } else if offset < 0 {
        (num_in + offset).max(0)
    } else {
        offset
    };
    let length = if length < 0 {
        num_in - offset + length
    } else if offset.unsigned_abs().saturating_add(length.unsigned_abs()) > num_in.unsigned_abs() {
        num_in - offset
    } else {
        length
    };
    (offset, length.max(0))
}

/// `array_slice(array $array, int $offset, ?int $length = null, bool
/// $preserve_keys = false): array`, php's `PHP_FUNCTION(array_slice)`
/// (`Z/ext/standard/array.c:3909`): the elements from `$offset` for
/// `$length`, clamped as `array_splice` clamps, string keys kept and
/// integer keys renumbered unless `$preserve_keys`; an offset past the
/// end, or a length that leaves nothing, is the process's empty array,
/// `RETURN_EMPTY_ARRAY`, which costs nothing.
///
/// ```php
/// $m = memory_get_usage();
/// $a = array_slice([1], 1); $b = array_slice([1, 2], 0, -2);
/// var_dump(memory_get_usage() - $m);
/// debug_zval_dump($a);
/// ```
/// ```expect
/// int(0)
/// array(0) interned {
/// }
/// ```
///
/// ```php
/// $a = ["a" => 1, 5 => 2, 3, "b" => 4, 7 => 5];
/// var_dump(array_slice($a, 1, 3), array_slice($a, -2, 1, true), array_slice($a, 9), array_slice($a, 1, -3), array_slice([1, 2, 3], 0, null, true));
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   int(2)
///   [1]=>
///   int(3)
///   ["b"]=>
///   int(4)
/// }
/// array(1) {
///   ["b"]=>
///   int(4)
/// }
/// array(0) {
/// }
/// array(1) {
///   [0]=>
///   int(2)
/// }
/// array(3) {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
///   [2]=>
///   int(3)
/// }
/// ```
#[php_function]
fn array_slice(
    req: &mut Request,
    array: PhpArray,
    offset: i64,
    #[php(default = null)] length: Option<i64>,
    #[php(default = false)] preserve_keys: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let (input, preserve) = (array, preserve_keys);
    let num_in = i64::try_from(input.len()).unwrap_or(i64::MAX);
    if offset > num_in {
        return Ok(Owner::take(req, PhpArray::empty()));
    }
    let (start, taken) = clamp_splice(num_in, offset, length.unwrap_or(num_in));
    if taken <= 0 {
        return Ok(Owner::take(req, PhpArray::empty()));
    }
    let mut out = PhpArray::new(req)?;
    // `ZEND_HASH_FOREACH_KEY_VAL` over the slice's range, which no code of
    // the program reaches: the rows read under the borrowed request, views
    // the argument keeps.
    let rows: Vec<(Key, Value)> = walk::rows(req, input).collect();
    let from = usize::try_from(start).unwrap_or(usize::MAX);
    let count = usize::try_from(taken).unwrap_or(usize::MAX);
    for (key, element) in rows.into_iter().skip(from).take(count) {
        let value = copied(req, element);
        match key {
            Key::Int(_) if !preserve => out.append(req, value)?,
            _ => {
                let key = Owner::take(req, key.clone());
                out.set(req, key, value)?;
            }
        }
    }
    Ok(out)
}

/// php's `Error` for an append past `ZEND_LONG_MAX`.
#[allow(dead_code)]
fn occupied(req: &mut Request) -> Thrown {
    throw_error(
        req,
        "Error",
        "Cannot add element to the array as the next element is already occupied",
    )
}

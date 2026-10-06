//! The callback walks (#1980 candidate 3): a native that runs code of
//! the program for each row of a table, a callback, a magic method or a
//! handler a diagnostic reaches, walks the table here, and the walk owns
//! what it walks while that code runs. Each walk ports one of php's
//! iteration modes once:
//!
//! - [`held`] holds the table for the walk where the program could
//!   give it up, php's `ZVAL_COPY` or `GC_ADDREF` of it, and
//!   [`held_guarded`] guards its recursion beside, php's
//!   `GC_PROTECT_RECURSION`. The default for a body that needs the
//!   request mutably: `extract`, `serialize`, `print_r`.
//! - [`live`] follows php's hash iterator over what a reference holds,
//!   the iterator `foreach` by reference stands on: the holder read again
//!   after each step, a row unset ahead skipped and one appended
//!   visited, and php's `TypeError` once the holder holds no array or
//!   object. `array_walk` and `extract` under `EXTR_REFS`.
//!
//! A walk takes the table and lends the body each row's key and value
//! for that step. The body answers a [`Step`]: std's `ControlFlow` to go
//! on or stop early with a value, and `Err` for a throwable, which `?`
//! hands on. The walk owns the loop and calls the body, so what it took
//! is given back on every way out, `?` included, by the walk itself and
//! never by a `Drop` (design §5.1: nothing runs implicitly on drop). A
//! body keeps a row past its step only by taking an owner of its own
//! ([`crate::api::values::Owner::take`]).

use std::ops::ControlFlow;

use php_heap::{Arr, PropertyTable, TableRow};

use crate::api::raising::{Thrown, fatal, throw_chained, throw_error};
use crate::api::request::Request;
use crate::api::values::{Key, Owner, PhpArray, PhpObject, PhpRef, PhpString, Value};

/// What a walk's body answers for one row: `Continue` to go on to the
/// next, `Break` to end the walk there with what the walk answers, and
/// `Err`, what `?` hands on, a throwable that ends the walk as php's
/// loops end on `EG(exception)`.
pub type Step<B = ()> = Result<ControlFlow<B>, Thrown>;

/// A table as a held walk's caller came by it, which says whether the
/// walk must hold it ([`held`]).
#[derive(Debug, Clone, Copy)]
pub enum Reached {
    /// Held by what the caller reached it from, which cannot be written
    /// in place while the walk runs: the call's argument, a table the
    /// native owns, or a row of a table a held walk is lending, which is
    /// written only after the table holding it separates. The walk takes
    /// nothing, so the refcounts the body's code sees are php's.
    Held(PhpArray),
    /// Through a reference the program shares, which code the body runs
    /// may assign over and so give the table its last owner up: the
    /// walk holds one owner of the table the reference holds as it
    /// starts, and under that owner what writes the table separates it.
    Through(PhpRef, PhpArray),
}

impl Reached {
    /// How a value lent as a row, or passed as an argument, reaches the
    /// table it names: an array as it stands, a reference through the
    /// reference; `None` for anything that names no array.
    #[must_use]
    pub fn of(value: &Value) -> Option<Reached> {
        if let Some(cell) = value.as_php_ref() {
            return cell
                .get()
                .as_array()
                .map(|table| Reached::Through(*cell, *table));
        }
        value.as_array().map(|table| Reached::Held(*table))
    }

    /// The table walked.
    #[must_use]
    pub fn table(&self) -> PhpArray {
        match self {
            Reached::Held(table) | Reached::Through(_, table) => *table,
        }
    }

    /// The owner the walk takes: one where the program could give the
    /// table up, none otherwise.
    fn hold(self, req: &Request) -> Option<Owner<PhpArray>> {
        match self {
            Reached::Held(_) => None,
            Reached::Through(_, table) => Some(Owner::take(req, table)),
        }
    }
}

/// How a walk that guards its recursion ended ([`held_guarded`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a walk inside the table already walked nothing, which the caller shows"]
pub enum Guarded<B> {
    /// The rows were walked: `Some` what the body broke with, `None`
    /// past the last row.
    Walked(Option<B>),
    /// A guarded walk is inside the table already, `GC_IS_RECURSIVE`,
    /// and nothing was walked: what a dumper prints `*RECURSION*` for.
    Recursive,
}

/// php's walk over a table it holds: the rows of the table `reached`
/// names in order, each lent to `body` with its key, a reference row
/// lent as the reference. A table reached through a reference is held
/// by one owner until the walk ends ([`Reached::Through`]), as php's
/// `extract` copies its array first (`ZVAL_COPY`, "The array might be
/// stored in a local variable that will be overwritten") and
/// `zend_array_sort_ex` holds what it sorts, so the rows it lends stand
/// while the body runs. The answer is what `body` broke with, `None`
/// where the walk went past the last row.
///
/// ```php
/// function f() { $k = 'kkkkk'; $v = 'vvvvv'; $z = 'ZZZZZ';
///   $arr = ['arr' => 1, $k . $k => $v . $v];
///   echo extract($arr), ','; $x = $z . $z; echo $arr, ',', $kkkkkkkkkk, "\n"; }
/// f();
/// ```
/// ```expect
/// 2,1,vvvvvvvvvv
/// ```
pub fn held<B>(
    req: &mut Request,
    reached: Reached,
    mut body: impl FnMut(&mut Request, &Key, &Value) -> Step<B>,
) -> Result<Option<B>, Thrown> {
    let owner = reached.hold(req);
    let walked = rows(req, reached.table(), &mut body);
    if let Some(owner) = owner {
        owner.give_up(req);
    }
    walked
}

/// [`held`] guarding its recursion, as php's dumpers and encoders guard
/// a table they descend into: where a guarded walk is inside the table
/// already (`GC_IS_RECURSIVE`) nothing is walked and the answer says
/// so; otherwise the table is marked for the walk
/// (`GC_PROTECT_RECURSION`) and unmarked as it ends, on every way out,
/// inside the walk's owner, so the mark is never cleared on a freed
/// table. An immutable table is marked by neither, as php's is not,
/// since nothing can make one hold itself.
///
/// ```php
/// $a = [1]; $a[] = &$a;
/// print_r($a); echo "\n";
/// ```
/// ```expect
/// Array
/// (
///     [0] => 1
///     [1] => Array
///  *RECURSION*
/// )
///
/// ```
pub fn held_guarded<B>(
    req: &mut Request,
    reached: Reached,
    mut body: impl FnMut(&mut Request, &Key, &Value) -> Step<B>,
) -> Result<Guarded<B>, Thrown> {
    let table = reached.table();
    if table.is_recursive() {
        return Ok(Guarded::Recursive);
    }
    let owner = reached.hold(req);
    table.protect();
    let walked = rows(req, table, &mut body);
    table.unprotect();
    if let Some(owner) = owner {
        owner.give_up(req);
    }
    walked.map(Guarded::Walked)
}

/// The rows of `table` from its first bucket, each lent to `body`, until
/// the last or the first that breaks or throws. The table is read again
/// at each step, so a row stands as long as the bucket that holds it.
fn rows<B>(
    req: &mut Request,
    table: PhpArray,
    body: &mut impl FnMut(&mut Request, &Key, &Value) -> Step<B>,
) -> Result<Option<B>, Thrown> {
    let mut pos = 0;
    while let Some((at, key, value)) = table.live_from(pos) {
        pos = at.saturating_add(1);
        if let ControlFlow::Break(broke) = body(req, &key, &value)? {
            return Ok(Some(broke));
        }
    }
    Ok(None)
}

/// One row a [`live`] walk lends its body: the key and the value as the
/// table held them when the walk stepped onto them, each owned by the
/// walk for the step, so a body whose code unsets the row can still
/// read it, and where the row stands, so the body can make it a
/// reference in place ([`Row::make_ref`]).
#[derive(Debug)]
pub struct Row {
    key: Owner<Key>,
    value: Owner<Value>,
    at: At,
    /// The walk's hash iterator, registered at the table the row is in,
    /// which tells the row whether that table still stands.
    iterator: u32,
}

/// Where a [`Row`] stands.
#[derive(Debug, Clone, Copy)]
enum At {
    /// Bucket `pos` of an array, an object's dynamic table among them.
    Bucket(Arr, u32),
    /// Declared slot `slot` of `owner`, whose dynamic table is `table`:
    /// the instance of the object `held` the reference `holder` held (a
    /// lazy object's is its own once initialised).
    Slot {
        holder: PhpRef,
        held: PhpObject,
        owner: PhpObject,
        table: Arr,
        slot: u32,
    },
}

impl Row {
    /// The row's key, as the table held it.
    #[must_use]
    pub fn key(&self) -> &Key {
        &self.key
    }

    /// The row's value, a reference where the table holds one.
    #[must_use]
    pub fn value(&self) -> &Value {
        &self.value
    }

    /// The row made a reference in place, php's `ZVAL_MAKE_REF` over the
    /// bucket: the reference the table holds there already, or a new one
    /// holding the value, written into the table as it stands, shared or
    /// not, as php writes it, and into a typed property's slot with the
    /// property as its type source (`ZEND_REF_ADD_TYPE_SOURCE`). From
    /// here the walk owns the reference for the step. Asked before the
    /// body runs any code of the program, as php's walk makes it before
    /// its call; once that code has moved the row, the answer is the
    /// `InternalError` it is, never a write elsewhere.
    pub fn make_ref(&mut self, req: &mut Request) -> Result<PhpRef, Thrown> {
        if let Some(cell) = self.value.as_php_ref() {
            return Ok(*cell);
        }
        let moved = |req: &mut Request| {
            fatal(
                req,
                "InternalError: a walk's row was made a reference after its table moved",
            )
        };
        // The table stands while the walk's iterator still names it: a
        // table destroyed under the walk poisons it. An object's slot
        // stands while the reference still holds the object too.
        let place = match self.at {
            At::Bucket(table, pos) if req.heap_mut().iterator_on(self.iterator, table) => {
                table.value_ref_at(pos)
            }
            At::Slot {
                holder,
                held,
                owner,
                table,
                slot,
            } if req.heap_mut().iterator_on(self.iterator, table)
                && holder.get().as_object() == Some(&held) =>
            {
                let at = owner.heap().slot_ref(slot);
                if let Some(at) = at.filter(|at| at.read().is_same_value(self.value.to_zval()))
                    && let Some((typed, name)) = req.slot_typed_property(owner.class(), slot)
                {
                    req.fetch_typed_slot_ref(at, &typed, &name)?;
                    let Some(cell) = Value::of_zval(at.read()).as_php_ref().copied() else {
                        return Err(moved(req));
                    };
                    return Ok(self.lend(req, cell));
                }
                at
            }
            At::Bucket(..) | At::Slot { .. } => None,
        };
        let Some(place) = place.filter(|place| place.read().is_same_value(self.value.to_zval()))
        else {
            return Err(moved(req));
        };
        let cell = req.reference(*self.value)?;
        place.write(cell.as_value().to_zval());
        Ok(self.lend(req, cell))
    }

    /// The reference `cell`, which the row's place holds from here, lent
    /// in place of the value it holds: the walk's owner of the value
    /// given up for one of the reference.
    fn lend(&mut self, req: &mut Request, cell: PhpRef) -> PhpRef {
        let owned = Owner::take(req, cell.as_value());
        let value = std::mem::replace(&mut self.value, owned);
        value.give_up(req);
        cell
    }

    /// The walk's owners of the row given up as the step ends.
    fn give_up(self, req: &mut Request) {
        self.value.give_up(req);
        self.key.give_up(req);
    }
}

/// php's walk by hash iterator over what the reference `holder` holds,
/// `php_array_walk`'s loop (`Z/ext/standard/array.c:1438`): an array,
/// separated first as php's parser separates a by-reference array
/// argument, or an object's property table, from its first row. One hash
/// iterator registered at the table (`zend_hash_iterator_add`) carries
/// the walk: it is moved past each row before the body runs, as
/// `foreach` moves it, and the holder is read again after every step, so
/// a row the body unset ahead is not visited, one it appended is, and
/// one it unset and wrote again is visited again where it now stands.
/// Another array in the holder is walked on from where the iterator
/// stands in it, a copy of the walked one at the copy's position and any
/// other at its own pointer, separated first where it is shared
/// (`zend_hash_iterator_pos_ex`); another object from its table's
/// pointer; anything else is php's `TypeError`, "Iterated value is no
/// longer an array or object". The table is not held: the holder holds
/// it, as php's walk holds none, and the iterator is given back on
/// every way out (`zend_hash_iterator_del`).
///
/// The answer is what `body` broke with, `None` where the walk went past
/// the last row; a throwable a step left pending ends the walk too, as
/// php's `while (!EG(exception))` does. What holds no array or object at
/// the start is the `InternalError` it is.
///
/// ```php
/// $a = ['a' => 1, 'b' => 2, 'c' => 3];
/// array_walk($a, function ($v, $k) use (&$a) { echo $k; if ($k === 'a') { unset($a['b']); $a['d'] = 4; } });
/// $o = new stdClass; $o->a = 1; $o->b = 2;
/// array_walk($o, function ($v, $k) { global $o; echo $k; if ($k === 'a') { $o = [9 => 'x']; } });
/// try { array_walk($o, function () { global $o; $o = 1; }); } catch (TypeError $e) { echo ' ', $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// acda9 Iterated value is no longer an array or object
/// ```
pub fn live<B>(
    req: &mut Request,
    holder: PhpRef,
    mut body: impl FnMut(&mut Request, &mut Row) -> Step<B>,
) -> Result<Option<B>, Thrown> {
    let Some(at) = registered(req, holder)? else {
        return Ok(None);
    };
    let walked = steps(req, holder, at, &mut body);
    req.heap_mut().iterator_del(at);
    walked
}

/// The hash iterator a live walk over `holder` stands on, registered at
/// its first row: `None` for a table with no row, which php walks not at
/// all.
fn registered(req: &mut Request, holder: PhpRef) -> Result<Option<u32>, Thrown> {
    let now = holder.get();
    if let Some(object) = now.as_object().copied() {
        let (_, table) = req.pointer_table(object, false)?;
        if table.is_empty() {
            return Ok(None);
        }
        return Ok(Some(req.heap_mut().iterator_add(table.table(), 0)));
    }
    if now.as_array().is_none() {
        return Err(fatal(
            req,
            "InternalError: a live walk's reference holds no array or object",
        ));
    }
    let Some(table) = holder.separate_array(req)? else {
        return Err(fatal(req, "InternalError: a live walk's array is gone"));
    };
    let Some((first, _, _)) = table.live_from(0) else {
        return Ok(None);
    };
    Ok(Some(req.heap_mut().iterator_add(table.heap(), first)))
}

/// [`live`]'s loop over iterator `at`, until a step finds no row, the
/// body breaks or throws, or a step leaves a throwable pending. A
/// throwable does not end the step at once: `zend_call_function`
/// answers `SUCCESS` with `EG(exception)` set, and php's loop reads the
/// holder again before it tests `EG(exception)`, so a holder left holding
/// no array or object raises the `TypeError` with the throwable as its
/// `previous`.
fn steps<B>(
    req: &mut Request,
    holder: PhpRef,
    at: u32,
    body: &mut impl FnMut(&mut Request, &mut Row) -> Step<B>,
) -> Result<Option<B>, Thrown> {
    loop {
        let Some(mut row) = next_row(req, holder, at)? else {
            return Ok(None);
        };
        let step = body(req, &mut row);
        row.give_up(req);
        let thrown = match step {
            Ok(ControlFlow::Break(broke)) => return Ok(Some(broke)),
            Ok(ControlFlow::Continue(())) if !req.is_pending() => continue,
            Ok(ControlFlow::Continue(())) => match req.take_pending() {
                Some(throwable) => Thrown::of(Some(throwable)),
                None => return Err(Thrown::of(None)),
            },
            Err(thrown) => thrown,
        };
        let now = holder.get();
        if thrown.exception().is_some() && now.as_array().is_none() && now.as_object().is_none() {
            return Err(throw_chained(req, thrown, "TypeError", NO_TABLE));
        }
        return Err(thrown);
    }
}

/// php's `TypeError` once a live walk's holder holds no array or object.
const NO_TABLE: &str = "Iterated value is no longer an array or object";

/// The row iterator `at` stands at in what `holder` holds now, the
/// iterator moved past it; `None` past the last row.
fn next_row(req: &mut Request, holder: PhpRef, at: u32) -> Result<Option<Row>, Thrown> {
    let now = holder.get();
    if let Some(array) = now.as_array().copied() {
        let moved = req.heap_mut().iterator_pos_ex(at, array.heap());
        let (pos, table) = req.fatal_of(moved)?;
        if !table.is_same(array.heap()) {
            // `ZVAL_ARR(array, zend_array_dup(ht))`: the reference holds
            // the copy from here, the shared table one owner fewer.
            holder.heap().val_ref().write(php_heap::Zval::array(table));
        }
        let Some((found, key, value)) = table.live_from(pos) else {
            return Ok(None);
        };
        // `zend_hash_move_forward_ex`: the next live bucket, or the end.
        let next = found.saturating_add(1);
        let next = table.live_from(next).map_or(next, |(next, _, _)| next);
        req.heap_mut().iterator_set_pos(at, next);
        return Ok(Some(Row {
            key: Owner::take(req, Key::of(key)),
            value: Owner::take(req, Value::of_zval(value)),
            at: At::Bucket(table, found),
            iterator: at,
        }));
    }
    if let Some(held) = now.as_object().copied() {
        let (owner, table) = req.pointer_table(held, false)?;
        return object_row(req, holder, at, held, owner, table);
    }
    Err(throw_error(req, "TypeError", NO_TABLE))
}

/// [`next_row`] over an object's property table in php's positions: the
/// declared slots, then the dynamic table, the iterator's place among
/// the slots its lead and its place in the table its position. A slot
/// nothing holds is stepped over, as php steps over an `IS_UNDEF` behind
/// an `IS_INDIRECT`; a declared row's key is its mangled name.
fn object_row(
    req: &mut Request,
    holder: PhpRef,
    iterator: u32,
    held: PhpObject,
    owner: PhpObject,
    table: PropertyTable,
) -> Result<Option<Row>, Thrown> {
    let declared = table.declared();
    let pos = req.heap_mut().iterator_pos(iterator, table.table());
    let lead = req.heap_mut().iterator_lead(iterator);
    let mut found = if lead < declared {
        lead
    } else {
        declared + pos
    };
    let (row, value) = loop {
        if found >= table.used() {
            return Ok(None);
        }
        match table.row_at(found) {
            Some((row, value)) if !value.is_undef() => break (row, value),
            _ => found = found.saturating_add(1),
        }
    };
    // `zend_hash_move_forward_ex`: the next live position, or the end.
    let mut next = found.saturating_add(1);
    while next < table.used() && table.row_at(next).is_none() {
        next = next.saturating_add(1);
    }
    let heap = req.heap_mut();
    if next < declared {
        heap.iterator_set_lead(iterator, next);
    } else {
        heap.iterator_set_lead(iterator, declared);
        heap.iterator_set_pos(iterator, next - declared);
    }
    let (key, at) = match row {
        TableRow::Slot(slot) => {
            let Some(name) = req.slot_name(owner.class(), slot)? else {
                return Err(fatal(
                    req,
                    "InternalError: a declared slot of an object's table with no name",
                ));
            };
            let at = At::Slot {
                holder,
                held,
                owner,
                table: table.table(),
                slot,
            };
            (Key::Str(PhpString::of(name)), at)
        }
        TableRow::Entry(key) => (Key::of(key), At::Bucket(table.table(), found - declared)),
    };
    Ok(Some(Row {
        key: Owner::take(req, key),
        value: Owner::take(req, Value::of_zval(value)),
        at,
        iterator,
    }))
}

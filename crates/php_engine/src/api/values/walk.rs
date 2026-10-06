//! The walks over a table (#1980 candidate 3), each porting one of php's
//! iteration modes once. A native walks a table in one of four ways:
//!
//! - [`rows`], the rows walk, an iterator that borrows the request, so a
//!   body that could reach code of the program does not compile in it:
//!   php's walk that counts on the table's own holder to keep it, held
//!   at compile time.
//! - [`foreach`], the foreach walk: the rows as they stand, up to where
//!   they ended as the walk began. It holds the table where php's native
//!   holds it ([`Hold`]) and guards its recursion where php's native
//!   guards it ([`foreach_guarded`]), and where it holds nothing it ends
//!   at the table's destruction. [`foreach_read`] reads each row under
//!   the borrow first and lends only the rows whose answer can reach
//!   code of the program, `in_array`'s comparisons. It walks an array or
//!   an object's property table laid down for it ([`ObjectTable`]), the
//!   declared slots and then the dynamic rows. `extract`, `serialize`, `print_r`,
//!   `var_dump`. [`properties`] is the same walk over the table
//!   `zend_get_properties_for` hands a reader ([`HeldProperties`]).
//! - [`live`], the Live walk, follows php's hash iterator over what a
//!   reference holds, the iterator `foreach` by reference stands on: the
//!   holder read again after each step, a row unset ahead skipped and
//!   one appended visited, and php's `TypeError` once the holder holds
//!   no array or object. `array_walk`.
//! - [`detached`], the Detached walk, works on a private copy of the
//!   table and swaps it back, as `php_usort` does, so code the body runs
//!   neither sees the table changing nor changes the one being worked
//!   on. The `usort` family.
//!
//! The last three are the callback walks, for a native that runs code of
//! the program for each row: a callback, a magic method or a handler a
//! diagnostic reaches. [`foreach`] and [`live`] keep each step's key and
//! value alive for the step and lend them to the body as a [`Row`], so a
//! body whose code unsets the row still reads it: owned where php's
//! native takes an owner of the row ([`live`], whose call copies its
//! arguments), lent as views where what the walk holds keeps them, and
//! otherwise pinned, an uncounted guard under which a free the row's
//! last owner would make waits for the step's end, so every count the
//! program reads is php's. The body answers a [`Step`]: std's
//! `ControlFlow` to go on or stop early with a value, and `Err` for a
//! throwable, which `?` hands on. [`detached`] lends the body the whole
//! copy instead, since what walks its rows is the sort the body runs,
//! whose comparisons are lent the rows; it answers what the body did.
//! The walk owns the loop and calls the body, so what it took
//! is given back on every way out, `?` included, by the walk itself and
//! never by a `Drop` (design §5.1: nothing runs implicitly on drop). A
//! body keeps a row past its step only by taking an owner of its own
//! ([`crate::api::values::Owner::take`]).

use std::marker::PhantomData;
use std::ops::ControlFlow;

use php_heap::{Arr, PropertyTable, Str, TableRow, Zval};

use crate::api::abi::Ref;
use crate::api::raising::{Thrown, fatal, throw_chained, throw_error};
use crate::api::request::Request;
use crate::api::values::{
    ArrayIter, Held, HeldProperties, HeldRow, Kept, Key, Owner, PhpArray, PhpObject, PhpRef,
    PhpString, Value,
};

/// What a walk's body answers for one row: `Continue` to go on to the
/// next, `Break` to end the walk there with what the walk answers, and
/// `Err`, what `?` hands on, a throwable that ends the walk as php's
/// loops end on `EG(exception)`.
pub type Step<B = ()> = Result<ControlFlow<B>, Thrown>;

/// php's `ZEND_HASH_FOREACH_KEY_VAL` over a table nothing can change
/// while it is walked: the rows of `table` in order, each a view of its
/// key and of its value as the table holds it, a reference row as the
/// reference ([`Value::deref`] follows it). The walk holds no owner, as
/// php's holds none, because the body cannot reach code of the program:
/// the walk borrows the request until it ends, and everything that can
/// reach that code, a conversion that calls `__toString`, a diagnostic
/// a handler reads, a release that runs a destructor, takes the request
/// mutably. So the views the walk lends stand while it runs. A body
/// that needs the request mutably is a callback walk's, [`foreach`] for
/// php's `ZEND_HASH_FOREACH`; one that reads every row and then acts on
/// what it read ends the walk first, a `find` or a `collect` of what it
/// read, and acts after.
///
/// The compiler holds a body to that, `E0502`: here the conversion that
/// can call `__toString` takes the request the walk borrows.
///
/// ```compile_fail,E0502
/// # use php_engine::api::coerce;
/// # use php_engine::api::raising::Thrown;
/// # use php_engine::api::request::Request;
/// # use php_engine::api::values::{PhpArray, walk};
/// fn strings(req: &mut Request, table: PhpArray) -> Result<(), Thrown> {
///     for (_, value) in walk::rows(req, table) {
///         coerce::to_string(req, &value)?;
///     }
///     Ok(())
/// }
/// # fn main() {}
/// ```
///
/// What the compiler cannot hold is a view copied out of the walk and
/// kept past it, the same view a `collect` of the rows hands on: a view
/// carries no owner (design §5.1), so once the walk ends nothing keeps
/// the row it names, and code of the program can give it up. A debug
/// build catches two uses of it while the request is served: the
/// freed-use guard refuses and reports an owner taken or given up on
/// the freed block, and the quarantine holds the freed slot back from
/// reuse and reports a write into it as the slot leaves. A read through
/// it is caught by neither; the quarantine only keeps the slot from
/// being another value's for its bin's next sixteen frees.
///
/// `array_key_last` and `array_last` read the last row on it, a
/// reference row followed:
///
/// ```php
/// $a = ['x' => 1, 'y' => 2];
/// $r = &$a['y'];
/// var_dump(array_key_last($a), array_last($a));
/// ```
/// ```expect
/// string(1) "y"
/// int(2)
/// ```
pub fn rows(_request: &Request, table: PhpArray) -> Rows<'_> {
    Rows {
        rows: table.iter(),
        request: PhantomData,
    }
}

/// The [`rows`] walk: an iterator over a table's rows that borrows the
/// request while it lives.
#[derive(Debug)]
#[must_use = "a rows walk reads nothing until it is iterated"]
pub struct Rows<'r> {
    rows: ArrayIter,
    /// The request borrowed for the walk, which is what keeps a body
    /// from running code of the program.
    request: PhantomData<&'r Request>,
}

impl Iterator for Rows<'_> {
    type Item = (Key, Value);

    fn next(&mut self) -> Option<(Key, Value)> {
        self.rows.next()
    }
}

/// Whether a [`foreach`] walk holds the table it walks: php's native's
/// own choice, named at each call site with the line of php's that makes
/// it, never read off how the caller came by the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// php's native takes an owner of the table before it walks it,
    /// `ZVAL_COPY` into a local or `GC_ADDREF`: the walk holds one owner
    /// until it ends, so code the body runs that writes the table
    /// separates it and the walk reads the rows it began with, and
    /// nothing the program does frees the table under the walk.
    Owner,
    /// php's native takes none and counts on what holds the table: the
    /// walk reads the rows as they stand, so a row the body's code
    /// rewrites in place is read as it now is, and a table the program
    /// gives up mid-walk is destroyed where php destroys it.
    None,
}

/// How a walk that guards its recursion ended ([`foreach_guarded`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "a walk inside the table already walked nothing, which the caller shows"]
pub enum Guarded<B> {
    /// The rows were walked: `Some` what the body broke with, `None`
    /// past the last row or where the table was destroyed under a walk
    /// holding nothing.
    Walked(Option<B>),
    /// A guarded walk is inside the table already, `GC_IS_RECURSIVE`,
    /// and nothing was walked: what a dumper prints `*RECURSION*` for.
    Recursive,
}

/// php's `ZEND_HASH_FOREACH` over `table` from a native whose body runs
/// code of the program: the rows in order, each lent to `body` as a
/// [`Row`] kept alive for its step, a reference row as the reference. The rows are read as they stand when the walk reaches
/// them, up to where they ended as the walk began, php's `_count` fixed
/// at the start: a row the body's code unsets ahead is not visited, one
/// it rewrites in place is read as it now is, and one it appends past
/// the end is not visited. Nothing reads the table's holder again, and
/// nothing tests `EG(exception)`: a throwable the body's code leaves
/// pending does not end the walk, and the body ends it by breaking or by
/// answering `Err`. The answer is what `body` broke with, `None` where
/// the walk went past the last row.
///
/// `hold` is php's native's choice ([`Hold`]). Under [`Hold::Owner`] the
/// walk holds one owner of the table until it ends, given up as php's
/// native gives it up ([`GiveUp`]), and lends each row as a view, which the
/// table it holds keeps. Under [`Hold::None`] it holds none, as php's
/// holds none, and pins each row it lends: where code the body runs
/// gives up the row's last owner, its free, and an object's destructor
/// with it, waits for the step's end, and where that code or the body
/// takes an owner of it again, nothing is freed. A pin is no count, so
/// a count the program reads during the step, and a separation a write
/// makes, are php's. Where the program gives the table up mid-walk, the
/// walk registers a hash iterator at the table, which the table's
/// destruction poisons, and ends at the first step that finds it
/// poisoned. That costs a walk one slot of the request's iterator list,
/// taken and given back, and a table a walk names has every write that
/// moves its rows test the list; each step costs one test of the slot,
/// an index and a compare. An immutable table registers none, since
/// nothing destroys one or writes it in place. A walk that ends this
/// way is a deliberate difference from php, which a native's regression
/// test states and a divergence row records once a compared site
/// reaches it.
///
/// A row appended under the walk is past the end it fixed as it began,
/// as php's is:
///
/// ```php
/// class C { function __serialize(): array { $GLOBALS['r'][] = 'added'; return []; } }
/// $r = [new C, 1];
/// $outer = [&$r];
/// echo serialize($outer), "\n", count($r), "\n";
/// ```
/// ```expect
/// a:1:{i:0;a:2:{i:0;O:1:"C":0:{}i:1;i:1;}}
/// 3
/// ```
///
/// A row whose last owner the program gives up during its own step is
/// freed as the step ends, a table's rows and an object's destructor
/// with it. Where php reads nothing of the row after that owner goes,
/// nothing tells the two moments apart: here `serialize`'s nested table,
/// given up by the last row's `__serialize`, is destroyed as its step
/// ends, and php destroys it at the `unset`, both before the next row:
///
/// ```php
/// class D { function __destruct() { echo "D gone\n"; } }
/// class C { function __serialize(): array { unset($GLOBALS['r'][0]); return []; } }
/// $r = [[new D, new C], 1];
/// $o = [&$r];
/// echo serialize($o), "\n";
/// ```
/// ```expect
/// D gone
/// a:1:{i:0;a:2:{i:0;a:2:{i:0;O:1:"D":0:{}i:1;O:1:"C":0:{}}i:1;i:1;}}
/// ```
///
/// The walk takes no owner of a row php's walk does not take, so it
/// leaves no possible root of the cycle collector (design §5.12) php's
/// does not leave: a walk over rows of arrays and objects leaves the
/// collector's buffer as php's leaves it:
///
/// ```php
/// $a = [[1], [2], new stdClass];
/// gc_collect_cycles();
/// $roots = gc_status()['roots'];
/// ob_start(); print_r($a); ob_end_clean();
/// echo gc_status()['roots'] - $roots, "\n";
/// ```
/// ```expect
/// 0
/// ```
///
/// An object's property table ([`ObjectTable`]) is walked in php's
/// positions, the declared slots and then the dynamic table, a slot
/// nothing holds stepped over as `ZEND_HASH_FOREACH_KEY_VAL_IND` steps
/// over an `IS_INDIRECT` to `IS_UNDEF`, and each declared row keyed by
/// its mangled name. The hold, the guard and the end at the table's
/// destruction stand on the dynamic table, `zobj->properties`, which is
/// what php's `GC_TRY_ADDREF` holds; a declared row is read in the slot
/// as it stands when the walk reaches it, as php's `IS_INDIRECT` is.
pub fn foreach<B>(
    req: &mut Request,
    table: impl Into<Table>,
    hold: Hold,
    mut body: impl FnMut(&mut Request, &mut Row) -> Step<B>,
) -> Result<Option<B>, Thrown> {
    let table = table.into();
    table.pin();
    let standing = Standing::taken(req, table.heap(), hold);
    let walked = foreach_steps(req, &table, standing.watch(), &mut body);
    standing.given_up(req);
    table.unpin(req);
    walked
}

/// [`foreach`] guarding its recursion, as php's dumpers guard a table
/// they descend into: where a guarded walk is inside the table already
/// (`GC_IS_RECURSIVE`) nothing is taken or walked and the answer says
/// so; otherwise the table is held as `hold` says, marked for the walk
/// (`GC_PROTECT_RECURSION`), and unmarked as it ends, on every way out,
/// before the walk's owner is given up. A table destroyed under a walk
/// holding nothing is not unmarked, since it is gone, where php's
/// `GC_TRY_UNPROTECT_RECURSION` writes the freed table. An immutable
/// table is marked by neither, as php's is not, since nothing can make
/// one hold itself. Over an object's property table the mark is on its
/// dynamic table, `zobj->properties`, as `php_json_encode_array` marks
/// the table `zend_get_properties_for` hands it; a reader that guards
/// the object instead, as `print_r` and `var_dump` do, guards it around
/// a [`foreach`] walk ([`PhpObject::protect_in`]). A write that
/// separates the table leaves the mark on the table the walk holds.
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
pub fn foreach_guarded<B>(
    req: &mut Request,
    table: impl Into<Table>,
    hold: Hold,
    mut body: impl FnMut(&mut Request, &mut Row) -> Step<B>,
) -> Result<Guarded<B>, Thrown> {
    let table = table.into();
    let guarded = table.heap();
    if guarded.is_recursive() {
        return Ok(Guarded::Recursive);
    }
    table.pin();
    let standing = Standing::taken(req, guarded, hold);
    guarded.protect();
    let walked = foreach_steps(req, &table, standing.watch(), &mut body);
    if standing.watch().stands(req, guarded.heap()) {
        guarded.unprotect();
    }
    standing.given_up(req);
    table.unpin(req);
    walked.map(Guarded::Walked)
}

/// php's walk over the table `zend_get_properties_for` handed a reader
/// ([`PhpObject::props_held`]), as `print_hash` walks it with
/// `ZEND_HASH_FOREACH_KEY_VAL_IND`: a [`foreach`] walk, each row lent to
/// `body` as a [`Row`], over what the properties hold, which is what
/// php's reader holds. The object's own table is held by the owner
/// `zend_std_get_properties_for`'s `GC_TRY_ADDREF` takes, so a write
/// the body's code makes to a dynamic property separates the table and
/// the walk reads the rows it began with, while a declared row is read
/// in its slot as it stands and the object is pinned for the walk, as
/// [`foreach`] pins it; the class's debug table by its owner; and rows
/// the class answers by an owner of each, so a row the body's code
/// gives up is still read. A typed property nothing has written is no
/// row. The walk takes nothing beyond each step's row and gives the
/// properties back never: the reader clears its recursion guard and
/// then releases them ([`HeldProperties::release`]), as php's
/// `zend_release_properties` comes after its
/// `ZEND_GUARD_OR_GC_UNPROTECT_RECURSION`, so a destructor the release
/// serves finds the object unmarked. The answer is what `body` broke
/// with, `None` past the last row.
///
/// ```php
/// #[AllowDynamicProperties]
/// class O { public $a; public $b = 'b'; }
/// class N { function __debugInfo() { global $o; unset($o->b, $o->d); $o->e = 'e'; return []; } }
/// $o = new O; $o->a = new N; $o->d = str_repeat('d', 3);
/// print_r($o); echo "\n"; print_r($o);
/// ```
/// ```expect
/// O Object
/// (
///     [a] => N Object
///         (
///         )
///
///     [d] => ddd
/// )
///
/// O Object
/// (
///     [a] => N Object
///         (
///         )
///
///     [e] => e
/// )
/// ```
pub fn properties<B>(
    req: &mut Request,
    held: &HeldProperties,
    mut body: impl FnMut(&mut Request, &mut Row) -> Step<B>,
) -> Result<Option<B>, Thrown> {
    match &held.held {
        Held::Table(table, owner) => {
            let table = Table::Object(table.clone());
            table.pin();
            let walked = foreach_steps(req, &table, Watch::over(**owner), &mut body);
            table.unpin(req);
            walked
        }
        Held::Debug(table, _) => {
            let watch = Watch::over(**table);
            foreach_steps(req, &Table::Array(**table), watch, &mut body)
        }
        Held::Rows(rows) => {
            for (key, kept) in rows {
                let Kept::Value(value) = kept else {
                    continue;
                };
                let mut row = Row {
                    key: Owner::take(req, (**key).clone()),
                    value: Owner::take(req, **value),
                    at: At::Lent,
                    watch: Watch::Held,
                };
                let step = body(req, &mut row);
                row.give_up(req);
                if let ControlFlow::Break(broke) = step? {
                    return Ok(Some(broke));
                }
            }
            Ok(None)
        }
    }
}

/// What a [`foreach`] walk reads: an array, or an object's property
/// table laid down for the walk.
#[derive(Debug, Clone)]
pub enum Table {
    /// An array's rows.
    Array(PhpArray),
    /// An object's rows in php's positions, declared then dynamic.
    Object(ObjectTable),
}

impl From<PhpArray> for Table {
    fn from(array: PhpArray) -> Table {
        Table::Array(array)
    }
}

impl From<ObjectTable> for Table {
    fn from(table: ObjectTable) -> Table {
        Table::Object(table)
    }
}

impl Table {
    /// The table a hold, a guard and a watch stand on: the array, or the
    /// object's dynamic table, `zobj->properties`.
    fn heap(&self) -> PhpArray {
        match self {
            Table::Array(array) => *array,
            Table::Object(object) => object.dynamic(),
        }
    }

    /// An object's pinned for the walk ([`php_heap::Obj::pin`]), so
    /// that where code the body runs frees it, its slots are emptied
    /// and stepped over, never read freed: neither php's `IS_INDIRECT`
    /// nor the dynamic table the walk may hold keeps the object.
    fn pin(&self) {
        if let Table::Object(object) = self {
            object.owner.heap().pin();
        }
    }

    /// [`Table::pin`] let go, the object's bytes given back where it was
    /// freed under the walk.
    fn unpin(&self, req: &mut Request) {
        if let Table::Object(object) = self {
            req.heap_mut().unpin(object.owner.heap());
        }
    }

    /// The position past the last, `nNumUsed`, as it stands now.
    fn end(&self) -> u32 {
        match self {
            Table::Array(array) => array.heap().num_used(),
            Table::Object(object) => object.table.used(),
        }
    }

    /// The first row at or after `pos` below `end` that holds a value:
    /// its position, its key, its value and where it stands.
    fn held_from(&self, pos: u32, end: u32) -> Option<(u32, Key, Zval, At)> {
        match self {
            Table::Array(array) => {
                let table = array.heap();
                let (found, key, value) =
                    table.live_from(pos).filter(|(found, ..)| *found < end)?;
                Some((found, Key::of(key), value, At::Bucket(table, found)))
            }
            Table::Object(object) => object.held_from(pos, end),
        }
    }
}

/// An object's property table laid down for a walk, php's `Z_OBJPROP`:
/// the table `rebuild_object_properties` lays down, which the object
/// keeps from then on, read in php's positions, one `IS_INDIRECT` per
/// declared slot and then the dynamic rows, with the key each declared
/// slot stands under, `zend_mangle_property_name`'s `\0*\0p` for a
/// protected property and `\0C\0p` for a private one `C` declared,
/// interned as php's are. Laying it down and naming its slots takes the
/// request mutably, a lazy object's initializer and a class's
/// `get_properties` among what it runs
/// ([`PhpObject::property_table`]); the rows are then lent one at a
/// time as [`foreach`] lends them. A view: it holds no owner of the
/// object or the table.
#[derive(Debug, Clone)]
pub struct ObjectTable {
    /// The object whose table it is: a lazy object's instance once
    /// initialised.
    owner: PhpObject,
    table: PropertyTable,
    /// The key of each declared slot, in slot order; none where the
    /// table holds copies of the declared rows.
    names: Box<[Str]>,
}

impl ObjectTable {
    /// `owner`'s table `table`, its declared slots named by `names`,
    /// one per position [`PropertyTable::declared`] gives the slots.
    pub(crate) fn of(owner: PhpObject, table: PropertyTable, names: Box<[Str]>) -> ObjectTable {
        ObjectTable {
            owner,
            table,
            names,
        }
    }

    /// The object's dynamic table, `zobj->properties`, which a hold
    /// holds.
    pub(crate) fn dynamic(&self) -> PhpArray {
        PhpArray::of(self.table.table())
    }

    /// [`Table::held_from`] over the object's positions.
    fn held_from(&self, pos: u32, end: u32) -> Option<(u32, Key, Zval, At)> {
        let (found, row, value) = held_at(self.table, pos, end)?;
        let table = self.table.table();
        Some(match row {
            TableRow::Slot(slot) => {
                let at = At::Slot {
                    holder: None,
                    owner: self.owner,
                    table,
                    slot,
                };
                (found, self.slot_key(slot)?, value, at)
            }
            TableRow::Entry(key) => {
                let at = At::Bucket(table, found - self.table.declared());
                (found, Key::of(key), value, at)
            }
        })
    }

    /// The key declared slot `slot` stands under.
    fn slot_key(&self, slot: u32) -> Option<Key> {
        let name = self.names.get(usize::try_from(slot).ok()?)?;
        Some(Key::Str(PhpString::of(*name)))
    }

    /// The rows that hold a value, in php's positions: views, no owner
    /// taken.
    pub(crate) fn rows(&self) -> impl Iterator<Item = (Key, Value)> + '_ {
        let end = self.table.used();
        let mut pos = 0;
        std::iter::from_fn(move || {
            let (found, key, value, _) = self.held_from(pos, end)?;
            pos = found.saturating_add(1);
            Some((key, Value::of_zval(value)))
        })
    }

    /// How many rows hold a value.
    pub(crate) fn held(&self) -> usize {
        self.rows().count()
    }

    /// The rows as php's debug dumpers walk them, `ZEND_HASH_FOREACH`
    /// without `_IND`: a slot nothing holds shown as `uninitialized(T)`
    /// where its property is typed (`zend_get_typed_property_info_for_slot`)
    /// and stepped over where it is not. Views, no owner taken.
    pub(crate) fn shown(&self, req: &Request) -> Vec<(Key, HeldRow)> {
        let placed = req.placed_properties(self.owner.class());
        (0..self.table.used())
            .filter_map(|pos| {
                let (row, value) = self.table.row_at(pos)?;
                match row {
                    TableRow::Slot(slot) if value.is_undef() => {
                        let (_, property) = placed.get(usize::try_from(slot).ok()?)?;
                        let ty = property.ty.as_ref()?;
                        let shown = HeldRow::Uninitialized(crate::natives::type_text(ty));
                        Some((self.slot_key(slot)?, shown))
                    }
                    TableRow::Slot(slot) => {
                        Some((self.slot_key(slot)?, HeldRow::Value(Value::of_zval(value))))
                    }
                    TableRow::Entry(key) => {
                        Some((Key::of(key), HeldRow::Value(Value::of_zval(value))))
                    }
                }
            })
            .collect()
    }
}

/// The reader of an object's table in php's positions that the foreach,
/// rows and live walks step through: the first position at or after `pos`
/// below `end` that holds a value, a tombstone and a slot nothing holds
/// stepped over, as php's walks step over an `IS_UNDEF` behind an
/// `IS_INDIRECT`; what stands there and its value.
fn held_at(table: PropertyTable, mut pos: u32, end: u32) -> Option<(u32, TableRow, Zval)> {
    while pos < end {
        if let Some((row, value)) = table.row_at(pos)
            && !value.is_undef()
        {
            return Some((pos, row, value));
        }
        pos = pos.saturating_add(1);
    }
    None
}

/// What a [`foreach`] walk took so that the table it walks stands, or
/// so that it knows when the table no longer does, given back as the
/// walk ends.
enum Standing {
    /// [`Hold::Owner`]: the walk's owner of the table.
    Held(Owner<PhpArray>),
    /// [`Hold::None`] over a table the program can destroy: the walk's
    /// hash iterator registered at it.
    Watched(u32),
    /// [`Hold::None`] over an immutable table, which nothing destroys.
    Immutable,
}

impl Standing {
    /// What a walk over `table` takes as `hold` says.
    fn taken(req: &mut Request, table: PhpArray, hold: Hold) -> Standing {
        match hold {
            Hold::Owner => Standing::Held(Owner::take(req, table)),
            Hold::None if table.heap().is_immutable() => Standing::Immutable,
            Hold::None => Standing::Watched(req.heap_mut().iterator_add(table.heap(), 0)),
        }
    }

    /// How each row the walk lends knows its table still stands.
    fn watch(&self) -> Watch {
        match self {
            Standing::Held(owner) => Watch::over(**owner),
            Standing::Watched(at) => Watch::Iterator(*at),
            Standing::Immutable => Watch::Immutable,
        }
    }

    /// What the walk took given back: the owner given up, `zval_ptr_dtor`,
    /// or the iterator, `zend_hash_iterator_del`.
    fn given_up(self, req: &mut Request) {
        match self {
            Standing::Held(owner) => owner.give_up(req),
            Standing::Watched(at) => req.heap_mut().iterator_del(at),
            Standing::Immutable => {}
        }
    }
}

/// [`foreach`]'s loop over `table` below where its rows ended as the
/// walk began, while `watch` says the table stands.
fn foreach_steps<B>(
    req: &mut Request,
    table: &Table,
    watch: Watch,
    body: &mut impl FnMut(&mut Request, &mut Row) -> Step<B>,
) -> Result<Option<B>, Thrown> {
    let end = table.end();
    let watched = table.heap().heap();
    let mut pos = 0;
    while watch.stands(req, watched) {
        let Some((found, key, value, at)) = table.held_from(pos, end) else {
            break;
        };
        pos = found.saturating_add(1);
        let mut row = Row {
            key: Owner::take(req, key),
            value: Owner::take(req, Value::of_zval(value)),
            at,
            watch,
        };
        let step = body(req, &mut row);
        row.give_up(req);
        if let ControlFlow::Break(broke) = step? {
            return Ok(Some(broke));
        }
    }
    Ok(None)
}

/// One row a callback walk lends its body ([`foreach`], [`live`]): the
/// key and the value as the table held them when the walk stepped onto
/// them, each owned by the walk for the step, so a body whose code
/// unsets the row can still read it, and where the row stands, so the
/// body can make it a reference in place ([`Row::make_ref`]).
#[derive(Debug)]
pub struct Row {
    key: Owner<Key>,
    value: Owner<Value>,
    at: At,
    /// How the row knows the table it is in still stands.
    watch: Watch,
}

/// How a walk knows the table its rows are in still stands.
#[derive(Debug, Clone, Copy)]
enum Watch {
    /// The walk's hash iterator, registered at the table, which the
    /// table's destruction poisons.
    Iterator(u32),
    /// The walk holds an owner of the table.
    Held,
    /// The table is immutable: nothing destroys it, and nothing writes
    /// it in place.
    Immutable,
}

impl Watch {
    /// How a walk knows `table`, which it holds an owner of, stands:
    /// held, or immutable, which nothing writes in place.
    fn over(table: PhpArray) -> Watch {
        if table.heap().is_immutable() {
            Watch::Immutable
        } else {
            Watch::Held
        }
    }

    /// Whether `table` still stands: the iterator still names it, or the
    /// walk holds it, or it is immutable. The test reads the iterator
    /// alone, never `table`, so it is sound where `table` was freed.
    fn stands(self, req: &mut Request, table: Arr) -> bool {
        match self {
            Watch::Iterator(at) => req.heap_mut().iterator_on(at, table),
            Watch::Held | Watch::Immutable => true,
        }
    }

    /// Whether a row of `table` may be written in place: the walk's
    /// iterator still names it. A table the walk holds is written by no
    /// php native's walk, and a write there could land in the walk's own
    /// copy once the program's write separated it; an immutable one is
    /// written by nothing.
    fn writable(self, req: &mut Request, table: Arr) -> bool {
        match self {
            Watch::Iterator(_) => self.stands(req, table),
            Watch::Held | Watch::Immutable => false,
        }
    }
}

/// Where a [`Row`] stands.
#[derive(Debug, Clone, Copy)]
enum At {
    /// Bucket `pos` of an array, an object's dynamic table among them.
    Bucket(Arr, u32),
    /// Declared slot `slot` of `owner`, whose dynamic table is `table`.
    /// Under a live walk, `holder` is the reference and the object it
    /// held, whose instance `owner` is (a lazy object's is its own once
    /// initialised), which a write asks still holds it.
    Slot {
        holder: Option<(PhpRef, PhpObject)>,
        owner: PhpObject,
        table: Arr,
        slot: u32,
    },
    /// A row of rows a class built for its reader, which stands in no
    /// table a write could reach.
    Lent,
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

    /// The value's refcount as php's walk reads it, `Z_REFCOUNT_P` over
    /// the bucket: the owners besides the walk's own for the step, which
    /// php's walk does not take. One for what carries no count, as
    /// [`Value::refcount`] answers.
    #[must_use]
    pub fn refcount(&self) -> u32 {
        let count = self.value.refcount();
        if self.value.is_refcounted() {
            count.saturating_sub(1)
        } else {
            count
        }
    }

    /// The row made a reference in place, php's `ZVAL_MAKE_REF` over the
    /// bucket: the reference the table holds there already, or a new one
    /// holding the value, written into the table as it stands, shared or
    /// not, as php writes it, and into a typed property's slot with the
    /// property as its type source (`ZEND_REF_ADD_TYPE_SOURCE`). From
    /// here the walk owns the reference for the step. Asked before the
    /// body runs any code of the program, as php's walk makes it before
    /// its call; once that code has moved the row, the answer is the
    /// `InternalError` it is, never a write elsewhere. A row of a
    /// [`Hold::Owner`] walk or of an immutable table is never written,
    /// as no php native's walk writes one, and is that `InternalError`
    /// too.
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
        // The table is written while the walk's iterator still names it:
        // a table destroyed under the walk poisons it ([`Watch::writable`]).
        // An object's slot stands while the reference still holds the
        // object too.
        let watch = self.watch;
        let place = match self.at {
            At::Bucket(table, pos) if watch.writable(req, table) => table.value_ref_at(pos),
            At::Slot {
                holder,
                owner,
                table,
                slot,
            } if watch.writable(req, table)
                && holder.is_none_or(|(holder, held)| holder.get().as_object() == Some(&held)) =>
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
            At::Bucket(..) | At::Slot { .. } | At::Lent => None,
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
            Ok(ControlFlow::Continue(())) => match req.take_pending_view() {
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
            watch: Watch::Iterator(at),
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
    let from = if lead < declared {
        lead
    } else {
        declared + pos
    };
    let Some((found, row, value)) = held_at(table, from, table.used()) else {
        return Ok(None);
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
                holder: Some((holder, held)),
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
        watch: Watch::Iterator(iterator),
    }))
}

/// php's walk over a private copy swapped back, `php_usort`
/// (`Z/ext/standard/array.c:878`): the array the by-reference argument
/// `holder` held on entry copied (`zend_array_dup`, "so the in-place
/// modifications will not be visible to the callback function"), the
/// copy lent to `body`, which sorts or walks it, and the copy written
/// into the caller's variable after, whatever the body answered: written
/// first, as `ZVAL_ARR(array, arr)` writes it, with no type consulted,
/// and the array the variable held given up after (`zval_ptr_dtor` of
/// the garbage), so a destructor that serves reads the sorted copy. A
/// temporary passed where a reference was asked for keeps nothing. Code the body runs that writes the
/// variable writes the original, which the swap gives up: an append a
/// comparator makes is not in the sorted array. An array with no row is
/// not copied, and the body not called, as `php_usort` returns before;
/// the answer is then `None`.
///
/// ```php
/// $a = [3, 1, 2]; usort($a, function ($x, $y) { global $a; $a[] = 9; return $x <=> $y; }); echo json_encode($a), "\n";
/// $b = ['c' => 3, 'a' => 1]; uksort($b, function ($x, $y) { global $b; unset($b['a']); return $x <=> $y; }); echo json_encode($b), "\n";
/// ```
/// ```expect
/// [1,2,3]
/// {"a":1,"c":3}
/// ```
///
/// ```php
/// class D { function __destruct() { global $a; echo "dtor: ", json_encode($a), "\n"; } }
/// $a = [3, 1, 2]; $i = 0;
/// usort($a, function ($x, $y) use (&$i) { global $a; if ($i++ == 0) { $a[] = new D; } return $x <=> $y; });
/// $a = [3, 1, 2]; $i = 0;
/// usort($a, function ($x, $y) use (&$i) { global $a; if ($i++ == 0) { $a = new D; } return $x <=> $y; });
/// ```
/// ```expect
/// dtor: [1,2,3]
/// dtor: [1,2,3]
/// ```
pub fn detached<T>(
    req: &mut Request,
    holder: &Ref<PhpArray>,
    body: impl FnOnce(&mut Request, &mut PhpArray) -> Result<T, Thrown>,
) -> Result<Option<T>, Thrown> {
    let Some(held) = holder.get().copied() else {
        return Ok(None);
    };
    if held.is_empty() {
        return Ok(None);
    }
    let mut copy = held.copy(req)?.hand_on();
    let answered = body(req, &mut copy);
    match holder.reference() {
        Some(cell) => cell.convert(req, copy.to_value()),
        None => req.release(copy.to_value()),
    }
    answered.map(Some)
}

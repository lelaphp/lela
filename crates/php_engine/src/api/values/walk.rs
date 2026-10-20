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
use crate::api::raising::{Thrown, fatal, throw_error, throw_error_chained};
use crate::api::request::Request;
use crate::api::values::{
    HeldProperties, Key, Owner, PhpArray, PhpObject, PhpRef, PhpString, Value, View,
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
///
/// An object's property table is walked in two steps: laid down and its
/// slots named with the request mutably ([`PhpObject::property_table`]),
/// then its rows read under the borrow, in php's positions, the declared
/// slots that hold a value and then the dynamic rows, each declared row
/// under its mangled name. A body that needs the request mutably does
/// not compile there either:
///
/// ```compile_fail,E0502
/// # use php_engine::api::coerce;
/// # use php_engine::api::raising::Thrown;
/// # use php_engine::api::request::Request;
/// # use php_engine::api::values::{PhpObject, walk};
/// fn strings(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
///     let table = object.property_table(req)?;
///     for (_, value) in walk::rows(req, table) {
///         coerce::to_string(req, &value)?;
///     }
///     Ok(())
/// }
/// # fn main() {}
/// ```
pub fn rows(_request: &Request, table: impl Into<Table>) -> Rows<'_> {
    let rows = match table.into() {
        Table::Array(array) => Walked::Array(array.heap().iter()),
        Table::Object(table) => Walked::Positions {
            end: table.table.used(),
            table,
            pos: 0,
        },
    };
    Rows {
        rows,
        request: PhantomData,
    }
}

/// The [`rows`] walk: an iterator over a table's rows that borrows the
/// request while it lives.
#[derive(Debug)]
#[must_use = "a rows walk reads nothing until it is iterated"]
pub struct Rows<'r> {
    rows: Walked,
    /// The request borrowed for the walk, which is what keeps a body
    /// from running code of the program.
    request: PhantomData<&'r Request>,
}

/// Where a [`Rows`] walk stands.
#[derive(Debug)]
enum Walked {
    Array(php_heap::ArrIter),
    /// The next position of an object's table to read, and the end.
    Positions {
        table: ObjectTable,
        pos: u32,
        end: u32,
    },
}

impl Iterator for Rows<'_> {
    type Item = (Key, Value);

    fn next(&mut self) -> Option<(Key, Value)> {
        match &mut self.rows {
            Walked::Array(rows) => rows.next().map(super::view),
            Walked::Positions { table, pos, end } => {
                let (found, key, value, _) = table.held_from(*pos, *end)?;
                *pos = found.saturating_add(1);
                Some((key, Value::of_slot(value)))
            }
        }
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
    /// nothing the program does frees the table under the walk. The
    /// owner is given up as the native gives it up ([`GiveUp`]), named
    /// beside the hold.
    Owner(GiveUp),
    /// php's native takes none and counts on what holds the table: the
    /// walk reads the rows as they stand, so a row the body's code
    /// rewrites in place is read as it now is, and a table the program
    /// gives up mid-walk is destroyed where php destroys it.
    None,
}

/// How php's native gives up the owner of the table it held for its walk
/// ([`Hold::Owner`]), which decides whether the table, where owners are
/// left, is a possible root of the cycle collector (design §5.12): the
/// count of roots `gc_status()` reads after the call is php's.
///
/// `var_export` gives its `GC_ADDREF` up with `GC_DELREF`
/// (`Z/ext/standard/var.c:587`) and leaves no root, where `var_dump`'s
/// `GC_DTOR_NO_REF` (`Z/ext/standard/var.c:162`) leaves one per array it
/// walked that owners are left of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GiveUp {
    /// `zval_ptr_dtor`, `GC_DTOR` or `GC_DTOR_NO_REF`: the table
    /// destroyed at its last owner, and where owners are left a possible
    /// root ([`Owner::give_up`]).
    Dtor,
    /// `zval_ptr_dtor_nogc`, `GC_DELREF` or `zend_array_release`: the
    /// table destroyed at its last owner, and no possible root
    /// ([`Owner::give_up_nogc`]). `GC_DELREF` alone destroys nothing at
    /// zero; a native that gives up with it holds an owner it never
    /// gives up last, so the two read alike.
    DtorNogc,
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

/// What a walk over an object's properties ([`properties`]) does at a
/// typed property nothing has written, php's `IS_INDIRECT` to an
/// `IS_UNDEF` slot: the reader's choice of php's macro.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unwritten {
    /// Stepped over, as `ZEND_HASH_FOREACH_KEY_VAL_IND` steps over it:
    /// `print_r`'s `print_hash`.
    Skipped,
    /// Lent as a row whose [`Row::uninitialized`] names the property's
    /// type and whose value is undef, as the dumpers'
    /// `ZEND_HASH_FOREACH_KEY_VAL` finds it and
    /// `zend_get_typed_property_info_for_slot` types it: `var_dump` and
    /// `debug_zval_dump`, which print `uninitialized(T)`. An untyped
    /// slot nothing holds is stepped over still, as php's dumpers step
    /// over one.
    Shown,
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
    let standing = Standing::taken(req, &table, hold);
    let walked = foreach_steps(req, &table, standing.watch(), Unwritten::Skipped, &mut body);
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
    let guarded = table.array();
    if guarded.is_recursive() {
        return Ok(Guarded::Recursive);
    }
    table.pin();
    let standing = Standing::taken(req, &table, hold);
    guarded.protect();
    let walked = foreach_steps(req, &table, standing.watch(), Unwritten::Skipped, &mut body);
    if standing.watch().stands(req, guarded.heap()) {
        guarded.unprotect();
    }
    standing.given_up(req);
    table.unpin(req);
    walked.map(Guarded::Walked)
}

/// php's walk over the table `zend_get_properties_for` handed a reader
/// ([`PhpObject::properties_for`]), as `print_hash` walks it with
/// `ZEND_HASH_FOREACH_KEY_VAL_IND`: a [`foreach`] walk, each row lent to
/// `body` as a [`Row`], over the table the properties hold, held by the
/// owner php's reader holds: the object's own table by the owner
/// `zend_std_get_properties_for`'s `GC_TRY_ADDREF` takes, so a write
/// the body's code makes to a dynamic property separates the table and
/// the walk reads the rows it began with, while a declared row is read
/// in its slot as it stands, pinned for its step, and the object is
/// pinned for the walk, as [`foreach`] pins them; a table the class
/// keeps by the owner its `GC_ADDREF` takes; and a table built for the
/// reader, php's `*is_temp`, by its one owner. A row is lent as a view,
/// which what the properties hold keeps, as php's walk takes no owner
/// of it. A typed property nothing has written is no row, or under
/// [`Unwritten::Shown`] a row whose [`Row::uninitialized`] names its
/// type, as `unwritten` says. The walk gives the properties back never:
/// the reader releases them ([`HeldProperties::release`]) where php's
/// reader calls `zend_release_properties`, which is its own order
/// beside its recursion guard: the json encoder clears the guard first,
/// so a destructor the release serves finds the object unmarked, and
/// `php_var_dump` and `php_debug_zval_dump` release before the closing
/// brace and clear it after. The answer
/// is what `body` broke with, `None` past the last row.
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
    unwritten: Unwritten,
    mut body: impl FnMut(&mut Request, &mut Row) -> Step<B>,
) -> Result<Option<B>, Thrown> {
    let table = held.table.clone();
    table.pin();
    let watch = Watch::over(*held.owner);
    let walked = foreach_steps(req, &table, watch, unwritten, &mut body);
    table.unpin(req);
    walked
}

/// What a walk reads, php's `HashTable *`: an array, or an object's
/// property table laid down for the walk. [`PhpObject::property_table`]
/// answers either, as php's `Z_OBJPROP` answers the class's
/// `get_properties`: the object's own table, or a table the class keeps
/// in its place (`sxe_get_properties`'s `sxe->properties`).
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
    /// The table a hold, a guard and a watch stand on, php's
    /// `HashTable *` itself: the array, or the object's dynamic table,
    /// `zobj->properties`, which `GC_PROTECT_RECURSION` marks.
    #[must_use]
    pub fn array(&self) -> PhpArray {
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
/// ([`PhpObject::property_table`]); the rows are then read under the
/// request as [`rows`] borrows it, or lent one at a time as [`foreach`]
/// lends them. A view: it holds no owner of the object or the table.
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

    /// The object whose table it is.
    pub(crate) fn owner(&self) -> PhpObject {
        self.owner
    }

    /// The table in php's positions.
    pub(crate) fn positions(&self) -> PropertyTable {
        self.table
    }

    /// How many rows hold a value.
    pub(crate) fn held(&self) -> usize {
        let end = self.table.used();
        std::iter::successors(self.held_from(0, end), |(found, ..)| {
            self.held_from(found.saturating_add(1), end)
        })
        .count()
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

    /// [`ObjectTable::held_from`] as php's dumpers read the positions
    /// ([`Unwritten::Shown`]): a declared slot nothing holds is the row
    /// too where its property is typed, with its type's text, and is
    /// stepped over where it is not.
    fn shown_from(
        &self,
        req: &Request,
        mut pos: u32,
        end: u32,
    ) -> Option<(u32, Key, Zval, At, Option<String>)> {
        while pos < end {
            match self.table.row_at(pos) {
                Some((TableRow::Slot(slot), value)) if value.is_undef() => {
                    let placed =
                        req.placed_property(self.owner.class(), usize::try_from(slot).ok()?);
                    if let Some((_, property)) = placed
                        && let Some(ty) = property.ty.as_ref()
                    {
                        let at = At::Slot {
                            holder: None,
                            owner: self.owner,
                            table: self.table.table(),
                            slot,
                        };
                        let shown = Some(php_unit::type_to_string(ty));
                        return Some((pos, self.slot_key(slot)?, value, at, shown));
                    }
                }
                Some(_) => {
                    if let Some((found, key, value, at)) = self.held_from(pos, pos + 1) {
                        return Some((found, key, value, at, None));
                    }
                }
                None => {}
            }
            pos = pos.saturating_add(1);
        }
        None
    }

    /// The key declared slot `slot` stands under.
    fn slot_key(&self, slot: u32) -> Option<Key> {
        let name = self.names.get(usize::try_from(slot).ok()?)?;
        Some(Key::Str(PhpString::of(*name)))
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
    /// [`Hold::Owner`]: the walk's owner of the table, and how it is
    /// given up.
    Held(Owner<PhpArray>, GiveUp),
    /// [`Hold::None`] over a table the program can destroy: the walk's
    /// hash iterator registered at it.
    Watched(u32),
    /// [`Hold::None`] over an immutable table, which nothing destroys.
    Immutable,
}

impl Standing {
    /// What a walk over `table` takes as `hold` says.
    fn taken(req: &mut Request, table: &Table, hold: Hold) -> Standing {
        let table = table.array();
        match hold {
            Hold::Owner(give_up) => Standing::Held(Owner::take(req, table), give_up),
            Hold::None if table.heap().is_immutable() => Standing::Immutable,
            Hold::None => Standing::Watched(req.heap_mut().iterator_add(table.heap(), 0)),
        }
    }

    /// How each row the walk lends knows its table still stands.
    fn watch(&self) -> Watch {
        match self {
            Standing::Held(owner, _) => Watch::over(**owner),
            Standing::Watched(at) => Watch::Iterator(*at),
            Standing::Immutable => Watch::Immutable,
        }
    }

    /// What the walk took given back: the owner given up as the native
    /// gives it up ([`GiveUp`]), or the iterator, `zend_hash_iterator_del`.
    fn given_up(self, req: &mut Request) {
        match self {
            Standing::Held(owner, GiveUp::Dtor) => owner.give_up(req),
            Standing::Held(owner, GiveUp::DtorNogc) => owner.give_up_nogc(req),
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
    unwritten: Unwritten,
    body: &mut impl FnMut(&mut Request, &mut Row) -> Step<B>,
) -> Result<Option<B>, Thrown> {
    let end = table.end();
    let watched = table.array().heap();
    let mut pos = 0;
    while watch.stands(req, watched) {
        // The row is laid out by a call of its own, so what finding it
        // takes is off the stack while the body runs: a walk the body
        // re-enters, a nested array compared or dumped, costs a level
        // only what this loop keeps.
        let mut lent = next_lent(req, table, (&mut pos, end), watch, unwritten);
        let Some(row) = lent.as_mut() else {
            break;
        };
        let step = body(req, row);
        row.let_go(req);
        if let ControlFlow::Break(broke) = step? {
            return Ok(Some(broke));
        }
    }
    Ok(None)
}

/// The row a [`foreach`] walk lends next, at or after `pos` below `end`,
/// its key and value kept for the step as [`Lent::of`] says, and `pos`
/// left one past it; `None` past the last.
#[inline(never)]
fn next_lent(
    req: &mut Request,
    table: &Table,
    (pos, end): (&mut u32, u32),
    watch: Watch,
    unwritten: Unwritten,
) -> Option<Row> {
    let found = match (table, unwritten) {
        (Table::Object(object), Unwritten::Shown) => object.shown_from(req, *pos, end),
        _ => table
            .held_from(*pos, end)
            .map(|(found, key, value, at)| (found, key, value, at, None)),
    };
    let (found, key, value, at, uninitialized) = found?;
    *pos = found.saturating_add(1);
    let lent = Lent::of(watch, at);
    Some(Row {
        key: Keep::lent(req, key, lent),
        value: Keep::lent(req, Value::of_slot(value), lent),
        at,
        watch,
        uninitialized,
    })
}

/// How a callback walk keeps the rows it lends for their step: php's
/// native's choice where it takes an owner, and otherwise what the walk
/// holds decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lent {
    /// One owner of the key and of the value, given up as the step
    /// ends: the live walk, since `php_array_walk` copies the row into
    /// its call's arguments.
    Owned,
    /// A pin ([`Pin`]): nothing the walk holds keeps the row, and php's
    /// native takes no owner of it.
    Pinned,
    /// A view: what the walk holds keeps the row, the table under
    /// [`Hold::Owner`] or the reader's hold, or an immutable table.
    Viewed,
}

impl Lent {
    /// How a [`foreach`] walk keeps a row standing `at`, whose table it
    /// knows stands by `watch`: a bucket of a table the walk holds or
    /// that is immutable as a view; a bucket of a table the walk holds
    /// nothing of pinned, and so is a declared slot always, since neither
    /// a hold nor an immutable table is of the object's slots, which
    /// php's `IS_INDIRECT` reads as they stand.
    fn of(watch: Watch, at: At) -> Lent {
        match (at, watch) {
            (At::Slot { .. }, _) | (At::Bucket(..), Watch::Iterator(_)) => Lent::Pinned,
            (At::Bucket(..), Watch::Held | Watch::Immutable) => Lent::Viewed,
        }
    }
}

/// A walk's pin over a row's key or value, kept for its step where
/// nothing else the walk holds keeps the row and php's native takes no
/// owner of it (design §4.4, #2159, #2230): no count, so the count the
/// program reads is php's, and where the row's last owner goes during
/// the step the row is destroyed there, as php destroys it, and only its
/// bytes wait for the pin to be let go ([`php_heap::Heap::pin_block`]),
/// so the walk reads it destroyed and never freed. Taken from a view and let go
/// by the walk on every way out, never by a `Drop` (design §5.1). Never a
/// way to own: nothing reads it as a count, and the body hands a sink an
/// owner it takes of the value, never the pin.
#[must_use = "a pin is let go with Pin::let_go"]
#[derive(Debug)]
struct Pin<T: View> {
    view: T,
}

impl<T: View> Pin<T> {
    /// A pin over what `view` names, one of its pins however many stand,
    /// so a walk's pin and a walk's pin in a fiber suspended inside it
    /// are let go in either order (#2291).
    fn take(req: &mut Request, view: T) -> Pin<T> {
        req.heap_mut().pin_block(view.counted().to_zval());
        Pin { view }
    }

    /// The pin let go: the bytes of what the view names given back
    /// where it was destroyed while the pins stood and this one was the
    /// last.
    fn let_go(self, req: &mut Request) {
        req.heap_mut().unpin_block(self.view.counted().to_zval());
    }
}

/// A row's key or value as the walk keeps it for the step ([`Lent`]).
#[derive(Debug)]
enum Keep<T: View> {
    Owned(Owner<T>),
    Pinned(Pin<T>),
    Viewed(T),
}

impl<T: View> Keep<T> {
    /// `view` kept as `lent` says; a view of what counts no owner, a
    /// number or an interned string, which nothing the body does can
    /// free, kept as a view where a pin would stand.
    fn lent(req: &mut Request, view: T, lent: Lent) -> Keep<T> {
        match lent {
            Lent::Owned => Keep::Owned(Owner::take(req, view)),
            Lent::Pinned if !view.counted().to_zval().is_refcounted() => Keep::Viewed(view),
            Lent::Pinned => Keep::Pinned(Pin::take(req, view)),
            Lent::Viewed => Keep::Viewed(view),
        }
    }

    /// How the row is kept.
    fn lent_as(&self) -> Lent {
        match self {
            Keep::Owned(_) => Lent::Owned,
            Keep::Pinned(_) => Lent::Pinned,
            Keep::Viewed(_) => Lent::Viewed,
        }
    }

    /// The view.
    fn get(&self) -> &T {
        match self {
            Keep::Owned(owner) => owner,
            Keep::Pinned(pin) => &pin.view,
            Keep::Viewed(view) => view,
        }
    }

    /// What the walk took for the step given back: an owner as
    /// `zval_ptr_dtor_nogc` gives one up, since php's walk holds none
    /// and so leaves no possible root of the cycle collector; a pin let
    /// go. The view is left, which nothing keeps from here. In place, so
    /// a walk keeps its row in one place across the body's call, which a
    /// recursion through the walk pays for at every level.
    fn let_go(&mut self, req: &mut Request) {
        if let Keep::Viewed(_) = self {
            return;
        }
        let view = self.get().clone();
        match std::mem::replace(self, Keep::Viewed(view)) {
            Keep::Owned(owner) => owner.give_up_nogc(req),
            Keep::Pinned(pin) => pin.let_go(req),
            Keep::Viewed(_) => {}
        }
    }
}

/// One row a callback walk lends its body ([`foreach`], [`live`]): the
/// key and the value as the table held them when the walk stepped onto
/// them, each kept alive for the step, owned, pinned or held by what the
/// walk holds, so a body whose code unsets the row can still read it,
/// and where the row stands, so the body can make it a reference in
/// place ([`Row::make_ref`]). A body that keeps the key or the value past
/// the step, or hands either to a sink, takes an owner of its own
/// ([`Owner::take`]).
#[derive(Debug)]
pub struct Row {
    key: Keep<Key>,
    value: Keep<Value>,
    at: At,
    /// How the row knows the table it is in still stands.
    watch: Watch,
    /// The type's text of the typed property nothing has written that
    /// the row stands for ([`Unwritten::Shown`]).
    uninitialized: Option<String>,
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
}

impl Row {
    /// The row's key, as the table held it.
    #[must_use]
    pub fn key(&self) -> &Key {
        self.key.get()
    }

    /// The row's value, a reference where the table holds one.
    #[must_use]
    pub fn value(&self) -> &Value {
        self.value.get()
    }

    /// Where the row stands for a typed property nothing has written,
    /// lent by a walk over [`Unwritten::Shown`]: the text of its type,
    /// `zend_type_to_string` of what
    /// `zend_get_typed_property_info_for_slot` finds, which a dumper
    /// prints as `uninitialized(T)`. The row's value is then undef.
    #[must_use]
    pub fn uninitialized(&self) -> Option<&str> {
        self.uninitialized.as_deref()
    }

    /// The row made a reference in place, php's `ZVAL_MAKE_REF` over the
    /// bucket: the reference the table holds there already, or a new one
    /// holding the value, written into the table as it stands, shared or
    /// not, as php writes it, and into a typed property's slot with the
    /// property as its type source (`ZEND_REF_ADD_TYPE_SOURCE`). From
    /// here the walk keeps the reference for the step as it kept the
    /// value. Asked before the
    /// body runs any code of the program, as php's walk makes it before
    /// its call; once that code has moved the row, the answer is the
    /// `InternalError` it is, never a write elsewhere. A row of a
    /// [`Hold::Owner`] walk or of an immutable table is never written,
    /// as no php native's walk writes one, and is that `InternalError`
    /// too.
    pub fn make_ref(&mut self, req: &mut Request) -> Result<PhpRef, Thrown> {
        if let Some(cell) = self.value().as_php_ref() {
            return Ok(cell);
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
                && holder.is_none_or(|(holder, held)| holder.get().as_object() == Some(held)) =>
            {
                let at = owner.heap().slot_ref(slot);
                if let Some(at) = at.filter(|at| at.read().is_same_value(self.value().to_zval()))
                    && let Some((typed, name)) = req.slot_typed_property(owner.class(), slot)
                {
                    req.fetch_typed_slot_ref(at, &typed, &name)?;
                    let Some(cell) = Value::of_slot(at.read()).as_php_ref() else {
                        return Err(moved(req));
                    };
                    return Ok(self.lend(req, cell));
                }
                at
            }
            At::Bucket(..) | At::Slot { .. } => None,
        };
        let Some(place) = place.filter(|place| place.read().is_same_value(self.value().to_zval()))
        else {
            return Err(moved(req));
        };
        let cell = req.reference(*self.value())?;
        place.write(cell.as_value().to_zval());
        Ok(self.lend(req, cell))
    }

    /// The reference `cell`, which the row's place holds from here, lent
    /// in place of the value it holds, kept as the value was: the
    /// reference taken first, then the value let go, which the reference
    /// holds.
    fn lend(&mut self, req: &mut Request, cell: PhpRef) -> PhpRef {
        let lent = match self.value.lent_as() {
            Lent::Owned => Lent::Owned,
            Lent::Pinned | Lent::Viewed => Lent::of(self.watch, self.at),
        };
        let kept = Keep::lent(req, cell.as_value(), lent);
        let mut value = std::mem::replace(&mut self.value, kept);
        value.let_go(req);
        cell
    }

    /// What the walk kept of the row given back as the step ends
    /// ([`Keep::let_go`]), the value and then the key, each left a view.
    fn let_go(&mut self, req: &mut Request) {
        self.value.let_go(req);
        self.key.let_go(req);
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
    if let Some(object) = now.as_object() {
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
///
/// ```php
/// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, "\n"; }
/// pk(function () { $a = [1, 2]; try { array_walk($a, function ($v) use (&$a) { $a = 1; throw new Exception('w'); }); } catch (TypeError $e) {} });
/// ```
/// ```expect
/// 6048
/// ```
fn steps<B>(
    req: &mut Request,
    holder: PhpRef,
    at: u32,
    body: &mut impl FnMut(&mut Request, &mut Row) -> Step<B>,
) -> Result<Option<B>, Thrown> {
    loop {
        let mut lent = next_row(req, holder, at)?;
        let Some(row) = lent.as_mut() else {
            return Ok(None);
        };
        let step = body(req, row);
        row.let_go(req);
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
            // `zend_type_error`, its `zend_vspprintf` buffer standing
            // across the throw.
            return Err(throw_error_chained(req, thrown, "TypeError", NO_TABLE));
        }
        return Err(thrown);
    }
}

/// php's `TypeError` once a live walk's holder holds no array or object.
const NO_TABLE: &str = "Iterated value is no longer an array or object";

/// Where a hash iterator's step leaves it past the row it found: php's
/// two ways, which differ where a row lands in a hole the step passed
/// over, as one appended after the last row was unset does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Advance {
    /// One past the row, `pos++` in `FE_FETCH_RW`'s loop
    /// (`Z/Zend/zend_vm_def.h`, `ZEND_FE_FETCH_RW`).
    Past,
    /// The next live row, `zend_hash_move_forward_ex`, as
    /// `php_array_walk` moves before its call.
    NextLive,
}

/// One step of hash iterator `at` over the array the reference `holder`
/// holds, `array`: its position (`zend_hash_iterator_pos_ex`), which
/// separates an array the program shared and moves the iterator onto
/// an array the program put in the reference, the reference made to
/// hold the copy (`ZVAL_ARR(array, zend_array_dup(ht))`); the first
/// live row from there; and the iterator left past it as `advance`
/// says. The table the row is in, its position, its key and its value,
/// or `None` past the last row. `FE_FETCH_RW` and the Live walk both
/// step through it.
pub(crate) fn array_step(
    req: &mut Request,
    holder: PhpRef,
    array: PhpArray,
    at: u32,
    advance: Advance,
) -> Result<Option<(Arr, u32, php_heap::Key, Zval)>, php_heap::Fatal> {
    let (pos, table) = req.heap_mut().iterator_pos_ex(at, array.heap())?;
    if !table.is_same(array.heap()) {
        holder.heap().val_ref().write(Zval::array(table));
    }
    let Some((found, key, value)) = table.live_from(pos) else {
        return Ok(None);
    };
    let past = found.saturating_add(1);
    let next = match advance {
        Advance::Past => past,
        Advance::NextLive => table.live_from(past).map_or(past, |(next, _, _)| next),
    };
    req.heap_mut().iterator_set_pos(at, next);
    Ok(Some((table, found, key, value)))
}

/// One step of hash iterator `at` over an object's property table in
/// php's positions, the declared slots and then the dynamic table: the
/// iterator moved onto `table` first (`zend_hash_iterator_pos`), so
/// one the program left on another object's table stands at this
/// table's pointer, and only then its place read, among the slots its
/// lead and in the table its position; the first row from there that
/// holds a value and that `admits` (php's scope check, or none), a slot
/// nothing holds stepped over as php steps over an `IS_UNDEF` behind an
/// `IS_INDIRECT`; and the iterator left past it as `advance` says. Its
/// position, what stands there and its value, or `None` past the last,
/// the iterator then at the end. `stepped` says whether the declared
/// slots are walked from the lead or the dynamic table alone
/// ([`Stepped`]). `FE_FETCH_RW` and `FE_FETCH_R` over an object
/// ([`Request::object_walk`]) and the Live walk step through it.
pub(crate) fn object_step(
    req: &mut Request,
    at: u32,
    (table, stepped): (PropertyTable, Stepped),
    advance: Advance,
    mut admits: impl FnMut(&Request, &TableRow) -> bool,
) -> Option<(u32, TableRow, Zval)> {
    let declared = table.declared();
    let pos = req.heap_mut().iterator_pos(at, table.table());
    let lead = req.heap_mut().iterator_lead(at);
    let mut from = match stepped {
        Stepped::Positions if lead < declared => lead,
        Stepped::Positions | Stepped::Dynamic => declared + pos,
    };
    let end = table.used();
    let found = loop {
        let Some((found, row, value)) = held_at(table, from, end) else {
            break None;
        };
        from = found.saturating_add(1);
        if admits(req, &row) {
            break Some((found, row, value));
        }
    };
    let next = match (found, advance) {
        (None, _) => end,
        (Some((found, ..)), Advance::Past) => found.saturating_add(1),
        (Some((found, ..)), Advance::NextLive) => {
            let mut next = found.saturating_add(1);
            while next < end && table.row_at(next).is_none() {
                next = next.saturating_add(1);
            }
            next
        }
    };
    let heap = req.heap_mut();
    if next < declared {
        heap.iterator_set_lead(at, next);
    } else {
        if stepped == Stepped::Positions {
            heap.iterator_set_lead(at, declared);
        }
        heap.iterator_set_pos(at, next - declared);
    }
    found
}

/// What an [`object_step`] walks of an object's positions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stepped {
    /// The declared slots from the iterator's lead, then the dynamic
    /// table.
    Positions,
    /// The dynamic table alone, the lead left as it stands: a hooked
    /// class's walk keeps its own place among its declared rows
    /// (`zend_hooked_object_get_iterator`).
    Dynamic,
}

/// The row iterator `at` stands at in what `holder` holds now, the
/// iterator moved past it ([`array_step`], [`object_step`]); `None`
/// past the last row.
fn next_row(req: &mut Request, holder: PhpRef, at: u32) -> Result<Option<Row>, Thrown> {
    let now = holder.get();
    if let Some(array) = now.as_array() {
        let stepped = array_step(req, holder, array, at, Advance::NextLive);
        let Some((table, found, key, value)) = req.fatal_of(stepped)? else {
            return Ok(None);
        };
        return Ok(Some(Row {
            key: Keep::lent(req, Key::of(key), Lent::Owned),
            value: Keep::lent(req, Value::of_slot(value), Lent::Owned),
            at: At::Bucket(table, found),
            watch: Watch::Iterator(at),
            uninitialized: None,
        }));
    }
    if let Some(held) = now.as_object() {
        let (owner, table) = req.pointer_table(held, false)?;
        return object_row(req, holder, at, held, owner, table);
    }
    Err(throw_error(req, "TypeError", NO_TABLE))
}

/// [`next_row`] over an object's property table in php's positions
/// ([`object_step`]), every row admitted, as `php_array_walk` reads the
/// table with no scope check; a declared row's key is its mangled name.
fn object_row(
    req: &mut Request,
    holder: PhpRef,
    iterator: u32,
    held: PhpObject,
    owner: PhpObject,
    table: PropertyTable,
) -> Result<Option<Row>, Thrown> {
    let Some((found, row, value)) = object_step(
        req,
        iterator,
        (table, Stepped::Positions),
        Advance::NextLive,
        |_, _| true,
    ) else {
        return Ok(None);
    };
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
        TableRow::Entry(key) => (
            Key::of(key),
            At::Bucket(table.table(), found - table.declared()),
        ),
    };
    Ok(Some(Row {
        key: Keep::lent(req, key, Lent::Owned),
        value: Keep::lent(req, Value::of_slot(value), Lent::Owned),
        at,
        watch: Watch::Iterator(iterator),
        uninitialized: None,
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
    let mut copy = held.copy(req)?;
    let answered = copy.written(|copy| body(req, copy));
    match holder.reference() {
        Some(cell) => cell.convert(req, copy.into_value()),
        None => copy.give_up(req),
    }
    answered.map(Some)
}

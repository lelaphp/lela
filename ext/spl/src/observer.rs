//! `spl_observer.stub.php`: `SplObserver` and `SplSubject`, then
//! `SplObjectStorage` and `MultipleIterator` over php's
//! `spl_SplObjectStorage`, as `PHP_MINIT_FUNCTION(spl_observer)`
//! registers them. The storage is a table in insertion order keyed by
//! the object's id, or by what `getHash()` answers where a user
//! class overrides it, each row the object with one more owner and its
//! information; the iterator is a position in it and `intern->index`
//! beside it.

mod table;

use std::collections::HashMap;
use std::ops::ControlFlow;

use php_ext::abi::{
    CloneObj, DimensionRead, ExtensionBuilder, GcTable, Instance, ObjectHandlers, Std, This, Union,
    Untyped,
};
use php_ext::attr::php_class;
use php_ext::call::{call_method, clone_members, new_object};
use php_ext::coerce::{compare, identical, truthy, value_name};
use php_ext::raising::{Thrown, throw, throw_exception_ex, type_error};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Emalloc, Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};
use php_ext_standard::Serializer;

use crate::{ArrayAccess, Countable, Iterator, Record, SeekableIterator, Serializable};
use table::Table;

/// A row's key: the object's id, or the string `getHash()` answered.
#[derive(Clone, PartialEq, Eq, Hash)]
enum RowKey {
    Id(u32),
    Hash(Vec<u8>),
}

/// One row: its key, and its owners of the object and the information.
type Row = (RowKey, Owner<PhpObject>, Owner<Value>);

/// `spl_SplObjectStorage`: the rows with their places, what php's table
/// lays down for them, the position, `intern->index`, and
/// `MultipleIterator`'s flags.
#[derive(Default)]
pub(crate) struct StorageState {
    rows: Vec<Option<Row>>,
    places: HashMap<RowKey, usize>,
    table: Table,
    pos: usize,
    index: i64,
    flags: i64,
}

/// The payload: `spl_SplObjectStorage`'s fields, boxed, inside the `sizeof` php
/// allocates (design §5.13).
pub(crate) type Storage = Box<StorageState>;

impl StorageState {
    fn len(&self) -> usize {
        self.places.len()
    }

    /// The live rows in order, views.
    fn live(&self) -> Vec<(PhpObject, Value)> {
        self.rows
            .iter()
            .flatten()
            .map(|(_, object, inf)| (**object, **inf))
            .collect()
    }

    /// `zend_hash_internal_pointer_reset_ex`.
    fn reset(&mut self) {
        self.pos = self.valid_from(0);
    }

    fn valid_from(&self, from: usize) -> usize {
        let mut at = from;
        while at < self.rows.len() && self.rows[at].is_none() {
            at += 1;
        }
        at
    }

    fn here(&self) -> Option<(PhpObject, Value)> {
        self.row_at(self.pos).map(|(_, object, inf)| (object, inf))
    }

    /// The first live row at or after `from` and where it stands, views:
    /// one step of a walk that reads the table again at every row, as
    /// `SPL_SAFE_HASH_FOREACH_PTR` and `zend_hash_get_current_data_ptr_ex`
    /// read it, so a row the user code an earlier row reached detached
    /// is not visited and one it attached is.
    fn row_at(&self, from: usize) -> Option<(usize, PhpObject, Value)> {
        let at = self.valid_from(from);
        self.rows
            .get(at)
            .and_then(Option::as_ref)
            .map(|(_, object, inf)| (at, **object, **inf))
    }

    fn forward(&mut self) {
        self.pos = self.forward_from(self.pos);
    }

    /// `zend_hash_move_forward_ex` over a position: past the live row at
    /// or after `pos`, as the table stands now, so where user code
    /// detached the row at `pos` the step passes the row after it.
    fn forward_from(&self, pos: usize) -> usize {
        self.valid_from(self.valid_from(pos) + 1)
    }

    fn backward(&mut self) {
        let mut at = self.valid_from(self.pos);
        while at > 0 {
            at -= 1;
            if self.rows[at].is_some() {
                break;
            }
        }
        self.pos = at;
    }

    /// The information replaced, or a row appended; the old value or,
    /// for a new row, `None`.
    /// The row under `key` given `inf`, the information it held and the
    /// owner of the object it needs none of answered; or a new row, which
    /// keeps both.
    fn attach(
        &mut self,
        key: RowKey,
        object: Owner<PhpObject>,
        inf: Owner<Value>,
    ) -> Option<(Owner<Value>, Owner<PhpObject>)> {
        if let Some(at) = self.places.get(&key)
            && let Some(Some(row)) = self.rows.get_mut(*at)
        {
            return Some((std::mem::replace(&mut row.2, inf), object));
        }
        self.places.insert(key.clone(), self.rows.len());
        self.rows.push(Some((key, object, inf)));
        None
    }

    fn detach(&mut self, key: &RowKey) -> Option<(Owner<PhpObject>, Owner<Value>)> {
        let at = self.places.remove(key)?;
        self.rows
            .get_mut(at)
            .and_then(Option::take)
            .map(|(_, object, inf)| (object, inf))
    }
}

/// `spl_handler_SplObjectStorage`, and `MultipleIterator`'s beside it.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// The two interfaces and the two classes onto `builder`, in the order
/// `PHP_MINIT_FUNCTION(spl_observer)` registers them.
pub(crate) fn register(builder: ExtensionBuilder) -> ExtensionBuilder {
    builder
        .class::<SplObserver>()
        .class::<SplSubject>()
        .class::<SplObjectStorage>()
        .class::<MultipleIterator>()
}

/// `interface SplObserver`, what an `SplSubject` notifies.
///
/// ```php
/// class Subject implements SplSubject {
///     private $observers;
///     function __construct() { $this->observers = new SplObjectStorage; }
///     function attach(SplObserver $o): void { $this->observers[$o] = null; }
///     function detach(SplObserver $o): void { $this->observers->offsetUnset($o); }
///     function notify(): void { foreach ($this->observers as $o) $o->update($this); }
/// }
/// class Printer implements SplObserver {
///     function __construct(private string $name) {}
///     function update(SplSubject $s): void { echo "{$this->name} updated by ", get_class($s), "\n"; }
/// }
/// $s = new Subject; $a = new Printer('a'); $s->attach($a); $s->attach(new Printer('b')); $s->attach($a);
/// $s->notify();
/// ```
/// ```expect
/// a updated by Subject
/// b updated by Subject
/// ```
#[php_class(interface)]
pub(crate) struct SplObserver;

#[php_class]
impl SplObserver {
    #[php(tentative)]
    fn update(this: This<Self>, subject: Instance<SplSubject>);
}

/// `interface SplSubject`, what keeps and notifies its observers.
#[php_class(interface)]
pub(crate) struct SplSubject;

#[php_class]
impl SplSubject {
    #[php(tentative)]
    fn attach(this: This<Self>, observer: Instance<SplObserver>);
    #[php(tentative)]
    fn detach(this: This<Self>, observer: Instance<SplObserver>);
    #[php(tentative)]
    fn notify(this: This<Self>);
}

/// `class SplObjectStorage implements Countable, SeekableIterator,
/// Serializable, ArrayAccess`; `offsetExists`, `offsetSet` and
/// `offsetUnset` are php's aliases of `contains`, `attach` and `detach`,
/// each a body of its own here since the three declare `$object`
/// untyped where the three they alias declare `object`.
///
/// ```php
/// $s = new SplObjectStorage; $a = new stdClass; $b = new stdClass;
/// $s->offsetSet($a, 1); $s->offsetSet($b, 2);
/// var_dump($s->offsetExists($a), $s->count(), $s->offsetGet($b));
/// $s->offsetUnset($b); $s->rewind(); var_dump($s->valid(), $s->key(), $s->current() === $a, $s->getInfo());
/// $s->setInfo('i'); $s->next(); var_dump($s->valid());
/// $t = new SplObjectStorage; $t->unserialize($s->serialize()); var_dump(count($t));
/// $u = new SplObjectStorage; $u->__unserialize($s->__serialize()); var_dump(count($u->__debugInfo()["\0SplObjectStorage\0storage"]));
/// $s->seek(0); var_dump($s->addAll($t), $s->removeAll($t), $s->removeAllExcept($u), strlen($s->getHash($a)));
/// var_dump($s->contains($a)); $s->attach($a); $s->detach($a);
/// ```
/// ```expect
/// bool(true)
/// int(2)
/// int(2)
/// bool(true)
/// int(0)
/// bool(true)
/// int(1)
/// bool(false)
/// int(1)
/// int(1)
/// int(2)
/// int(1)
/// int(1)
/// int(32)
///
/// Deprecated: Method SplObjectStorage::contains() is deprecated since 8.5, use method SplObjectStorage::offsetExists() instead in %s on line 10
/// bool(true)
///
/// Deprecated: Method SplObjectStorage::attach() is deprecated since 8.5, use method SplObjectStorage::offsetSet() instead in %s on line 10
///
/// Deprecated: Method SplObjectStorage::detach() is deprecated since 8.5, use method SplObjectStorage::offsetUnset() instead in %s on line 10
/// ```
///
/// `zend_object_alloc(sizeof(spl_SplObjectStorage))`, which
/// `memory_get_usage()` counts (design §5.13):
///
/// ```php
/// foreach ([false, true] as $shown) foreach (['SplObjectStorage', 'MultipleIterator'] as $c) {
///     $r = new ReflectionClass($c);
///     $m = memory_get_usage();
///     $o = $r->newInstanceWithoutConstructor();
///     $used = memory_get_usage() - $m;
///     $o = null;
///     if ($shown) echo $c, ' ', $used, "\n";
/// }
/// ```
/// ```expect
/// SplObjectStorage 160
/// MultipleIterator 160
/// ```
#[php_class(
    implements(Countable, SeekableIterator, Serializable, ArrayAccess),
    size = 144, payload = Storage,
    handlers = HANDLERS
)]
pub(crate) struct SplObjectStorage;

#[php_class]
impl SplObjectStorage {
    /// `attach(object $object, mixed $info = null): void`, deprecated in
    /// 8.5 for `offsetSet`.
    ///
    /// ```php
    /// $s = new SplObjectStorage; $a = new stdClass; $b = new stdClass;
    /// $s[$a] = 'first'; $s->offsetSet($b, 'second'); $s[$a] = 'again';
    /// var_dump(count($s), $s[$a], isset($s[$b]), $s->offsetExists(new stdClass));
    /// $s->attach($b);
    /// ```
    /// ```expect
    /// int(2)
    /// string(5) "again"
    /// bool(true)
    /// bool(false)
    ///
    /// Deprecated: Method SplObjectStorage::attach() is deprecated since 8.5, use method SplObjectStorage::offsetSet() instead in %s on line 5
    /// ```
    #[php(
        tentative,
        deprecated(
            since = "8.5",
            message = "use method SplObjectStorage::offsetSet() instead"
        )
    )]
    fn attach(
        req: &mut Request,
        this: This<Self>,
        object: PhpObject,
        #[php(default = null)] info: Value,
    ) -> Result<(), Thrown> {
        storage_attach(req, this.object(), object, info)
    }

    /// `detach(object $object): void`, `offsetUnset`: the row gone and the
    /// position reset.
    ///
    /// ```php
    /// $s = new SplObjectStorage; $a = new stdClass; $s[$a] = 1;
    /// $s->offsetUnset($a); $s->offsetUnset(new stdClass);
    /// var_dump(count($s));
    /// try { $s[$a]; } catch (UnexpectedValueException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(0)
    /// Object not found
    /// ```
    #[php(
        tentative,
        deprecated(
            since = "8.5",
            message = "use method SplObjectStorage::offsetUnset() instead"
        )
    )]
    fn detach(req: &mut Request, this: This<Self>, object: PhpObject) -> Result<(), Thrown> {
        detach_row(req, this.object(), object)
    }

    /// `contains(object $object): bool`, `offsetExists`; the witness is the
    /// class's.
    #[php(
        tentative,
        deprecated(
            since = "8.5",
            message = "use method SplObjectStorage::offsetExists() instead"
        )
    )]
    fn contains(req: &mut Request, this: This<Self>, object: PhpObject) -> Result<bool, Thrown> {
        storage_contains(req, this.object(), object)
    }

    /// `addAll(SplObjectStorage $storage): int`.
    ///
    /// ```php
    /// $a = new stdClass; $b = new stdClass; $c = new stdClass;
    /// $x = new SplObjectStorage; $x[$a] = 1; $x[$b] = 2;
    /// $y = new SplObjectStorage; $y[$b] = 'B'; $y[$c] = 'C';
    /// var_dump($x->addAll($y), $x[$b]);
    /// var_dump($x->removeAll($y), $x->removeAllExcept($y));
    /// ```
    /// ```expect
    /// int(3)
    /// string(1) "B"
    /// int(1)
    /// int(0)
    /// ```
    ///
    /// The source is walked live and each row held across its attach, so
    /// a `getHash` that detaches the row, and the one after it, from the
    /// source frees nothing the walk still reads; `key()` is reset
    /// whether the walk finished or threw.
    ///
    /// ```php
    /// class Evil extends SplObjectStorage {
    ///     public $throw = false;
    ///     public function getHash($o): string {
    ///         global $src;
    ///         if ($this->throw) throw new Exception("no hash for {$o->n}");
    ///         echo "getHash {$o->n}\n";
    ///         unset($src[$o]);
    ///         if ($o->n == 1) foreach ($src as $p) { unset($src[$p]); break; }
    ///         return spl_object_hash($o);
    ///     }
    /// }
    /// $src = new SplObjectStorage;
    /// foreach ([1, 2, 3] as $n) { $o = new stdClass; $o->n = $n; $src[$o] = "i$n"; }
    /// unset($o);
    /// $e = new Evil;
    /// var_dump($e->addAll($src), count($src));
    /// foreach ($e as $i => $o) echo $i, $o->n, $e->getInfo(), "\n";
    /// $src[$o] = 'again'; $e->next(); $e->throw = true;
    /// try { $e->addAll($src); } catch (Exception $x) { echo $x->getMessage(), "\n"; }
    /// var_dump(count($e), $e->key());
    /// ```
    /// ```expect
    /// getHash 1
    /// getHash 3
    /// int(2)
    /// int(0)
    /// 01i1
    /// 13i3
    /// no hash for 3
    /// int(2)
    /// int(0)
    /// ```
    #[php(tentative)]
    fn addAll(
        req: &mut Request,
        this: This<Self>,
        storage: Instance<SplObjectStorage>,
    ) -> Result<i64, Thrown> {
        let this = this.object();
        add_all(req, this, storage.object())?;
        let n = with(req, this, |s| s.len())?;
        Ok(i64::try_from(n).unwrap_or(i64::MAX))
    }

    /// `removeAll(SplObjectStorage $storage): int`.
    ///
    /// The source is walked live by its own position, so a row detached
    /// here is asked for once more before the walk moves on, and a row a
    /// `getHash` detached from the source is never asked for.
    ///
    /// ```php
    /// class Seen extends SplObjectStorage {
    ///     public $src;
    ///     public function getHash($o): string {
    ///         echo "getHash {$o->n}\n";
    ///         if ($this->src && $o->n == 1) foreach ($this->src as $p) if ($p->n == 2) { unset($this->src[$p]); break; }
    ///         return spl_object_hash($o);
    ///     }
    /// }
    /// $s = new Seen; $src = new SplObjectStorage;
    /// foreach ([1, 2, 3, 4] as $n) { $o = new stdClass; $o->n = $n; $src[$o] = $n; if ($n != 2) $s[$o] = $n; }
    /// unset($o, $p);
    /// echo "--\n";
    /// $s->src = $src;
    /// var_dump($s->removeAll($src), count($src));
    /// $s->src = null; foreach ([5, 6] as $n) { $o = new stdClass; $o->n = $n; $s[$o] = $n; }
    /// echo "--\n";
    /// var_dump($s->removeAll($s));
    /// ```
    /// ```expect
    /// getHash 1
    /// getHash 3
    /// getHash 4
    /// --
    /// getHash 1
    /// getHash 1
    /// getHash 3
    /// getHash 3
    /// getHash 4
    /// getHash 4
    /// int(0)
    /// int(3)
    /// getHash 5
    /// getHash 6
    /// --
    /// getHash 5
    /// getHash 6
    /// int(0)
    /// ```
    #[php(tentative)]
    fn removeAll(
        req: &mut Request,
        this: This<Self>,
        storage: Instance<SplObjectStorage>,
    ) -> Result<i64, Thrown> {
        let this = this.object();
        let walked = remove_all(req, this, storage.object());
        let n = with(req, this, |s| {
            s.reset();
            s.index = 0;
            s.len()
        })?;
        walked?;
        Ok(i64::try_from(n).unwrap_or(i64::MAX))
    }

    /// `removeAllExcept(SplObjectStorage $storage): int`.
    ///
    /// The storage is walked live and each row's object held across the
    /// `getHash` of the storage it is tested against, so one that detaches
    /// a later row frees nothing the walk still reads; the position is
    /// reset whether the walk finished or threw.
    ///
    /// ```php
    /// class Evil extends SplObjectStorage {
    ///     public $throw = false;
    ///     public function getHash($o): string {
    ///         global $s;
    ///         if ($this->throw) throw new Exception("no hash for {$o->n}");
    ///         echo "getHash {$o->n}\n";
    ///         if ($o->n == 1) foreach ($s as $p) if ($p->n == 2) { unset($s[$p]); break; }
    ///         return spl_object_hash($o);
    ///     }
    /// }
    /// $s = new SplObjectStorage; $keep = new Evil;
    /// foreach ([1, 2, 3] as $n) { $o = new stdClass; $o->n = $n; $s[$o] = $n; }
    /// $keep[$o] = 'kept';
    /// unset($o, $p);
    /// var_dump($s->removeAllExcept($keep), count($s));
    /// $s->next(); $keep->throw = true;
    /// try { $s->removeAllExcept($keep); } catch (Exception $x) { echo $x->getMessage(), "\n"; }
    /// var_dump(count($s), $s->key(), $s->current()->n);
    /// ```
    /// ```expect
    /// getHash 3
    /// getHash 1
    /// getHash 3
    /// int(1)
    /// int(1)
    /// no hash for 3
    /// int(1)
    /// int(0)
    /// int(3)
    /// ```
    #[php(tentative)]
    fn removeAllExcept(
        req: &mut Request,
        this: This<Self>,
        storage: Instance<SplObjectStorage>,
    ) -> Result<i64, Thrown> {
        let this = this.object();
        let walked = remove_all_except(req, this, storage.object());
        let n = with(req, this, |s| {
            s.reset();
            s.index = 0;
            s.len()
        })?;
        walked?;
        Ok(i64::try_from(n).unwrap_or(i64::MAX))
    }

    /// `getInfo(): mixed`: the information at the position.
    ///
    /// ```php
    /// $s = new SplObjectStorage; $s[new stdClass] = 'one'; $s[new ArrayObject] = 'two';
    /// foreach ($s as $i => $o) { echo $i, get_class($o), $s->getInfo(), ";"; $s->setInfo("#$i"); }
    /// $s->rewind(); echo "\n", $s->getInfo(), $s->key(), "\n";
    /// $s->seek(1); echo get_class($s->current()), "\n";
    /// try { $s->seek(2); } catch (OutOfBoundsException $e) { echo $e->getMessage(), "\n"; }
    /// $s->next(); try { $s->current(); } catch (RuntimeException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// 0stdClassone;1ArrayObjecttwo;
    /// #00
    /// ArrayObject
    /// Seek position 2 is out of range
    /// Called current() on invalid iterator
    /// ```
    #[php(tentative)]
    fn getInfo(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        let here = with(req, this.object(), |s| s.here())?;
        Ok(here.map_or_else(Value::null, |(_, inf)| inf.deref()))
    }

    /// `setInfo(mixed $info): void`; the witness is `getInfo`'s.
    #[php(tentative)]
    fn setInfo(req: &mut Request, this: This<Self>, info: Value) -> Result<(), Thrown> {
        let inf = Owner::take(req, info.deref());
        let old = with(req, this.object(), |s| {
            let at = s.valid_from(s.pos);
            match s.rows.get_mut(at).and_then(Option::as_mut) {
                Some(row) => Ok(std::mem::replace(&mut row.2, inf)),
                None => Err(inf),
            }
        })?;
        match old {
            Ok(old) | Err(old) => old.give_up(req),
        }
        Ok(())
    }

    /// `count(int $mode = COUNT_NORMAL): int`, the rows, and under
    /// `COUNT_RECURSIVE` each row's information counted in as php's
    /// `php_count_recursive` walks the table of rows.
    ///
    /// ```php
    /// $s = new SplObjectStorage; $s[new stdClass] = [1, 2];
    /// var_dump(count($s), $s->count(COUNT_RECURSIVE));
    /// ```
    /// ```expect
    /// int(1)
    /// int(1)
    /// ```
    #[php(tentative)]
    fn count(
        req: &mut Request,
        this: This<Self>,
        #[php(default = COUNT_NORMAL)] mode: i64,
    ) -> Result<i64, Thrown> {
        let _ = mode;
        let n = with(req, this.object(), |s| s.len())?;
        Ok(i64::try_from(n).unwrap_or(i64::MAX))
    }

    /// `rewind(): void`.
    #[php(tentative)]
    fn rewind(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        with(req, this.object(), |s| {
            s.reset();
            s.index = 0;
        })
    }

    /// `valid(): bool`.
    #[php(tentative)]
    fn valid(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        with(req, this.object(), |s| s.here().is_some())
    }

    /// `key(): int`, `intern->index`.
    #[php(tentative)]
    fn key(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        with(req, this.object(), |s| s.index)
    }

    /// `current(): object`.
    #[php(tentative)]
    fn current(req: &mut Request, this: This<Self>) -> Result<PhpObject, Thrown> {
        match with(req, this.object(), |s| s.here())? {
            Some((object, _)) => Ok(object),
            None => Err(throw(
                req,
                "RuntimeException",
                "Called current() on invalid iterator",
            )),
        }
    }

    /// `next(): void`.
    #[php(tentative)]
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        with(req, this.object(), |s| {
            s.forward();
            s.index += 1;
        })
    }

    /// `seek(int $offset): void`; the witness is `getInfo`'s and the
    /// class's.
    fn seek(req: &mut Request, this: This<Self>, offset: i64) -> Result<(), Thrown> {
        let this = this.object();
        let position = offset;
        let len = with(req, this, |s| s.len())?;
        if position < 0 || usize::try_from(position).is_ok_and(|p| p >= len) {
            return Err(throw_exception_ex(
                req,
                "OutOfBoundsException",
                &format!("Seek position {position} is out of range"),
            ));
        }
        with(req, this, |s| {
            if position == 0 || s.index - position > position {
                s.reset();
                s.index = 0;
            }
            while position > s.index {
                s.forward();
                s.index += 1;
            }
            while position < s.index {
                s.backward();
                s.index -= 1;
            }
        })
    }

    /// `unserialize(string $data): void`: the `x:i:count` head, each
    /// `;obj,inf` row attached, then `;m:` with the members, read through
    /// one var hash so a row may name an earlier one by `r:`, as php reads
    /// them; where the data breaks off php's `UnexpectedValueException`
    /// names the offset the reader stopped at.
    ///
    /// ```php
    /// $s = new SplObjectStorage;
    /// $s->unserialize('x:i:2;O:8:"stdClass":0:{},s:1:"a";;r:2;,i:5;;m:a:1:{s:1:"p";i:1;}');
    /// var_dump(count($s), $s->p);
    /// foreach ($s as $i => $o) var_dump($s[$o]);
    /// foreach (['x:i:-1;;m:a:0:{}', 'x:i:1;;i:1;;m:a:0:{}', 'y'] as $bad) {
    ///     try { (new SplObjectStorage)->unserialize($bad); } catch (UnexpectedValueException $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// Deprecated: Creation of dynamic property SplObjectStorage::$p is deprecated in %s on line 3
    /// int(1)
    /// int(1)
    /// int(5)
    /// Error at offset 6 of 16 bytes
    /// Error at offset 6 of 20 bytes
    /// Error at offset 0 of 1 bytes
    /// ```
    ///
    /// A row read again replaces its information, and the old
    /// information and the object are held until the reader is done, as
    /// `var_push_dtor` holds them, so what they free comes back to the
    /// store in php's order.
    ///
    /// ```php
    /// foreach (['x:i:3;O:8:"stdClass":0:{},O:8:"stdClass":0:{};R:2;,i:1;;O:8:"stdClass":0:{},r:2;;m:a:0:{}', 'x:i:3;O:8:"stdClass":0:{},O:8:"stdClass":0:{};r:2;,i:1;;O:8:"stdClass":0:{},r:2;;m:a:0:{}'] as $blob) {
    ///     try {
    ///         $so = new SplObjectStorage; $so->unserialize($blob);
    ///         $ids = []; foreach ($so as $i => $o) $ids[] = spl_object_id($o) . ":" . (is_object($so[$o]) ? spl_object_id($so[$o]) : $so[$o]);
    ///         echo implode(" ", $ids), "\n";
    ///     } catch (UnexpectedValueException $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// Error at offset 46 of 89 bytes
    /// 1:1 2:1
    /// ```
    ///
    /// A read that throws breaks the record too: the throwable is
    /// chained under the `UnexpectedValueException`, at the offset the
    /// reader stopped at, and so is a `getHash()` that throws.
    ///
    /// ```php
    /// class T { public int $p; }
    /// class S extends SplObjectStorage { function getHash($o): string { throw new Exception("hash"); } }
    /// $p = function ($e) { echo get_class($e), ": ", $e->getMessage(), " <- ", get_class($e->getPrevious()), ": ", $e->getPrevious()->getMessage(), "\n"; };
    /// try { (new SplObjectStorage)->unserialize('x:i:1;O:1:"T":1:{s:1:"p";s:1:"x";},N;;m:a:0:{}'); } catch (Throwable $e) { $p($e); }
    /// try { (new S)->unserialize('x:i:1;O:8:"stdClass":0:{},N;;m:a:0:{}'); } catch (Throwable $e) { $p($e); }
    /// ```
    /// ```expect
    /// UnexpectedValueException: Error at offset 33 of 46 bytes <- TypeError: Cannot assign string to property T::$p of type int
    /// UnexpectedValueException: Error at offset 28 of 37 bytes <- Exception: hash
    /// ```
    #[php(tentative)]
    fn unserialize(req: &mut Request, this: This<Self>, data: PhpString) -> Result<(), Thrown> {
        let this = this.object();
        let data = data.as_bytes().to_vec();
        if data.is_empty() {
            return Ok(());
        }
        let mut reader = php_ext_standard::Unserializer::new(req, &data)?;
        let mut replaced = Vec::new();
        let read = read_storage(req, this, &mut reader, &data, &mut replaced);
        let (read, broke_at) = crate::record_read(req, this, &reader, read);
        let finished = reader.finish(req, read);
        crate::give_up_all(req, replaced);
        crate::record_end(req, finished, broke_at, data.len())
    }

    /// `serialize(): string`, php's `x:i:count;` then `obj,inf;` per row
    /// and `m:` with the members. The rows are walked live and each is held
    /// across its write, as php holds them (GH-16588): a `__serialize` that
    /// empties the storage leaves the row being written whole.
    ///
    /// ```php
    /// class C { function __serialize(): array { global $s; $s->removeAll($s); return []; } }
    /// $s = new SplObjectStorage; $s[new C] = new stdClass; $s[new stdClass] = 1;
    /// var_dump($s->serialize(), count($s));
    /// ```
    /// ```expect
    /// string(47) "x:i:2;O:1:"C":0:{},O:8:"stdClass":0:{};m:a:0:{}"
    /// int(0)
    /// ```
    ///
    /// A row whose `__serialize` detaches that row moves the walk past
    /// the row after it, as `zend_hash_move_forward_ex` steps.
    ///
    /// ```php
    /// class C { function __construct(public $n) {} function __serialize(): array { global $s; if ($this->n == 1) unset($s[$this]); return ['n' => $this->n]; } }
    /// $s = new SplObjectStorage; $keep = [];
    /// foreach ([1, 2, 3] as $n) { $keep[] = $o = new C($n); $s[$o] = $n; }
    /// echo $s->serialize(), "\n", count($s), "\n";
    /// ```
    /// ```expect
    /// x:i:3;O:1:"C":1:{s:1:"n";i:1;},i:1;;O:1:"C":1:{s:1:"n";i:3;},i:3;;m:a:0:{}
    /// 2
    /// ```
    #[php(tentative)]
    fn serialize(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let this = this.object();
        let count = with(req, this, |s| s.len())?;
        crate::serialized_parts(req, |req, serializer, out| {
            out.extend_from_slice(b"x:");
            let count = Value::int(i64::try_from(count).unwrap_or(i64::MAX));
            serializer.write(req, out, &count)?;
            serialize_rows(req, this, serializer, out)?;
            out.extend_from_slice(b"m:");
            let members = crate::members(req, this)?;
            let written = serializer.write(req, out, &Value::array(*members));
            members.give_up(req);
            written
        })
    }

    /// `offsetExists($object): bool`, php's alias of `contains()`, its
    /// `$object` untyped and held as `Z_PARAM_OBJ` holds it; the witness
    /// is the class's.
    #[php(tentative)]
    fn offsetExists(
        req: &mut Request,
        this: This<Self>,
        object: Untyped<PhpObject>,
    ) -> Result<bool, Thrown> {
        storage_contains(req, this.object(), object.0)
    }

    /// `offsetGet($object): mixed`: the information, or php's
    /// `UnexpectedValueException` for an object the storage has not.
    #[php(tentative)]
    fn offsetGet(
        req: &mut Request,
        this: This<Self>,
        object: Untyped<PhpObject>,
    ) -> Result<Value, Thrown> {
        let this = this.object();
        let key = row_key(req, this, object.0)?;
        let found = with(req, this, |s| {
            s.places
                .get(&key)
                .and_then(|at| s.rows[*at].as_ref().map(|row| *row.2))
        })?;
        match found {
            Some(inf) => Ok(inf.deref()),
            None => Err(throw_exception_ex(
                req,
                "UnexpectedValueException",
                "Object not found",
            )),
        }
    }

    /// `offsetSet($object, mixed $info = null): void`, php's alias of
    /// `attach()`, each row laid down as php's `intern->storage` lays it
    /// down. The table starts packed under a small id and turns to a hash where
    /// an id lands behind the end, each row a 24-byte element; a
    /// `getHash()` row keeps its string as the bucket's key; a storage read
    /// back by `unserialize()` makes the same allocations:
    ///
    /// ```php
    /// function d($f) { global $r; $m = memory_get_usage(); $f(); $r[] = memory_get_usage() - $m; }
    /// $o = new stdClass; $p = new stdClass; $q = new stdClass;
    /// d(function () use (&$s) { $s = new SplObjectStorage; });
    /// d(function () use ($s, $o) { $s[$o] = null; });
    /// d(function () use ($s) { $s[new stdClass] = null; });
    /// d(function () use ($s, $p) { $s->offsetSet($p); });
    /// d(function () use ($s, $q) { $s[$q] = 5; });
    /// d(function () use ($s, $q) { unset($s[$q]); });
    /// $t = new SplObjectStorage; $t[$o] = 1; $ser = serialize($t);
    /// d(function () use (&$u, $ser) { $u = unserialize($ser); });
    /// d(function () use (&$u) { $u = null; });
    /// echo implode(" ", $r), "\n"; $r = [];
    /// class H extends SplObjectStorage { function getHash($o): string { return spl_object_hash($o); } }
    /// $h = new H;
    /// d(function () use ($h, $o) { $h[$o] = null; });
    /// d(function () use ($h, $p) { $h[$p] = null; });
    /// d(function () use ($h, $p) { unset($h[$p]); });
    /// d(function () use ($h) { $h->removeAll($h); });
    /// $w = new SplObjectStorage; $all = [];
    /// for ($i = 0; $i < 9; $i++) { $all[] = new stdClass; }
    /// foreach ($all as $x) { d(function () use ($w, $x) { $w[$x] = null; }); }
    /// d(function () use (&$w) { $w = null; });
    /// d(function () use (&$s) { $s = null; });
    /// echo implode(" ", $r), "\n";
    /// ```
    /// ```expect
    /// 160 184 64 184 24 -24 600 -600
    /// 408 88 -88 -88 344 24 24 24 24 24 24 24 344 -1016 -592
    /// ```
    #[php(tentative)]
    fn offsetSet(
        req: &mut Request,
        this: This<Self>,
        object: Untyped<PhpObject>,
        #[php(default = null)] info: Value,
    ) -> Result<(), Thrown> {
        storage_attach(req, this.object(), object.0, info)
    }

    /// `offsetUnset($object): void`, php's alias of `detach()`: the row
    /// given up object first, then its information
    /// ([`give_up_row`]), as a detach, an `unset` and the storage's own
    /// end give every row up.
    ///
    /// ```php
    /// class D { function __construct(public $n) {} function __destruct() { echo "dtor {$this->n}\n"; } }
    /// class S extends SplObjectStorage { function getHash($o): string { return 'k'; } }
    /// $s = new S; $s[new D('o1')] = new D('i1');
    /// $s->offsetUnset(new D('x'));
    /// echo "--\n";
    /// $t = new SplObjectStorage; $t[new D('o2')] = new D('i2');
    /// unset($t);
    /// ```
    /// ```expect
    /// dtor o1
    /// dtor i1
    /// dtor x
    /// --
    /// dtor o2
    /// dtor i2
    /// ```
    #[php(tentative)]
    fn offsetUnset(
        req: &mut Request,
        this: This<Self>,
        object: Untyped<PhpObject>,
    ) -> Result<(), Thrown> {
        detach_row(req, this.object(), object.0)
    }

    /// `getHash(object $object): string`, `spl_object_hash`'s string; a
    /// class overriding it keys the storage by what it answers.
    ///
    /// ```php
    /// class ByName extends SplObjectStorage {
    ///     public function getHash($o): string { return $o->name; }
    /// }
    /// $a = new stdClass; $a->name = 'x'; $b = new stdClass; $b->name = 'x';
    /// $s = new ByName; $s[$a] = 1; $s[$b] = 2;
    /// var_dump(count($s), $s[$a], strlen((new SplObjectStorage)->getHash($a)));
    /// ```
    /// ```expect
    /// int(1)
    /// int(2)
    /// int(32)
    /// ```
    #[php(tentative)]
    fn getHash(
        req: &mut Request,
        this: This<Self>,
        object: PhpObject,
    ) -> Result<Owner<PhpString>, Thrown> {
        let _ = this;
        let hash = format!("{:016x}0000000000000000", object.index());
        PhpString::new(req, hash.as_bytes())
    }

    /// `__serialize(): array`: the rows as object then information, then
    /// the members.
    ///
    /// ```php
    /// $s = new SplObjectStorage; $o = new stdClass; $s[$o] = 'i';
    /// var_dump($s->__serialize());
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   array(2) {
    ///     [0]=>
    ///     object(stdClass)#2 (0) {
    ///     }
    ///     [1]=>
    ///     string(1) "i"
    ///   }
    ///   [1]=>
    ///   array(0) {
    ///   }
    /// }
    /// ```
    #[php(tentative)]
    fn __serialize(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let this = this.object();
        let rows = with(req, this, |s| s.live())?;
        let mut list = PhpArray::new(req)?;
        for (object, inf) in rows {
            list.append(req, Owner::take(req, object.as_value().deref()))?;
            list.append(req, Owner::take(req, inf.deref()))?;
        }
        let mut out = PhpArray::new(req)?;
        out.append(req, list.into_value())?;
        let members = crate::members(req, this)?;
        out.append(req, members.into_value())?;
        Ok(out)
    }

    /// `__unserialize(array $data): void`.
    ///
    /// ```php
    /// $s = new SplObjectStorage;
    /// try { $s->__unserialize([[new stdClass], []]); } catch (UnexpectedValueException $e) { echo $e->getMessage(), "\n"; }
    /// try { $s->__unserialize([[1, 2], []]); } catch (UnexpectedValueException $e) { echo $e->getMessage(), "\n"; }
    /// $s->__unserialize([[new stdClass, 'x'], []]); var_dump(count($s));
    /// ```
    /// ```expect
    /// Odd number of elements
    /// Non-object key
    /// int(1)
    /// ```
    ///
    /// What `unserialize()` hands it for an empty storage is two empty
    /// arrays, the process's own (`ZVAL_EMPTY_ARRAY`), so the peak is
    /// php's:
    ///
    /// ```php
    /// $s = serialize(new SplObjectStorage);
    /// unserialize($s); $u = memory_get_usage(); memory_reset_peak_usage(); $x = unserialize($s);
    /// echo memory_get_peak_usage() - $u, " ", memory_get_usage() - $u, "\n";
    /// ```
    /// ```expect
    /// 16920 192
    /// ```
    #[php(tentative)]
    fn __unserialize(req: &mut Request, this: This<Self>, data: PhpArray) -> Result<(), Thrown> {
        let this = this.object();
        let storage = data.get(&Key::Int(0)).and_then(|v| v.deref().as_array());
        let members = data.get(&Key::Int(1)).map(|v| v.deref());
        let (Some(storage), Some(members)) = (storage, members.filter(|m| m.as_array().is_some()))
        else {
            return Err(throw(
                req,
                "UnexpectedValueException",
                "Incomplete or ill-typed serialization data",
            ));
        };
        if storage.len() % 2 != 0 {
            return Err(throw(
                req,
                "UnexpectedValueException",
                "Odd number of elements",
            ));
        }
        // `ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(storage_zv), val)`
        // (`Z/ext/spl/spl_observer.c:1068`), no owner of the table taken:
        // the argument holds it while an attach serves the class's
        // `getHash`. Each key row is kept for the next step by an owner of
        // its own, where php keeps a pointer into the table.
        let mut key: Option<Owner<Value>> = None;
        let walked = walk::foreach(req, storage, Hold::None, |req, row| -> walk::Step {
            let Some(object) = key.take() else {
                key = Some(Owner::take(req, *row.value()));
                return Ok(ControlFlow::Continue(()));
            };
            let target = object.as_object();
            object.give_up(req);
            let Some(target) = target else {
                return Err(throw(req, "UnexpectedValueException", "Non-object key"));
            };
            storage_attach(req, this, target, row.value().deref())?;
            Ok(ControlFlow::Continue(()))
        });
        if let Some(object) = key {
            object.give_up(req);
        }
        walked?;
        crate::load_members(req, this, &members)
    }

    /// `__debugInfo(): array`: the properties, then the rows under the
    /// private name `storage` of `SplObjectStorage`, each an `obj` and `inf`
    /// pair.
    ///
    /// ```php
    /// $s = new SplObjectStorage; $s[new stdClass] = 1;
    /// var_dump($s);
    /// ```
    /// ```expect
    /// object(SplObjectStorage)#1 (1) {
    ///   ["storage":"SplObjectStorage":private]=>
    ///   array(1) {
    ///     [0]=>
    ///     array(2) {
    ///       ["obj"]=>
    ///       object(stdClass)#2 (0) {
    ///       }
    ///       ["inf"]=>
    ///       int(1)
    ///     }
    ///   }
    /// }
    /// ```
    #[php(tentative)]
    fn __debugInfo(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let this = this.object();
        let rows = with(req, this, |s| s.live())?;
        let mut out = PhpArray::new(req)?;
        for (key, value) in crate::std_properties(req, this)? {
            out.set(req, Owner::take(req, key), Owner::take(req, value))?;
        }
        let mut storage = PhpArray::new(req)?;
        for (object, inf) in rows {
            let mut row = PhpArray::new(req)?;
            row.set_str(req, b"obj", Owner::take(req, object.as_value().deref()))?;
            row.set_str(req, b"inf", Owner::take(req, inf.deref()))?;
            storage.append(req, row.into_value())?;
        }
        out.set_str(req, b"\0SplObjectStorage\0storage", storage.into_value())?;
        Ok(out)
    }
}

/// `class MultipleIterator implements Iterator`: its sub-iterators
/// walked in step, their rows in an `SplObjectStorage`'s payload.
///
/// ```php
/// $m = new MultipleIterator(MultipleIterator::MIT_NEED_ANY | MultipleIterator::MIT_KEYS_ASSOC);
/// $m->attachIterator(new ArrayIterator([1, 2, 3]), 'n');
/// $m->attachIterator(new ArrayIterator(['a', 'b']), 'l');
/// foreach ($m as $k => $v) echo json_encode($k), json_encode($v), "\n";
/// var_dump($m->getFlags(), $m->countIterators());
/// $m->setFlags(MultipleIterator::MIT_NEED_ALL);
/// foreach ($m as $v) echo implode(',', $v), ";";
/// $m->rewind(); echo "\n", json_encode($m->key()), json_encode($m->current()), $m->valid() ? "valid" : "", "\n";
/// $i = new ArrayIterator([]); $m->attachIterator($i); var_dump($m->containsIterator($i)); $m->detachIterator($i); $m->next(); var_dump($m->valid(), count($m->__debugInfo()["\0SplObjectStorage\0storage"]));
/// ```
/// ```expect
/// {"n":0,"l":0}{"n":1,"l":"a"}
/// {"n":1,"l":1}{"n":2,"l":"b"}
/// {"n":2,"l":null}{"n":3,"l":null}
/// int(2)
/// int(2)
/// 1,a;2,b;
/// [0,0][1,"a"]valid
/// bool(true)
/// bool(true)
/// int(2)
/// ```
#[php_class(implements(Iterator), size = 144, payload = Storage, handlers = HANDLERS)]
pub(crate) struct MultipleIterator;

#[php_class]
impl MultipleIterator {
    const MIT_NEED_ANY: i64 = 0;
    const MIT_NEED_ALL: i64 = 1;
    const MIT_KEYS_NUMERIC: i64 = 0;
    const MIT_KEYS_ASSOC: i64 = 2;

    /// `MultipleIterator::__construct(int $flags = MIT_NEED_ALL|MIT_KEYS_NUMERIC)`;
    /// the witness is the class's.
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = MultipleIterator::MIT_NEED_ALL | MultipleIterator::MIT_KEYS_NUMERIC)]
        flags: i64,
    ) -> Result<(), Thrown> {
        with(req, this.object(), |s| s.flags = flags)
    }

    /// `getFlags(): int`.
    #[php(tentative)]
    fn getFlags(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        with(req, this.object(), |s| s.flags)
    }

    /// `setFlags(int $flags): void`.
    #[php(tentative)]
    fn setFlags(req: &mut Request, this: This<Self>, flags: i64) -> Result<(), Thrown> {
        with(req, this.object(), |s| s.flags = flags)
    }

    /// `attachIterator(Iterator $iterator, string|int|null $info = null):
    /// void`: an info a row has already is php's `InvalidArgumentException`.
    ///
    /// ```php
    /// $m = new MultipleIterator(MultipleIterator::MIT_KEYS_ASSOC);
    /// $a = new ArrayIterator([1]);
    /// $m->attachIterator($a, 'x');
    /// try { $m->attachIterator(new ArrayIterator([2]), 'x'); } catch (InvalidArgumentException $e) { echo $e->getMessage(), "\n"; }
    /// $m->attachIterator(new ArrayIterator([3]));
    /// try { $m->current(); } catch (InvalidArgumentException $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($m->containsIterator($a)); $m->detachIterator($a); var_dump($m->containsIterator($a));
    /// try { $m->attachIterator($a, new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Key duplication error
    /// Sub-Iterator is associated with NULL
    /// bool(true)
    /// bool(false)
    /// MultipleIterator::attachIterator(): Argument #2 ($info) must be of type string|int|null, stdClass given
    /// ```
    #[php(tentative)]
    fn attachIterator(
        req: &mut Request,
        this: This<Self>,
        iterator: Instance<Iterator>,
        #[php(default = null)] info: Option<Union<PhpString, i64>>,
    ) -> Result<(), Thrown> {
        let this = this.object();
        let info = info.map(|info| match info {
            Union::A(s) => Value::string(s),
            Union::B(n) => Value::int(n),
        });
        if let Some(info) = info {
            let rows = with(req, this, |s| s.live())?;
            for (_, inf) in &rows {
                if identical(req, inf, &info)? {
                    return Err(throw(
                        req,
                        "InvalidArgumentException",
                        "Key duplication error",
                    ));
                }
            }
        }
        storage_attach(req, this, iterator.object(), info.unwrap_or_default())
    }

    /// `detachIterator(Iterator $iterator): void`; the witness is
    /// `attachIterator`'s.
    #[php(tentative)]
    fn detachIterator(
        req: &mut Request,
        this: This<Self>,
        iterator: Instance<Iterator>,
    ) -> Result<(), Thrown> {
        let this = this.object();
        storage_detach(req, this, iterator.object())?;
        with(req, this, |s| {
            s.reset();
            s.index = 0;
        })
    }

    /// `containsIterator(Iterator $iterator): bool`.
    #[php(tentative)]
    fn containsIterator(
        req: &mut Request,
        this: This<Self>,
        iterator: Instance<Iterator>,
    ) -> Result<bool, Thrown> {
        storage_contains(req, this.object(), iterator.object())
    }

    /// `countIterators(): int`.
    #[php(tentative)]
    fn countIterators(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let n = with(req, this.object(), |s| s.len())?;
        Ok(i64::try_from(n).unwrap_or(i64::MAX))
    }

    /// `MultipleIterator::rewind(): void`.
    #[php(tentative)]
    fn rewind(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        each(req, this.object(), b"rewind")
    }

    /// `MultipleIterator::valid(): bool`: all valid under `MIT_NEED_ALL`,
    /// any under `MIT_NEED_ANY`, `false` with none attached.
    #[php(tentative)]
    fn valid(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = this.object();
        let (len, flags) = with(req, this, |s| (s.len(), s.flags))?;
        if len == 0 {
            return Ok(false);
        }
        let expect = flags & Self::MIT_NEED_ALL != 0;
        with(req, this, |s| s.reset())?;
        while let Some((iterator, held, _)) = held_here(req, this)? {
            let answered = crate::method(req, iterator, b"valid");
            held.give_up(req);
            let answered = answered?;
            let valid = answered.as_bool() == Some(true);
            answered.give_up(req);
            if expect != valid {
                return Ok(!expect);
            }
            with(req, this, |s| s.forward())?;
        }
        Ok(expect)
    }

    /// `MultipleIterator::key(): array`.
    #[php(tentative)]
    fn key(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        get_all(req, this.object(), "key")
    }

    /// `MultipleIterator::current(): array`.
    ///
    /// The sub-iterators are walked by the storage's own position and each
    /// held across its calls, so one that detaches another frees nothing
    /// the walk still reads, and a `detachIterator` resets the position
    /// the walk goes on from.
    ///
    /// ```php
    /// class It extends ArrayIterator {
    ///     public $drop = null;
    ///     function __construct(public $name) { parent::__construct([$name]); }
    ///     function valid(): bool {
    ///         global $m;
    ///         echo "valid {$this->name}\n";
    ///         if ($this->drop) { $m->detachIterator($this->drop === 'self' ? $this : $this->drop); $this->drop = null; }
    ///         return parent::valid();
    ///     }
    ///     function __destruct() { echo "gone {$this->name}\n"; }
    /// }
    /// $m = new MultipleIterator(MultipleIterator::MIT_NEED_ANY | MultipleIterator::MIT_KEYS_ASSOC);
    /// foreach (['a', 'b', 'c', 'd'] as $n) $m->attachIterator(new It($n), $n);
    /// foreach ($m->__debugInfo()["\0SplObjectStorage\0storage"] as $row) $its[$row['inf']] = $row['obj'];
    /// $its['a']->drop = $its['b']; $its['c']->drop = 'self';
    /// unset($its, $row);
    /// var_dump($m->current());
    /// echo "--\n";
    /// var_dump($m->valid(), $m->countIterators());
    /// ```
    /// ```expect
    /// valid a
    /// gone b
    /// valid c
    /// gone c
    /// valid d
    /// array(3) {
    ///   ["a"]=>
    ///   string(1) "a"
    ///   ["c"]=>
    ///   string(1) "c"
    ///   ["d"]=>
    ///   string(1) "d"
    /// }
    /// --
    /// valid a
    /// bool(true)
    /// int(2)
    /// gone a
    /// gone d
    /// ```
    ///
    /// A sub-iterator's `current()` or `key()` that throws is php's
    /// `RuntimeException` with what it threw as its previous; a `valid()`
    /// that throws reads as not valid, what it threw handed on, or
    /// chained under the `MIT_NEED_ALL` refusal.
    ///
    /// ```php
    /// class It extends ArrayIterator { function current(): mixed { throw new Exception("inner"); } }
    /// $m = new MultipleIterator; $m->attachIterator(new It([1]));
    /// try { $m->current(); } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), " / ", $e->getPrevious()?->getMessage(), "\n"; }
    /// class V extends ArrayIterator { function valid(): bool { throw new Exception("valid"); } }
    /// $n = new MultipleIterator(MultipleIterator::MIT_NEED_ANY); $n->attachIterator(new V([1])); $n->attachIterator(new ArrayIterator([2]));
    /// try { var_dump($n->current()); } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), " / ", $e->getPrevious()?->getMessage() ?? '-', "\n"; }
    /// $n = new MultipleIterator(MultipleIterator::MIT_NEED_ALL); $n->attachIterator(new V([1]));
    /// try { var_dump($n->key()); } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), " / ", $e->getPrevious()?->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// RuntimeException: Failed to call sub iterator method / inner
    /// Exception: valid / -
    /// RuntimeException: Called key() with non valid sub iterator / valid
    /// ```
    #[php(tentative)]
    fn current(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        get_all(req, this.object(), "current")
    }

    /// `MultipleIterator::next(): void`.
    #[php(tentative)]
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        each(req, this.object(), b"next")
    }

    #[php(tentative, implementation = SplObjectStorage::__debugInfo)]
    fn __debugInfo(this: This<Self>) -> PhpArray;
}

fn with<R>(
    req: &mut Request,
    object: PhpObject,
    change: impl FnOnce(&mut Storage) -> R,
) -> Result<R, Thrown> {
    if let Some(s) = object.native_mut::<Storage>(req) {
        return Ok(change(s));
    }
    Err(php_ext::raising::fatal(
        req,
        "InternalError: an SPL storage method served on an object without its payload",
    ))
}

/// `spl_object_storage_get_hash`: the object's id, or `getHash()`'s string
/// where a user class overrides it, its `TypeError` for anything else.
fn row_key(req: &mut Request, this: PhpObject, object: PhpObject) -> Result<RowKey, Thrown> {
    let (key, string) = hashed(req, this, object)?;
    if let Some(string) = string {
        string.give_up(req);
    }
    Ok(key)
}

/// [`row_key`] with the string `getHash()` answered, which a new row's
/// bucket keeps as its key and `spl_object_storage_free_hash` releases
/// otherwise.
fn hashed(
    req: &mut Request,
    this: PhpObject,
    object: PhpObject,
) -> Result<(RowKey, Option<Owner<Value>>), Thrown> {
    if !crate::overridden(req, this, b"gethash") {
        return Ok((RowKey::Id(object.index()), None));
    }
    let answered = call_method(req, &this, b"getHash", &[object.as_value()])?;
    if let Some(bytes) = answered.as_str_bytes().map(<[u8]>::to_vec) {
        return Ok((RowKey::Hash(bytes), Some(answered)));
    }
    let class = String::from_utf8_lossy(&crate::class_name(req, this.class())).into_owned();
    let name = value_name(req, &answered);
    answered.give_up(req);
    Err(type_error(
        req,
        &format!("{class}::getHash(): Return value must be of type string, {name} returned"),
    ))
}

/// `spl_object_storage_addall`: every row of `other` attached to `this`,
/// `other` walked live as `SPL_SAFE_HASH_FOREACH_PTR` walks it, each
/// row's object and a copy of its information held across the attach,
/// whose `getHash` may detach the row from `other`; then `intern->index`
/// reset, whether the walk finished or a `getHash` threw. php goes on
/// past a failed attach, but with a throwable pending every later
/// `getHash` call is skipped and attaches nothing, so the walk stops
/// there.
fn add_all(req: &mut Request, this: PhpObject, other: PhpObject) -> Result<(), Thrown> {
    let mut from = 0;
    let walked = loop {
        let row = match with(req, other, |s| s.row_at(from)) {
            Ok(Some(row)) => row,
            Ok(None) => break Ok(()),
            Err(thrown) => break Err(thrown),
        };
        let (at, object, inf) = row;
        from = at + 1;
        let held_object = Owner::take(req, object.as_value());
        let inf = Owner::take(req, inf);
        let attached = storage_attach(req, this, object, *inf);
        inf.give_up(req);
        held_object.give_up(req);
        if let Err(thrown) = attached {
            break Err(thrown);
        }
    };
    with(req, this, |s| s.index = 0)?;
    walked
}

/// `SplObjectStorage::removeAll`'s walk: each row of `other` detached from
/// `this`, `other` walked by a position of its own, php's
/// `nInternalPointer` read through `zend_hash_get_current_data_ptr`.
/// The position moves on, `zend_hash_move_forward`, only where the
/// detach found no row, so a row detached here is asked for once more,
/// and read again it is the row after it where `this` is `other`. php
/// holds nothing here and neither does the walk: the row's object is
/// handed to `getHash` alone, whose frame owns it for the call, and the
/// row is read again after. A throwing `getHash` stops the walk, as in
/// [`add_all`].
fn remove_all(req: &mut Request, this: PhpObject, other: PhpObject) -> Result<(), Thrown> {
    let mut pos = 0;
    while let Some((_, object, _)) = with(req, other, |s| s.row_at(pos))? {
        if !storage_detach(req, this, object)? {
            pos = with(req, other, |s| s.forward_from(pos))?;
        }
    }
    Ok(())
}

/// `SplObjectStorage::removeAllExcept`'s walk: each row of `this` that
/// `other` does not contain detached, `this` walked live as
/// `SPL_SAFE_HASH_FOREACH_PTR` walks it and each row's object held,
/// `GC_ADDREF(elem_obj)`, across the `getHash` of either storage. A
/// throwing `getHash` stops the walk, as in [`add_all`].
fn remove_all_except(req: &mut Request, this: PhpObject, other: PhpObject) -> Result<(), Thrown> {
    let mut from = 0;
    while let Some((at, object, _)) = with(req, this, |s| s.row_at(from))? {
        from = at + 1;
        let held = Owner::take(req, object.as_value());
        let tested = match storage_contains(req, other, object) {
            Ok(false) => storage_detach(req, this, object).map(|_| ()),
            kept => kept.map(|_| ()),
        };
        held.give_up(req);
        tested?;
    }
    Ok(())
}

/// `spl_object_storage_attach`: the information of a row replaced, or a
/// row appended holding one more owner of the object, its place and
/// its element laid down as php's table lays them down.
fn storage_attach(
    req: &mut Request,
    this: PhpObject,
    object: PhpObject,
    inf: Value,
) -> Result<(), Thrown> {
    let (key, string) = hashed(req, this, object)?;
    let inf = Owner::take(req, inf.deref());
    let held = Owner::take(req, object);
    if let Some((old, held)) = with(req, this, |s| s.attach(key.clone(), held, inf))? {
        crate::give_up(req, string);
        held.give_up(req);
        old.give_up(req);
        return Ok(());
    }
    let copied = match overridden_bits(req, this) {
        Ok(bits) => bits & SOS_OVERRIDDEN_WRITE != 0,
        Err(thrown) => {
            crate::give_up(req, string);
            return Err(thrown);
        }
    };
    let mut table = with(req, this, |s| std::mem::take(&mut s.table))?;
    let placed = table.place(req, key, string, copied);
    with(req, this, |s| s.table = table)?;
    placed
}

/// `spl_object_storage_detach`: `true` where a row went.
fn storage_detach(req: &mut Request, this: PhpObject, object: PhpObject) -> Result<bool, Thrown> {
    let key = row_key(req, this, object)?;
    detach_key(req, this, &key)
}

/// `zend_hash_index_del` or `zend_hash_del` of the row under `key`:
/// `true` where a row went, given up as php's table gives it up.
fn detach_key(req: &mut Request, this: PhpObject, key: &RowKey) -> Result<bool, Thrown> {
    let gone = with(req, this, |s| {
        let row = s.detach(key)?;
        Some((row, s.table.remove(key)))
    })?;
    let Some(((object, inf), held)) = gone else {
        return Ok(false);
    };
    let (string, element) = held.map_or((None, None), |(s, e)| (s, Some(e)));
    // `zend_hash_del` releases the key before the destructor runs.
    if let Some(string) = string {
        string.give_up(req);
    }
    give_up_row(req, object, inf, element);
    Ok(true)
}

/// `spl_object_storage_dtor`, the table's destructor for every row a
/// detach or the storage's end gives up: the object released, then the
/// information, `zend_object_release(el->obj)` before
/// `zval_ptr_dtor(&el->inf)`, then the element, `efree(el)`.
fn give_up_row(
    req: &mut Request,
    object: Owner<PhpObject>,
    inf: Owner<Value>,
    element: Option<Emalloc>,
) {
    object.give_up(req);
    inf.give_up(req);
    if let Some(element) = element {
        element.free(req);
    }
}

/// `SplObjectStorage::detach`'s body: the row gone and the position
/// reset.
fn detach_row(req: &mut Request, this: PhpObject, object: PhpObject) -> Result<(), Thrown> {
    storage_detach(req, this, object)?;
    with(req, this, |s| {
        s.reset();
        s.index = 0;
    })
}

/// `spl_object_storage_contains`.
fn storage_contains(req: &mut Request, this: PhpObject, object: PhpObject) -> Result<bool, Thrown> {
    let key = row_key(req, this, object)?;
    with(req, this, |s| s.places.contains_key(&key))
}

impl ObjectHandlers for Handlers {
    /// `spl_SplObjectStorage_free_storage`: `zend_object_std_dtor`, then
    /// the storage destroyed.
    ///
    /// ```php
    /// class D { function __construct(public $n) {} function __destruct() { echo $this->n, "\n"; } }
    /// class S extends SplObjectStorage { public $p; }
    /// $o = new S; $o[new D('storage')] = 1; $o->p = new D('prop'); unset($o);
    /// ```
    /// ```expect
    /// prop
    /// storage
    /// ```
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        Std.free_obj(req, object);
        let (rows, mut table) = object
            .native_mut::<Storage>(req)
            .map(|s| {
                s.places.clear();
                (std::mem::take(&mut s.rows), std::mem::take(&mut s.table))
            })
            .unwrap_or_default();
        // `zend_hash_destroy`: each row's destructor, then its key, and
        // the data last.
        for (key, object, inf) in rows.into_iter().flatten() {
            let (string, element) = table
                .remove(&key)
                .map_or((None, None), |(s, e)| (s, Some(e)));
            give_up_row(req, object, inf, element);
            if let Some(string) = string {
                string.give_up(req);
            }
        }
        table.free(req);
    }

    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        if let Some(s) = object.native::<Storage>(req) {
            for (object, inf) in s.live() {
                visit(object.as_value());
                visit(inf);
            }
        }
        GcTable::Properties
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_storage)
    }

    /// `spl_object_storage_read_dimension`: a key no row holds is
    /// `null` under `BP_VAR_IS`, and every value comes back a copy even
    /// under a write fetch, so no entry is taken by reference.
    fn read_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        read: DimensionRead,
    ) -> Result<Owner<Value>, Thrown> {
        let target = offset.and_then(|o| o.deref().as_object());
        match target {
            Some(target) if overridden_bits(req, object)? & SOS_OVERRIDDEN_READ == 0 => {
                let found = with(req, object, |s| {
                    s.places
                        .get(&RowKey::Id(target.index()))
                        .and_then(|at| s.rows[*at].as_ref().map(|row| *row.2))
                })?;
                match found {
                    Some(inf) => Ok(Owner::take(req, inf.deref())),
                    None if read == DimensionRead::Silent => Ok(Owner::null()),
                    None => Err(throw_exception_ex(
                        req,
                        "UnexpectedValueException",
                        "Object not found",
                    )),
                }
            }
            _ => Std.read_dimension(req, object, offset, read),
        }
    }

    fn write_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let target = offset.and_then(|o| o.deref().as_object());
        // The bits are read only for an object offset, as the guard
        // read them; the value's owner is given up where they fail.
        let bits = match target.map(|_| overridden_bits(req, object)) {
            Some(Err(thrown)) => {
                value.give_up(req);
                return Err(thrown);
            }
            Some(Ok(bits)) => bits,
            None => 0,
        };
        match target {
            Some(target) if bits & SOS_OVERRIDDEN_WRITE == 0 => {
                if crate::is_a(req, object.class(), b"MultipleIterator")
                    && !crate::is_a(req, target.class(), b"Iterator")
                {
                    value.give_up(req);
                    return Err(type_error(
                        req,
                        "Can only attach objects that implement the Iterator interface",
                    ));
                }
                let attached = storage_attach(req, object, target, *value);
                value.give_up(req);
                attached
            }
            _ => Std.write_dimension(req, object, offset, value),
        }
    }

    fn has_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: &Value,
        not_empty: bool,
    ) -> Result<bool, Thrown> {
        let Some(target) = offset.deref().as_object() else {
            return Std.has_dimension(req, object, offset, not_empty);
        };
        if overridden_bits(req, object)? & SOS_OVERRIDDEN_READ != 0 {
            return Std.has_dimension(req, object, offset, not_empty);
        }
        let found = with(req, object, |s| {
            s.places
                .get(&RowKey::Id(target.index()))
                .and_then(|at| s.rows[*at].as_ref().map(|row| *row.2))
        })?;
        match found {
            None => Ok(false),
            Some(inf) if not_empty => truthy(req, &inf),
            Some(_) => Ok(true),
        }
    }

    fn unset_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: &Value,
    ) -> Result<(), Thrown> {
        match offset.deref().as_object() {
            Some(target) if overridden_bits(req, object)? & SOS_OVERRIDDEN_UNSET == 0 => {
                detach_key(req, object, &RowKey::Id(target.index()))?;
                Ok(())
            }
            _ => Std.unset_dimension(req, object, offset),
        }
    }

    /// `spl_object_storage_compare_objects`: two `SplObjectStorage`s of
    /// the class itself compared row by row, `zend_hash_compare` over
    /// `spl_object_storage_compare_info`; a row the comparison of an
    /// earlier one detached is not compared.
    ///
    /// ```php
    /// class T {
    ///     function __construct(public $s, public $drop = null) {}
    ///     function __toString(): string {
    ///         global $l;
    ///         echo "toString {$this->s}\n";
    ///         if ($this->drop) { unset($l[$this->drop]); $this->drop = null; }
    ///         return $this->s;
    ///     }
    ///     function __destruct() { echo "gone {$this->s}\n"; }
    /// }
    /// $a = new stdClass; $b = new stdClass; $c = new stdClass;
    /// $l = new SplObjectStorage; $r = new SplObjectStorage;
    /// $l[$a] = new T('x', $b); $l[$b] = new T('y'); $l[$c] = new T('z');
    /// $r[$a] = 'x'; $r[$b] = 'y'; $r[$c] = 'z';
    /// var_dump($l == $r, count($l));
    /// ```
    /// ```expect
    /// toString x
    /// gone y
    /// toString z
    /// bool(true)
    /// int(2)
    /// gone x
    /// gone z
    /// ```
    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<std::cmp::Ordering>, Thrown> {
        let (Some(l), Some(r)) = (left.as_object(), right.as_object()) else {
            return Std.compare(req, left, right);
        };
        if l == r {
            return Ok(Some(std::cmp::Ordering::Equal));
        }
        let base = php_ext::call::lookup_class(req, b"SplObjectStorage")?;
        if Some(l.class()) != base || Some(r.class()) != base {
            return Ok(None);
        }
        let (left_len, right_len) = (with(req, l, |s| s.len())?, with(req, r, |s| s.len())?);
        if left_len != right_len {
            return Ok(Some(left_len.cmp(&right_len)));
        }
        // `zend_hash_compare` walks the left table live, reading each slot
        // as it stands, and finds each row's key in the right one again;
        // the two informations are held across `zend_compare`, which can
        // reach PHP code that detaches either row.
        let mut from = 0;
        while let Some((at, key, inf)) = with(req, l, |s| {
            let at = s.valid_from(from);
            s.rows
                .get(at)
                .and_then(Option::as_ref)
                .map(|(key, _, inf)| (at, key.clone(), **inf))
        })? {
            from = at + 1;
            let other = with(req, r, |s| {
                s.places
                    .get(&key)
                    .and_then(|at| s.rows.get(*at))
                    .and_then(Option::as_ref)
                    .map(|row| *row.2)
            })?;
            let Some(other) = other else {
                return Ok(Some(std::cmp::Ordering::Greater));
            };
            let inf = Owner::take(req, inf);
            let other = Owner::take(req, other);
            let ordering = compare(req, &inf, &other);
            other.give_up(req);
            inf.give_up(req);
            let ordering = ordering?;
            if ordering != std::cmp::Ordering::Equal {
                return Ok(Some(ordering));
            }
        }
        Ok(Some(std::cmp::Ordering::Equal))
    }
}

const SOS_OVERRIDDEN_READ: i64 = 1;
const SOS_OVERRIDDEN_WRITE: i64 = 2;
const SOS_OVERRIDDEN_UNSET: i64 = 4;

/// `intern->flags` as the dimension handlers test it: for a subclass of
/// `SplObjectStorage` the `SOS_OVERRIDDEN_*` bits
/// `spl_object_storage_new_ex` sets for a `getHash`, `offsetGet`,
/// `offsetExists`, `offsetSet` or `offsetUnset` of its own; for a
/// `MultipleIterator`, which shares the field, its `MIT_*` flags, so
/// `MIT_NEED_ALL` reads as the read bit and `MIT_KEYS_ASSOC` as the
/// write bit, as they do in php.
///
/// ```php
/// $any = new MultipleIterator(MultipleIterator::MIT_NEED_ANY);
/// $any[new ArrayIterator([1])] = 'a';
/// var_dump($any->countIterators());
/// $assoc = new MultipleIterator(MultipleIterator::MIT_NEED_ANY | MultipleIterator::MIT_KEYS_ASSOC);
/// try { $assoc[new ArrayIterator([1])] = 'a'; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $any[new ArrayIterator([1])]; } catch (Exception $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// $all = new MultipleIterator();
/// try { $all[new ArrayIterator([1])]; } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1)
/// Cannot use object of type MultipleIterator as array
/// UnexpectedValueException: Object not found
/// Error: Cannot use object of type MultipleIterator as array
/// ```
fn overridden_bits(req: &mut Request, object: PhpObject) -> Result<i64, Thrown> {
    if crate::is_a(req, object.class(), b"MultipleIterator") {
        return with(req, object, |s| s.flags);
    }
    let hash = crate::overridden(req, object, b"gethash");
    let mut bits = 0;
    if hash
        || crate::overridden(req, object, b"offsetget")
        || crate::overridden(req, object, b"offsetexists")
    {
        bits |= SOS_OVERRIDDEN_READ;
    }
    if hash || crate::overridden(req, object, b"offsetset") {
        bits |= SOS_OVERRIDDEN_WRITE;
    }
    if hash || crate::overridden(req, object, b"offsetunset") {
        bits |= SOS_OVERRIDDEN_UNSET;
    }
    Ok(bits)
}

/// `spl_object_storage_clone`: `spl_object_storage_new_ex(ce, orig)`, a
/// fresh object of the class whose rows are the original's attached
/// through [`add_all`], each keyed by the copy's own `getHash` while the
/// copy's members are its class's defaults, then the members copied and
/// `__clone` served (`zend_objects_clone_members`). `MultipleIterator`'s
/// flags are the constructor's and a copy starts without them, as php's
/// does.
///
/// ```php
/// class H extends SplObjectStorage {
///     public $tag = 'default';
///     public $b;
///     public function getHash($o): string { global $orig; echo "getHash {$o->n} {$this->tag}\n"; if ($o->n == 'a' && $orig->b) { $orig->offsetUnset($orig->b); } return spl_object_hash($o); }
///     public function __clone() { echo "__clone ", count($this), " {$this->tag}\n"; }
/// }
/// $orig = new H; $orig->tag = 'orig';
/// $a = new stdClass; $a->n = 'a'; $b = new stdClass; $b->n = 'b';
/// $orig[$a] = 1; $orig[$b] = 2;
/// echo "clone\n"; $k = clone $orig;
/// $orig->b = $b;
/// echo "clone\n"; $j = clone $orig;
/// $m = new MultipleIterator(MultipleIterator::MIT_KEYS_ASSOC | MultipleIterator::MIT_NEED_ANY);
/// var_dump(count($orig), count($j), (clone $m)->getFlags());
/// ```
/// ```expect
/// getHash a orig
/// getHash b orig
/// clone
/// getHash a default
/// getHash b default
/// __clone 2 orig
/// clone
/// getHash a default
/// getHash b orig
/// __clone 1 orig
/// int(1)
/// int(1)
/// int(0)
/// ```
fn clone_storage(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    let copy = new_object(req, object.class())?;
    let filled = add_all(req, *copy, object).and_then(|()| clone_members(req, &copy, &object));
    if let Err(thrown) = filled {
        copy.give_up(req);
        return Err(thrown);
    }
    // The `clone_obj` slot's return, the one owner the copy was made with.
    Ok(copy)
}

/// `SplObjectStorage::serialize`'s walk over the storage, live by a
/// position of its own as `zend_hash_move_forward_ex` walks it, so a row
/// an earlier row's `__serialize` detached is not written and one it
/// attached is, and where a row's `__serialize` detached that row the
/// step passes the row after it too. Each row's object and information
/// are held across its write, php's `obj` and `inf` copies, and given
/// back after the step as php gives them back; the var hash keeps an
/// owner of each object it records until the serializer is done, so one
/// a later `__serialize` frees cannot have its place reused meanwhile.
fn serialize_rows(
    req: &mut Request,
    this: PhpObject,
    serializer: &mut Serializer,
    out: &mut Vec<u8>,
) -> Result<(), Thrown> {
    let mut pos = 0;
    while let Some((_, object, inf)) = with(req, this, |s| s.row_at(pos))? {
        let object = Owner::take(req, object.as_value());
        let inf = Owner::take(req, inf);
        let written = serializer.write(req, out, &object).and_then(|()| {
            out.push(b',');
            serializer.write(req, out, &inf)
        });
        let next = with(req, this, |s| s.forward_from(pos));
        object.give_up(req);
        inf.give_up(req);
        written?;
        out.push(b';');
        pos = next?;
    }
    Ok(())
}

/// The body of `SplObjectStorage::unserialize` over one reader: the
/// members once the rows are read, or the offset php's `outexcept`
/// reports, a read or a `getHash()` that threw going there too. The
/// count and the members are the var hash's, php's `var_tmp_var`; each
/// row's object and information are read as php reads them into locals
/// and `var_replace`s into the row, the storage's to keep.
fn read_storage(
    req: &mut Request,
    this: PhpObject,
    reader: &mut php_ext_standard::Unserializer<'_>,
    data: &[u8],
    replaced: &mut Vec<Owner<Value>>,
) -> Result<Record, Thrown> {
    if !data.starts_with(b"x:") {
        return Ok(Record::Broke(0));
    }
    reader.set_offset(2);
    let Some(count) = reader.value(req)? else {
        return Ok(Record::Broke(reader.offset()));
    };
    // `--p` back onto the `;` the count's own value ended with.
    reader.set_offset(reader.offset().saturating_sub(1));
    let Some(count) = count.as_int().filter(|n| *n >= 0) else {
        return Ok(Record::Broke(reader.offset()));
    };
    for _ in 0..count {
        if data.get(reader.offset()) != Some(&b';') {
            return Ok(Record::Broke(reader.offset()));
        }
        reader.set_offset(reader.offset() + 1);
        if !matches!(data.get(reader.offset()), Some(b'O' | b'C' | b'r')) {
            return Ok(Record::Broke(reader.offset()));
        }
        let Some(entry) = reader.local(req)? else {
            return Ok(Record::Broke(reader.offset()));
        };
        let inf = if data.get(reader.offset()) == Some(&b',') {
            reader.set_offset(reader.offset() + 1);
            match reader.local(req) {
                Ok(Some(read)) => read,
                Ok(None) => {
                    entry.give_up(req);
                    return Ok(Record::Broke(reader.offset()));
                }
                Err(thrown) => {
                    entry.give_up(req);
                    return Err(thrown);
                }
            }
        } else {
            Owner::null()
        };
        let Some(target) = entry.as_object() else {
            entry.give_up(req);
            inf.give_up(req);
            return Ok(Record::Broke(reader.offset()));
        };
        // A row read again keeps its information and its object until
        // the var hash is destroyed (`var_push_dtor`), so the attach that
        // replaces the information frees nothing meanwhile.
        let key = match row_key(req, this, target) {
            Ok(key) => key,
            Err(thrown) => {
                entry.give_up(req);
                inf.give_up(req);
                return Err(thrown);
            }
        };
        let found = with(req, this, |s| {
            s.places
                .get(&key)
                .and_then(|at| s.rows[*at].as_ref().map(|row| (*row.1, *row.2)))
        })?;
        if let Some((object, old)) = found {
            replaced.push(Owner::take(req, old));
            replaced.push(Owner::take(req, object.as_value()));
        }
        let attached = storage_attach(req, this, target, *inf);
        entry.give_up(req);
        inf.give_up(req);
        attached?;
    }
    if data.get(reader.offset()) != Some(&b';') {
        return Ok(Record::Broke(reader.offset()));
    }
    let at = reader.offset() + 1;
    if data.get(at) != Some(&b'm') {
        return Ok(Record::Broke(at));
    }
    if data.get(at + 1) != Some(&b':') {
        return Ok(Record::Broke(at + 1));
    }
    reader.set_offset(at + 2);
    let Some(members) = reader.value(req)? else {
        return Ok(Record::Broke(reader.offset()));
    };
    if members.as_array().is_none() {
        return Ok(Record::Broke(reader.offset()));
    }
    Ok(Record::Read(Some(members)))
}

/// The sub-iterator at the storage's position, held across what is
/// served on it, and its information, a view: one step of php's walks
/// over `intern->pos`, which read the row at the position as it stands,
/// `zend_hash_get_current_data_ptr_ex`, and take `GC_ADDREF(it)`. The
/// position is the storage's own, so a `detachIterator` the call
/// reaches resets it and the walk goes on from the first row, as in
/// php.
fn held_here(
    req: &mut Request,
    this: PhpObject,
) -> Result<Option<(PhpObject, Owner<Value>, Value)>, Thrown> {
    let here = with(req, this, |s| s.here())?;
    Ok(here.map(|(iterator, inf)| (iterator, Owner::take(req, iterator.as_value()), inf)))
}

/// Each sub-iterator served `method`, from the first row by the
/// storage's position, until one throws.
fn each(req: &mut Request, this: PhpObject, method: &[u8]) -> Result<(), Thrown> {
    with(req, this, |s| s.reset())?;
    while let Some((iterator, held, _)) = held_here(req, this)? {
        let served = crate::effect(req, iterator, method);
        held.give_up(req);
        served?;
        with(req, this, |s| s.forward())?;
    }
    Ok(())
}

/// `spl_multiple_iterator_get_all`: each sub-iterator's current or key,
/// keyed by its information under `MultipleIterator::MIT_KEYS_ASSOC`,
/// the flags read after each row's calls as php reads them.
fn get_all(req: &mut Request, this: PhpObject, method: &str) -> Result<Owner<PhpArray>, Thrown> {
    if with(req, this, |s| s.len())? == 0 {
        return Err(throw_exception_ex(
            req,
            "RuntimeException",
            &format!("Called {method}() on an invalid iterator"),
        ));
    }
    let mut out = PhpArray::new(req)?;
    let walked = get_all_rows(req, this, method, &mut out);
    if let Err(thrown) = walked {
        out.give_up(req);
        return Err(thrown);
    }
    Ok(out)
}

/// `spl_multiple_iterator_get_all`'s walk onto `out`, each row's
/// sub-iterator and a copy of its information held across its calls,
/// given back as `OBJ_RELEASE(it)` and then `zval_ptr_dtor(&inf)`.
fn get_all_rows(
    req: &mut Request,
    this: PhpObject,
    method: &str,
    out: &mut Owner<PhpArray>,
) -> Result<(), Thrown> {
    with(req, this, |s| s.reset())?;
    while let Some((iterator, held, inf)) = held_here(req, this)? {
        let inf = Owner::take(req, inf);
        let row = get_one(req, this, iterator, *inf, method, out);
        held.give_up(req);
        inf.give_up(req);
        row?;
        with(req, this, |s| s.forward())?;
    }
    Ok(())
}

/// One row of `spl_multiple_iterator_get_all` onto `out`.
fn get_one(
    req: &mut Request,
    this: PhpObject,
    iterator: PhpObject,
    inf: Value,
    method: &str,
    out: &mut Owner<PhpArray>,
) -> Result<(), Thrown> {
    // A `valid()` that throws is read as not valid, with what it threw
    // pending: the walk stops after this row, and a `MIT_NEED_ALL`
    // refusal chains it.
    let (valid, pending) = match crate::method(req, iterator, b"valid") {
        Ok(answered) => {
            let valid = answered.as_bool() == Some(true);
            answered.give_up(req);
            (valid, None)
        }
        Err(thrown) => (false, Some(thrown)),
    };
    // Both refusals are `zend_throw_exception` over a fixed string, one
    // per method, which formats nothing.
    let value = if valid {
        crate::method(req, iterator, method.as_bytes()).map_err(|thrown| {
            php_ext::raising::throw_chained(
                req,
                thrown,
                "RuntimeException",
                "Failed to call sub iterator method",
            )
        })?
    } else if with(req, this, |s| s.flags)? & MultipleIterator::MIT_NEED_ALL != 0 {
        let message = format!("Called {method}() with non valid sub iterator");
        return Err(match pending {
            Some(thrown) => {
                php_ext::raising::throw_chained(req, thrown, "RuntimeException", &message)
            }
            None => throw(req, "RuntimeException", &message),
        });
    } else if let Some(thrown) = pending {
        return Err(thrown);
    } else {
        Owner::null()
    };
    if with(req, this, |s| s.flags)? & MultipleIterator::MIT_KEYS_ASSOC == 0 {
        return out.append(req, value);
    }
    let key = match inf.as_ref() {
        ValueRef::Int(n) => Owner::index(n),
        ValueRef::Str(s) => Owner::take(req, Key::lookup(s)),
        _ => {
            value.give_up(req);
            return Err(throw(
                req,
                "InvalidArgumentException",
                "Sub-Iterator is associated with NULL",
            ));
        }
    };
    out.set(req, key, value)
}

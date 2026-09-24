//! `spl_observer.stub.php`: `SplObserver` and `SplSubject`, then
//! `SplObjectStorage` and `MultipleIterator` over php's
//! `spl_SplObjectStorage`, as `PHP_MINIT_FUNCTION(spl_observer)`
//! registers them. The storage is a table in insertion order keyed by
//! the object's id, or by what `getHash()` answers where a user
//! class overrides it, each row the object with one more owner and its
//! information; the iterator is a position in it and `intern->index`
//! beside it.

use std::collections::HashMap;

use php_ext::abi::{CloneObj, ObjectHandlers, PropertyPurpose, Std};
use php_ext::call::{call_method, clone_with};
use php_ext::coerce::{compare, identical, truthy, value_name};
use php_ext::raising::{Thrown, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::Entry;
use crate::stub::{self, Declared};

const MIT_NEED_ALL: i64 = 1;
const MIT_KEYS_ASSOC: i64 = 2;

/// A row's key: the object's id, or the string `getHash()` answered.
#[derive(Clone, PartialEq, Eq, Hash)]
enum RowKey {
    Id(u32),
    Hash(Vec<u8>),
}

/// `spl_SplObjectStorage`: the rows with their places, the position,
/// `intern->index`, and `MultipleIterator`'s flags.
#[derive(Default)]
pub(crate) struct Storage {
    rows: Vec<Option<(RowKey, PhpObject, Value)>>,
    places: HashMap<RowKey, usize>,
    pos: usize,
    index: i64,
    flags: i64,
}

impl Storage {
    fn len(&self) -> usize {
        self.places.len()
    }

    /// The live rows in order, views.
    fn live(&self) -> Vec<(PhpObject, Value)> {
        self.rows
            .iter()
            .flatten()
            .map(|(_, object, inf)| (*object, *inf))
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
        self.rows
            .get(self.valid_from(self.pos))
            .and_then(|row| row.as_ref())
            .map(|(_, object, inf)| (*object, *inf))
    }

    fn forward(&mut self) {
        let at = self.valid_from(self.pos);
        self.pos = self.valid_from(at + 1);
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
    fn attach(&mut self, key: RowKey, object: PhpObject, inf: Value) -> Result<Value, PhpObject> {
        if let Some(at) = self.places.get(&key)
            && let Some(Some(row)) = self.rows.get_mut(*at)
        {
            return Ok(std::mem::replace(&mut row.2, inf));
        }
        self.places.insert(key.clone(), self.rows.len());
        self.rows.push(Some((key, object, inf)));
        Err(object)
    }

    fn detach(&mut self, key: &RowKey) -> Option<(PhpObject, Value)> {
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

const STUB: &str = "
interface SplObserver {
    /** @tentative-return-type */ public function update(SplSubject $subject): void;
}
interface SplSubject {
    /** @tentative-return-type */ public function attach(SplObserver $observer): void;
    /** @tentative-return-type */ public function detach(SplObserver $observer): void;
    /** @tentative-return-type */ public function notify(): void;
}
class SplObjectStorage implements Countable, SeekableIterator, Serializable, ArrayAccess {
    /** @tentative-return-type */ #[\\Deprecated(since: '8.5', message: \"use method SplObjectStorage::offsetSet() instead\")]
    public function attach(object $object, mixed $info = null): void {}
    /** @tentative-return-type */ #[\\Deprecated(since: '8.5', message: \"use method SplObjectStorage::offsetUnset() instead\")]
    public function detach(object $object): void {}
    /** @tentative-return-type */ #[\\Deprecated(since: '8.5', message: \"use method SplObjectStorage::offsetExists() instead\")]
    public function contains(object $object): bool {}
    /** @tentative-return-type */ public function addAll(SplObjectStorage $storage): int {}
    /** @tentative-return-type */ public function removeAll(SplObjectStorage $storage): int {}
    /** @tentative-return-type */ public function removeAllExcept(SplObjectStorage $storage): int {}
    /** @tentative-return-type */ public function getInfo(): mixed {}
    /** @tentative-return-type */ public function setInfo(mixed $info): void {}
    /** @tentative-return-type */ public function count(int $mode = COUNT_NORMAL): int {}
    /** @tentative-return-type */ public function rewind(): void {}
    /** @tentative-return-type */ public function valid(): bool {}
    /** @tentative-return-type */ public function key(): int {}
    /** @tentative-return-type */ public function current(): object {}
    /** @tentative-return-type */ public function next(): void {}
    public function seek(int $offset): void {}
    /** @tentative-return-type */ public function unserialize(string $data): void {}
    /** @tentative-return-type */ public function serialize(): string {}
    /** @tentative-return-type */ public function offsetExists($object): bool {}
    /** @tentative-return-type */ public function offsetGet($object): mixed {}
    /** @tentative-return-type */ public function offsetSet($object, mixed $info = null): void {}
    /** @tentative-return-type */ public function offsetUnset($object): void {}
    /** @tentative-return-type */ public function getHash(object $object): string {}
    /** @tentative-return-type */ public function __serialize(): array {}
    /** @tentative-return-type */ public function __unserialize(array $data): void {}
    /** @tentative-return-type */ public function __debugInfo(): array {}
}
class MultipleIterator implements Iterator {
    public const int MIT_NEED_ANY = 0;
    public const int MIT_NEED_ALL = 1;
    public const int MIT_KEYS_NUMERIC = 0;
    public const int MIT_KEYS_ASSOC = 2;
    public function __construct(int $flags = MultipleIterator::MIT_NEED_ALL|MultipleIterator::MIT_KEYS_NUMERIC) {}
    /** @tentative-return-type */ public function getFlags(): int {}
    /** @tentative-return-type */ public function setFlags(int $flags): void {}
    /** @tentative-return-type */ public function attachIterator(Iterator $iterator, string|int|null $info = null): void {}
    /** @tentative-return-type */ public function detachIterator(Iterator $iterator): void {}
    /** @tentative-return-type */ public function containsIterator(Iterator $iterator): bool {}
    /** @tentative-return-type */ public function countIterators(): int {}
    /** @tentative-return-type */ public function rewind(): void {}
    /** @tentative-return-type */ public function valid(): bool {}
    /** @tentative-return-type */ public function key(): array {}
    /** @tentative-return-type */ public function current(): array {}
    /** @tentative-return-type */ public function next(): void {}
    /** @tentative-return-type */ public function __debugInfo(): array {}
}
";

/// The interfaces and classes, each method with its entry;
/// `offsetExists` is php's alias of `contains`.
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
pub(crate) fn classes() -> Vec<Declared> {
    let rows: [(&str, Entry); 38] = [
        ("SplObjectStorage::attach", deprecated_attach),
        ("SplObjectStorage::detach", deprecated_detach),
        ("SplObjectStorage::contains", deprecated_contains),
        ("SplObjectStorage::addAll", add_all),
        ("SplObjectStorage::removeAll", remove_all),
        ("SplObjectStorage::removeAllExcept", remove_all_except),
        ("SplObjectStorage::getInfo", get_info),
        ("SplObjectStorage::setInfo", set_info),
        ("SplObjectStorage::count", count),
        ("SplObjectStorage::rewind", rewind),
        ("SplObjectStorage::valid", valid),
        ("SplObjectStorage::key", key),
        ("SplObjectStorage::current", current),
        ("SplObjectStorage::next", next),
        ("SplObjectStorage::seek", seek),
        ("SplObjectStorage::unserialize", unserialize),
        ("SplObjectStorage::serialize", serialize),
        ("SplObjectStorage::offsetExists", contains),
        ("SplObjectStorage::offsetGet", offset_get),
        ("SplObjectStorage::offsetSet", attach),
        ("SplObjectStorage::offsetUnset", detach),
        ("SplObjectStorage::getHash", get_hash),
        ("SplObjectStorage::__serialize", magic_serialize),
        ("SplObjectStorage::__unserialize", magic_unserialize),
        ("SplObjectStorage::__debugInfo", debug_info),
        ("MultipleIterator::__construct", multiple_construct),
        ("MultipleIterator::getFlags", multiple_get_flags),
        ("MultipleIterator::setFlags", multiple_set_flags),
        ("MultipleIterator::attachIterator", attach_iterator),
        ("MultipleIterator::detachIterator", detach_iterator),
        ("MultipleIterator::containsIterator", contains_iterator),
        ("MultipleIterator::countIterators", count),
        ("MultipleIterator::rewind", multiple_rewind),
        ("MultipleIterator::valid", multiple_valid),
        ("MultipleIterator::key", multiple_key),
        ("MultipleIterator::current", multiple_current),
        ("MultipleIterator::next", multiple_next),
        ("MultipleIterator::__debugInfo", debug_info),
    ];
    stub::classes(STUB, &rows)
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
    if !crate::overridden(req, this, b"gethash") {
        return Ok(RowKey::Id(object.index()));
    }
    let answered = call_method(req, &this, b"getHash", &[object.as_value()])?;
    if let Some(bytes) = answered.as_str_bytes().map(<[u8]>::to_vec) {
        req.release(answered);
        return Ok(RowKey::Hash(bytes));
    }
    let class = String::from_utf8_lossy(&crate::class_name(req, this.class())).into_owned();
    let name = value_name(req, &answered);
    req.release(answered);
    Err(throw(
        req,
        "TypeError",
        &format!("{class}::getHash(): Return value must be of type string, {name} returned"),
    ))
}

/// `spl_object_storage_attach`: the information of a row replaced, or a
/// row appended holding one more owner of the object.
fn storage_attach(
    req: &mut Request,
    this: PhpObject,
    object: PhpObject,
    inf: Value,
) -> Result<(), Thrown> {
    let key = row_key(req, this, object)?;
    let inf = crate::copied(req, inf);
    match with(req, this, |s| s.attach(key, object, inf))? {
        Ok(old) => req.release(old),
        Err(object) => req.addref(&object.as_value()),
    }
    Ok(())
}

/// `spl_object_storage_detach`: `true` where a row went.
fn storage_detach(req: &mut Request, this: PhpObject, object: PhpObject) -> Result<bool, Thrown> {
    let key = row_key(req, this, object)?;
    match with(req, this, |s| s.detach(&key))? {
        Some((object, inf)) => {
            req.release(inf);
            req.release(object.as_value());
            Ok(true)
        }
        None => Ok(false),
    }
}

/// `spl_object_storage_contains`.
fn storage_contains(req: &mut Request, this: PhpObject, object: PhpObject) -> Result<bool, Thrown> {
    let key = row_key(req, this, object)?;
    with(req, this, |s| s.places.contains_key(&key))
}

impl ObjectHandlers for Handlers {
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let rows = object
            .native_mut::<Storage>(req)
            .map(|s| {
                s.places.clear();
                std::mem::take(&mut s.rows)
            })
            .unwrap_or_default();
        for (_, object, inf) in rows.into_iter().flatten() {
            req.release(inf);
            req.release(object.as_value());
        }
    }

    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        if let Some(s) = object.native::<Storage>(req) {
            for (object, inf) in s.live() {
                visit(object.as_value());
                visit(inf);
            }
        }
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_storage)
    }

    fn read_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
    ) -> Result<Value, Thrown> {
        let target = offset.and_then(|o| o.deref().as_object().copied());
        match target {
            Some(target) if !read_overridden(req, object) => {
                let found = with(req, object, |s| {
                    s.places
                        .get(&RowKey::Id(target.index()))
                        .and_then(|at| s.rows[*at].as_ref().map(|row| row.2))
                })?;
                match found {
                    Some(inf) => Ok(crate::copied(req, inf)),
                    None => Err(throw(req, "UnexpectedValueException", "Object not found")),
                }
            }
            _ => Std.read_dimension(req, object, offset),
        }
    }

    fn write_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        value: Value,
    ) -> Result<(), Thrown> {
        let target = offset.and_then(|o| o.deref().as_object().copied());
        match target {
            Some(target)
                if !crate::overridden(req, object, b"offsetset")
                    && !crate::overridden(req, object, b"gethash") =>
            {
                if crate::is_a(req, object.class(), b"MultipleIterator")
                    && !crate::is_a(req, target.class(), b"Iterator")
                {
                    req.release(value);
                    return Err(throw(
                        req,
                        "TypeError",
                        "Can only attach objects that implement the Iterator interface",
                    ));
                }
                let attached = storage_attach(req, object, target, value);
                req.release(value);
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
        let Some(target) = offset.deref().as_object().copied() else {
            return Std.has_dimension(req, object, offset, not_empty);
        };
        if read_overridden(req, object) {
            return Std.has_dimension(req, object, offset, not_empty);
        }
        let found = with(req, object, |s| {
            s.places
                .get(&RowKey::Id(target.index()))
                .and_then(|at| s.rows[*at].as_ref().map(|row| row.2))
        })?;
        match found {
            None => Ok(false),
            Some(inf) if not_empty => Ok(truthy(req, &inf)),
            Some(_) => Ok(true),
        }
    }

    fn unset_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: &Value,
    ) -> Result<(), Thrown> {
        match offset.deref().as_object().copied() {
            Some(target)
                if !crate::overridden(req, object, b"offsetunset")
                    && !crate::overridden(req, object, b"gethash") =>
            {
                let key = RowKey::Id(target.index());
                if let Some((object, inf)) = with(req, object, |s| s.detach(&key))? {
                    req.release(inf);
                    req.release(object.as_value());
                }
                Ok(())
            }
            _ => Std.unset_dimension(req, object, offset),
        }
    }

    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<std::cmp::Ordering>, Thrown> {
        let (Some(l), Some(r)) = (left.as_object().copied(), right.as_object().copied()) else {
            return Std.compare(req, left, right);
        };
        if l == r {
            return Ok(Some(std::cmp::Ordering::Equal));
        }
        let base = php_ext::call::lookup_class(req, b"SplObjectStorage")?;
        if Some(l.class()) != base || Some(r.class()) != base {
            return Ok(None);
        }
        let a = with(req, l, |s| {
            s.rows
                .iter()
                .flatten()
                .map(|(k, _, i)| (k.clone(), *i))
                .collect::<Vec<_>>()
        })?;
        let b = with(req, r, |s| s.places.clone())?;
        let b_rows = with(req, r, |s| {
            s.rows
                .iter()
                .map(|row| row.as_ref().map(|x| x.2))
                .collect::<Vec<_>>()
        })?;
        if a.len() != b.len() {
            return Ok(Some(a.len().cmp(&b.len())));
        }
        for (key, inf) in a {
            let Some(other) = b.get(&key).and_then(|at| b_rows[*at]) else {
                return Ok(Some(std::cmp::Ordering::Greater));
            };
            let ordering = compare(req, &inf, &other)?;
            if ordering != std::cmp::Ordering::Equal {
                return Ok(Some(ordering));
            }
        }
        Ok(Some(std::cmp::Ordering::Equal))
    }
}

/// `SOS_OVERRIDDEN_READ_DIMENSION`.
fn read_overridden(req: &mut Request, object: PhpObject) -> bool {
    crate::overridden(req, object, b"gethash")
        || crate::overridden(req, object, b"offsetget")
        || crate::overridden(req, object, b"offsetexists")
}

/// `spl_object_storage_clone`: every row attached anew to the copy.
fn clone_storage(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    let (rows, flags) = with(req, object, |s| {
        (
            s.rows.iter().flatten().cloned().collect::<Vec<_>>(),
            s.flags,
        )
    })?;
    let mut copy = Storage {
        flags,
        ..Storage::default()
    };
    for (key, row_object, inf) in rows {
        req.addref(&row_object.as_value());
        req.addref(&inf);
        let _ = copy.attach(key, row_object, inf);
    }
    clone_with(req, &object, copy)
}

/// The three methods 8.5 deprecates, each raising php's deprecation
/// before it does what its replacement does.
fn deprecated_method(req: &mut Request, method: &str, instead: &str) {
    php_ext::raising::deprecated(
        req,
        &format!(
            "Method SplObjectStorage::{method}() is deprecated since 8.5, use method SplObjectStorage::{instead}() instead"
        ),
    );
}

/// `attach()`, deprecated for `offsetSet()`; the witness is `offsetSet`'s.
fn deprecated_attach(req: &mut Request) -> Result<Value, Thrown> {
    deprecated_method(req, "attach", "offsetSet");
    attach(req)
}

/// `detach()`, deprecated for `offsetUnset()`; the witness is
/// `offsetUnset`'s.
fn deprecated_detach(req: &mut Request) -> Result<Value, Thrown> {
    deprecated_method(req, "detach", "offsetUnset");
    detach(req)
}

/// `contains()`, deprecated for `offsetExists()`; the witness is
/// `offsetSet`'s.
fn deprecated_contains(req: &mut Request) -> Result<Value, Thrown> {
    deprecated_method(req, "contains", "offsetExists");
    contains(req)
}

/// `attach(object $object, mixed $info = null): void`, deprecated in
/// 8.5 for `offsetSet`, which is the same entry.
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
fn attach(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 2)?;
    let this = crate::this(req)?;
    let object = crate::object(req, 1, None)?;
    let inf = crate::given(req, 2).map_or_else(Value::null, |v| v.deref());
    storage_attach(req, this, object, inf)?;
    Ok(Value::null())
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
fn detach(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let object = crate::object(req, 1, None)?;
    storage_detach(req, this, object)?;
    with(req, this, |s| {
        s.reset();
        s.index = 0;
    })?;
    Ok(Value::null())
}

/// `contains(object $object): bool`, `offsetExists`.
fn contains(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let object = crate::object(req, 1, None)?;
    Ok(Value::bool(storage_contains(req, this, object)?))
}

/// The storage argument at 1, an `SplObjectStorage`.
fn other(req: &mut Request) -> Result<PhpObject, Thrown> {
    crate::arity(req, 1, 1)?;
    crate::object(req, 1, Some("SplObjectStorage"))
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
fn add_all(req: &mut Request) -> Result<Value, Thrown> {
    let this = crate::this(req)?;
    let other = other(req)?;
    let rows = with(req, other, |s| s.live())?;
    for (object, inf) in rows {
        storage_attach(req, this, object, inf)?;
    }
    let n = with(req, this, |s| {
        s.index = 0;
        s.len()
    })?;
    Ok(Value::int(i64::try_from(n).unwrap_or(i64::MAX)))
}

/// `removeAll(SplObjectStorage $storage): int`; the witness is `addAll`'s.
fn remove_all(req: &mut Request) -> Result<Value, Thrown> {
    let this = crate::this(req)?;
    let other = other(req)?;
    let rows = with(req, other, |s| s.live())?;
    for (object, _) in rows {
        storage_detach(req, this, object)?;
    }
    let n = with(req, this, |s| {
        s.reset();
        s.index = 0;
        s.len()
    })?;
    Ok(Value::int(i64::try_from(n).unwrap_or(i64::MAX)))
}

/// `removeAllExcept(SplObjectStorage $storage): int`; the witness is
/// `addAll`'s.
fn remove_all_except(req: &mut Request) -> Result<Value, Thrown> {
    let this = crate::this(req)?;
    let other = other(req)?;
    let rows = with(req, this, |s| s.live())?;
    for (object, _) in rows {
        req.addref(&object.as_value());
        let kept = storage_contains(req, other, object);
        let done = match kept {
            Ok(false) => storage_detach(req, this, object).map(|_| ()),
            Ok(true) => Ok(()),
            Err(thrown) => Err(thrown),
        };
        req.release(object.as_value());
        done?;
    }
    let n = with(req, this, |s| {
        s.reset();
        s.index = 0;
        s.len()
    })?;
    Ok(Value::int(i64::try_from(n).unwrap_or(i64::MAX)))
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
fn get_info(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let here = with(req, this, |s| s.here())?;
    Ok(here.map_or_else(Value::null, |(_, inf)| crate::copied(req, inf)))
}

/// `setInfo(mixed $info): void`; the witness is `getInfo`'s.
fn set_info(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let inf = crate::value(req, 1);
    let inf = crate::copied(req, inf);
    let old = with(req, this, |s| {
        let at = s.valid_from(s.pos);
        match s.rows.get_mut(at).and_then(Option::as_mut) {
            Some(row) => Ok(std::mem::replace(&mut row.2, inf)),
            None => Err(inf),
        }
    })?;
    match old {
        Ok(old) | Err(old) => req.release(old),
    }
    Ok(Value::null())
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
fn count(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 1)?;
    let this = crate::this(req)?;
    let n = with(req, this, |s| s.len())?;
    Ok(Value::int(i64::try_from(n).unwrap_or(i64::MAX)))
}

/// `rewind(): void`.
fn rewind(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    with(req, this, |s| {
        s.reset();
        s.index = 0;
    })?;
    Ok(Value::null())
}

/// `valid(): bool`.
fn valid(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    Ok(Value::bool(with(req, this, |s| s.here().is_some())?))
}

/// `key(): int`, `intern->index`.
fn key(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    Ok(Value::int(with(req, this, |s| s.index)?))
}

/// `current(): object`.
fn current(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    match with(req, this, |s| s.here())? {
        Some((object, _)) => Ok(crate::copied(req, object.as_value())),
        None => Err(throw(
            req,
            "RuntimeException",
            "Called current() on invalid iterator",
        )),
    }
}

/// `next(): void`.
fn next(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    with(req, this, |s| {
        s.forward();
        s.index += 1;
    })?;
    Ok(Value::null())
}

/// `seek(int $offset): void`.
fn seek(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let position = crate::long(req, 1)?;
    let len = with(req, this, |s| s.len())?;
    if position < 0 || usize::try_from(position).is_ok_and(|p| p >= len) {
        return Err(throw(
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
    })?;
    Ok(Value::null())
}

/// `serialize(): string`, php's `x:i:count;` then `obj,inf;` per row
/// and `m:` with the members.
///
/// The witness waits on the ext/standard native it calls (#712).
fn serialize(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let rows = with(req, this, |s| s.live())?;
    let mut out = b"x:".to_vec();
    out.extend(crate::serialized(
        req,
        Value::int(i64::try_from(rows.len()).unwrap_or(i64::MAX)),
    )?);
    for (object, inf) in rows {
        out.extend(crate::serialized(req, object.as_value())?);
        out.push(b',');
        out.extend(crate::serialized(req, inf)?);
        out.push(b';');
    }
    out.extend_from_slice(b"m:");
    let members = crate::members(req, this)?;
    let text = crate::serialized(req, Value::array(members));
    req.release(Value::array(members));
    out.extend(text?);
    crate::string_value(req, &out)
}

/// `unserialize(string $data): void`; the witness is `serialize`'s.
fn unserialize(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let data = crate::string(req, 1)?.as_bytes().to_vec();
    if data.is_empty() {
        return Ok(Value::null());
    }
    let refuse = |req: &mut Request, at: usize| {
        Err(throw(
            req,
            "UnexpectedValueException",
            &format!("Error at offset {at} of {} bytes", data.len()),
        ))
    };
    if !data.starts_with(b"x:") {
        return refuse(req, 0);
    }
    let Some(end) = crate::serialized_end(&data, 2) else {
        return refuse(req, 2);
    };
    let Some(count) = crate::unserialized(req, &data[2..end])?.and_then(|c| c.as_int()) else {
        return refuse(req, 2);
    };
    let mut at = end;
    for _ in 0..count.max(0) {
        let Some(object_end) = crate::serialized_end(&data, at) else {
            return refuse(req, at);
        };
        let Some(object) = crate::unserialized(req, &data[at..object_end])? else {
            return refuse(req, at);
        };
        at = object_end;
        let mut inf = Value::null();
        if data.get(at) == Some(&b',') {
            let Some(inf_end) = crate::serialized_end(&data, at + 1) else {
                return refuse(req, at);
            };
            inf = crate::unserialized(req, &data[at + 1..inf_end])?.unwrap_or_default();
            at = inf_end;
        }
        let Some(target) = object.as_object().copied() else {
            return refuse(req, at);
        };
        storage_attach(req, this, target, inf)?;
        req.release(inf);
        req.release(object);
        if data.get(at) != Some(&b';') {
            return refuse(req, at);
        }
        at += 1;
    }
    if !data[at..].starts_with(b"m:") {
        return refuse(req, at);
    }
    if let Some(members) = crate::unserialized(req, &data[at + 2..])? {
        crate::load_members(req, this, &members)?;
        req.release(members);
    }
    Ok(Value::null())
}

/// `offsetGet($object): mixed`: the information, or php's
/// `UnexpectedValueException` for an object the storage has not.
fn offset_get(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let object = crate::object(req, 1, None)?;
    let key = row_key(req, this, object)?;
    let found = with(req, this, |s| {
        s.places
            .get(&key)
            .and_then(|at| s.rows[*at].as_ref().map(|row| row.2))
    })?;
    match found {
        Some(inf) => Ok(crate::copied(req, inf)),
        None => Err(throw(req, "UnexpectedValueException", "Object not found")),
    }
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
fn get_hash(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let object = crate::object(req, 1, None)?;
    let hash = format!("{:016x}0000000000000000", object.index());
    crate::string_value(req, hash.as_bytes())
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
fn magic_serialize(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let rows = with(req, this, |s| s.live())?;
    let mut list = PhpArray::new(req)?;
    for (object, inf) in rows {
        list.append(req, crate::copied(req, object.as_value()))?;
        list.append(req, crate::copied(req, inf))?;
    }
    let mut out = PhpArray::new(req)?;
    out.append(req, Value::array(list))?;
    let members = crate::members(req, this)?;
    out.append(req, Value::array(members))?;
    Ok(Value::array(out))
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
fn magic_unserialize(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let data = crate::array(req, 1)?;
    let storage = data
        .get(&Key::Int(0))
        .and_then(|v| v.deref().as_array().copied());
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
    let values: Vec<Value> = storage.snapshot().into_iter().map(|(_, v)| v).collect();
    for pair in values.chunks(2) {
        let (Some(object), Some(inf)) = (pair.first(), pair.get(1)) else {
            break;
        };
        let Some(target) = object.as_object().copied() else {
            return Err(throw(req, "UnexpectedValueException", "Non-object key"));
        };
        storage_attach(req, this, target, *inf)?;
    }
    crate::load_members(req, this, &members)?;
    Ok(Value::null())
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
fn debug_info(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let rows = with(req, this, |s| s.live())?;
    let mut out = PhpArray::new(req)?;
    for (name, value) in this.props_for(req, PropertyPurpose::Debug)? {
        req.addref(&value);
        out.set(req, Key::Str(name), value)?;
    }
    let mut storage = PhpArray::new(req)?;
    for (object, inf) in rows {
        let mut row = PhpArray::new(req)?;
        let key = PhpString::new(req, b"obj")?;
        row.set(req, Key::Str(key), crate::copied(req, object.as_value()))?;
        let key = PhpString::new(req, b"inf")?;
        row.set(req, Key::Str(key), crate::copied(req, inf))?;
        storage.append(req, Value::array(row))?;
    }
    let name = PhpString::new(req, b"\0SplObjectStorage\0storage")?;
    out.set(req, Key::Str(name), Value::array(storage))?;
    Ok(Value::array(out))
}

/// `MultipleIterator::__construct(int $flags = MIT_NEED_ALL|MIT_KEYS_NUMERIC)`.
///
/// ```php
/// $m = new MultipleIterator(MultipleIterator::MIT_NEED_ANY | MultipleIterator::MIT_KEYS_ASSOC);
/// $m->attachIterator(new ArrayIterator([1, 2, 3]), 'n');
/// $m->attachIterator(new ArrayIterator(['a', 'b']), 'l');
/// foreach ($m as $k => $v) echo json_encode($k), json_encode($v), "\n";
/// var_dump($m->getFlags(), $m->countIterators());
/// $m->setFlags(MultipleIterator::MIT_NEED_ALL);
/// foreach ($m as $v) echo implode(',', $v), ";";
/// ```
/// ```expect
/// {"n":0,"l":0}{"n":1,"l":"a"}
/// {"n":1,"l":1}{"n":2,"l":"b"}
/// {"n":2,"l":null}{"n":3,"l":null}
/// int(2)
/// int(2)
/// 1,a;2,b;
/// ```
fn multiple_construct(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 1)?;
    let this = crate::this(req)?;
    let flags = crate::long_or(req, 1, MIT_NEED_ALL)?;
    with(req, this, |s| s.flags = flags)?;
    Ok(Value::null())
}

/// `getFlags(): int`.
fn multiple_get_flags(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    Ok(Value::int(with(req, this, |s| s.flags)?))
}

/// `setFlags(int $flags): void`.
fn multiple_set_flags(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let flags = crate::long(req, 1)?;
    with(req, this, |s| s.flags = flags)?;
    Ok(Value::null())
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
/// ```
/// ```expect
/// Key duplication error
/// Sub-Iterator is associated with NULL
/// bool(true)
/// bool(false)
/// ```
fn attach_iterator(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 2)?;
    let this = crate::this(req)?;
    let iterator = crate::object(req, 1, Some("Iterator"))?;
    let info = crate::given(req, 2).map(|v| v.deref());
    let info = match info {
        Some(v) if matches!(v.as_ref(), ValueRef::Null) => None,
        Some(v) if matches!(v.as_ref(), ValueRef::Int(_) | ValueRef::Str(_)) => Some(v),
        Some(v) => {
            let coerced = php_ext::coerce::coerce_param(
                req,
                &v,
                &php_ext::decl::Type::Union(vec![
                    php_ext::decl::Type::String,
                    php_ext::decl::Type::Int,
                    php_ext::decl::Type::Null,
                ]),
                2,
            )?;
            Some(coerced)
        }
        None => None,
    };
    if let Some(info) = info {
        let rows = with(req, this, |s| s.live())?;
        if rows.iter().any(|(_, inf)| identical(inf, &info)) {
            return Err(throw(
                req,
                "InvalidArgumentException",
                "Key duplication error",
            ));
        }
    }
    storage_attach(req, this, iterator, info.unwrap_or_default())?;
    Ok(Value::null())
}

/// `detachIterator(Iterator $iterator): void`; the witness is
/// `attachIterator`'s.
fn detach_iterator(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let iterator = crate::object(req, 1, Some("Iterator"))?;
    storage_detach(req, this, iterator)?;
    with(req, this, |s| {
        s.reset();
        s.index = 0;
    })?;
    Ok(Value::null())
}

/// `containsIterator(Iterator $iterator): bool`.
fn contains_iterator(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let iterator = crate::object(req, 1, Some("Iterator"))?;
    Ok(Value::bool(storage_contains(req, this, iterator)?))
}

/// Each sub-iterator served `method`, in order, until one throws.
fn each(req: &mut Request, this: PhpObject, method: &[u8]) -> Result<(), Thrown> {
    let rows = with(req, this, |s| s.live())?;
    for (iterator, _) in rows {
        crate::effect(req, iterator, method)?;
    }
    Ok(())
}

/// `MultipleIterator::rewind(): void`.
fn multiple_rewind(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    each(req, this, b"rewind")?;
    Ok(Value::null())
}

/// `MultipleIterator::next(): void`.
fn multiple_next(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    each(req, this, b"next")?;
    Ok(Value::null())
}

/// `MultipleIterator::valid(): bool`: all valid under `MIT_NEED_ALL`,
/// any under `MIT_NEED_ANY`, `false` with none attached.
fn multiple_valid(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let (rows, flags) = with(req, this, |s| (s.live(), s.flags))?;
    if rows.is_empty() {
        return Ok(Value::bool(false));
    }
    let expect = flags & MIT_NEED_ALL != 0;
    for (iterator, _) in rows {
        let answered = crate::method(req, iterator, b"valid")?;
        let valid = answered.as_bool() == Some(true);
        req.release(answered);
        if expect != valid {
            return Ok(Value::bool(!expect));
        }
    }
    Ok(Value::bool(expect))
}

/// `spl_multiple_iterator_get_all`: each sub-iterator's current or key,
/// keyed by its information under `MIT_KEYS_ASSOC`.
fn get_all(req: &mut Request, method: &str) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let (rows, flags) = with(req, this, |s| (s.live(), s.flags))?;
    if rows.is_empty() {
        return Err(throw(
            req,
            "RuntimeException",
            &format!("Called {method}() on an invalid iterator"),
        ));
    }
    let mut out = PhpArray::new(req)?;
    for (iterator, inf) in rows {
        let answered = crate::method(req, iterator, b"valid")?;
        let valid = answered.as_bool() == Some(true);
        req.release(answered);
        let value = if valid {
            crate::method(req, iterator, method.as_bytes())?
        } else if flags & MIT_NEED_ALL != 0 {
            req.release(Value::array(out));
            return Err(throw(
                req,
                "RuntimeException",
                &format!("Called {method}() with non valid sub iterator"),
            ));
        } else {
            Value::null()
        };
        if flags & MIT_KEYS_ASSOC != 0 {
            let key = match inf.as_ref() {
                ValueRef::Int(n) => Key::Int(n),
                ValueRef::Str(s) => Key::from_string(
                    crate::copied(req, Value::string(*s))
                        .as_php_string()
                        .copied()
                        .unwrap_or(*s),
                ),
                _ => {
                    req.release(value);
                    req.release(Value::array(out));
                    return Err(throw(
                        req,
                        "InvalidArgumentException",
                        "Sub-Iterator is associated with NULL",
                    ));
                }
            };
            out.set(req, key, value)?;
        } else {
            out.append(req, value)?;
        }
    }
    Ok(Value::array(out))
}

/// `MultipleIterator::current(): array`.
fn multiple_current(req: &mut Request) -> Result<Value, Thrown> {
    get_all(req, "current")
}

/// `MultipleIterator::key(): array`.
fn multiple_key(req: &mut Request) -> Result<Value, Thrown> {
    get_all(req, "key")
}

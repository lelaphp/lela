//! `spl_array.c`: `ArrayObject`, `ArrayIterator` and
//! `RecursiveArrayIterator` over one payload, php's `spl_array_object`
//! ([`SplArrayObject`]), and one handler table, `spl_handler_ArrayObject`
//! ([`Handlers`]).
//!
//! The storage is an array the object holds, another `ArrayObject` or
//! `ArrayIterator` it reads through (`SPL_ARRAY_USE_OTHER`, which is
//! what `getIterator()` hands out), or an object whose properties it
//! reads and writes. Each object keeps its own position in the table,
//! php's `ht_iter`, a hash iterator registered at the array it walks
//! ([`Position`]), so a write that moves the buckets moves it too. A
//! dimension read,
//! write, `isset` and `unset` goes to the class's `offsetGet`,
//! `offsetSet`, `offsetExists` and `offsetUnset` where a user class
//! overrides them (`fptr_offset_*`), and to the table otherwise; under
//! `ARRAY_AS_PROPS` a property the object does not have is an element.
//! The sort methods hand the table to `ext/standard`'s natives by
//! reference, as `spl_array_method` does, and `serialize` writes php's
//! `x:i:flags;storage;m:members` over `serialize()`.

use std::cmp::Ordering;

use php_ext::abi::{
    Callable, CloneObj, CurrentPlace, DimensionRead, ExtensionBuilder, GcTable, Instance,
    ObjectHandlers, PropertiesFor, PropertyPlace, PropertyPurpose, PropertyRead, PropertyTest, Raw,
    Std, This, True, Union,
};
use php_ext::attr::php_class;
use php_ext::call::{autoload, call, call_method, clone_with, instantiate, lookup_class};
use php_ext::coerce::{callable_checked, compare, to_long, truthy};
use php_ext::raising::{
    Level, Thrown, deprecated, illegal_container_offset, throw, throw_error, warning,
};
use php_ext::request::Request;
use php_ext::values::walk;
use php_ext::values::{
    ClassId, DeclaredRows, HashIterator, Key, Owner, PhpArray, PhpObject, PhpRef, PhpString, Place,
    Value, ValueRef,
};

use crate::{
    ArrayAccess, Countable, Iterator, IteratorAggregate, Record, RecursiveIterator,
    SeekableIterator, Serializable,
};

const IS_SELF: i64 = 0x0100_0000;
const USE_OTHER: i64 = 0x0200_0000;
const INT_MASK: i64 = 0xFFFF_0000;
const CLONE_MASK: i64 = 0x0100_FFFF;

/// php's `spl_array_object` beside the `std` object: the storage, the
/// flags a user sets, the position, the class `getIterator()` makes and
/// the count of sorts under way (`nApplyCount`).
#[derive(Default)]
pub(crate) struct SplArrayObject {
    storage: Held,
    /// `ar_flags` less `SPL_ARRAY_IS_SELF` and `SPL_ARRAY_USE_OTHER`,
    /// which the storage is.
    flags: i64,
    position: Position,
    iterator_class: Option<ClassId>,
    sorting: u32,
}

/// The payload as a handler reads it, copied out: the storage's views
/// beside the fields.
#[derive(Clone, Copy)]
struct Payload {
    storage: Storage,
    flags: i64,
    position: Position,
    iterator_class: Option<ClassId>,
    sorting: u32,
}

/// What `intern->array` is, with the two bits php reads it by
/// (`SPL_ARRAY_IS_SELF`, `SPL_ARRAY_USE_OTHER`) folded in, decoded
/// once: each handler matches it, and no bit is tested beside it.
/// The payload's owner of what it holds, given up where the storage is
/// replaced and as the object is freed.
#[derive(Default)]
enum Held {
    /// Nothing written yet: php's `array_init` in
    /// `spl_array_object_new_ex`, laid down as [`Held::Array`] on the
    /// first touch.
    #[default]
    Fresh,
    /// An array the object holds, shared copy-on-write with whoever else
    /// holds it.
    Array(Owner<PhpArray>),
    /// `SPL_ARRAY_USE_OTHER`: another `ArrayObject` or `ArrayIterator`
    /// read through; what `getIterator()` hands out.
    Other(Owner<PhpObject>),
    /// Another object's property table, held by the object.
    Object(Owner<PhpObject>),
    /// `SPL_ARRAY_IS_SELF`: this object's own property table, no owner
    /// taken.
    OwnProps,
}

/// [`Held`]'s views, what a read of the payload matches.
#[derive(Clone, Copy)]
enum Storage {
    Fresh,
    Array(PhpArray),
    Other(PhpObject),
    Object(PhpObject),
    OwnProps,
}

impl Held {
    /// The views of what the storage holds.
    fn seen(&self) -> Storage {
        match self {
            Held::Fresh => Storage::Fresh,
            Held::Array(array) => Storage::Array(**array),
            Held::Other(object) => Storage::Other(**object),
            Held::Object(object) => Storage::Object(**object),
            Held::OwnProps => Storage::OwnProps,
        }
    }

    /// The storage's owner given up, `zval_ptr_dtor(&intern->array)`.
    fn give_up(self, req: &mut Request) {
        match self {
            Held::Array(array) => array.give_up(req),
            Held::Other(object) | Held::Object(object) => object.give_up(req),
            Held::Fresh | Held::OwnProps => {}
        }
    }
}

/// Where the object's walk stands, php's `ht_iter`.
#[derive(Default, Clone, Copy)]
enum Position {
    /// `(uint32_t)-1`: no walk has read it yet, or the storage was
    /// replaced under it; the first read lays one down at the first
    /// element (`spl_array_create_ht_iter`).
    #[default]
    Unset,
    /// A hash iterator registered at the array the object reads, which
    /// the array's writes move.
    Hash(HashIterator),
    /// The row of a property table the walk stands at.
    Row(u32),
}

impl Storage {
    /// `ar_flags`' two storage bits, as `serialize` writes them.
    const fn bits(self) -> i64 {
        match self {
            Storage::OwnProps => IS_SELF,
            Storage::Other(_) => USE_OTHER,
            Storage::Fresh | Storage::Array(_) | Storage::Object(_) => 0,
        }
    }

    /// What the storage holds of the heap, as a value.
    fn held(self) -> Option<Value> {
        match self {
            Storage::Array(array) => Some(Value::array(array)),
            Storage::Other(object) | Storage::Object(object) => Some(object.as_value()),
            Storage::Fresh | Storage::OwnProps => None,
        }
    }
}

/// `spl_handler_ArrayObject`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// The three classes onto `builder`, in the order
/// `PHP_MINIT_FUNCTION(spl_array)` registers them.
pub(crate) fn register(builder: ExtensionBuilder) -> ExtensionBuilder {
    builder
        .class::<ArrayObject>()
        .class::<ArrayIterator>()
        .class::<RecursiveArrayIterator>()
}

/// `class ArrayObject implements IteratorAggregate, ArrayAccess,
/// Serializable, Countable`.
///
/// ```php
/// echo implode(', ', class_implements('ArrayObject')), "\n";
/// echo implode(', ', class_implements('RecursiveArrayIterator')), "\n";
/// $c = clone new ArrayObject([1, 2]);
/// var_dump($c == new ArrayObject([1, 2]), $c == new ArrayObject([2]), (array) new ArrayIterator(['k' => 'v']));
/// $o = new ArrayObject(['b' => 'x10', 'a' => 'X9', 'c' => 'x2']);
/// $o->ksort(); echo implode(',', array_keys($o->getArrayCopy())), "\n";
/// $o->uasort(fn($l, $r) => strcmp($l, $r)); echo implode(',', (array) $o), "\n";
/// $o->uksort(fn($l, $r) => strcmp($r, $l)); echo implode(',', array_keys((array) $o)), "\n";
/// $o->natsort(); echo implode(',', (array) $o), "\n";
/// $o->natcasesort(); echo implode(',', (array) $o), "\n";
/// echo $o->serialize(), " ", $o->getIteratorClass(), " ", count($o->__debugInfo()), "\n";
/// ```
/// ```expect
/// IteratorAggregate, Traversable, ArrayAccess, Serializable, Countable
/// Countable, Serializable, ArrayAccess, Iterator, Traversable, SeekableIterator, RecursiveIterator
/// bool(true)
/// bool(false)
/// array(1) {
///   ["k"]=>
///   string(1) "v"
/// }
/// a,b,c
/// X9,x10,x2
/// c,b,a
/// X9,x2,x10
/// x2,X9,x10
/// x:i:0;a:3:{s:1:"c";s:2:"x2";s:1:"a";s:2:"X9";s:1:"b";s:3:"x10";};m:a:0:{} ArrayIterator 1
/// ```
///
/// `spl_array_object_new_ex`: the object by `zend_object_alloc` and the
/// table `array_init` lays down, which `memory_get_usage()` counts (design
/// §5.13):
///
/// ```php
/// foreach ([false, true] as $shown) foreach (['ArrayObject', 'ArrayIterator', 'RecursiveArrayIterator'] as $c) {
///     $r = new ReflectionClass($c);
///     $m = memory_get_usage();
///     $o = $r->newInstanceWithoutConstructor();
///     $used = memory_get_usage() - $m;
///     $o = null;
///     if ($shown) echo $c, ' ', $used, "\n";
/// }
/// ```
/// ```expect
/// ArrayObject 216
/// ArrayIterator 216
/// RecursiveArrayIterator 216
/// ```
#[php_class(
    implements(IteratorAggregate, ArrayAccess, Serializable, Countable),
    size = 144, payload = SplArrayObject,
    handlers = HANDLERS
)]
pub(crate) struct ArrayObject;

#[php_class]
impl ArrayObject {
    const STD_PROP_LIST: i64 = 1;
    const ARRAY_AS_PROPS: i64 = 2;

    /// `ArrayObject::__construct(array|object $array = [], int $flags = 0,
    /// string $iteratorClass = ArrayIterator::class)`: nothing for a call
    /// with no argument, then php's `|AlC` in order.
    ///
    /// ```php
    /// $o = new ArrayObject(['b' => 2, 'a' => 1]);
    /// var_dump($o, count($o), $o['a'], isset($o['c']), $o->getIteratorClass());
    /// $o['c'] = 3; unset($o['b']); $o[] = 4;
    /// print_r($o->getArrayCopy());
    /// ```
    /// ```expect
    /// object(ArrayObject)#1 (1) {
    ///   ["storage":"ArrayObject":private]=>
    ///   array(2) {
    ///     ["b"]=>
    ///     int(2)
    ///     ["a"]=>
    ///     int(1)
    ///   }
    /// }
    /// int(2)
    /// int(1)
    /// bool(false)
    /// string(13) "ArrayIterator"
    /// Array
    /// (
    ///     [a] => 1
    ///     [c] => 3
    ///     [0] => 4
    /// )
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = [])] array: Raw<Union<PhpArray, PhpObject>>,
        #[php(default = 0)] flags: Raw<i64>,
        #[php(default = ArrayIterator::class)] iteratorClass: Raw<PhpString>,
    ) -> Result<(), Thrown> {
        let this = this.object();
        if req.frames().argument_count() == 0 {
            return Ok(());
        }
        let given = array_or_object(req, array.value(), 1)?;
        let flags = flags.hold(req)?;
        let name = iteratorClass.hold(req)?;
        let class = iterator_class_argument(req, name, 3)?;
        with(req, this, |p| p.iterator_class = Some(class))?;
        let just_array = req.frames().argument_count() == 1;
        set_array(req, this, &given, flags & !INT_MASK, just_array)
    }

    /// `offsetExists(mixed $key): bool`: whether the key is there, a `null`
    /// element included.
    ///
    /// ```php
    /// $o = new ArrayObject(['n' => null, 1 => 0]);
    /// var_dump($o->offsetExists('n'), isset($o[1]), empty($o[1]), $o->offsetExists('x'));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// ```
    #[php(tentative)]
    fn offsetExists(req: &mut Request, this: This<Self>, key: Value) -> Result<bool, Thrown> {
        has(req, this.object(), &key, 2, false)
    }

    /// `offsetGet(mixed $key): mixed`: the element, the warning for a key
    /// the table does not have.
    ///
    /// ```php
    /// $o = new ArrayObject([1 => 'one']);
    /// var_dump($o->offsetGet(1), $o->offsetGet('1'), $o['nope']);
    /// ```
    /// ```expect
    ///
    /// Warning: Undefined array key "nope" in %s on line 3
    /// string(3) "one"
    /// string(3) "one"
    /// NULL
    /// ```
    #[php(tentative)]
    fn offsetGet(req: &mut Request, this: This<Self>, key: Value) -> Result<Value, Thrown> {
        read(req, this.object(), &key, true)
    }

    /// `offsetSet(mixed $key, mixed $value): void`, `null` appending.
    ///
    /// ```php
    /// $o = new ArrayObject();
    /// $o->offsetSet('k', 'v'); $o->offsetSet(null, 'next');
    /// var_dump($o->getArrayCopy());
    /// try { $o[[]] = 1; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { $o[new stdClass] = 1; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// array(2) {
    ///   ["k"]=>
    ///   string(1) "v"
    ///   [0]=>
    ///   string(4) "next"
    /// }
    /// Cannot access offset of type array on ArrayObject
    /// Cannot access offset of type stdClass on ArrayObject
    /// ```
    ///
    /// A string key is a view of the offset until the table takes its own
    /// owner of it, so a key built for the call and freed after it stays
    /// in the table (bug #66834).
    ///
    /// ```php
    /// class R extends ArrayObject { function offsetSet($k, $v): void { parent::offsetSet(str_rot13($k), $v); } }
    /// $r = new R; $r['foo'] = 1; $r[str_repeat('b', 3)] = 2;
    /// var_dump($r->getArrayCopy());
    /// ```
    /// ```expect
    /// array(2) {
    ///   ["sbb"]=>
    ///   int(1)
    ///   ["ooo"]=>
    ///   int(2)
    /// }
    /// ```
    #[php(tentative)]
    fn offsetSet(
        req: &mut Request,
        this: This<Self>,
        key: Value,
        value: Value,
    ) -> Result<(), Thrown> {
        write(req, this.object(), Some(&key), value)
    }

    /// `offsetUnset(mixed $key): void`.
    ///
    /// ```php
    /// $o = new ArrayObject(['a' => 1, 'b' => 2]);
    /// $o->offsetUnset('a'); $o->offsetUnset('missing');
    /// var_dump(count($o));
    /// ```
    /// ```expect
    /// int(1)
    /// ```
    #[php(tentative)]
    fn offsetUnset(req: &mut Request, this: This<Self>, key: Value) -> Result<(), Thrown> {
        unset(req, this.object(), &key)
    }

    /// `append(mixed $value): void`, refused over an object's properties.
    ///
    /// ```php
    /// $o = new ArrayObject([5 => 'x']);
    /// $o->append('y');
    /// var_dump($o->getArrayCopy());
    /// ```
    /// ```expect
    /// array(2) {
    ///   [5]=>
    ///   string(1) "x"
    ///   [6]=>
    ///   string(1) "y"
    /// }
    /// ```
    #[php(tentative)]
    fn append(req: &mut Request, this: This<Self>, value: Value) -> Result<(), Thrown> {
        let this = this.object();
        if is_object(req, this)? {
            let class = String::from_utf8_lossy(&crate::class_name(req, this.class())).into_owned();
            return Err(throw_error(
                req,
                "Error",
                &format!("Cannot append properties to objects, use {class}::offsetSet() instead"),
            ));
        }
        let value = Owner::take(req, value.deref());
        Handlers.write_dimension(req, this, None, value)
    }

    /// `getArrayCopy(): array`: the table as an array of its own.
    ///
    /// ```php
    /// $a = [1, 2]; $o = new ArrayObject($a);
    /// $copy = $o->getArrayCopy(); $copy[] = 3;
    /// var_dump(count($o), count($copy));
    /// ```
    /// ```expect
    /// int(2)
    /// int(3)
    /// ```
    #[php(tentative)]
    fn getArrayCopy(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        table(req, this.object())?.copy(req)
    }

    /// `count(): int`, `spl_array_object_count_elements_helper`: the
    /// elements, or an object's public properties.
    ///
    /// ```php
    /// class Counted extends ArrayObject { function count(): int { return 42; } }
    /// $c = new Counted([1, 2]);
    /// var_dump($c->count(), count($c), (new ArrayObject([1, 2, 3]))->count());
    /// class Held { public $a = 1; protected $b = 2; private $c = 3; public $d = 4; }
    /// $h = @new ArrayObject(new Held);
    /// var_dump(count($h));
    /// foreach ($h as $k => $v) echo "$k=$v;";
    /// ```
    /// ```expect
    /// int(42)
    /// int(42)
    /// int(3)
    /// int(2)
    /// a=1;d=4;
    /// ```
    ///
    /// After a sort has laid the object's table down as copies of its
    /// declared rows, a protected one is a plain value to the count,
    /// which counts it, and iteration still leaves it out.
    ///
    /// ```php
    /// #[AllowDynamicProperties]
    /// class C { public $b = 'q'; protected $a = 'z'; }
    /// $c = new C; $c->{'5'} = 'm';
    /// $ao = @new ArrayObject($c);
    /// var_dump(count($ao), $ao->count());
    /// $ao->asort();
    /// var_dump(count($ao), $ao->count());
    /// foreach ($ao as $k => $v) echo "$k=$v;";
    /// ```
    /// ```expect
    /// int(2)
    /// int(2)
    /// int(3)
    /// int(3)
    /// 5=m;b=q;
    /// ```
    #[php(tentative)]
    fn count(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let n = table(req, this.object())?.len(req)?;
        Ok(i64::try_from(n).unwrap_or(i64::MAX))
    }

    /// `getFlags(): int`.
    ///
    /// ```php
    /// $o = new ArrayObject([], ArrayObject::ARRAY_AS_PROPS);
    /// var_dump($o->getFlags());
    /// $o->setFlags(ArrayObject::STD_PROP_LIST);
    /// var_dump($o->getFlags());
    /// ```
    /// ```expect
    /// int(2)
    /// int(1)
    /// ```
    #[php(tentative)]
    fn getFlags(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        Ok(flags(req, this.object())? & !INT_MASK)
    }

    /// `setFlags(int $flags): void`. Under `ARRAY_AS_PROPS` a property the
    /// object does not declare is an element.
    ///
    /// ```php
    /// $o = new ArrayObject(['a' => 1]);
    /// $o->setFlags(ArrayObject::ARRAY_AS_PROPS);
    /// $o->b = 2;
    /// var_dump($o->a, isset($o->b), $o->getArrayCopy());
    /// ```
    /// ```expect
    /// int(1)
    /// bool(true)
    /// array(2) {
    ///   ["a"]=>
    ///   int(1)
    ///   ["b"]=>
    ///   int(2)
    /// }
    /// ```
    #[php(tentative)]
    fn setFlags(req: &mut Request, this: This<Self>, flags: i64) -> Result<(), Thrown> {
        with(req, this.object(), |p| {
            p.flags = (p.flags & INT_MASK) | (flags & !INT_MASK);
        })
    }

    /// `asort(int $flags = SORT_REGULAR): true`, `ext/standard`'s `asort`
    /// over the table: the storage array, or an object's rows, laid down
    /// again in the sorted order.
    ///
    /// ```php
    /// $o = new ArrayObject(['b' => 2, 'a' => 3, 'c' => 1]);
    /// $o->asort();
    /// $d = (object) ['y' => 'b', 'x' => 'a'];
    /// $p = @new ArrayObject($d);
    /// $p->asort(); $p->ksort();
    /// var_dump($o->getArrayCopy(), $d);
    /// ```
    /// ```expect
    /// array(3) {
    ///   ["c"]=>
    ///   int(1)
    ///   ["b"]=>
    ///   int(2)
    ///   ["a"]=>
    ///   int(3)
    /// }
    /// object(stdClass)#2 (2) {
    ///   ["x"]=>
    ///   string(1) "a"
    ///   ["y"]=>
    ///   string(1) "b"
    /// }
    /// ```
    #[php(tentative)]
    fn asort(
        req: &mut Request,
        this: This<Self>,
        #[php(default = SORT_REGULAR)] flags: i64,
    ) -> Result<True, Thrown> {
        sort_through(req, this.object(), b"asort", Some(Value::int(flags)))
    }

    /// `ksort(int $flags = SORT_REGULAR): true`; the witness is the
    /// class's.
    #[php(tentative)]
    fn ksort(
        req: &mut Request,
        this: This<Self>,
        #[php(default = SORT_REGULAR)] flags: i64,
    ) -> Result<True, Thrown> {
        sort_through(req, this.object(), b"ksort", Some(Value::int(flags)))
    }

    /// `uasort(callable $callback): true`, the callable handed on as it
    /// was given, `ext/standard`'s `uasort` its check; the witness is the
    /// class's.
    #[php(tentative)]
    fn uasort(
        req: &mut Request,
        this: This<Self>,
        callback: Raw<Callable>,
    ) -> Result<True, Thrown> {
        sort_through(req, this.object(), b"uasort", Some(callback.value()))
    }

    /// `uksort(callable $callback): true`; the witness is the class's.
    #[php(tentative)]
    fn uksort(
        req: &mut Request,
        this: This<Self>,
        callback: Raw<Callable>,
    ) -> Result<True, Thrown> {
        sort_through(req, this.object(), b"uksort", Some(callback.value()))
    }

    /// `natsort(): true`; the witness is the class's.
    #[php(tentative)]
    fn natsort(req: &mut Request, this: This<Self>) -> Result<True, Thrown> {
        sort_through(req, this.object(), b"natsort", None)
    }

    /// `natcasesort(): true`; the witness is the class's.
    #[php(tentative)]
    fn natcasesort(req: &mut Request, this: This<Self>) -> Result<True, Thrown> {
        sort_through(req, this.object(), b"natcasesort", None)
    }

    /// `unserialize(string $data): void`, the inverse of `serialize`, read
    /// through one var hash as php's is: the `x:i:flags;` head, the storage
    /// and its `;`, then `m:` with the members. Where the record breaks off
    /// php's `UnexpectedValueException` names the offset the reader stopped
    /// at; a storage `spl_array_set_array` refuses leaves what it threw
    /// pending while the members are still read, as php's does.
    ///
    /// ```php
    /// $o = unserialize('C:11:"ArrayObject":33:{x:i:2;a:1:{s:1:"k";i:7;};m:a:0:{}}');
    /// var_dump($o->k, $o->getFlags());
    /// foreach (['x:i:0;r:3;;m:a:0:{}', 'y:i:0;', 'x:i:0;a:0:{}m:a:0:{}'] as $bad) {
    ///     try { (new ArrayObject)->unserialize($bad); } catch (UnexpectedValueException $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// int(7)
    /// int(2)
    /// Error at offset 10 of 19 bytes
    /// Error at offset 0 of 6 bytes
    /// Error at offset 12 of 20 bytes
    /// ```
    ///
    /// A read that throws breaks the record too, the throwable chained
    /// under the `UnexpectedValueException` at the offset the reader
    /// stopped at.
    ///
    /// ```php
    /// class T { public int $p; }
    /// spl_autoload_register(function ($c) { throw new Exception("auto $c"); });
    /// $p = function ($e) { echo get_class($e), ": ", $e->getMessage(), " <- ", get_class($e->getPrevious()), ": ", $e->getPrevious()->getMessage(), "\n"; };
    /// try { (new ArrayObject)->unserialize('x:i:0;a:1:{i:0;O:1:"T":1:{s:1:"p";s:1:"x";}};m:a:0:{}'); } catch (Throwable $e) { $p($e); }
    /// try { (new ArrayObject)->unserialize('x:i:0;O:1:"Q":0:{};m:a:0:{}'); } catch (Throwable $e) { $p($e); }
    /// ```
    /// ```expect
    /// UnexpectedValueException: Error at offset 42 of 53 bytes <- TypeError: Cannot assign string to property T::$p of type int
    /// UnexpectedValueException: Error at offset 6 of 27 bytes <- Exception: auto Q
    /// ```
    #[php(tentative)]
    fn unserialize(req: &mut Request, this: This<Self>, data: PhpString) -> Result<(), Thrown> {
        let this = this.object();
        let data = data.as_bytes().to_vec();
        if data.is_empty() {
            return Ok(());
        }
        sorting_refused(req, this)?;
        let mut reader = php_ext_standard::Unserializer::new(req, &data)?;
        let mut pending = None;
        let read = read_record(req, this, &mut reader, &data, &mut pending);
        let (read, broke_at) = crate::record_read(req, this, &reader, read);
        // What `spl_array_set_array` threw stands over the rest of the
        // read and the var hash's destruction, as `EG(exception)` does, so
        // no delayed call is made under it.
        let read = match (read, pending) {
            (read, None) => read,
            (Ok(()), Some(stands)) => Err(stands),
            (Err(thrown), Some(stands)) => Err(stands.chain(req, thrown)),
        };
        let finished = reader.finish(req, read);
        crate::record_end(req, finished, broke_at, data.len())
    }

    /// `serialize(): string`, php's `x:i:flags;storage;m:members`; the
    /// witness is the class's.
    #[php(tentative)]
    fn serialize(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let this = this.object();
        let flags = ar_flags(req, this)? & CLONE_MASK;
        crate::serialized_parts(req, |req, serializer, out| {
            out.extend_from_slice(b"x:");
            serializer.write(req, out, &Value::int(flags))?;
            if flags & IS_SELF == 0 {
                let stored = storage(req, this)?;
                let written = serializer.write(req, out, &stored);
                stored.give_up(req);
                written?;
                out.push(b';');
            }
            out.extend_from_slice(b"m:");
            let members = crate::members(req, this)?;
            let written = serializer.write(req, out, &Value::array(*members));
            members.give_up(req);
            written
        })
    }

    /// `__serialize(): array`: the flags, the storage, the members and the
    /// iterator class, `null` for `ArrayIterator`.
    ///
    /// ```php
    /// class Bag extends ArrayObject { public $label = 'bag'; }
    /// $b = new Bag(['x' => 1], 0, 'RecursiveArrayIterator');
    /// var_dump($b->__serialize());
    /// ```
    /// ```expect
    /// array(4) {
    ///   [0]=>
    ///   int(0)
    ///   [1]=>
    ///   array(1) {
    ///     ["x"]=>
    ///     int(1)
    ///   }
    ///   [2]=>
    ///   array(1) {
    ///     ["label"]=>
    ///     string(3) "bag"
    ///   }
    ///   [3]=>
    ///   string(22) "RecursiveArrayIterator"
    /// }
    /// ```
    #[php(tentative)]
    fn __serialize(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let this = this.object();
        let flags = ar_flags(req, this)?;
        let iterator_class = payload(req, this)?.iterator_class;
        let mut out = PhpArray::new(req)?;
        out.append(req, Owner::int(flags & CLONE_MASK))?;
        let stored = if flags & IS_SELF != 0 {
            Owner::null()
        } else {
            storage(req, this)?
        };
        out.append(req, stored)?;
        let members = crate::members(req, this)?;
        out.append(req, members.into_value())?;
        let class = match iterator_class {
            Some(class) if !is_array_iterator(req, class) => {
                let name = crate::class_name(req, class);
                crate::string_value(req, &name)?
            }
            _ => Owner::null(),
        };
        out.append(req, class)?;
        Ok(out)
    }

    /// `__unserialize(array $data): void`.
    ///
    /// ```php
    /// $o = new ArrayObject;
    /// $o->__unserialize([2, ['k' => 7], ['p' => 1], null]);
    /// var_dump($o->k, $o->getFlags(), $o->p);
    /// try { (new ArrayObject)->__unserialize([]); } catch (UnexpectedValueException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    ///
    /// Deprecated: Creation of dynamic property ArrayObject::$p is deprecated in %s on line 3
    /// int(7)
    /// int(2)
    /// int(1)
    /// Incomplete or ill-typed serialization data
    /// ```
    ///
    /// The storage `unserialize()` read is held with one more owner, and
    /// the members' empty array is the process's own, so the peak is
    /// php's:
    ///
    /// ```php
    /// $s = serialize(new ArrayObject([1, 2, 3]));
    /// unserialize($s); $u = memory_get_usage(); memory_reset_peak_usage(); $x = unserialize($s);
    /// echo memory_get_peak_usage() - $u, " ", memory_get_usage() - $u, "\n";
    /// ```
    /// ```expect
    /// 17352 568
    /// ```
    ///
    /// The class named is `zend_lookup_class`, which asks the autoloaders.
    ///
    /// ```php
    /// spl_autoload_register(function ($c) { echo "load $c\n"; if ($c === 'It') { class It extends ArrayIterator {} } });
    /// $a = new ArrayObject([1]);
    /// $a->__unserialize([0, [], [], 'It']);
    /// echo $a->getIteratorClass(), "\n";
    /// ```
    /// ```expect
    /// load It
    /// It
    /// ```
    #[php(tentative)]
    fn __unserialize(req: &mut Request, this: This<Self>, data: PhpArray) -> Result<(), Thrown> {
        let this = this.object();
        let flags = data.get(&Key::Int(0)).map(|v| v.deref());
        let stored = data.get(&Key::Int(1)).map(|v| v.deref());
        let members = data.get(&Key::Int(2)).map(|v| v.deref());
        let class = data.get(&Key::Int(3)).map(|v| v.deref());
        let (Some(flags), Some(stored), Some(members)) = (
            flags.and_then(|f| f.as_int()),
            stored,
            members.filter(|m| m.as_array().is_some()),
        ) else {
            return Err(throw(
                req,
                "UnexpectedValueException",
                "Incomplete or ill-typed serialization data",
            ));
        };
        if class.is_some_and(|c| !matches!(c.as_ref(), ValueRef::Null | ValueRef::Str(_))) {
            return Err(throw(
                req,
                "UnexpectedValueException",
                "Incomplete or ill-typed serialization data",
            ));
        }
        set_flags_and_self(req, this, flags)?;
        if flags & IS_SELF == 0 {
            if stored.as_array().is_none() && stored.as_object().is_none() {
                return Err(throw(
                    req,
                    "InvalidArgumentException",
                    "Passed variable is not an array or object",
                ));
            }
            set_array(req, this, &stored, 0, true)?;
        }
        crate::load_members(req, this, &members)?;
        if let Some(name) = class.and_then(|c| c.as_str_bytes().map(<[u8]>::to_vec)) {
            let shown = String::from_utf8_lossy(&name).into_owned();
            let Some(found) = autoload(req, &name)? else {
                return Err(throw(
                    req,
                    "UnexpectedValueException",
                    &format!(
                        "Cannot deserialize ArrayObject with iterator class '{shown}'; no such class exists"
                    ),
                ));
            };
            if !crate::is_a(req, found, b"Iterator") {
                return Err(throw(
                    req,
                    "UnexpectedValueException",
                    &format!(
                        "Cannot deserialize ArrayObject with iterator class '{shown}'; this class does not implement the Iterator interface"
                    ),
                ));
            }
            with(req, this, |p| p.iterator_class = Some(found))?;
        }
        Ok(())
    }

    /// `getIterator(): Iterator`: a new object of the iterator class
    /// reading through this one, with its own position.
    ///
    /// ```php
    /// $o = new ArrayObject(['a' => 1, 'b' => 2]);
    /// $it = $o->getIterator();
    /// $o['c'] = 3;
    /// var_dump(get_class($it), iterator_to_array($it));
    /// foreach ($o as $k => $v) echo "$k=$v;";
    /// ```
    /// ```expect
    /// string(13) "ArrayIterator"
    /// array(3) {
    ///   ["a"]=>
    ///   int(1)
    ///   ["b"]=>
    ///   int(2)
    ///   ["c"]=>
    ///   int(3)
    /// }
    /// a=1;b=2;c=3;
    /// ```
    #[php(tentative)]
    fn getIterator(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<Iterator>>, Thrown> {
        let this = this.object();
        let (flags, class) = {
            let p = payload(req, this)?;
            (p.flags, p.iterator_class)
        };
        let class = match class {
            Some(class) => class,
            None => match lookup_class(req, b"ArrayIterator")? {
                Some(class) => class,
                None => {
                    return Err(php_ext::raising::fatal(
                        req,
                        "InternalError: ArrayObject::getIterator served with ArrayIterator unlinked",
                    ));
                }
            },
        };
        let iterator = php_ext::call::new_object(req, class)?;
        let held = Owner::take(req, this);
        let old = with(req, *iterator, |p| {
            p.flags = flags & CLONE_MASK;
            p.iterator_class = None;
            std::mem::replace(&mut p.storage, Held::Other(held))
        });
        match old {
            Ok(old) => old.give_up(req),
            Err(thrown) => {
                iterator.give_up(req);
                return Err(thrown);
            }
        }
        Ok(iterator.into_instance())
    }

    /// `exchangeArray(array|object $array): array`: the old table handed
    /// back as a copy, the new one in its place.
    ///
    /// ```php
    /// $o = new ArrayObject([1, 2]);
    /// var_dump($o->exchangeArray(['x' => 'y']), $o->getArrayCopy());
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(2)
    /// }
    /// array(1) {
    ///   ["x"]=>
    ///   string(1) "y"
    /// }
    /// ```
    #[php(tentative)]
    fn exchangeArray(
        req: &mut Request,
        this: This<Self>,
        array: Raw<Union<PhpArray, PhpObject>>,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let this = this.object();
        let given = array_or_object(req, array.value(), 1)?;
        sorting_refused(req, this)?;
        let old = table(req, this)?.copy(req)?;
        if let Err(thrown) = set_array(req, this, &given, 0, true) {
            old.give_up(req);
            return Err(thrown);
        }
        Ok(old)
    }

    /// `setIteratorClass(string $iteratorClass): void`.
    ///
    /// ```php
    /// $o = new ArrayObject([[1]]);
    /// $o->setIteratorClass('RecursiveArrayIterator');
    /// var_dump($o->getIteratorClass(), get_class($o->getIterator()));
    /// try { $o->setIteratorClass('stdClass'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(22) "RecursiveArrayIterator"
    /// string(22) "RecursiveArrayIterator"
    /// ArrayObject::setIteratorClass(): Argument #1 ($iteratorClass) must be a class name derived from ArrayIterator, stdClass given
    /// ```
    #[php(tentative)]
    fn setIteratorClass(
        req: &mut Request,
        this: This<Self>,
        iteratorClass: PhpString,
    ) -> Result<(), Thrown> {
        let class = iterator_class_argument(req, iteratorClass, 1)?;
        with(req, this.object(), |p| p.iterator_class = Some(class))
    }

    /// `getIteratorClass(): string`; the witness is `setIteratorClass`'s.
    #[php(tentative)]
    fn getIteratorClass(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let name = match payload(req, this.object())?.iterator_class {
            Some(class) => crate::class_name(req, class),
            None => b"ArrayIterator".to_vec(),
        };
        PhpString::new(req, &name)
    }

    /// `__debugInfo(): array`, what `var_dump` shows.
    ///
    /// ```php
    /// class Tagged extends ArrayIterator { protected $tag = 't'; }
    /// foreach ((new Tagged([1]))->__debugInfo() as $name => $value) echo str_replace("\0", '\0', $name), ' => ', json_encode($value), "\n";
    /// ```
    /// ```expect
    /// \0*\0tag => "t"
    /// \0ArrayIterator\0storage => [1]
    /// ```
    #[php(tentative)]
    fn __debugInfo(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        spl_array_debug_info(req, this.object())
    }
}

/// `class ArrayIterator implements SeekableIterator, ArrayAccess,
/// Serializable, Countable`: the methods it shares with `ArrayObject`
/// are php's `@implementation-alias`es and take the same entry.
///
/// ```php
/// $it = new ArrayIterator([3 => 'c', 1 => 'a']);
/// foreach ($it as $k => $v) echo "$k=$v;";
/// echo "\n", count($it), " ", $it->getFlags(), "\n";
/// $it->rewind(); $it->next();
/// var_dump($it->valid(), $it->key(), $it->current());
/// $it->next();
/// var_dump($it->valid(), $it->key(), $it->current());
/// $it->seek(0); var_dump($it->key());
/// ```
/// ```expect
/// 3=c;1=a;
/// 2 0
/// bool(true)
/// int(1)
/// string(1) "a"
/// bool(false)
/// NULL
/// NULL
/// int(3)
/// ```
#[php_class(
    implements(SeekableIterator, ArrayAccess, Serializable, Countable),
    size = 144, payload = SplArrayObject,
    handlers = HANDLERS
)]
pub(crate) struct ArrayIterator;

#[php_class]
impl ArrayIterator {
    const STD_PROP_LIST: i64 = ArrayObject::STD_PROP_LIST;
    const ARRAY_AS_PROPS: i64 = ArrayObject::ARRAY_AS_PROPS;

    /// `ArrayIterator::__construct(array|object $array = [], int $flags = 0)`:
    /// nothing for a call with no argument, then php's `|Al` in order; the
    /// witness is the class's. The first iterator of a request costs what
    /// every later one does:
    ///
    /// ```php
    /// $m = memory_get_usage(); $a = new ArrayIterator([]); $b = memory_get_usage();
    /// $c = new ArrayIterator([]);
    /// var_dump($b - $m, memory_get_usage() - $b);
    /// ```
    /// ```expect
    /// int(216)
    /// int(216)
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = [])] array: Raw<Union<PhpArray, PhpObject>>,
        #[php(default = 0)] flags: Raw<i64>,
    ) -> Result<(), Thrown> {
        let this = this.object();
        if req.frames().argument_count() == 0 {
            return Ok(());
        }
        let given = array_or_object(req, array.value(), 1)?;
        let flags = flags.hold(req)?;
        let just_array = req.frames().argument_count() == 1;
        set_array(req, this, &given, flags & !INT_MASK, just_array)
    }

    #[php(tentative, implementation = ArrayObject::offsetExists)]
    fn offsetExists(this: This<Self>, key: Value) -> bool;
    #[php(tentative, implementation = ArrayObject::offsetGet)]
    fn offsetGet(this: This<Self>, key: Value) -> Value;
    #[php(tentative, implementation = ArrayObject::offsetSet)]
    fn offsetSet(this: This<Self>, key: Value, value: Value);
    #[php(tentative, implementation = ArrayObject::offsetUnset)]
    fn offsetUnset(this: This<Self>, key: Value);
    #[php(tentative, implementation = ArrayObject::append)]
    fn append(this: This<Self>, value: Value);
    #[php(tentative, implementation = ArrayObject::getArrayCopy)]
    fn getArrayCopy(this: This<Self>) -> PhpArray;
    #[php(tentative, implementation = ArrayObject::count)]
    fn count(this: This<Self>) -> i64;
    #[php(tentative, implementation = ArrayObject::getFlags)]
    fn getFlags(this: This<Self>) -> i64;
    #[php(tentative, implementation = ArrayObject::setFlags)]
    fn setFlags(this: This<Self>, flags: i64);
    #[php(tentative, implementation = ArrayObject::asort)]
    fn asort(this: This<Self>, #[php(default = SORT_REGULAR)] flags: i64) -> True;
    #[php(tentative, implementation = ArrayObject::ksort)]
    fn ksort(this: This<Self>, #[php(default = SORT_REGULAR)] flags: i64) -> True;
    #[php(tentative, implementation = ArrayObject::uasort)]
    fn uasort(this: This<Self>, callback: Callable) -> True;
    #[php(tentative, implementation = ArrayObject::uksort)]
    fn uksort(this: This<Self>, callback: Callable) -> True;
    #[php(tentative, implementation = ArrayObject::natsort)]
    fn natsort(this: This<Self>) -> True;
    #[php(tentative, implementation = ArrayObject::natcasesort)]
    fn natcasesort(this: This<Self>) -> True;
    #[php(tentative, implementation = ArrayObject::unserialize)]
    fn unserialize(this: This<Self>, data: PhpString);
    #[php(tentative, implementation = ArrayObject::serialize)]
    fn serialize(this: This<Self>) -> PhpString;
    #[php(tentative, implementation = ArrayObject::__serialize)]
    fn __serialize(this: This<Self>) -> PhpArray;
    #[php(tentative, implementation = ArrayObject::__unserialize)]
    fn __unserialize(this: This<Self>, data: PhpArray);

    /// `ArrayIterator::rewind(): void`.
    #[php(tentative)]
    fn rewind(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        set_position(req, this.object(), 0)
    }

    /// `ArrayIterator::current(): mixed`, `null` past the end.
    #[php(tentative)]
    fn current(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        Ok(match here(req, this.object())? {
            Some((_, _, value)) => value.deref(),
            None => Value::null(),
        })
    }

    /// `ArrayIterator::key(): string|int|null`.
    #[php(tentative)]
    fn key(req: &mut Request, this: This<Self>) -> Result<Option<Union<PhpString, i64>>, Thrown> {
        Ok(match here(req, this.object())? {
            Some((_, Key::Int(n), _)) => Some(Union::B(n)),
            Some((_, Key::Str(s), _)) => Some(Union::A(s)),
            None => None,
        })
    }

    /// `ArrayIterator::next(): void`. The position is a hash iterator
    /// on the storage, so a write that turns it from packed into a hash,
    /// or deletes and a rehash, leave it on the element it stood at.
    ///
    /// ```php
    /// $a = new ArrayIterator([1 => 1, 3 => 3, 5 => 5]);
    /// $a->rewind(); $a->next(); echo $a->key(), "\n";
    /// $a['x'] = 'x'; echo $a->key(), "\n"; $a->next(); echo $a->key(), "\n";
    /// $b = new ArrayIterator([]); $b->rewind(); $b[] = 1; var_dump($b->valid());
    /// $o = new ArrayObject(range(0, 9)); $it = $o->getIterator();
    /// for ($i = 0; $i < 7; $i++) { $it->next(); }
    /// for ($i = 0; $i < 6; $i++) { unset($o[$i]); }
    /// for ($i = 0; $i < 8; $i++) { $o[] = $i; }
    /// var_dump($it->key(), $it->current());
    /// ```
    /// ```expect
    /// 3
    /// 3
    /// 5
    /// bool(true)
    /// int(7)
    /// int(7)
    /// ```
    #[php(tentative)]
    fn next(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        step(req, this.object())?;
        Ok(())
    }

    /// `ArrayIterator::valid(): bool`.
    #[php(tentative)]
    fn valid(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        Ok(here(req, this.object())?.is_some())
    }

    /// `ArrayIterator::seek(int $offset): void`.
    ///
    /// ```php
    /// $it = new ArrayIterator(['a', 'b', 'c']);
    /// $it->seek(2); var_dump($it->current());
    /// try { $it->seek(5); } catch (OutOfBoundsException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// string(1) "c"
    /// Seek position 5 is out of range
    /// ```
    #[php(tentative)]
    fn seek(req: &mut Request, this: This<Self>, offset: i64) -> Result<(), Thrown> {
        let this = this.object();
        let position = offset;
        if position >= 0 {
            set_position(req, this, 0)?;
            let mut ok = true;
            for _ in 0..position {
                if !step(req, this)? {
                    ok = false;
                    break;
                }
            }
            if ok && here(req, this)?.is_some() {
                return Ok(());
            }
        }
        Err(throw(
            req,
            "OutOfBoundsException",
            &format!("Seek position {position} is out of range"),
        ))
    }

    #[php(tentative, implementation = ArrayObject::__debugInfo)]
    fn __debugInfo(this: This<Self>) -> PhpArray;
}

/// `class RecursiveArrayIterator extends ArrayIterator implements
/// RecursiveIterator`: an array element, or an object one where
/// `CHILD_ARRAYS_ONLY` is off, is a child.
///
/// ```php
/// $r = new RecursiveArrayIterator([[1, 2], 'x', new ArrayObject([3])]);
/// var_dump($r->hasChildren(), $r->getChildren()->count());
/// $r->next(); $r->next(); var_dump(get_class($r->getChildren()));
/// foreach (new RecursiveIteratorIterator(new RecursiveArrayIterator([1, [2, [3]]])) as $v) echo $v;
/// ```
/// ```expect
/// bool(true)
/// int(2)
///
/// Deprecated: ArrayIterator::__construct(): Using an object as a backing array for ArrayIterator is deprecated, as it allows violating class constraints and invariants in %s on line 4
/// string(22) "RecursiveArrayIterator"
/// 123
/// ```
#[php_class(extends(ArrayIterator), implements(RecursiveIterator))]
pub(crate) struct RecursiveArrayIterator;

#[php_class]
impl RecursiveArrayIterator {
    const CHILD_ARRAYS_ONLY: i64 = 4;

    /// `RecursiveArrayIterator::hasChildren(): bool`: an array, or an
    /// object where `CHILD_ARRAYS_ONLY` is off.
    ///
    /// ```php
    /// $r = new RecursiveArrayIterator(['a' => [1, [2]], 'b' => 2, 'c' => new ArrayObject([3])]);
    /// var_dump($r->hasChildren(), get_class($r->getChildren()), $r->getChildren()->getArrayCopy());
    /// $r->next(); var_dump($r->hasChildren(), $r->getChildren());
    /// $r->next(); var_dump($r->hasChildren());
    /// $r = new RecursiveArrayIterator([new ArrayObject([3])], RecursiveArrayIterator::CHILD_ARRAYS_ONLY);
    /// var_dump($r->hasChildren());
    /// ```
    /// ```expect exit=255
    /// bool(true)
    /// string(22) "RecursiveArrayIterator"
    /// array(2) {
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   array(1) {
    ///     [0]=>
    ///     int(2)
    ///   }
    /// }
    ///
    /// Fatal error: Uncaught TypeError: ArrayIterator::__construct(): Argument #1 ($array) must be of type array, int given in %s:4
    /// Stack trace:
    /// #0 [internal function]: ArrayIterator->__construct(2, 0)
    /// #1 %s(4): RecursiveArrayIterator->getChildren()
    /// #2 {main}
    ///   thrown in %s on line 4
    /// ```
    #[php(tentative)]
    fn hasChildren(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let this = this.object();
        let flags = flags(req, this)?;
        Ok(match here(req, this)? {
            Some((_, _, value)) => {
                let value = value.deref();
                value.as_array().is_some()
                    || (value.as_object().is_some() && flags & Self::CHILD_ARRAYS_ONLY == 0)
            }
            None => false,
        })
    }

    /// `RecursiveArrayIterator::getChildren(): ?RecursiveArrayIterator`: an
    /// object of this class over the element, or the element itself where
    /// it is one already; the witness is the class's.
    #[php(tentative)]
    fn getChildren(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Option<Owner<Instance<RecursiveArrayIterator>>>, Thrown> {
        let this = this.object();
        let flags = flags(req, this)?;
        let Some((_, _, value)) = here(req, this)? else {
            return Ok(None);
        };
        let value = value.deref();
        if let Some(object) = value.as_object() {
            if flags & Self::CHILD_ARRAYS_ONLY != 0 {
                return Ok(None);
            }
            if req.symbols().instance_of(object.class(), this.class()) {
                return Ok(Some(Owner::take(req, object).into_instance()));
            }
        }
        let child = instantiate(req, this.class(), &[value, Value::int(flags)])?;
        Ok(Some(child.into_instance()))
    }
}

/// The payload of `object`, copied out, or the internal-error fatal
/// for an object that has none, which no class of this file makes.
fn payload(req: &mut Request, object: PhpObject) -> Result<Payload, Thrown> {
    if let Some(p) = object.native::<SplArrayObject>(req) {
        return Ok(Payload {
            storage: p.storage.seen(),
            flags: p.flags,
            position: p.position,
            iterator_class: p.iterator_class,
            sorting: p.sorting,
        });
    }
    Err(missing(req))
}

/// The payload of `object` changed by `change`.
fn with<R>(
    req: &mut Request,
    object: PhpObject,
    change: impl FnOnce(&mut SplArrayObject) -> R,
) -> Result<R, Thrown> {
    if let Some(p) = object.native_mut::<SplArrayObject>(req) {
        return Ok(change(p));
    }
    Err(missing(req))
}

fn missing(req: &mut Request) -> Thrown {
    php_ext::raising::fatal(
        req,
        "InternalError: an SPL array method served on an object without its payload",
    )
}

/// What `spl_array_get_hash_table` answers: the array and the object
/// holding it, or the object whose properties are the table.
#[derive(Clone, Copy)]
enum Table {
    Array(PhpObject, PhpArray),
    Props(PhpObject),
}

/// `spl_array_get_hash_table`, `SPL_ARRAY_USE_OTHER` followed.
fn table(req: &mut Request, object: PhpObject) -> Result<Table, Thrown> {
    let mut at = object;
    for _ in 0..64 {
        match payload(req, at)?.storage {
            Storage::OwnProps => return Ok(Table::Props(at)),
            Storage::Object(backing) => return Ok(Table::Props(backing)),
            Storage::Other(backing) => at = backing,
            Storage::Array(array) => return Ok(Table::Array(at, array)),
            Storage::Fresh => return Ok(Table::Array(at, laid_down(req, at)?)),
        }
    }
    Err(php_ext::raising::fatal(
        req,
        "InternalError: an ArrayObject reads through more objects than any can hold",
    ))
}

/// `spl_array_is_object`.
fn is_object(req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
    Ok(matches!(table(req, object)?, Table::Props(_)))
}

/// `array_init(&intern->array)` on the first touch: the payload's own
/// empty array laid down, its view answered.
fn laid_down(req: &mut Request, owner: PhpObject) -> Result<PhpArray, Thrown> {
    let array = PhpArray::new(req)?;
    let view = *array;
    let old = with(req, owner, |p| {
        std::mem::replace(&mut p.storage, Held::Array(array))
    })?;
    old.give_up(req);
    Ok(view)
}

/// `SEPARATE_ARRAY` on the array `owner`'s payload holds, through the
/// payload's owner, which moves to the copy where one is made: the view a
/// write then writes in place, the array the payload's alone. The owner is
/// out of the payload for the separation only, which runs no code of the
/// program.
fn separated(req: &mut Request, owner: PhpObject) -> Result<PhpArray, Thrown> {
    let held = with(req, owner, |p| match std::mem::take(&mut p.storage) {
        Held::Array(array) => Some(array),
        other => {
            p.storage = other;
            None
        }
    })?;
    let Some(mut array) = held else {
        return Err(php_ext::raising::fatal(
            req,
            "InternalError: an SPL array written through a storage that holds no array",
        ));
    };
    let separated = array.separate(req);
    let view = *array;
    let old = with(req, owner, |p| {
        std::mem::replace(&mut p.storage, Held::Array(array))
    })?;
    old.give_up(req);
    separated.map(|()| view)
}

/// Whether `key` is a protected or private property's mangled name,
/// which `spl_array_skip_protected` reads off its first byte.
fn mangled(key: &Key) -> bool {
    matches!(key, Key::Str(s) if s.as_bytes().starts_with(b"\0"))
}

impl Table {
    /// The elements in order, views, no owner taken; a property table's
    /// protected and private rows left out, as `spl_array_skip_protected`
    /// leaves them.
    fn rows(self, req: &mut Request) -> Result<Vec<(Key, Value)>, Thrown> {
        Ok(self
            .every_row(req)?
            .into_iter()
            .filter(|(key, _)| !mangled(key))
            .collect())
    }

    /// Every row, a property table's protected and private ones among
    /// them under their mangled names: what `zend_array_dup` copies and
    /// what `spl_array_get_properties_for` hands a cast; views.
    fn every_row(self, req: &mut Request) -> Result<Vec<(Key, Value)>, Thrown> {
        let table = self.walked(req)?;
        Ok(walk::rows(req, table).collect())
    }

    /// The table itself, php's `HashTable *` that
    /// `spl_array_get_hash_table` answers: the storage, or the backing
    /// object's `Z_OBJPROP`.
    fn walked(self, req: &mut Request) -> Result<walk::Table, Thrown> {
        match self {
            Table::Array(_, array) => Ok(walk::Table::Array(array)),
            Table::Props(object) => object.property_table(req),
        }
    }

    /// `zend_symtable_find_ind`, a view; over an object an integer
    /// offset is the name spelt in decimal, as `get_hash_key` spells it
    /// for one, and so for [`Table::set`] and [`Table::unset`].
    fn get(self, req: &mut Request, key: &Key) -> Result<Option<Value>, Thrown> {
        match self {
            Table::Array(_, array) => Ok(array.get(key)),
            Table::Props(object) => {
                let name = key.to_prop_name(req)?;
                let row = object.row(req, &Key::Str(*name));
                name.give_up(req);
                row
            }
        }
    }

    /// `zend_hash_update_ind`: `value` is the table's from here, and
    /// `key` a view, the array taking its own owner of a string key.
    fn set(self, req: &mut Request, key: &Key, value: Owner<Value>) -> Result<(), Thrown> {
        match self {
            Table::Array(owner, _) => {
                let mut array = match separated(req, owner) {
                    Ok(array) => array,
                    Err(thrown) => {
                        value.give_up(req);
                        return Err(thrown);
                    }
                };
                let key = Owner::take(req, key.clone());
                array.set(req, key, value)
            }
            Table::Props(object) => {
                let name = match key.to_prop_name(req) {
                    Ok(name) => name,
                    Err(thrown) => {
                        value.give_up(req);
                        return Err(thrown);
                    }
                };
                let written = object.set_row(req, &Key::Str(*name), value);
                name.give_up(req);
                written
            }
        }
    }

    /// `zend_hash_next_index_insert`: `value`'s owner is the table's from here,
    /// laid down at the table's `nNextFreeElement`, an object's property
    /// table's among them. An integer offset over an object is the
    /// string that spells it ([`hash_key`] then [`Table::set`]), so it
    /// moves no next free key, and an unset of one finds no integer row.
    ///
    /// ```php
    /// $o = new stdClass; $ao = @new ArrayObject($o); $ao[] = str_repeat("a", 3); var_dump($o, count($ao));
    /// $s = new stdClass; $s->x = 1; $as = @new ArrayObject($s);
    /// $as[] = 'a'; $as[5] = 'five'; $as[] = 'b'; unset($as[1]); $as[] = 'c';
    /// var_dump($s, count($as));
    /// #[AllowDynamicProperties] class D { public $p = 1; protected $q = 2; }
    /// $d = new D; $i = @new ArrayIterator($d); $i[] = 'x';
    /// $c = clone $d; $over = @new ArrayObject(@new ArrayObject($c)); $over[] = 'y';
    /// var_dump($d, $c, count($i));
    /// $full = new ArrayObject([PHP_INT_MAX => 1]); $full[] = 2; $full->append(3);
    /// var_dump(count($full));
    /// ```
    /// ```expect
    /// object(stdClass)#1 (1) {
    ///   [0]=>
    ///   string(3) "aaa"
    /// }
    /// int(1)
    /// object(stdClass)#3 (5) {
    ///   ["x"]=>
    ///   int(1)
    ///   [0]=>
    ///   string(1) "a"
    ///   ["5"]=>
    ///   string(4) "five"
    ///   [1]=>
    ///   string(1) "b"
    ///   [2]=>
    ///   string(1) "c"
    /// }
    /// int(5)
    /// object(D)#5 (3) {
    ///   ["p"]=>
    ///   int(1)
    ///   ["q":protected]=>
    ///   int(2)
    ///   [0]=>
    ///   string(1) "x"
    /// }
    /// object(D)#7 (4) {
    ///   ["p"]=>
    ///   int(1)
    ///   ["q":protected]=>
    ///   int(2)
    ///   [0]=>
    ///   string(1) "x"
    ///   [1]=>
    ///   string(1) "y"
    /// }
    /// int(2)
    /// int(1)
    /// ```
    fn append(self, req: &mut Request, value: Owner<Value>) -> Result<(), Thrown> {
        // php ignores what `zend_hash_next_index_insert` answers, so a
        // taken next element is no `Error` here, as it is for `$a[]`.
        match self {
            Table::Array(owner, _) => {
                let mut array = match separated(req, owner) {
                    Ok(array) => array,
                    Err(thrown) => {
                        value.give_up(req);
                        return Err(thrown);
                    }
                };
                array.try_append(req, value).map(drop)
            }
            Table::Props(object) => object.append_row(req, value).map(drop),
        }
    }

    fn unset(self, req: &mut Request, key: &Key) -> Result<(), Thrown> {
        match self {
            Table::Array(owner, array) => {
                if !array.contains(key) {
                    return Ok(());
                }
                separated(req, owner)?.unset(req, key).map(drop)
            }
            Table::Props(object) => {
                let name = key.to_prop_name(req)?;
                let unset = object.unset_row(req, &Key::Str(*name));
                name.give_up(req);
                unset
            }
        }
    }

    /// `spl_array_object_count_elements_helper` over a property table:
    /// a declared row the slots stand for, php's `IS_INDIRECT`, is left
    /// out under a mangled name, and a plain value counts whatever its
    /// key, so a protected or private row the table holds as a copy
    /// counts where iteration leaves it out.
    fn len(self, req: &mut Request) -> Result<usize, Thrown> {
        match self {
            Table::Array(_, array) => Ok(array.len()),
            Table::Props(object) => {
                let rows = self.every_row(req)?;
                Ok(match object.declared_rows() {
                    DeclaredRows::Copied => rows.len(),
                    DeclaredRows::Slots => rows.iter().filter(|(key, _)| !mangled(key)).count(),
                })
            }
        }
    }

    /// The table as an array of its own, one owner, `zend_array_dup`:
    /// a property table's every row, mangled names and all.
    fn copy(self, req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
        match self {
            Table::Array(_, array) => array.copy(req),
            Table::Props(_) => {
                let mut out = PhpArray::new(req)?;
                for (key, value) in self.every_row(req)? {
                    let value = Owner::take(req, value);
                    let key = Owner::take(req, key.clone());
                    out.set(req, key, value)?;
                }
                Ok(out)
            }
        }
    }

    /// The element at `key` made a reference in place, `found` what the
    /// table holds there, `null` laid down where it holds nothing; the
    /// cell handed back with one more owner. `key` is a view, as for
    /// [`Table::set`].
    fn element_ref(
        self,
        req: &mut Request,
        key: &Key,
        found: Option<Value>,
    ) -> Result<Owner<PhpRef>, Thrown> {
        match self {
            Table::Array(owner, _) => {
                if let Some(cell) = found.and_then(|v| v.as_php_ref()) {
                    return Ok(Owner::take(req, cell));
                }
                let mut array = separated(req, owner)?;
                let held = Owner::take(req, found.map_or_else(Value::null, |v| v.deref()));
                // The cell's first owner goes to the slot's caller, a second
                // to the table.
                let cell = PhpRef::new(req, held)?;
                let kept = Owner::take(req, cell.as_value());
                let key = Owner::take(req, key.clone());
                if let Err(thrown) = array.set(req, key, kept) {
                    cell.give_up(req);
                    return Err(thrown);
                }
                Ok(cell)
            }
            Table::Props(backing) => backing.row_ref(req, key),
        }
    }

    /// The first element at or past the bucket `pos`, with the bucket it
    /// stands at.
    fn at(self, req: &mut Request, pos: u32) -> Result<Option<(u32, Key, Value)>, Thrown> {
        match self {
            Table::Array(_, array) => Ok(array.live_from(pos)),
            Table::Props(_) => {
                let rows = self.rows(req)?;
                let at = usize::try_from(pos).unwrap_or(usize::MAX);
                Ok(rows
                    .into_iter()
                    .nth(at)
                    .map(|(key, value)| (pos, key, value)))
            }
        }
    }
}

/// `get_hash_key`: the key an offset makes, php's offset rules, or
/// `None` for an array or an object, which the caller refuses as
/// `zend_illegal_container_offset` does.
fn hash_key(req: &mut Request, offset: &Value) -> Result<Option<Key>, Thrown> {
    Ok(Some(match offset.deref().as_ref() {
        ValueRef::Null | ValueRef::Undef => {
            deprecated(
                req,
                "Using null as an array offset is deprecated, use an empty string instead",
            );
            // `ZSTR_EMPTY_ALLOC()`: the interned empty string, no owner.
            Key::Str(PhpString::interned(req, b"")?)
        }
        ValueRef::Str(s) => Key::lookup(s),
        ValueRef::Resource(r) => {
            let id = r.id();
            warning(
                req,
                &format!("Resource ID#{id} used as offset, casting to integer ({id})"),
            );
            Key::Int(id)
        }
        ValueRef::Float(f) => Key::Int(crate::float_key(req, f)?),
        ValueRef::Bool(b) => Key::Int(i64::from(b)),
        ValueRef::Int(n) => Key::Int(n),
        _ => return Ok(None),
    }))
}

/// `zend_illegal_container_offset(object->ce->name, offset, read)`.
fn illegal(req: &mut Request, object: PhpObject, offset: &Value, read: DimensionRead) -> Thrown {
    let class = req.symbols().class_name(object.class());
    illegal_container_offset(req, &class, offset, read)
}

fn sorting_refused(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    if payload(req, object)?.sorting > 0 {
        return Err(throw_error(
            req,
            "Error",
            "Modification of ArrayObject during sorting is prohibited",
        ));
    }
    Ok(())
}

/// `spl_array_read_dimension_ex` without the overrides: the element, one
/// more owner, the warning for a key the table does not have under a
/// read that warns.
fn read(req: &mut Request, object: PhpObject, offset: &Value, warn: bool) -> Result<Value, Thrown> {
    let Some(key) = hash_key(req, offset)? else {
        let read = if warn {
            DimensionRead::Warning
        } else {
            DimensionRead::Silent
        };
        return Err(illegal(req, object, offset, read));
    };
    let table = table(req, object)?;
    if let Some(value) = table.get(req, &key)? {
        Ok(value.deref())
    } else {
        if warn {
            warning(req, &format!("Undefined array key {}", shown(&key)));
        }
        Ok(Value::null())
    }
}

/// `spl_array_get_dimension_ptr` under a write fetch, then
/// `spl_array_read_dimension_ex`'s `ZVAL_NEW_REF`: the element made a
/// reference in place, laid down `null` where the table has none (with
/// php's warning first under `BP_VAR_RW`), and the cell handed back with
/// one more owner. Under `BP_VAR_UNSET` a key the table has not is
/// `None`, php's `EG(uninitialized_zval)`, and nothing is laid down.
fn place(
    req: &mut Request,
    object: PhpObject,
    offset: &Value,
    read: DimensionRead,
) -> Result<Option<Owner<PhpRef>>, Thrown> {
    if read != DimensionRead::Unset {
        sorting_refused(req, object)?;
    }
    let Some(key) = hash_key(req, offset)? else {
        if read != DimensionRead::Unset {
            return Err(illegal(req, object, offset, DimensionRead::Write));
        }
        // `BP_VAR_UNSET` names `unset`, and answers
        // `&EG(uninitialized_zval)`, which the fetch notices as no place.
        let thrown = illegal(req, object, offset, DimensionRead::Unset);
        let class = String::from_utf8_lossy(&crate::class_name(req, object.class())).into_owned();
        php_ext::raising::raise_while_pending(
            req,
            &thrown,
            Level::Notice,
            &format!("Indirect modification of overloaded element of {class} has no effect"),
        );
        return Err(thrown);
    };
    let table = table(req, object)?;
    let found = table.get(req, &key)?;
    if found.is_none() {
        match read {
            DimensionRead::Unset => return Ok(None),
            DimensionRead::ReadWrite => {
                warning(req, &format!("Undefined array key {}", shown(&key)));
            }
            _ => {}
        }
    }
    table.element_ref(req, &key, found).map(Some)
}

/// `spl_array_it_get_current_data` under `by_ref`, the `current_place`
/// slot: the element the object stands at made a reference in place
/// (`FE_FETCH_RW`'s `ZVAL_MAKE_REF` over the zval php's function points
/// at), `None` past the last element. Over an object the iterator holds
/// itself, neither its own table (`SPL_ARRAY_IS_SELF`) nor another
/// iterator's (`SPL_ARRAY_USE_OTHER`), a row that is no reference yet
/// is a property's: a typed one's cell is typed by it
/// ([`PhpObject::typed_row_ref`]), and a typed `readonly` one is
/// refused before anything is made, since the reference would let the
/// loop write it.
///
/// ```php
/// class A { public int $n = 1; public $u = "u"; public function __construct(public readonly string $r = "r") {} }
/// $a = new A;
/// try {
///     foreach (@new ArrayIterator($a) as $k => &$v) {
///         echo "$k\n";
///         if ($k === "n") { try { $v = "x"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; } $v = "2"; }
///         if ($k === "u") { $v = []; }
///     }
/// } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// unset($v);
/// var_dump($a);
/// ```
/// ```expect
/// n
/// Cannot assign string to reference held by property A::$n of type int
/// u
/// Cannot acquire reference to readonly property A::$r
/// object(A)#1 (3) {
///   ["n"]=>
///   int(2)
///   ["u"]=>
///   array(0) {
///   }
///   ["r"]=>
///   string(1) "r"
/// }
/// ```
fn current_place(req: &mut Request, object: PhpObject) -> Result<Option<Owner<PhpRef>>, Thrown> {
    let Some((_, key, value)) = here(req, object)? else {
        return Ok(None);
    };
    if let Storage::Object(backing) = payload(req, object)?.storage {
        let unbound = backing
            .row(req, &key)?
            .is_none_or(|row| row.as_php_ref().is_none());
        if unbound {
            refuse_readonly(req, backing, &key)?;
        }
        return backing.typed_row_ref(req, &key).map(Some);
    }
    let table = table(req, object)?;
    table.element_ref(req, &key, Some(value)).map(Some)
}

/// `spl_array_it_get_current_data`'s refusal: the property a row of
/// `backing` is (`zend_get_property_info` over its class, the first
/// declaration of the name up the chain, the row's key being a public
/// property's or a dynamic one's), typed and `readonly`, is refused with
/// php's `Error`, named by the class that declared it.
fn refuse_readonly(req: &mut Request, backing: PhpObject, key: &Key) -> Result<(), Thrown> {
    let Key::Str(name) = key else {
        return Ok(());
    };
    let name = name.as_bytes().to_vec();
    let mut at = Some(backing.class());
    for _ in 0..64 {
        let Some(class) = at else {
            return Ok(());
        };
        let declared = req.symbols().class_decl(class).and_then(|decl| {
            decl.properties
                .iter()
                .find(|p| p.is_instance && *p.name.0 == *name)
                .map(|p| (p.ty.is_some() && p.is_readonly, decl.name.0.clone()))
        });
        if let Some((refused, declaring)) = declared {
            if !refused {
                return Ok(());
            }
            let message = format!(
                "Cannot acquire reference to readonly property {}::${}",
                String::from_utf8_lossy(&declaring),
                String::from_utf8_lossy(&name)
            );
            return Err(throw_error(req, "Error", &message));
        }
        at = req.symbols().class_parent(class);
    }
    Ok(())
}

/// A key as php's `Undefined array key` warning shows it, a string
/// quoted.
fn shown(key: &Key) -> String {
    match key {
        Key::Int(n) => n.to_string(),
        Key::Str(s) => format!("\"{}\"", String::from_utf8_lossy(s.as_bytes())),
    }
}

/// `spl_array_write_dimension_ex` without the overrides; the value is
/// taken with one more owner, the caller keeping its own.
fn write(
    req: &mut Request,
    object: PhpObject,
    offset: Option<&Value>,
    value: Value,
) -> Result<(), Thrown> {
    sorting_refused(req, object)?;
    let table = table(req, object)?;
    let value = Owner::take(req, value);
    // `if (!offset || Z_TYPE_P(offset) == IS_NULL)`: a reference to
    // `null` is no `null` here, and is the key `""` below.
    let offset = offset.filter(|o| !matches!(o.as_ref(), ValueRef::Null | ValueRef::Undef));
    let Some(offset) = offset else {
        return table.append(req, value);
    };
    let key = match hash_key(req, offset) {
        Ok(Some(key)) => key,
        Ok(None) => {
            value.give_up(req);
            return Err(illegal(req, object, offset, DimensionRead::Write));
        }
        Err(thrown) => {
            value.give_up(req);
            return Err(thrown);
        }
    };
    table.set(req, &key, value)
}

/// `spl_array_unset_dimension_ex` without the overrides; the position
/// moves past an element unset under it.
fn unset(req: &mut Request, object: PhpObject, offset: &Value) -> Result<(), Thrown> {
    sorting_refused(req, object)?;
    let Some(key) = hash_key(req, offset)? else {
        return Err(illegal(req, object, offset, DimensionRead::Unset));
    };
    let table = table(req, object)?;
    table.unset(req, &key)
}

/// `spl_array_has_dimension_ex`: `how` is php's `check_empty`, 0 for
/// `isset`, 1 for `empty` and 2 for `offsetExists`.
fn has(
    req: &mut Request,
    object: PhpObject,
    offset: &Value,
    how: u8,
    inherited: bool,
) -> Result<bool, Thrown> {
    let mut value: Option<Owner<Value>> = None;
    let get_overridden = inherited && crate::overridden(req, object, b"offsetget");
    if inherited && crate::overridden(req, object, b"offsetexists") {
        let answered = call_method(req, &object, b"offsetExists", &[*offset])?;
        let exists = truthy(req, &answered);
        answered.give_up(req);
        let exists = exists?;
        if !exists {
            return Ok(false);
        }
        if how == 0 {
            return Ok(true);
        }
        if get_overridden {
            value = Some(call_method(req, &object, b"offsetGet", &[*offset])?);
        }
    }
    let value = if let Some(value) = value {
        value
    } else {
        let Some(key) = hash_key(req, offset)? else {
            return Err(illegal(req, object, offset, DimensionRead::Silent));
        };
        let table = table(req, object)?;
        let Some(found) = table.get(req, &key)? else {
            return Ok(false);
        };
        if how == 2 {
            return Ok(true);
        }
        if how == 1 && get_overridden {
            call_method(req, &object, b"offsetGet", &[*offset])?
        } else {
            Owner::take(req, found.deref())
        }
    };
    let answer = if how > 0 {
        truthy(req, &value)
    } else {
        Ok(!matches!((*value).deref().as_ref(), ValueRef::Null))
    };
    value.give_up(req);
    answer
}

/// The flags a user sets, the storage bits left out.
fn flags(req: &mut Request, object: PhpObject) -> Result<i64, Thrown> {
    Ok(payload(req, object)?.flags)
}

/// php's whole `ar_flags`: the flags with the storage's two bits, what
/// `serialize` and `__serialize` write.
fn ar_flags(req: &mut Request, object: PhpObject) -> Result<i64, Thrown> {
    let p = payload(req, object)?;
    Ok(p.flags | p.storage.bits())
}

/// Whether the property `name` stands on the object as `std` sees it,
/// which `ARRAY_AS_PROPS` leaves to the object.
fn as_props(req: &mut Request, object: PhpObject, name: &[u8]) -> Result<bool, Thrown> {
    Ok(flags(req, object)? & ArrayObject::ARRAY_AS_PROPS != 0
        && !Std.has_property(req, object, name, PropertyTest::Exists)?)
}

impl ObjectHandlers for Handlers {
    /// `spl_array_iterator`: the iterator and its `by_ref`.
    fn iterator_size(&self) -> usize {
        96
    }

    /// `spl_array_object_new_ex` with no object to copy:
    /// `array_init(&intern->array)`, the table a constructor replaces.
    fn create_object(&self, req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
        let array = PhpArray::new(req)?;
        let left = match object.native_mut::<SplArrayObject>(req) {
            Some(p) if matches!(p.storage, Held::Fresh) => {
                std::mem::replace(&mut p.storage, Held::Array(array))
            }
            _ => Held::Array(array),
        };
        left.give_up(req);
        Ok(())
    }

    /// `spl_array_object_free_storage`: the position forgotten, then
    /// `zend_object_std_dtor`, then the storage given up.
    ///
    /// ```php
    /// class D { function __construct(public $n) {} function __destruct() { echo $this->n, "\n"; } }
    /// class A extends ArrayObject { public $p; }
    /// $o = new A([new D('storage')]); $o->p = new D('prop'); unset($o);
    /// ```
    /// ```expect
    /// prop
    /// storage
    /// ```
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        forget_position(req, object).ok();
        Std.free_obj(req, object);
        let held = object
            .native_mut::<SplArrayObject>(req)
            .map(|p| std::mem::take(&mut p.storage));
        if let Some(held) = held {
            held.give_up(req);
        }
    }

    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        if let Some(held) = object
            .native::<SplArrayObject>(req)
            .and_then(|p| p.storage.seen().held())
        {
            visit(held);
        }
        GcTable::Properties
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_array)
    }

    fn current_place(&self) -> Option<CurrentPlace> {
        Some(current_place)
    }

    /// `spl_array_read_dimension_ex`: an overriding `offsetGet` served,
    /// after an overriding `offsetExists` said yes under `BP_VAR_IS`; else
    /// `spl_array_get_dimension_ptr` over the storage, and under a write
    /// fetch the element made a reference in place (`ZVAL_NEW_REF`) and
    /// handed back as the place the write goes through. `$o[]` hands the
    /// overriding `offsetGet` an argument it was not passed.
    ///
    /// ```php
    /// class B extends ArrayObject {
    ///     public function offsetGet($offset): mixed { var_dump($offset); return 5; }
    /// }
    /// $b = new B;
    /// try { $b[][5] = 'value'; } catch (Throwable $e) { echo $e::class, ': ', $e->getMessage(), "\n"; }
    /// class C extends ArrayObject {
    ///     public function offsetGet($offset = 'd'): mixed { var_dump($offset); return [1]; }
    /// }
    /// $c = new C;
    /// $c[][5] = 'value';
    /// ```
    /// ```expect
    ///
    /// Notice: Indirect modification of overloaded element of B has no effect in %s on line 6
    /// ArgumentCountError: B::offsetGet(): Argument #1 ($offset) not passed
    /// string(1) "d"
    ///
    /// Notice: Indirect modification of overloaded element of C has no effect in %s on line 11
    /// ```
    ///
    /// The notice is raised with what `offsetGet` threw pending, as
    /// `zend_fetch_dimension_address` raises it with `EG(exception)`
    /// set, so a user error handler is handed nothing of it:
    ///
    /// ```php
    /// set_error_handler(function ($no, $m) { echo "handler: $m\n"; return true; });
    /// class B extends ArrayObject { function offsetGet($o): mixed { throw new Exception("get"); } }
    /// $b = new B;
    /// try { $b[0][1] = 1; } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// get
    /// ```
    ///
    /// An offset no key is made of is `zend_illegal_container_offset`'s
    /// under the fetch's own type: `??` names `isset` and `empty`, and a
    /// nested `unset` names `unset` and is noticed as no place; a
    /// reference to `null` is the key `""`, and only a plain `null`
    /// appends; a float no long represents warns as an array's does.
    ///
    /// ```php
    /// $c = new ArrayObject();
    /// try { var_dump($c[[]] ?? 'x'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { unset($c[[]][0]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// $n = null; $r = &$n;
    /// $c[$r] = 5; $c[null] = 6;
    /// var_dump($c->getArrayCopy(), $c[1.8446744073709552E+19] ?? 'none');
    /// ```
    /// ```expect
    /// Cannot access offset of type array in isset or empty
    ///
    /// Notice: Indirect modification of overloaded element of ArrayObject has no effect in %s on line 4
    /// Cannot unset offset of type array on ArrayObject
    ///
    /// Deprecated: Using null as an array offset is deprecated, use an empty string instead in %s on line 6
    ///
    /// Warning: The float 1.8446744073709552E+19 is not representable as an int, cast occurred in %s on line 7
    /// array(2) {
    ///   [""]=>
    ///   int(5)
    ///   [0]=>
    ///   int(6)
    /// }
    /// int(6)
    /// ```
    fn read_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        read: DimensionRead,
    ) -> Result<Owner<Value>, Thrown> {
        let get = crate::overridden(req, object, b"offsetget");
        let silent = read == DimensionRead::Silent;
        if get || (silent && crate::overridden(req, object, b"offsetexists")) {
            if silent {
                let at = offset.copied().unwrap_or_default();
                if !has(req, object, &at, 0, true)? {
                    return Ok(Owner::null());
                }
            }
            // `if (!offset) { ZVAL_UNDEF(&tmp); offset = &tmp; }`: `$o[]`
            // hands the method an argument it was not passed.
            if get {
                let offset = offset.copied().unwrap_or_else(Value::unpassed);
                let got = call_method(req, &object, b"offsetGet", &[offset]);
                // A method that threw leaves `rv` undefined and the answer
                // `&EG(uninitialized_zval)`, which a write fetch takes for
                // no place: `zend_fetch_dimension_address`'s notice.
                if let Err(thrown) = &got
                    && matches!(
                        read,
                        DimensionRead::Write | DimensionRead::ReadWrite | DimensionRead::Unset
                    )
                {
                    let class = String::from_utf8_lossy(&crate::class_name(req, object.class()))
                        .into_owned();
                    php_ext::raising::raise_while_pending(
                        req,
                        thrown,
                        Level::Notice,
                        &format!(
                            "Indirect modification of overloaded element of {class} has no effect"
                        ),
                    );
                }
                return got;
            }
        }
        let Some(offset) = offset else {
            return Ok(Owner::null());
        };
        match read {
            // The slot hands the engine an owner of what it read.
            DimensionRead::Warning => {
                self::read(req, object, offset, true).map(|v| Owner::take(req, v.deref()))
            }
            DimensionRead::Silent => {
                self::read(req, object, offset, false).map(|v| Owner::take(req, v.deref()))
            }
            DimensionRead::Write | DimensionRead::ReadWrite | DimensionRead::Unset => {
                Ok(match place(req, object, offset, read)? {
                    Some(cell) => cell.into_value(),
                    None => Owner::null(),
                })
            }
        }
    }

    /// `spl_array_get_dimension_ptr` under `BP_VAR_W` and `BP_VAR_RW`:
    /// the element's own bucket, or the property's row over an object,
    /// `null` laid down where the storage has none, which `BP_VAR_RW`
    /// warns of; the storage separated first, as
    /// `spl_array_get_hash_table` hands out the object's own. The fetch
    /// lays it into its `VAR` as `IS_INDIRECT`, so `$o[$k] = &$v` binds
    /// the element to `$v`'s reference. An overriding `offsetGet`, an
    /// append and an unset are [`ObjectHandlers::read_dimension`]'s.
    ///
    /// ```php
    /// $ao = new ArrayObject;
    /// foreach ([1, 2, 3] as $i => $var) { $ao[$i] = &$var; }
    /// var_dump($ao);
    /// $x = 5; $ao['k'] = &$x; $x = 6; var_dump($ao['k']);
    /// $it = new ArrayIterator(['a' => [1]]); $it['a'][] = 2; $it['b'] .= 'x';
    /// var_dump($it->getArrayCopy());
    /// $o = new stdClass; $o->a = 1; $b = @new ArrayObject($o);
    /// $y = 5; $b['a'] = &$y; $y = 7; var_dump($o);
    /// ```
    /// ```expect
    /// object(ArrayObject)#1 (1) {
    ///   ["storage":"ArrayObject":private]=>
    ///   array(3) {
    ///     [0]=>
    ///     &int(3)
    ///     [1]=>
    ///     &int(3)
    ///     [2]=>
    ///     &int(3)
    ///   }
    /// }
    /// int(6)
    ///
    /// Warning: Undefined array key "b" in %s on line 6
    /// array(2) {
    ///   ["a"]=>
    ///   array(2) {
    ///     [0]=>
    ///     int(1)
    ///     [1]=>
    ///     int(2)
    ///   }
    ///   ["b"]=>
    ///   string(1) "x"
    /// }
    /// object(stdClass)#3 (1) {
    ///   ["a"]=>
    ///   &int(7)
    /// }
    /// ```
    fn dimension_place(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        fetch: DimensionRead,
    ) -> Result<Option<Place>, Thrown> {
        let (DimensionRead::Write | DimensionRead::ReadWrite) = fetch else {
            return Ok(None);
        };
        let Some(offset) = offset else {
            return Ok(None);
        };
        if crate::overridden(req, object, b"offsetget") {
            return Ok(None);
        }
        sorting_refused(req, object)?;
        let Some(key) = hash_key(req, offset)? else {
            return Err(illegal(req, object, offset, DimensionRead::Write));
        };
        let table = table(req, object)?;
        if table.get(req, &key)?.is_none() {
            if fetch == DimensionRead::ReadWrite {
                warning(req, &format!("Undefined array key {}", shown(&key)));
            }
            table.set(req, &key, Owner::null())?;
        }
        match table {
            Table::Array(owner, _) => {
                let array = separated(req, owner)?;
                Ok(array.place(&key))
            }
            Table::Props(backing) => backing.row_place(req, &key).map(Some),
        }
    }

    fn write_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        if crate::overridden(req, object, b"offsetset") {
            let offset = offset.copied().unwrap_or_default();
            let answered = call_method(req, &object, b"offsetSet", &[offset, *value]);
            value.give_up(req);
            answered?.give_up(req);
            return Ok(());
        }
        let written = write(req, object, offset, *value);
        value.give_up(req);
        written
    }

    fn has_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: &Value,
        not_empty: bool,
    ) -> Result<bool, Thrown> {
        has(req, object, offset, u8::from(not_empty), true)
    }

    fn unset_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: &Value,
    ) -> Result<(), Thrown> {
        if crate::overridden(req, object, b"offsetunset") {
            let answered = call_method(req, &object, b"offsetUnset", &[*offset])?;
            answered.give_up(req);
            return Ok(());
        }
        unset(req, object, offset)
    }

    fn count_elements(&self, req: &mut Request, object: PhpObject) -> Result<Option<i64>, Thrown> {
        if crate::overridden(req, object, b"count") {
            let answered = call_method(req, &object, b"count", &[])?;
            let n = to_long(req, &answered);
            answered.give_up(req);
            let n = n?;
            return Ok(Some(n));
        }
        let n = table(req, object)?.len(req)?;
        Ok(Some(i64::try_from(n).unwrap_or(i64::MAX)))
    }

    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        how: PropertyRead,
    ) -> Result<Owner<Value>, Thrown> {
        if as_props(req, object, name)? {
            let member = crate::string_value(req, name)?;
            let fetch = match how {
                PropertyRead::Warning => DimensionRead::Warning,
                PropertyRead::Silent => DimensionRead::Silent,
                PropertyRead::Write => DimensionRead::Write,
                PropertyRead::Unset => DimensionRead::Unset,
            };
            let read = self.read_dimension(req, object, Some(&member), fetch);
            member.give_up(req);
            return read;
        }
        Std.read_property(req, object, name, how)
    }

    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let props = match as_props(req, object, name.as_bytes()) {
            Ok(props) => props,
            Err(thrown) => {
                name.give_up(req);
                value.give_up(req);
                return Err(thrown);
            }
        };
        if props {
            // The name an `ArrayObject` writes a property under is the
            // offset's, a view; the slot gives the value up.
            let member = Value::string(*name);
            let written = self.write_dimension(req, object, Some(&member), value);
            name.give_up(req);
            return written;
        }
        Std.write_property(req, object, name, value)
    }

    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<PropertyPlace, Thrown> {
        if as_props(req, object, name).unwrap_or(false) {
            return Ok(PropertyPlace::Handlers);
        }
        Std.get_property_ptr_ptr(req, object, name)
    }

    /// `spl_array_has_property`: under `ARRAY_AS_PROPS` a name the
    /// object has no property of is an element, asked as `isset`,
    /// `empty` or `property_exists()` asks, the last answered by the
    /// key alone.
    ///
    /// ```php
    /// $o = new ArrayObject(['n' => null, 'z' => 0], ArrayObject::ARRAY_AS_PROPS);
    /// var_dump(property_exists($o, 'n'), isset($o->n), empty($o->z), property_exists($o, 'x'));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// ```
    fn has_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        if as_props(req, object, name)? {
            let member = crate::string_value(req, name)?;
            // `has_set_exists`: `ZEND_PROPERTY_ISSET`, `ZEND_PROPERTY_NOT_EMPTY`
            // and `ZEND_PROPERTY_EXISTS`, which `property_exists()` asks
            // and which a `null` element answers.
            let how = match test {
                PropertyTest::Isset => 0,
                PropertyTest::NotEmpty => 1,
                PropertyTest::Exists => 2,
            };
            let answer = has(req, object, &member, how, true);
            member.give_up(req);
            return answer;
        }
        Std.has_property(req, object, name, test)
    }

    fn unset_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        if as_props(req, object, name)? {
            let member = crate::string_value(req, name)?;
            let done = self.unset_dimension(req, object, &member);
            member.give_up(req);
            return done;
        }
        Std.unset_property(req, object, name)
    }

    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        let (Some(l), Some(r)) = (left.as_object(), right.as_object()) else {
            return Std.compare(req, left, right);
        };
        if l.native::<SplArrayObject>(req).is_none() || r.native::<SplArrayObject>(req).is_none() {
            return Std.compare(req, left, right);
        }
        let a = table(req, l)?.copy(req)?;
        let b = table(req, r)?.copy(req)?;
        let ordering = compare(req, &Value::array(*a), &Value::array(*b));
        a.give_up(req);
        b.give_up(req);
        match ordering? {
            Ordering::Equal => Std.compare(req, left, right),
            other => Ok(Some(other)),
        }
    }

    fn properties_overloaded(&self) -> bool {
        true
    }

    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<PropertiesFor, Thrown> {
        if flags(req, object)? & ArrayObject::STD_PROP_LIST != 0
            || !matches!(
                purpose,
                PropertyPurpose::ArrayCast | PropertyPurpose::VarExport | PropertyPurpose::Json
            )
        {
            return Std.get_properties_for(req, object, purpose);
        }
        // `spl_array_get_properties_for`: the storage itself, `GC_ADDREF`,
        // for a reader that leaves the object alone meanwhile, and a
        // `zend_array_dup` of it for the cast, which keeps what it reads.
        let rows = table(req, object)?;
        if purpose != PropertyPurpose::ArrayCast {
            return rows.walked(req).map(PropertiesFor::Table);
        }
        let rows = rows.every_row(req)?;
        let mut copy = PhpArray::new(req)?;
        for (key, value) in rows {
            if let Err(thrown) = copy.set(req, Owner::take(req, key), Owner::take(req, value)) {
                copy.give_up(req);
                return Err(thrown);
            }
        }
        Ok(PropertiesFor::Temporary(copy))
    }
}

/// `spl_array_get_debug_info`: the properties, then the storage under
/// the private name `storage` of `ArrayObject` or `ArrayIterator`.
fn spl_array_debug_info(req: &mut Request, object: PhpObject) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for (key, value) in crate::std_properties(req, object)? {
        let value = Owner::take(req, value);
        let key = Owner::take(req, key.clone());
        out.set(req, key, value)?;
    }
    let storage = match payload(req, object)?.storage {
        Storage::OwnProps => return Ok(out),
        Storage::Fresh => Value::array(laid_down(req, object)?),
        Storage::Array(array) => Value::array(array),
        Storage::Other(backing) | Storage::Object(backing) => backing.as_value(),
    };
    let storage = Owner::take(req, storage);
    let base: &[u8] = if crate::is_a(req, object.class(), b"ArrayIterator") {
        b"ArrayIterator"
    } else {
        b"ArrayObject"
    };
    let name = [b"\0".as_slice(), base, b"\0storage"].concat();
    out.set_str(req, &name, storage)?;
    Ok(out)
}

/// `spl_array_object_clone`: the flags and the iterator class copied;
/// an `ArrayObject` a copy of the table, an `ArrayIterator` reading
/// through the object it was cloned from.
fn clone_array(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    let (flags, iterator_class, storage) = {
        let p = payload(req, object)?;
        (p.flags, p.iterator_class, p.storage)
    };
    let storage = match storage {
        Storage::OwnProps => Held::OwnProps,
        _ if crate::is_a(req, object.class(), b"ArrayObject") => {
            Held::Array(table(req, object)?.copy(req)?)
        }
        _ => Held::Other(Owner::take(req, object)),
    };
    let copy = SplArrayObject {
        storage,
        flags: flags & CLONE_MASK,
        iterator_class,
        ..SplArrayObject::default()
    };
    clone_with(req, &object, copy)
}

/// `spl_array_set_array`: the storage replaced by `given`, an array held
/// as it is where the argument is its one owner (`ZVAL_COPY`) and copied
/// where it is shared (`zend_array_dup`), an `ArrayObject` or `ArrayIterator` read
/// through, and any other object's properties, each object php's
/// deprecation first. An object whose class answers `get_properties`
/// itself is refused, as `ZipArchive`'s does:
///
/// ```php
/// class Z extends ZipArchive { public $x = 1; }
/// try { $a = @new ArrayObject(new Z); } catch (Exception $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// try { @new ArrayIterator(new ZipArchive); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// try { @(new ArrayObject([]))->exchangeArray(new Z); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// $f = FFI::cdef();
/// foreach ([$f, $f->new('int'), $f->type('int'), new SensitiveParameterValue(1)] as $o) {
///     try { @new ArrayObject($o); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// InvalidArgumentException: Overloaded object of type Z is not compatible with ArrayObject
/// Overloaded object of type ZipArchive is not compatible with ArrayIterator
/// Overloaded object of type Z is not compatible with ArrayObject
/// Overloaded object of type FFI is not compatible with ArrayObject
/// Overloaded object of type FFI\CData is not compatible with ArrayObject
/// Overloaded object of type FFI\CType is not compatible with ArrayObject
/// Overloaded object of type SensitiveParameterValue is not compatible with ArrayObject
/// ```
///
/// ```php
/// $w = new ArrayObject([1]);
/// $m = memory_get_usage(); $a = new ArrayObject([1, 2]); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $b = new ArrayIterator([]); echo memory_get_usage() - $m, "\n";
/// $x = [1, 2, 3]; $x[] = 4;
/// $m = memory_get_usage(); $c = new ArrayObject($x); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $d = new IteratorIterator(new ArrayIterator([])); echo memory_get_usage() - $m, "\n";
/// $c[] = 9; var_dump(count($x), count($c));
/// ```
/// ```expect
/// 376
/// 216
/// 376
/// 504
/// int(4)
/// int(5)
/// ```
fn set_array(
    req: &mut Request,
    object: PhpObject,
    given: &Value,
    mut flags: i64,
    just_array: bool,
) -> Result<(), Thrown> {
    sorting_refused(req, object)?;
    let given = given.deref();
    let storage = if let Some(array) = given.as_array() {
        // `Z_REFCOUNT_P(array) == 1`: the argument's own array held with
        // one more owner (`ZVAL_COPY`); a shared one, and an immutable
        // one, whose count php keeps at two, copied (`zend_array_dup`).
        if given.is_refcounted() && given.refcount() == 1 {
            Held::Array(Owner::take(req, array))
        } else {
            Held::Array(array.copy(req)?)
        }
    } else if let Some(other) = given.as_object() {
        let what = if crate::is_a(req, object.class(), b"ArrayIterator") {
            "ArrayIterator"
        } else {
            "ArrayObject"
        };
        php_ext::raising::docref(
            req,
            Level::Deprecated,
            &format!(
                "Using an object as a backing array for {what} is deprecated, as it allows violating class constraints and invariants"
            ),
        );
        if other.native::<SplArrayObject>(req).is_some() {
            if just_array {
                flags = payload(req, other)?.flags & !INT_MASK;
            }
            if other == object {
                Held::OwnProps
            } else {
                Held::Other(Owner::take(req, other))
            }
        } else {
            if other.handlers(req).properties_overloaded() {
                let (of, into) = (
                    crate::class_name(req, other.class()),
                    crate::class_name(req, object.class()),
                );
                return Err(throw(
                    req,
                    "InvalidArgumentException",
                    &format!(
                        "Overloaded object of type {} is not compatible with {}",
                        String::from_utf8_lossy(&of),
                        String::from_utf8_lossy(&into)
                    ),
                ));
            }
            if crate::is_a(req, other.class(), b"UnitEnum") {
                let class =
                    String::from_utf8_lossy(&crate::class_name(req, object.class())).into_owned();
                return Err(throw(
                    req,
                    "InvalidArgumentException",
                    &format!("Enums are not compatible with {class}"),
                ));
            }
            Held::Object(Owner::take(req, other))
        }
    } else {
        return Ok(());
    };
    let old = with(req, object, |p| {
        p.flags |= flags;
        std::mem::replace(&mut p.storage, storage)
    })?;
    let forgot = forget_position(req, object);
    old.give_up(req);
    forgot
}

/// `Z_PARAM_ARRAY_OR_OBJECT` over `given`, the argument at `position`.
fn array_or_object(req: &mut Request, given: Value, position: u32) -> Result<Value, Thrown> {
    if given.as_array().is_some() || given.as_object().is_some() {
        return Ok(given);
    }
    let name = php_ext::coerce::value_name(req, &given);
    Err(php_ext::raising::arg_type_error(
        req,
        position,
        &format!("be of type array, {name} given"),
    ))
}

/// `Z_PARAM_CLASS` of a class implementing `ArrayIterator`, php's
/// `C` specifier under the stub's type.
fn iterator_class_argument(
    req: &mut Request,
    name: PhpString,
    position: u32,
) -> Result<ClassId, Thrown> {
    let found = autoload(req, name.as_bytes())?;
    match found {
        Some(class) if crate::is_a(req, class, b"ArrayIterator") => Ok(class),
        _ => {
            let shown = String::from_utf8_lossy(name.as_bytes()).into_owned();
            let tail = format!("be a class name derived from ArrayIterator, {shown} given");
            Err(php_ext::raising::arg_type_error(req, position, &tail))
        }
    }
}

/// `spl_array_method`: the table handed by reference to the native of
/// the same name, the sort counted meanwhile so a write from a callback
/// is refused, and what the native left written back.
fn sort_through(
    req: &mut Request,
    this: PhpObject,
    native: &[u8],
    extra: Option<Value>,
) -> Result<True, Thrown> {
    let name = crate::string_value(req, native)?;
    let callable: Option<Callable> = callable_checked(req, &name).ok();
    name.give_up(req);
    let Some(callable) = callable else {
        let shown = String::from_utf8_lossy(native).into_owned();
        return Err(throw_error(
            req,
            "Error",
            &format!("Cannot call method {shown} when function {shown} is disabled"),
        ));
    };
    let table = table(req, this)?;
    let handed = match table {
        Table::Array(_, array) => Owner::take(req, array),
        Table::Props(_) => table.copy(req)?,
    };
    let cell = PhpRef::new(req, handed.into_value())?;
    let mut arguments = vec![cell.as_value()];
    arguments.extend(extra);
    with(req, this, |p| p.sorting += 1)?;
    let answered = call(req, &callable, &arguments);
    with(req, this, |p| p.sorting -= 1)?;
    let sorted = cell.separate_array(req);
    let put = match sorted {
        Ok(Some(sorted)) => put_back(req, table, sorted),
        Ok(None) => Ok(()),
        Err(thrown) => Err(thrown),
    };
    cell.give_up(req);
    put?;
    answered?.give_up(req);
    Ok(True)
}

/// `spl_array_method`'s write-back, `*ht_ptr = Z_ARRVAL_P(ht_zv)`: the
/// sorted array made the storage, or, over an object, the object's
/// property table, which from then on holds its declared rows as the
/// copies the sort moved with the rest, the slots going on apart from
/// them, as php's table holds the plain values `zend_array_dup` left
/// when the sort separated it.
///
/// ```php
/// class C { public $prop1 = 'x'; public $prop2 = 'z'; private $prop3 = 'a'; public $prop4 = 'x'; }
/// $c = new C; $c->d = 'b';
/// $ao = @new ArrayObject($c);
/// $ao->ksort();
/// $c->prop1 = 'changed';
/// var_dump($c, $c->prop1, $ao['prop1']);
/// ```
/// ```expect
/// Deprecated: Creation of dynamic property C::$d is deprecated in %s on line 3
/// object(C)#1 (5) {
///   ["prop3":"C":private]=>
///   string(1) "a"
///   ["d"]=>
///   string(1) "b"
///   ["prop1"]=>
///   string(1) "x"
///   ["prop2"]=>
///   string(1) "z"
///   ["prop4"]=>
///   string(1) "x"
/// }
/// string(7) "changed"
/// string(1) "x"
/// ```
fn put_back(req: &mut Request, table: Table, sorted: PhpArray) -> Result<(), Thrown> {
    let sorted = Owner::take(req, sorted);
    match table {
        Table::Array(owner, _) => {
            let old = with(req, owner, |p| {
                std::mem::replace(&mut p.storage, Held::Array(sorted))
            })?;
            old.give_up(req);
            Ok(())
        }
        Table::Props(object) => object.replace_table(req, sorted),
    }
}

/// The storage as `serialize` and `__serialize` write it: the array or
/// the object read through, one more owner.
fn storage(req: &mut Request, object: PhpObject) -> Result<Owner<Value>, Thrown> {
    let value = match payload(req, object)?.storage {
        Storage::Other(backing) | Storage::Object(backing) => backing.as_value(),
        Storage::Array(array) => Value::array(array),
        Storage::Fresh | Storage::OwnProps => return Ok(PhpArray::new(req)?.into_value()),
    };
    Ok(Owner::take(req, value))
}

/// The body of `ArrayObject::unserialize` over one reader: the members
/// once the record is read, or the offset php's `outexcept` reports, a
/// read that threw going there too; a refusal of the storage is left in
/// `pending`. Each value read is the
/// var hash's, php's `var_tmp_var`, so a later `R:` may name it.
fn read_record(
    req: &mut Request,
    this: PhpObject,
    reader: &mut php_ext_standard::Unserializer<'_>,
    data: &[u8],
    pending: &mut Option<Thrown>,
) -> Result<Record, Thrown> {
    if !data.starts_with(b"x:") {
        return Ok(Record::Broke(0));
    }
    reader.set_offset(2);
    let Some(flags) = reader.value(req)? else {
        return Ok(Record::Broke(reader.offset()));
    };
    let Some(flags) = flags.as_int() else {
        return Ok(Record::Broke(reader.offset()));
    };
    // `--p` back onto the `;` the flags' own value ended with.
    let at = reader.offset().saturating_sub(1);
    if data.get(at) != Some(&b';') {
        return Ok(Record::Broke(at));
    }
    if flags & IS_SELF != 0 {
        set_flags_and_self(req, this, flags)?;
    } else {
        if !matches!(data.get(reader.offset()), Some(b'a' | b'O' | b'C' | b'r')) {
            return Ok(Record::Broke(reader.offset()));
        }
        let Some(stored) = reader.value(req)? else {
            return Ok(Record::Broke(reader.offset()));
        };
        if stored.as_array().is_none() && stored.as_object().is_none() {
            return Ok(Record::Broke(reader.offset()));
        }
        set_flags_and_self(req, this, flags)?;
        if let Err(thrown) = set_array(req, this, &stored, 0, true) {
            *pending = Some(thrown);
        }
        if data.get(reader.offset()) != Some(&b';') {
            return Ok(Record::Broke(reader.offset()));
        }
        reader.set_offset(reader.offset() + 1);
    }
    let at = reader.offset();
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

/// The flags an unserialization read, `ar_flags &= ~SPL_ARRAY_CLONE_MASK;
/// ar_flags |= flags & SPL_ARRAY_CLONE_MASK`: the user's bits written,
/// and `SPL_ARRAY_IS_SELF` among them made the storage, whatever the
/// object held before given up.
fn set_flags_and_self(req: &mut Request, object: PhpObject, flags: i64) -> Result<(), Thrown> {
    let own = flags & IS_SELF != 0;
    let old = with(req, object, |p| {
        p.flags = (p.flags & !CLONE_MASK) | (flags & CLONE_MASK & !IS_SELF);
        own.then(|| std::mem::replace(&mut p.storage, Held::OwnProps))
    })?;
    if let Some(old) = old {
        old.give_up(req);
    }
    Ok(())
}

fn is_array_iterator(req: &mut Request, class: ClassId) -> bool {
    lookup_class(req, b"ArrayIterator").ok().flatten() == Some(class)
}

/// `spl_array_get_pos_ptr`: the table the object reads and the position
/// it stands at there, laid down at the first element where the object
/// has none (`spl_array_create_ht_iter`). An array a hash iterator is
/// registered at is the object's alone first, as php's storage is from
/// `spl_array_set_array`'s `zend_array_dup` on, so the writes that move
/// its buckets are the ones through this object and move the iterator.
fn position(req: &mut Request, object: PhpObject) -> Result<(Table, u32), Thrown> {
    let table = table(req, object)?;
    let held = payload(req, object)?.position;
    match (table, held) {
        (Table::Array(_, array), Position::Hash(iterator)) => {
            Ok((table, iterator.position(req, &array)))
        }
        (Table::Props(_), Position::Row(row)) => Ok((table, row)),
        (Table::Array(owner, _), _) => {
            forget_position(req, object)?;
            let array = separated(req, owner)?;
            let iterator = HashIterator::new(req, &array, 0);
            with(req, object, |p| p.position = Position::Hash(iterator))?;
            Ok((Table::Array(owner, array), 0))
        }
        (Table::Props(_), _) => {
            forget_position(req, object)?;
            with(req, object, |p| p.position = Position::Row(0))?;
            Ok((table, 0))
        }
    }
}

/// `*spl_array_get_pos_ptr(...) = at`.
fn set_position(req: &mut Request, object: PhpObject, at: u32) -> Result<(), Thrown> {
    match position(req, object)?.0 {
        Table::Array(..) => {
            if let Position::Hash(iterator) = payload(req, object)?.position {
                iterator.set_position(req, at);
            }
        }
        Table::Props(_) => with(req, object, |p| p.position = Position::Row(at))?,
    }
    Ok(())
}

/// `zend_hash_iterator_del(intern->ht_iter)`, `ht_iter = -1`: the
/// position given up, where the storage is replaced or the object freed.
fn forget_position(req: &mut Request, object: PhpObject) -> Result<(), Thrown> {
    let held = with(req, object, |p| std::mem::take(&mut p.position))?;
    if let Position::Hash(iterator) = held {
        iterator.release(req);
    }
    Ok(())
}

/// The element at this object's position, with the bucket it stands at.
fn here(req: &mut Request, object: PhpObject) -> Result<Option<(u32, Key, Value)>, Thrown> {
    let (table, pos) = position(req, object)?;
    let found = table.at(req, pos)?;
    if let Some((at, _, _)) = found
        && at != pos
    {
        set_position(req, object, at)?;
    }
    Ok(found)
}

/// `spl_array_next_ex`: the position one past the element it stands at.
fn step(req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
    match here(req, object)? {
        Some((at, _, _)) => {
            set_position(req, object, at.saturating_add(1))?;
            Ok(here(req, object)?.is_some())
        }
        None => Ok(false),
    }
}

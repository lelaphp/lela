//! `spl_fixedarray.c`: `SplFixedArray`, a fixed row of elements indexed
//! from zero (`spl_fixedarray`), with php's offset rules
//! (`spl_offset_convert_to_ulong`), its `OutOfBoundsException` for an
//! index outside the row and its `Error` for `[]`, and the elements
//! shown before the properties wherever the properties are read
//! (`spl_fixedarray_object_get_properties_for`). `getIterator()` hands
//! out Zend's `InternalIterator`, which walks the elements through
//! [`Handlers::iterator_element`].

use php_ext::abi::{
    CloneObj, DimensionRead, GcTable, Instance, IteratorElement, ObjectHandlers, PropertiesFor,
    PropertyPurpose, Std, This, True, Untyped,
};
use php_ext::attr::php_class;
use php_ext::call::{call_method, clone_with, internal_iterator, new_object};
use php_ext::coerce::{to_long, truthy};
use php_ext::raising::{
    Thrown, arg_value_error, illegal_container_offset, throw, throw_error, throw_exception_ex,
    warning,
};
use php_ext::request::Request;
use php_ext::values::walk::{self};
use php_ext::values::{Emalloc, Key, Owner, PhpArray, PhpObject, Value, ValueRef};

use crate::throwables::Throwables;
use crate::{ArrayAccess, Countable, Iterator, IteratorAggregate, JsonSerializable};

/// `spl_fixedarray_object`'s row of elements.
///
/// ```php
/// $w = new SplFixedArray(3);
/// $m = memory_get_usage(); $a = new SplFixedArray(3); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $b = new SplFixedArray(0); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $a->setSize(10); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $a->setSize(0); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $c = SplFixedArray::fromArray([1, 2, 3, 4, 5]); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $d = SplFixedArray::fromArray([7 => 1], true); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $e = clone $c; echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 144
/// 96
/// 112
/// -160
/// 176
/// 224
/// 176
/// ```
#[derive(Default)]
pub(crate) struct RowState {
    elements: Vec<Owner<Value>>,
    /// `array->elements` as php allocates it on the request's heap,
    /// `sizeof(zval)` an element, and none for an empty row (design
    /// §5.13).
    allocation: Option<Emalloc>,
    /// `cached_resize`: the size a resize under way was last asked for,
    /// by itself or by a destructor it served; `None` while none is
    /// under way.
    resizing: Option<usize>,
}

/// The payload: `spl_fixedarray_object`'s fields, boxed, inside the `sizeof` php
/// allocates (design §5.13).
pub(crate) type Row = Box<RowState>;

/// `spl_fixedarray_resize`: the row cut or grown to `size`. The
/// elements cut are given up after the row has its new size, from the
/// first cut to the last (`spl_fixedarray_dtor_range`), or from the
/// last to the first where the row is emptied (`spl_fixedarray_dtor`);
/// a destructor they serve that resizes the row again is remembered and
/// served once the cut is done, the last size it asked for winning.
///
/// ```php
/// class Again { function __construct(public $a, public $n) {} function __destruct() { echo "dtor {$this->n}\n"; $this->a->setSize($this->n); } }
/// class Log { function __construct(public $n) {} function __destruct() { echo "log {$this->n}\n"; } }
/// $a = new SplFixedArray(4);
/// $a[1] = new Log(1); $a[2] = new Again($a, 3); $a[3] = new Log(3);
/// $a->setSize(1);
/// var_dump($a->getSize());
/// $a->setSize(0);
/// ```
/// ```expect
/// log 1
/// dtor 3
/// log 3
/// int(3)
/// ```
///
/// A destructor the clear serves finds the row under way, not unlaid, so
/// `__construct()` and `__unserialize()` lay nothing over it, and a
/// `setSize()` it asks is served after
/// (`SplFixedArray_setSize_destruct_reinit_during_clear.phpt`):
///
/// ```php
/// class R { public static $fn; function __destruct() { if ($f = self::$fn) { self::$fn = null; $f(); } } }
/// foreach ([fn ($a) => $a->__construct(5), fn ($a) => $a->setSize(3), fn ($a) => $a->__unserialize(["a", "b", "c"])] as $act) {
///     $a = new SplFixedArray(2); $a[0] = new R; $a[1] = "tail";
///     R::$fn = fn () => $act($a);
///     $a->setSize(0);
///     echo $a->getSize(), "\n";
/// }
/// ```
/// ```expect
/// 0
/// 3
/// 0
/// ```
fn resize(req: &mut Request, object: PhpObject, size: usize) -> Result<(), Thrown> {
    let (len, under_way) = with(req, object, |a| (a.elements.len(), a.resizing.is_some()))?;
    if size == len {
        return Ok(());
    }
    if under_way {
        return with(req, object, |a| a.resizing = Some(size));
    }
    // `safe_ferealloc` before the row grows: the overflow's fatal and the
    // memory limit's come before a slot is laid down, and where the limit
    // refused the growth the elements stand as they were (`RETURN_OOM()`);
    // the allocation that charged them is left to the request's end, which
    // the limit's fatal brings at the next check, where php keeps its
    // untouched block.
    if size > len && fit(req, object, size)?.is_err() {
        return Ok(());
    }
    let fill = nulls(size.saturating_sub(len));
    let cut = with(req, object, |a| {
        a.resizing = Some(size);
        if size < a.elements.len() {
            let cut = a.elements.split_off(size);
            a.elements.extend(fill);
            cut
        } else {
            a.elements.extend(fill);
            Vec::new()
        }
    })?;
    if size < len {
        // A row cut is never refused; were it, the row would stand as
        // it was, as a refused growth leaves it.
        match fit(req, object, size)? {
            Ok(()) | Err(Refused) => {}
        }
    }
    if size == 0 {
        crate::give_up_all(req, cut.into_iter().rev());
    } else {
        crate::give_up_all(req, cut);
    }
    let asked = with(req, object, |a| a.resizing.take())?.unwrap_or(size);
    if asked != size {
        return resize(req, object, asked);
    }
    Ok(())
}

/// `spl_handler_SplFixedArray`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

/// `class SplFixedArray implements IteratorAggregate, ArrayAccess,
/// Countable, JsonSerializable`.
///
/// ```php
/// $a = new SplFixedArray(3);
/// $a[0] = 'x'; $a['1'] = 'y';
/// var_dump($a);
/// foreach ($a as $k => $v) echo "$k=", var_export($v, true), ";";
/// echo "\n", count($a), " ", $a->count(), " ", $a->getSize(), "\n";
/// try { $a[3] = 1; } catch (RuntimeException $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// try { $a[] = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $a->offsetSet(2, 'z'); $a->offsetUnset(0); echo get_class($a->getIterator()), " ", implode(',', $a->toArray()), " ", json_encode($a->jsonSerialize()), "\n";
/// $b = new SplFixedArray; $b->__unserialize([7, 8]); var_dump($b->offsetGet(1));
/// $w = unserialize('O:13:"SplFixedArray":2:{i:0;s:1:"p";i:1;s:1:"q";}'); var_dump($w->toArray());
/// $w->__wakeup();
/// ```
/// ```expect
/// object(SplFixedArray)#1 (3) {
///   [0]=>
///   string(1) "x"
///   [1]=>
///   string(1) "y"
///   [2]=>
///   NULL
/// }
/// 0='x';1='y';2=NULL;
/// 3 3 3
/// OutOfBoundsException: Index invalid or out of range
/// [] operator not supported for SplFixedArray
/// InternalIterator ,y,z [null,"y","z"]
/// int(8)
/// array(2) {
///   [0]=>
///   string(1) "p"
///   [1]=>
///   string(1) "q"
/// }
///
/// Deprecated: Method SplFixedArray::__wakeup() is deprecated since 8.4, this method is obsolete, as serialization hooks are provided by __unserialize() and __serialize() in %s on line 12
/// ```
///
/// `zend_object_alloc(sizeof(spl_fixedarray_object))`, which
/// `memory_get_usage()` counts (design §5.13):
///
/// ```php
/// foreach ([false, true] as $shown) foreach (['SplFixedArray'] as $c) {
///     $r = new ReflectionClass($c);
///     $m = memory_get_usage();
///     $o = $r->newInstanceWithoutConstructor();
///     $used = memory_get_usage() - $m;
///     $o = null;
///     if ($shown) echo $c, ' ', $used, "\n";
/// }
/// ```
/// ```expect
/// SplFixedArray 96
/// ```
#[php_class(
    implements(IteratorAggregate, ArrayAccess, Countable, JsonSerializable),
    size = 88, payload = Row,
    handlers = HANDLERS
)]
pub(crate) struct SplFixedArray;

#[php_class]
impl SplFixedArray {
    /// `__construct(int $size = 0)`: `size` nulls, nothing on a second call.
    ///
    /// ```php
    /// try { new SplFixedArray(-1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// $a = new SplFixedArray(2); $a->__construct(5); var_dump($a->getSize());
    /// ```
    /// ```expect
    /// SplFixedArray::__construct(): Argument #1 ($size) must be greater than or equal to 0
    /// int(2)
    /// ```
    ///
    /// The row is asked for fallibly since 8.5.12: one the memory limit has
    /// no room for is refused and the limit's fatal raised as the call
    /// returns. Nor does a constructor called while a resize's clear is under
    /// way lay a row down:
    ///
    /// ```php
    /// ini_set('memory_limit', '4M');
    /// $a = new SplFixedArray(600000);
    /// var_dump("unreached");
    /// ```
    /// ```expect exit=255
    ///
    /// Fatal error: Allowed memory size of 4194304 bytes exhausted (tried to allocate 9601024 bytes) in %s on line 3
    /// ```
    ///
    /// ```php
    /// class D { public static $a; function __destruct() { self::$a->__construct(5); } }
    /// $a = new SplFixedArray(2); D::$a = $a; $a[0] = new D;
    /// $a->setSize(0);
    /// var_dump($a->getSize());
    /// ```
    /// ```expect exit=0
    /// int(0)
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = 0)] size: i64,
    ) -> Result<(), Thrown> {
        let this = this.object();
        if size < 0 {
            return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
        }
        // Nothing on a second call, nor while a resize is under way,
        // whose clear empties the row before its destructors run
        // (`spl_fixedarray_resize_in_progress`, 8.5.12).
        if !with(req, this, |a| a.elements.is_empty() && a.resizing.is_none())? {
            return Ok(());
        }
        let size = usize::try_from(size).unwrap_or(0);
        let Ok(allocation) = laid(req, size)? else {
            return Ok(()); // RETURN_OOM()
        };
        let elements = nulls(size);
        lay(req, this, elements, allocation)
    }

    /// `__wakeup(): void`, deprecated: the properties an old serialization
    /// left become the elements.
    ///
    /// The row is asked for fallibly since 8.5.12, before the properties are
    /// copied, so one the memory limit refuses leaves them standing and the
    /// limit's fatal is raised as the call returns:
    ///
    /// ```php
    /// ini_set('memory_limit', '16M');
    /// $a = new SplFixedArray();
    /// for ($i = 0; $i < 128 * 1024; $i++) { @$a->{$i} = 0; }
    /// $pad = str_repeat('a', max(16 * 1024 * 1024 - memory_get_usage(true) - 512 * 1024, 1));
    /// @$a->__wakeup();
    /// var_dump("unreached");
    /// ```
    /// ```expect exit=255
    ///
    /// Fatal error: Allowed memory size of 16777216 bytes exhausted (tried to allocate 2097152 bytes) in %s on line 6
    /// ```
    #[php(
        tentative,
        deprecated(
            since = "8.4",
            message = "this method is obsolete, as serialization hooks are provided by __unserialize() and __serialize()"
        )
    )]
    fn __wakeup(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = this.object();
        if with(req, this, |a| a.elements.is_empty() && a.resizing.is_none())? {
            let table = Std.get_properties(req, this)?;
            let props: Vec<(Key, Value)> = walk::rows(req, table).collect();
            // `spl_fixedarray_init` before the properties are copied and
            // cleaned, as php orders them: where the memory limit refused
            // the row, RETURN_OOM() with the properties standing.
            let Ok(allocation) = laid(req, props.len())? else {
                return Ok(());
            };
            let mut elements = Vec::with_capacity(props.len());
            for (key, value) in props {
                elements.push(Owner::take(req, value.deref()));
                if let Err(thrown) = this.unset_row(req, &key) {
                    crate::give_up_all(req, elements);
                    if let Some(allocation) = allocation {
                        allocation.free(req);
                    }
                    return Err(thrown);
                }
            }
            lay(req, this, elements, allocation)?;
        }
        Ok(())
    }

    /// `__serialize(): array`: the elements, then the properties by name.
    ///
    /// ```php
    /// $a = SplFixedArray::fromArray([1, 2]);
    /// var_dump($a->__serialize());
    /// $b = new SplFixedArray; $b->__unserialize([7, 8, 9]); var_dump($b->toArray());
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(2)
    /// }
    /// array(3) {
    ///   [0]=>
    ///   int(7)
    ///   [1]=>
    ///   int(8)
    ///   [2]=>
    ///   int(9)
    /// }
    /// ```
    fn __serialize(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let this = this.object();
        let elements = with(req, this, |a| views(&a.elements))?;
        let mut out = PhpArray::new(req)?;
        for value in elements {
            out.append(req, Owner::take(req, value.deref()))?;
        }
        for (key, value) in crate::std_properties(req, this)? {
            let value = Owner::take(req, value);
            let key = Owner::take(req, key.clone());
            out.set(req, key, value)?;
        }
        Ok(out)
    }

    /// `__unserialize(array $data): void`; the witness is the class's.
    /// The elements laid down at php's size and no property table where
    /// the data holds no member, so an unserialized array costs what php's
    /// does:
    ///
    /// ```php
    /// $m = memory_get_usage(); $u = unserialize(serialize(SplFixedArray::fromArray([1, 2, 3, 4, 5])));
    /// var_dump(memory_get_usage() - $m);
    /// ```
    /// ```expect
    /// int(176)
    /// ```
    fn __unserialize(req: &mut Request, this: This<Self>, data: PhpArray) -> Result<(), Thrown> {
        let this = this.object();
        if !with(req, this, |a| a.elements.is_empty() && a.resizing.is_none())? {
            return Ok(());
        }
        let mut elements = Vec::new();
        let mut members = PhpArray::new(req)?;
        // `ZEND_HASH_FOREACH_STR_KEY_VAL(data, key, elem)`, which no code of the
        // program reaches: the rows read under the borrowed request, views
        // the argument keeps, then written.
        let rows: Vec<(Key, Value)> = walk::rows(req, data)
            .map(|(key, value)| (key, value.deref()))
            .collect();
        for (key, value) in rows {
            let value = Owner::take(req, value);
            match key {
                Key::Int(_) => elements.push(value),
                Key::Str(_) => {
                    let key = Owner::take(req, key.clone());
                    if let Err(thrown) = members.set(req, key, value) {
                        crate::give_up_all(req, elements);
                        members.give_up(req);
                        return Err(thrown);
                    }
                }
            }
        }
        match lay_laid(req, this, elements) {
            Ok(true) => {}
            Ok(false) => {
                members.give_up(req);
                return Ok(()); // RETURN_OOM()
            }
            Err(thrown) => {
                members.give_up(req);
                return Err(thrown);
            }
        }
        let loaded = crate::load_members(req, this, &Value::array(*members));
        members.give_up(req);
        loaded
    }

    /// `count(): int`, and `getSize(): int`.
    #[php(tentative)]
    fn count(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let n = with(req, this.object(), |a| a.elements.len())?;
        Ok(i64::try_from(n).unwrap_or(i64::MAX))
    }

    /// `toArray(): array`, and `jsonSerialize(): array`.
    ///
    /// ```php
    /// $a = new SplFixedArray(2); $a[1] = 'b';
    /// var_dump($a->toArray());
    /// echo json_encode($a), "\n";
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   NULL
    ///   [1]=>
    ///   string(1) "b"
    /// }
    /// [null,"b"]
    /// ```
    #[php(tentative)]
    fn toArray(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let elements = with(req, this.object(), |a| views(&a.elements))?;
        if elements.is_empty() {
            return Ok(Owner::take(req, PhpArray::empty()));
        }
        let mut out = PhpArray::new(req)?;
        for value in elements {
            out.append(req, Owner::take(req, value.deref()))?;
        }
        Ok(out)
    }

    /// `fromArray(array $array, bool $preserveKeys = true): SplFixedArray`,
    /// a class method: the keys kept as indexes, the holes null, or the values in
    /// order.
    ///
    /// ```php
    /// var_dump(SplFixedArray::fromArray([3 => 'd', 1 => 'b'])->toArray());
    /// var_dump(SplFixedArray::fromArray([3 => 'd', 1 => 'b'], false)->toArray());
    /// try { SplFixedArray::fromArray(['k' => 1]); } catch (InvalidArgumentException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// array(4) {
    ///   [0]=>
    ///   NULL
    ///   [1]=>
    ///   string(1) "b"
    ///   [2]=>
    ///   NULL
    ///   [3]=>
    ///   string(1) "d"
    /// }
    /// array(2) {
    ///   [0]=>
    ///   string(1) "d"
    ///   [1]=>
    ///   string(1) "b"
    /// }
    /// array must contain only positive integer keys
    /// ```
    ///
    /// The row is asked for fallibly since 8.5.12, as the constructor's is.
    ///
    /// ```php
    /// ini_set('memory_limit', '4M');
    /// $a = SplFixedArray::fromArray([600000 => 1]);
    /// var_dump("unreached");
    /// ```
    /// ```expect exit=255
    ///
    /// Fatal error: Allowed memory size of 4194304 bytes exhausted (tried to allocate 9601024 bytes) in %s on line 3
    /// ```
    #[php(tentative)]
    fn fromArray(
        req: &mut Request,
        array: PhpArray,
        #[php(default = true)] preserveKeys: bool,
    ) -> Result<Owner<Instance<Self>>, Thrown> {
        // `ZEND_HASH_MAP_FOREACH_KEY`, then `ZEND_HASH_FOREACH_NUM_KEY_VAL`
        // or `ZEND_HASH_FOREACH_VAL` with `ZVAL_COPY_DEREF`
        // (`Z/ext/spl/spl_fixedarray.c:728`, `:748`, `:758`): read only,
        // under the borrowed request, each element one owner more.
        let len = array.len();
        let mut elements: Vec<Owner<Value>>;
        let allocation;
        if preserveKeys && len > 0 {
            let mut max = Some(0usize);
            for (key, _) in walk::rows(req, array) {
                match key {
                    Key::Int(n) if n >= 0 => {
                        max = max.map(|max| max.max(usize::try_from(n).unwrap_or(usize::MAX)));
                    }
                    _ => {
                        max = None;
                        break;
                    }
                }
            }
            let Some(max) = max else {
                let ce = Throwables::of(req)?;
                return Err(throw_exception_ex(
                    req,
                    ce.invalid_argument_exception,
                    "array must contain only positive integer keys",
                ));
            };
            let Some(size) = max.checked_add(1).filter(|s| i64::try_from(*s).is_ok()) else {
                let ce = Throwables::of(req)?;
                return Err(throw_exception_ex(
                    req,
                    ce.invalid_argument_exception,
                    "integer overflow detected",
                ));
            };
            let Ok(laid) = laid(req, size)? else {
                return refused_row(req); // RETURN_OOM()
            };
            allocation = laid;
            elements = nulls(size);
            let mut nulled = Vec::new();
            for (key, value) in walk::rows(req, array) {
                if let Key::Int(n) = key
                    && let Some(slot) = usize::try_from(n).ok().and_then(|n| elements.get_mut(n))
                {
                    let value = Owner::take(req, value.deref());
                    nulled.push(std::mem::replace(slot, value));
                }
            }
            crate::give_up_all(req, nulled);
        } else {
            let Ok(laid) = laid(req, len)? else {
                return refused_row(req); // RETURN_OOM()
            };
            allocation = laid;
            elements = walk::rows(req, array)
                .map(|(_, value)| Owner::take(req, value.deref()))
                .collect();
        }
        let object = match php_ext::call::lookup_class(req, b"SplFixedArray") {
            Ok(Some(class)) => new_object(req, class),
            Ok(None) => Err(php_ext::raising::fatal(
                req,
                "InternalError: SplFixedArray::fromArray served with its class unlinked",
            )),
            Err(thrown) => Err(thrown),
        };
        let object = match object {
            Ok(object) => object,
            Err(thrown) => {
                crate::give_up_all(req, elements);
                if let Some(allocation) = allocation {
                    allocation.free(req);
                }
                return Err(thrown);
            }
        };
        lay(req, *object, elements, allocation)?;
        Ok(object.into_instance())
    }

    #[php(tentative, implementation = SplFixedArray::count)]
    fn getSize(this: This<Self>) -> i64;

    /// `setSize(int $size): true`: the row grown with nulls or cut.
    ///
    /// ```php
    /// $a = SplFixedArray::fromArray([1, 2, 3]);
    /// $a->setSize(2); var_dump($a->toArray());
    /// $a->setSize(4); var_dump(count($a), $a[3]);
    /// try { $a->setSize(-1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(2)
    /// }
    /// int(4)
    /// NULL
    /// SplFixedArray::setSize(): Argument #1 ($size) must be greater than or equal to 0
    /// ```
    ///
    /// A growth is asked for fallibly since 8.5.12: one the memory limit has
    /// no room for is refused, the row left as it was, and the limit's fatal
    /// raised as the call returns.
    ///
    /// ```php
    /// ini_set('memory_limit', '4M');
    /// $a = new SplFixedArray(10);
    /// $a->setSize(600000);
    /// var_dump("unreached");
    /// ```
    /// ```expect exit=255
    ///
    /// Fatal error: Allowed memory size of 4194304 bytes exhausted (tried to allocate 9601024 bytes) in %s on line 4
    /// ```
    #[php(tentative)]
    fn setSize(req: &mut Request, this: This<Self>, size: i64) -> Result<True, Thrown> {
        if size < 0 {
            return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
        }
        let size = usize::try_from(size).unwrap_or(usize::MAX);
        resize(req, this.object(), size)?;
        Ok(True)
    }

    /// `offsetExists($index): bool`: inside the row and not null.
    ///
    /// ```php
    /// $a = new SplFixedArray(2); $a[0] = 0;
    /// var_dump(isset($a[0]), empty($a[0]), isset($a[1]), $a->offsetExists(5), $a->offsetGet(0));
    /// unset($a[0]); var_dump($a[0]);
    /// try { $a['x']; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { $a[new stdClass] = 1; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// int(0)
    /// NULL
    /// Cannot access offset of type string on SplFixedArray
    /// Cannot access offset of type stdClass on SplFixedArray
    /// ```
    #[php(tentative)]
    fn offsetExists(
        req: &mut Request,
        this: This<Self>,
        index: Untyped<Value>,
    ) -> Result<bool, Thrown> {
        has(req, this.object(), &index.0, false)
    }

    /// `offsetGet($index): mixed`; the witness is the class's.
    #[php(tentative)]
    fn offsetGet(
        req: &mut Request,
        this: This<Self>,
        index: Untyped<Value>,
    ) -> Result<Value, Thrown> {
        read(req, this.object(), Some(&index.0))
    }

    /// `offsetSet($index, mixed $value): void`.
    #[php(tentative)]
    fn offsetSet(
        req: &mut Request,
        this: This<Self>,
        index: Untyped<Value>,
        value: Value,
    ) -> Result<(), Thrown> {
        let offset = (!matches!(index.0.as_ref(), ValueRef::Null)).then_some(index.0);
        write(req, this.object(), offset.as_ref(), value)
    }

    /// `offsetUnset($index): void`, the element set back to null.
    #[php(tentative)]
    fn offsetUnset(
        req: &mut Request,
        this: This<Self>,
        index: Untyped<Value>,
    ) -> Result<(), Thrown> {
        unset(req, this.object(), &index.0)
    }

    /// `getIterator(): Iterator`, an `InternalIterator` over the elements;
    /// the witness is the class's.
    fn getIterator(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<Iterator>>, Thrown> {
        Ok(internal_iterator(req, &this.object())?.into_instance())
    }

    #[php(implementation = SplFixedArray::toArray)]
    fn jsonSerialize(this: This<Self>) -> PhpArray;
}

fn with<R>(
    req: &mut Request,
    object: PhpObject,
    change: impl FnOnce(&mut Row) -> R,
) -> Result<R, Thrown> {
    if let Some(a) = object.native_mut::<Row>(req) {
        return Ok(change(a));
    }
    Err(php_ext::raising::fatal(
        req,
        "InternalError: an SplFixedArray method served on an object without its payload",
    ))
}

/// `spl_offset_convert_to_ulong`: the index an offset makes, `None`
/// for a negative one, which no row reaches, and the `TypeError` for an
/// offset of no index type.
fn index(req: &mut Request, offset: &Value) -> Result<Option<usize>, Thrown> {
    let n = match offset.deref().as_ref() {
        ValueRef::Int(n) => n,
        ValueRef::Str(s) => match Key::lookup(s) {
            Key::Int(n) => n,
            Key::Str(_) => return Err(illegal(req, offset)),
        },
        ValueRef::Float(f) => crate::float_key(req, f)?,
        ValueRef::Bool(b) => i64::from(b),
        ValueRef::Resource(r) => {
            let id = r.id();
            warning(
                req,
                &format!("Resource ID#{id} used as offset, casting to integer ({id})"),
            );
            id
        }
        _ => return Err(illegal(req, offset)),
    };
    Ok(usize::try_from(n).ok())
}

/// `zend_illegal_container_offset(spl_ce_SplFixedArray->name, offset,
/// BP_VAR_R)`, which `spl_offset_convert_to_long`'s caller throws for
/// every fetch.
fn illegal(req: &mut Request, offset: &Value) -> Thrown {
    illegal_container_offset(req, "SplFixedArray", offset, DimensionRead::Warning)
}

fn out_of_range(req: &mut Request) -> Thrown {
    let ce = match Throwables::of(req) {
        Ok(ce) => ce,
        Err(fatal) => return fatal,
    };
    throw(
        req,
        ce.out_of_bounds_exception,
        "Index invalid or out of range",
    )
}

/// The index an offset makes, inside the row.
fn inside(req: &mut Request, object: PhpObject, offset: &Value) -> Result<usize, Thrown> {
    let at = index(req, offset)?;
    let size = with(req, object, |a| a.elements.len())?;
    match at {
        Some(at) if at < size => Ok(at),
        _ => Err(out_of_range(req)),
    }
}

/// `spl_fixedarray_object_has_dimension_helper`.
fn has(
    req: &mut Request,
    object: PhpObject,
    offset: &Value,
    not_empty: bool,
) -> Result<bool, Thrown> {
    let at = index(req, offset)?;
    let value = with(req, object, |a| {
        at.and_then(|at| a.elements.get(at).map(|v| **v))
    })?;
    Ok(match value {
        None => false,
        Some(value) if not_empty => truthy(req, &value)?,
        Some(value) => !matches!(value.deref().as_ref(), ValueRef::Null),
    })
}

fn read(req: &mut Request, object: PhpObject, offset: Option<&Value>) -> Result<Value, Thrown> {
    let Some(offset) = offset else {
        let ce = Throwables::of(req)?;
        return Err(throw_error(
            req,
            ce.error,
            "[] operator not supported for SplFixedArray",
        ));
    };
    let at = inside(req, object, offset)?;
    with(req, object, |a| *a.elements[at])
}

fn write(
    req: &mut Request,
    object: PhpObject,
    offset: Option<&Value>,
    value: Value,
) -> Result<(), Thrown> {
    let Some(offset) = offset else {
        let ce = Throwables::of(req)?;
        return Err(throw_error(
            req,
            ce.error,
            "[] operator not supported for SplFixedArray",
        ));
    };
    let at = inside(req, object, offset)?;
    let value = Owner::take(req, value.deref());
    let old = with(req, object, |a| {
        std::mem::replace(&mut a.elements[at], value)
    })?;
    old.give_up(req);
    Ok(())
}

fn unset(req: &mut Request, object: PhpObject, offset: &Value) -> Result<(), Thrown> {
    let at = inside(req, object, offset)?;
    let old = with(req, object, |a| {
        std::mem::replace(&mut a.elements[at], Owner::null())
    })?;
    old.give_up(req);
    Ok(())
}

impl ObjectHandlers for Handlers {
    /// `spl_fixedarray_it`: the iterator and its `current`.
    fn iterator_size(&self) -> usize {
        96
    }

    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let (elements, allocation) = object
            .native_mut::<Row>(req)
            .map(|a| (std::mem::take(&mut a.elements), a.allocation.take()))
            .unwrap_or_default();
        crate::give_up_all(req, elements);
        if let Some(allocation) = allocation {
            allocation.free(req);
        }
    }

    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        if let Some(a) = object.native::<Row>(req) {
            for value in &a.elements {
                visit(**value);
            }
        }
        GcTable::Properties
    }

    fn clone_obj(&self) -> Option<CloneObj> {
        Some(clone_array)
    }

    /// `spl_fixedarray_object_read_dimension`: under `BP_VAR_IS` an
    /// index `has_dimension` says is not there is `null`, asked before
    /// an overriding `offsetGet` is served.
    fn read_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        fetch: DimensionRead,
    ) -> Result<Owner<Value>, Thrown> {
        if fetch == DimensionRead::Silent
            && let Some(at) = offset
            && !self.has_dimension(req, object, at, false)?
        {
            return Ok(Owner::null());
        }
        if crate::overridden(req, object, b"offsetget") {
            let offset = offset.copied().unwrap_or_default();
            return call_method(req, &object, b"offsetGet", &[offset]);
        }
        // The slot hands the engine an owner of what it read.
        read(req, object, offset).map(|v| Owner::take(req, v.deref()))
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
        if crate::overridden(req, object, b"offsetexists") {
            let answered = call_method(req, &object, b"offsetExists", &[*offset])?;
            let answer = truthy(req, &answered);
            answered.give_up(req);
            let answer = answer?;
            return Ok(answer);
        }
        has(req, object, offset, not_empty)
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
        let n = with(req, object, |a| a.elements.len())?;
        Ok(Some(i64::try_from(n).unwrap_or(i64::MAX)))
    }

    fn properties_overloaded(&self) -> bool {
        true
    }

    /// `spl_fixedarray_object_get_properties_for`: a table of its own,
    /// the elements and then the object's properties under the keys its
    /// table holds them by, mangled where they are protected or private;
    /// `get_object_vars()`'s the properties alone, which, read from a
    /// copy, it hands back as they stand.
    ///
    /// ```php
    /// class F extends SplFixedArray { public $a = 1; protected $b = 2; private $c = 3; }
    /// echo json_encode(get_object_vars(new F(2))), "\n";
    /// var_dump(new F(1));
    /// ```
    /// ```expect
    /// {"a":1,"\u0000*\u0000b":2,"\u0000F\u0000c":3}
    /// object(F)#1 (4) {
    ///   [0]=>
    ///   NULL
    ///   ["a"]=>
    ///   int(1)
    ///   ["b":protected]=>
    ///   int(2)
    ///   ["c":"F":private]=>
    ///   int(3)
    /// }
    /// ```
    ///
    /// Each element is an integer key, and a property named `"0"` a
    /// string one beside it, which the `(array)` cast folds as php's
    /// `zend_proptable_to_symtable` does.
    ///
    /// ```php
    /// #[AllowDynamicProperties] class A extends SplFixedArray {}
    /// $a = new A(2); $a[0] = 'e0'; $a->{'0'} = 'p0'; $a->x = 1;
    /// var_dump($a); var_export($a); echo "\n"; var_dump((array)$a, get_object_vars($a)); echo json_encode($a), "\n";
    /// ```
    /// ```expect
    /// object(A)#1 (4) {
    ///   [0]=>
    ///   string(2) "e0"
    ///   [1]=>
    ///   NULL
    ///   ["0"]=>
    ///   string(2) "p0"
    ///   ["x"]=>
    ///   int(1)
    /// }
    /// \A::__set_state(array(
    ///    0 => 'e0',
    ///    1 => NULL,
    ///    '0' => 'p0',
    ///    'x' => 1,
    /// ))
    /// array(3) {
    ///   [0]=>
    ///   string(2) "p0"
    ///   [1]=>
    ///   NULL
    ///   ["x"]=>
    ///   int(1)
    /// }
    /// array(2) {
    ///   [0]=>
    ///   string(2) "p0"
    ///   ["x"]=>
    ///   int(1)
    /// }
    /// ["e0",null]
    /// ```
    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<PropertiesFor, Thrown> {
        match elements_table(req, object, purpose)? {
            Some(table) => Ok(PropertiesFor::Temporary(table)),
            None => Std.get_properties_for(req, object, purpose),
        }
    }

    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<php_ext::abi::DebugInfo>, Thrown> {
        let table = elements_table(req, object, PropertyPurpose::Debug)?;
        Ok(table.map(Into::into))
    }

    fn iterator_element(
        &self,
        req: &mut Request,
        object: PhpObject,
        index: usize,
    ) -> Result<Option<IteratorElement>, Thrown> {
        let value = with(req, object, |a| a.elements.get(index).map(|v| **v))?;
        Ok(value.map(|value| {
            let key = Value::int(i64::try_from(index).unwrap_or(i64::MAX));
            (Owner::take(req, key), Owner::take(req, value.deref()))
        }))
    }
}

/// `sizeof(zval)`: what each element of a row takes.
const ZVAL: usize = 16;

/// The bytes of a row of `size` elements, or php's `Possible integer
/// overflow in memory allocation` fatal where they overflow, as
/// `safe_emalloc` and `safe_erealloc` refuse them.
fn row_bytes(req: &mut Request, size: usize) -> Result<usize, Thrown> {
    size.checked_mul(ZVAL).ok_or_else(|| {
        php_ext::raising::fatal(
            req,
            &format!("Possible integer overflow in memory allocation ({size} * 16 + 0)"),
        )
    })
}

/// `spl_fixedarray_init`'s `safe_femalloc(size, sizeof(zval), 0,
/// fallible: true)` (8.5.12): the allocation of a row of `size`
/// elements, made before the row is built, so the overflow's fatal and
/// the memory limit's come first; none for an empty row, and
/// [`Refused`] where the memory limit had no room for it.
fn laid(req: &mut Request, size: usize) -> Result<Result<Option<Emalloc>, Refused>, Thrown> {
    let bytes = row_bytes(req, size)?;
    if bytes == 0 {
        return Ok(Ok(None));
    }
    let laid = req.memory_mut().fallible(|req| Emalloc::new(req, bytes))?;
    Ok(laid.map(Some).ok_or(Refused))
}

/// A row's allocation the memory limit refused, php's `FAILURE` from
/// `spl_fixedarray_init`, which the native returns on as `RETURN_OOM()`.
struct Refused;

/// `fromArray`'s answer where its row was refused: `RETURN_OOM()`'s
/// value is never read, the limit's fatal raised as the call returns,
/// so an object with an empty row stands for it.
fn refused_row(req: &mut Request) -> Result<Owner<Instance<SplFixedArray>>, Thrown> {
    let Some(class) = php_ext::call::lookup_class(req, b"SplFixedArray")? else {
        return Err(php_ext::raising::fatal(
            req,
            "InternalError: SplFixedArray::fromArray served with its class unlinked",
        ));
    };
    Ok(new_object(req, class)?.into_instance())
}

/// The row `elements` laid into the object with the allocation
/// [`laid`] made for it, what the object held before given back.
fn lay(
    req: &mut Request,
    object: PhpObject,
    elements: Vec<Owner<Value>>,
    allocation: Option<Emalloc>,
) -> Result<(), Thrown> {
    let (old, held) = with(req, object, |a| {
        (
            std::mem::replace(&mut a.elements, elements),
            std::mem::replace(&mut a.allocation, allocation),
        )
    })?;
    crate::give_up_all(req, old);
    if let Some(held) = held {
        held.free(req);
    }
    Ok(())
}

/// [`lay`] with the row allocated for `elements` first, which are given
/// up where the allocation is refused: `false` where the memory limit
/// refused it, the native's `RETURN_OOM()`.
fn lay_laid(
    req: &mut Request,
    object: PhpObject,
    elements: Vec<Owner<Value>>,
) -> Result<bool, Thrown> {
    match laid(req, elements.len()) {
        Ok(Ok(allocation)) => lay(req, object, elements, allocation).map(|()| true),
        Ok(Err(Refused)) => {
            crate::give_up_all(req, elements);
            Ok(false)
        }
        Err(thrown) => {
            crate::give_up_all(req, elements);
            Err(thrown)
        }
    }
}

/// `size` slots of `null`, each its owner.
fn nulls(size: usize) -> Vec<Owner<Value>> {
    (0..size).map(|_| Owner::null()).collect()
}

/// The elements' views.
fn views(elements: &[Owner<Value>]) -> Vec<Value> {
    elements.iter().map(|value| **value).collect()
}

/// `spl_fixedarray_resize`'s allocation for a row now `size` long:
/// given back where the row empties, `spl_fixedarray_init`'s where it
/// had none, and `safe_ferealloc(..., fallible: true)`'d otherwise
/// (8.5.12); [`Refused`] where the memory limit had no room for it.
fn fit(req: &mut Request, object: PhpObject, size: usize) -> Result<Result<(), Refused>, Thrown> {
    let held = with(req, object, |a| a.allocation.take())?;
    let fitted = match held {
        Some(held) if size == 0 => {
            held.free(req);
            None
        }
        None => match laid(req, size)? {
            Ok(laid) => laid,
            Err(Refused) => return Ok(Err(Refused)),
        },
        Some(held) => {
            let bytes = row_bytes(req, size)?;
            match req.memory_mut().fallible(|req| held.resize(req, bytes))? {
                Some(fitted) => Some(fitted),
                None => return Ok(Err(Refused)),
            }
        }
    };
    with(req, object, |a| a.allocation = fitted)?;
    Ok(Ok(()))
}

/// `spl_fixedarray_object_clone`: the elements with one more owner each.
///
/// The clone's row is asked for fallibly since 8.5.12: one the memory
/// limit refuses leaves the clone empty, and the limit's fatal is raised
/// at the next check.
///
/// ```php
/// ini_set('memory_limit', '4M');
/// $a = new SplFixedArray(150000);
/// $b = clone $a;
/// var_dump("unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 4194304 bytes exhausted (tried to allocate 2408448 bytes) in %s on line 3
/// ```
fn clone_array(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    let elements = with(req, object, |a| views(&a.elements))?;
    let elements: Vec<Owner<Value>> = elements
        .into_iter()
        .map(|value| Owner::take(req, value))
        .collect();
    let (elements, allocation) = match laid(req, elements.len()) {
        Ok(Ok(allocation)) => (elements, allocation),
        // `spl_fixedarray_copy_ctor`'s `FAILURE` since 8.5.12: the clone
        // with an empty row, the limit's fatal raised at the next check.
        Ok(Err(Refused)) => {
            crate::give_up_all(req, elements);
            (Vec::new(), None)
        }
        Err(thrown) => {
            crate::give_up_all(req, elements);
            return Err(thrown);
        }
    };
    clone_with(
        req,
        &object,
        Box::new(RowState {
            elements,
            allocation,
            resizing: None,
        }),
    )
}

/// `spl_fixedarray_object_get_properties_for`'s table, a temporary one:
/// each element under its integer index (`zend_hash_next_index_insert`),
/// then each property under its name (`zend_hash_add_new`), so a
/// property named `"0"` stands beside element 0, one owner of each.
/// "The array elements are not *real properties*": what
/// `get_object_vars()` reads is the object's own table alone, so its
/// mangled names reach the reader as they stand. `None` where there is
/// neither an element nor a property, and for a serializer, which php
/// asserts never asks, the class having `__serialize`: the reader reads
/// the object's own table then.
fn elements_table(
    req: &mut Request,
    object: PhpObject,
    purpose: PropertyPurpose,
) -> Result<Option<Owner<PhpArray>>, Thrown> {
    if purpose == PropertyPurpose::Serialize {
        return Ok(None);
    }
    let elements = if purpose == PropertyPurpose::GetObjectVars {
        Vec::new()
    } else {
        with(req, object, |a| views(&a.elements))?
    };
    let props = crate::std_properties(req, object)?;
    if elements.is_empty() && props.is_empty() {
        return Ok(None);
    }
    let rows = elements
        .into_iter()
        .enumerate()
        .map(|(at, value)| (Key::Int(i64::try_from(at).unwrap_or(i64::MAX)), value))
        .chain(props);
    let mut table = PhpArray::new(req)?;
    for (key, value) in rows {
        if let Err(thrown) = table.set(req, Owner::take(req, key), Owner::take(req, value)) {
            table.give_up(req);
            return Err(thrown);
        }
    }
    Ok(Some(table))
}

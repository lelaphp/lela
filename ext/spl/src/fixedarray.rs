//! `spl_fixedarray.c`: `SplFixedArray`, a fixed row of elements indexed
//! from zero (`spl_fixedarray`), with php's offset rules
//! (`spl_offset_convert_to_ulong`), its `OutOfBoundsException` for an
//! index outside the row and its `Error` for `[]`, and the elements
//! shown before the properties wherever the properties are read
//! (`spl_fixedarray_object_get_properties_for`). `getIterator()` hands
//! out Zend's `InternalIterator`, which walks the elements through
//! [`Handlers::iterator_element`].

use php_ext::abi::{
    CloneObj, DimensionRead, Instance, ObjectHandlers, PropertyPurpose, Std, This, True, Untyped,
};
use php_ext::attr::php_class;
use php_ext::call::{call_method, clone_with, internal_iterator, new_object};
use php_ext::coerce::{to_long, truthy};
use php_ext::raising::{Thrown, arg_value_error, throw, warning};
use php_ext::request::Request;
use php_ext::values::{Emalloc, Key, PhpArray, PhpObject, Value, ValueRef};

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
    elements: Vec<Value>,
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
fn resize(req: &mut Request, object: PhpObject, size: usize) -> Result<(), Thrown> {
    let (len, under_way) = with(req, object, |a| (a.elements.len(), a.resizing.is_some()))?;
    if size == len {
        return Ok(());
    }
    if under_way {
        return with(req, object, |a| a.resizing = Some(size));
    }
    // `safe_erealloc` before the row grows: the overflow's fatal and the
    // memory limit's come before a slot is laid down.
    if size > len {
        fit(req, object, size)?;
    }
    let cut = with(req, object, |a| {
        a.resizing = Some(size);
        if size < a.elements.len() {
            a.elements.split_off(size)
        } else {
            a.elements.resize(size, Value::null());
            Vec::new()
        }
    })?;
    if size < len {
        fit(req, object, size)?;
    }
    if size == 0 {
        for value in cut.into_iter().rev() {
            req.release(value);
        }
    } else {
        for value in cut {
            req.release(value);
        }
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
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = 0)] size: i64,
    ) -> Result<(), Thrown> {
        let this = this.object();
        if size < 0 {
            return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
        }
        if !with(req, this, |a| a.elements.is_empty())? {
            return Ok(());
        }
        let size = usize::try_from(size).unwrap_or(0);
        let allocation = laid(req, size)?;
        lay(req, this, vec![Value::null(); size], allocation)
    }

    /// `__wakeup(): void`, deprecated: the properties an old serialization
    /// left become the elements.
    #[php(
        tentative,
        deprecated(
            since = "8.4",
            message = "this method is obsolete, as serialization hooks are provided by __unserialize() and __serialize()"
        )
    )]
    fn __wakeup(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let this = this.object();
        if with(req, this, |a| a.elements.is_empty())? {
            let props = Std.get_properties(req, this)?;
            let mut elements = Vec::with_capacity(props.len());
            for (name, value) in props {
                elements.push(crate::copied(req, value));
                Std.unset_property(req, this, name.as_bytes())?;
            }
            let allocation = laid(req, elements.len())?;
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
    fn __serialize(req: &mut Request, this: This<Self>) -> Result<PhpArray, Thrown> {
        let this = this.object();
        let elements = with(req, this, |a| a.elements.clone())?;
        let mut out = PhpArray::new(req)?;
        for value in elements {
            out.append(req, crate::copied(req, value))?;
        }
        for (key, value) in crate::std_properties(req, this)? {
            req.addref(&value);
            let key = crate::owned_key(req, &key);
            out.set(req, key, value)?;
        }
        Ok(out)
    }

    /// `__unserialize(array $data): void`; the witness is the class's.
    fn __unserialize(req: &mut Request, this: This<Self>, data: PhpArray) -> Result<(), Thrown> {
        let this = this.object();
        if !with(req, this, |a| a.elements.is_empty())? {
            return Ok(());
        }
        let mut elements = Vec::new();
        let mut members = PhpArray::new(req)?;
        for (key, value) in data.snapshot() {
            match key {
                Key::Int(_) => elements.push(crate::copied(req, value)),
                Key::Str(_) => {
                    req.addref(&value);
                    let key = crate::owned_key(req, &key);
                    members.set(req, key, value)?;
                }
            }
        }
        let allocation = laid(req, elements.len())?;
        lay(req, this, elements, allocation)?;
        let loaded = crate::load_members(req, this, &Value::array(members));
        req.release(Value::array(members));
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
    fn toArray(req: &mut Request, this: This<Self>) -> Result<PhpArray, Thrown> {
        let elements = with(req, this.object(), |a| a.elements.clone())?;
        let mut out = PhpArray::new(req)?;
        for value in elements {
            out.append(req, crate::copied(req, value))?;
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
    #[php(tentative)]
    fn fromArray(
        req: &mut Request,
        array: PhpArray,
        #[php(default = true)] preserveKeys: bool,
    ) -> Result<Instance<Self>, Thrown> {
        let rows = array.snapshot();
        let mut elements: Vec<Value>;
        let allocation;
        if preserveKeys && !rows.is_empty() {
            let mut max = 0usize;
            for (key, _) in &rows {
                match key {
                    Key::Int(n) if *n >= 0 => {
                        max = max.max(usize::try_from(*n).unwrap_or(usize::MAX));
                    }
                    _ => {
                        return Err(throw(
                            req,
                            "InvalidArgumentException",
                            "array must contain only positive integer keys",
                        ));
                    }
                }
            }
            let Some(size) = max.checked_add(1).filter(|s| i64::try_from(*s).is_ok()) else {
                return Err(throw(
                    req,
                    "InvalidArgumentException",
                    "integer overflow detected",
                ));
            };
            allocation = laid(req, size)?;
            elements = vec![Value::null(); size];
            for (key, value) in rows {
                if let Key::Int(n) = key
                    && let Some(slot) = usize::try_from(n).ok().and_then(|n| elements.get_mut(n))
                {
                    *slot = value;
                }
            }
        } else {
            allocation = laid(req, rows.len())?;
            elements = rows.into_iter().map(|(_, v)| v).collect();
        }
        let elements: Vec<Value> = elements
            .into_iter()
            .map(|v| crate::copied(req, v))
            .collect();
        let Some(class) = php_ext::call::lookup_class(req, b"SplFixedArray")? else {
            return Err(php_ext::raising::fatal(
                req,
                "InternalError: SplFixedArray::fromArray served with its class unlinked",
            ));
        };
        let object = new_object(req, class)?;
        lay(req, object, elements, allocation)?;
        Ok(Instance::of(object))
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
    fn getIterator(req: &mut Request, this: This<Self>) -> Result<Instance<Iterator>, Thrown> {
        Ok(Instance::of(internal_iterator(req, &this.object())?))
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
        ValueRef::Str(s) => match Key::from_string(*s) {
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

fn illegal(req: &mut Request, offset: &Value) -> Thrown {
    let ty = crate::zval_type_name(req, offset);
    throw(
        req,
        "TypeError",
        &format!("Cannot access offset of type {ty} on SplFixedArray"),
    )
}

fn out_of_range(req: &mut Request) -> Thrown {
    throw(req, "OutOfBoundsException", "Index invalid or out of range")
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
        at.and_then(|at| a.elements.get(at).copied())
    })?;
    Ok(match value {
        None => false,
        Some(value) if not_empty => truthy(req, &value),
        Some(value) => !matches!(value.deref().as_ref(), ValueRef::Null),
    })
}

fn read(req: &mut Request, object: PhpObject, offset: Option<&Value>) -> Result<Value, Thrown> {
    let Some(offset) = offset else {
        return Err(throw(
            req,
            "Error",
            "[] operator not supported for SplFixedArray",
        ));
    };
    let at = inside(req, object, offset)?;
    let value = with(req, object, |a| a.elements[at])?;
    Ok(crate::copied(req, value))
}

fn write(
    req: &mut Request,
    object: PhpObject,
    offset: Option<&Value>,
    value: Value,
) -> Result<(), Thrown> {
    let Some(offset) = offset else {
        return Err(throw(
            req,
            "Error",
            "[] operator not supported for SplFixedArray",
        ));
    };
    let at = inside(req, object, offset)?;
    let value = crate::copied(req, value);
    let old = with(req, object, |a| {
        std::mem::replace(&mut a.elements[at], value)
    })?;
    req.release(old);
    Ok(())
}

fn unset(req: &mut Request, object: PhpObject, offset: &Value) -> Result<(), Thrown> {
    let at = inside(req, object, offset)?;
    let old = with(req, object, |a| {
        std::mem::replace(&mut a.elements[at], Value::null())
    })?;
    req.release(old);
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
        for value in elements {
            req.release(value);
        }
        if let Some(allocation) = allocation {
            allocation.free(req);
        }
    }

    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        if let Some(a) = object.native::<Row>(req) {
            for value in &a.elements {
                visit(*value);
            }
        }
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
    ) -> Result<Value, Thrown> {
        if fetch == DimensionRead::Silent
            && let Some(at) = offset
            && !self.has_dimension(req, object, at, false)?
        {
            return Ok(Value::null());
        }
        if crate::overridden(req, object, b"offsetget") {
            let offset = offset.copied().unwrap_or_default();
            return call_method(req, &object, b"offsetGet", &[offset]);
        }
        read(req, object, offset)
    }

    fn write_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        value: Value,
    ) -> Result<(), Thrown> {
        if crate::overridden(req, object, b"offsetset") {
            let offset = offset.copied().unwrap_or_default();
            let answered = call_method(req, &object, b"offsetSet", &[offset, value]);
            req.release(value);
            req.release(answered?);
            return Ok(());
        }
        let written = write(req, object, offset, value);
        req.release(value);
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
            req.release(answered);
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
            req.release(answered);
            return Ok(());
        }
        unset(req, object, offset)
    }

    fn count_elements(&self, req: &mut Request, object: PhpObject) -> Result<Option<i64>, Thrown> {
        if crate::overridden(req, object, b"count") {
            let answered = call_method(req, &object, b"count", &[])?;
            let n = to_long(req, &answered)?;
            req.release(answered);
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
    ) -> Result<Option<Vec<(Key, Value)>>, Thrown> {
        // php asserts no serializer asks, the class having `__serialize`;
        // what reads the table under that purpose reads the object's own.
        if purpose == PropertyPurpose::Serialize {
            return Ok(None);
        }
        // "The array elements are not *real properties*": what
        // `get_object_vars()` reads is the object's own table alone, a
        // copy, so its mangled names reach the reader as they stand.
        let elements = if purpose == PropertyPurpose::GetObjectVars {
            Vec::new()
        } else {
            with(req, object, |a| a.elements.clone())?
        };
        let props = crate::std_properties(req, object)?;
        if elements.is_empty() && props.is_empty() {
            return Ok(None);
        }
        // Each element under its integer index
        // (`zend_hash_next_index_insert`), then each property under its
        // name (`zend_hash_add_new`), so a property named `"0"` stands
        // beside element 0.
        let mut out: Vec<(Key, Value)> = Vec::with_capacity(elements.len() + props.len());
        for (at, value) in elements.into_iter().enumerate() {
            out.push((Key::Int(i64::try_from(at).unwrap_or(i64::MAX)), value));
        }
        out.extend(props);
        Ok(Some(out))
    }

    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<php_ext::abi::DebugInfo>, Thrown> {
        let rows = self.get_properties_for(req, object, PropertyPurpose::Debug)?;
        let Some(rows) = rows else {
            return Ok(None);
        };
        let mut out = PhpArray::new(req)?;
        for (key, value) in rows {
            req.addref(&value);
            out.set(req, key, value)?;
        }
        Ok(Some(out.into()))
    }

    fn iterator_element(
        &self,
        req: &mut Request,
        object: PhpObject,
        index: usize,
    ) -> Result<Option<(Value, Value)>, Thrown> {
        let value = with(req, object, |a| a.elements.get(index).copied())?;
        Ok(value.map(|value| {
            (
                Value::int(i64::try_from(index).unwrap_or(i64::MAX)),
                crate::copied(req, value),
            )
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

/// `spl_fixedarray_init`'s `safe_emalloc(size, sizeof(zval), 0)`: the
/// allocation of a row of `size` elements, made before the row is
/// built, so the overflow's fatal and the memory limit's come first;
/// none for an empty row.
fn laid(req: &mut Request, size: usize) -> Result<Option<Emalloc>, Thrown> {
    let bytes = row_bytes(req, size)?;
    if bytes == 0 {
        return Ok(None);
    }
    Emalloc::new(req, bytes).map(Some)
}

/// The row `elements` laid into the object with the allocation
/// [`laid`] made for it, what the object held before given back.
fn lay(
    req: &mut Request,
    object: PhpObject,
    elements: Vec<Value>,
    allocation: Option<Emalloc>,
) -> Result<(), Thrown> {
    let old = with(req, object, |a| {
        a.elements = elements;
        std::mem::replace(&mut a.allocation, allocation)
    })?;
    if let Some(old) = old {
        old.free(req);
    }
    Ok(())
}

/// `spl_fixedarray_resize`'s allocation for a row now `size` long:
/// given back where the row empties, `spl_fixedarray_init`'s where it
/// had none, and `erealloc`'d otherwise.
fn fit(req: &mut Request, object: PhpObject, size: usize) -> Result<(), Thrown> {
    let held = with(req, object, |a| a.allocation.take())?;
    let fitted = match held {
        Some(held) if size == 0 => {
            held.free(req);
            None
        }
        None => laid(req, size)?,
        Some(held) => {
            let bytes = row_bytes(req, size)?;
            Some(held.resize(req, bytes)?)
        }
    };
    with(req, object, |a| a.allocation = fitted)
}

/// `spl_fixedarray_object_clone`: the elements with one more owner each.
fn clone_array(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    let elements = with(req, object, |a| a.elements.clone())?;
    for value in &elements {
        req.addref(value);
    }
    let allocation = laid(req, elements.len())?;
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

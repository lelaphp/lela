//! `spl_fixedarray.c`: `SplFixedArray`, a fixed row of elements indexed
//! from zero (`spl_fixedarray`), with php's offset rules
//! (`spl_offset_convert_to_ulong`), its `OutOfBoundsException` for an
//! index outside the row and its `Error` for `[]`, and the elements
//! shown before the properties wherever the properties are read
//! (`spl_fixedarray_object_get_properties_for`). `getIterator()` hands
//! out Zend's `InternalIterator`, which walks the elements through
//! [`Handlers::iterator_element`].

use php_ext::abi::{CloneObj, DimensionRead, ObjectHandlers, PropertyPurpose, Std};
use php_ext::call::{call_method, clone_with, internal_iterator, new_object};
use php_ext::coerce::{to_long, truthy};
use php_ext::raising::{Thrown, arg_value_error, throw, warning};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::Entry;
use crate::stub::{self, Declared};

/// `spl_fixedarray_object`'s row of elements.
#[derive(Default)]
pub(crate) struct SplFixedArray {
    elements: Vec<Value>,
}

/// `spl_handler_SplFixedArray`.
pub(crate) struct Handlers;

pub(crate) static HANDLERS: Handlers = Handlers;

const STUB: &str = "
class SplFixedArray implements IteratorAggregate, ArrayAccess, Countable, JsonSerializable {
    public function __construct(int $size = 0) {}
    /** @tentative-return-type */ #[\\Deprecated(since: '8.4', message: 'this method is obsolete, as serialization hooks are provided by __unserialize() and __serialize()')]
    public function __wakeup(): void {}
    public function __serialize(): array {}
    public function __unserialize(array $data): void {}
    /** @tentative-return-type */ public function count(): int {}
    /** @tentative-return-type */ public function toArray(): array {}
    /** @tentative-return-type */ public static function fromArray(array $array, bool $preserveKeys = true): SplFixedArray {}
    /** @tentative-return-type */ public function getSize(): int {}
    /** @tentative-return-type */ public function setSize(int $size): true {}
    /** @tentative-return-type */ public function offsetExists($index): bool {}
    /** @tentative-return-type */ public function offsetGet($index): mixed {}
    /** @tentative-return-type */ public function offsetSet($index, mixed $value): void {}
    /** @tentative-return-type */ public function offsetUnset($index): void {}
    public function getIterator(): Iterator {}
    public function jsonSerialize(): array {}
}
";

/// The class, each method with its entry; `jsonSerialize` is php's
/// alias of `toArray`.
///
/// ```php
/// $a = new SplFixedArray(3);
/// $a[0] = 'x'; $a['1'] = 'y';
/// var_dump($a);
/// foreach ($a as $k => $v) echo "$k=", var_export($v, true), ";";
/// echo "\n", count($a), " ", $a->getSize(), "\n";
/// try { $a[3] = 1; } catch (RuntimeException $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// try { $a[] = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
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
/// 3 3
/// OutOfBoundsException: Index invalid or out of range
/// [] operator not supported for SplFixedArray
/// ```
pub(crate) fn classes() -> Vec<Declared> {
    let rows: [(&str, Entry); 16] = [
        ("SplFixedArray::__construct", construct),
        ("SplFixedArray::__wakeup", wakeup),
        ("SplFixedArray::__serialize", serialize),
        ("SplFixedArray::__unserialize", unserialize),
        ("SplFixedArray::count", count),
        ("SplFixedArray::toArray", to_array),
        ("SplFixedArray::fromArray", from_array),
        ("SplFixedArray::getSize", count),
        ("SplFixedArray::setSize", set_size),
        ("SplFixedArray::offsetExists", offset_exists),
        ("SplFixedArray::offsetGet", offset_get),
        ("SplFixedArray::offsetSet", offset_set),
        ("SplFixedArray::offsetUnset", offset_unset),
        ("SplFixedArray::getIterator", get_iterator),
        ("SplFixedArray::jsonSerialize", to_array),
        ("", to_array),
    ];
    stub::classes(STUB, &rows)
}

fn with<R>(
    req: &mut Request,
    object: PhpObject,
    change: impl FnOnce(&mut SplFixedArray) -> R,
) -> Result<R, Thrown> {
    if let Some(a) = object.native_mut::<SplFixedArray>(req) {
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
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let elements = object
            .native_mut::<SplFixedArray>(req)
            .map(|a| std::mem::take(&mut a.elements))
            .unwrap_or_default();
        for value in elements {
            req.release(value);
        }
    }

    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        if let Some(a) = object.native::<SplFixedArray>(req) {
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

    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<Option<Vec<(PhpString, Value)>>, Thrown> {
        // "The array elements are not *real properties*": what
        // `get_object_vars()` reads is the object's own table alone.
        if purpose == PropertyPurpose::GetObjectVars {
            return Ok(None);
        }
        let elements = with(req, object, |a| a.elements.clone())?;
        let props = Std.get_properties(req, object)?;
        if elements.is_empty() && props.is_empty() {
            return Ok(None);
        }
        let mut out = Vec::with_capacity(elements.len() + props.len());
        for (at, value) in elements.into_iter().enumerate() {
            out.push((PhpString::new(req, at.to_string().as_bytes())?, value));
        }
        out.extend(props);
        Ok(Some(out))
    }

    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<PhpArray>, Thrown> {
        let rows = self.get_properties_for(req, object, PropertyPurpose::Debug)?;
        let Some(rows) = rows else {
            return Ok(None);
        };
        let mut out = PhpArray::new(req)?;
        for (name, value) in rows {
            req.addref(&value);
            out.set(req, Key::from_string(name), value)?;
        }
        Ok(Some(out))
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

/// `safe_emalloc(size, sizeof(zval), 0)`'s checks before a row of
/// `size` elements is laid down: php's `Possible integer overflow in
/// memory allocation` fatal where the bytes overflow, and the memory
/// limit's where they do not fit, both before anything is built.
fn reserve(req: &mut Request, size: usize) -> Result<(), Thrown> {
    let Some(bytes) = size.checked_mul(16) else {
        return Err(php_ext::raising::fatal(
            req,
            &format!("Possible integer overflow in memory allocation ({size} * 16 + 0)"),
        ));
    };
    if bytes > 65536 {
        let probe = PhpString::filled(req, bytes, |_| {})?;
        req.release(Value::string(probe));
    }
    Ok(())
}

/// `spl_fixedarray_object_clone`: the elements with one more owner each.
fn clone_array(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    let elements = with(req, object, |a| a.elements.clone())?;
    for value in &elements {
        req.addref(value);
    }
    clone_with(req, &object, SplFixedArray { elements })
}

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
fn construct(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 1)?;
    let this = crate::this(req)?;
    let size = crate::long_or(req, 1, 0)?;
    if size < 0 {
        return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
    }
    if !with(req, this, |a| a.elements.is_empty())? {
        return Ok(Value::null());
    }
    let size = usize::try_from(size).unwrap_or(0);
    reserve(req, size)?;
    with(req, this, |a| a.elements = vec![Value::null(); size])?;
    Ok(Value::null())
}

/// `__wakeup(): void`, deprecated: the properties an old serialization
/// left become the elements.
fn wakeup(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    if with(req, this, |a| a.elements.is_empty())? {
        let props = Std.get_properties(req, this)?;
        let mut elements = Vec::with_capacity(props.len());
        for (name, value) in props {
            elements.push(crate::copied(req, value));
            Std.unset_property(req, this, name.as_bytes())?;
        }
        with(req, this, |a| a.elements = elements)?;
    }
    Ok(Value::null())
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
fn serialize(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let elements = with(req, this, |a| a.elements.clone())?;
    let mut out = PhpArray::new(req)?;
    for value in elements {
        out.append(req, crate::copied(req, value))?;
    }
    for (name, value) in Std.get_properties(req, this)? {
        req.addref(&value);
        out.set(req, Key::Str(name), value)?;
    }
    Ok(Value::array(out))
}

/// `__unserialize(array $data): void`; the witness is `__serialize`'s.
fn unserialize(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let data = crate::array(req, 1)?;
    if !with(req, this, |a| a.elements.is_empty())? {
        return Ok(Value::null());
    }
    let mut elements = Vec::new();
    for (key, value) in data.snapshot() {
        match key {
            Key::Int(_) => elements.push(crate::copied(req, value)),
            Key::Str(name) => {
                req.addref(&value);
                req.addref(&Value::string(name));
                Std.write_property(req, this, name, value)?;
            }
        }
    }
    with(req, this, |a| a.elements = elements)?;
    Ok(Value::null())
}

/// `count(): int` and `getSize(): int`.
fn count(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let n = with(req, this, |a| a.elements.len())?;
    Ok(Value::int(i64::try_from(n).unwrap_or(i64::MAX)))
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
fn to_array(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    let elements = with(req, this, |a| a.elements.clone())?;
    let mut out = PhpArray::new(req)?;
    for value in elements {
        out.append(req, crate::copied(req, value))?;
    }
    Ok(Value::array(out))
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
fn from_array(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 2)?;
    let data = crate::array(req, 1)?;
    let keys = crate::bool_or(req, 2, true)?;
    let rows = data.snapshot();
    let mut elements: Vec<Value>;
    if keys && !rows.is_empty() {
        let mut max = 0usize;
        for (key, _) in &rows {
            match key {
                Key::Int(n) if *n >= 0 => max = max.max(usize::try_from(*n).unwrap_or(usize::MAX)),
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
        reserve(req, size)?;
        elements = vec![Value::null(); size];
        for (key, value) in rows {
            if let Key::Int(n) = key
                && let Some(slot) = usize::try_from(n).ok().and_then(|n| elements.get_mut(n))
            {
                *slot = value;
            }
        }
    } else {
        elements = rows.into_iter().map(|(_, v)| v).collect();
    }
    let elements: Vec<Value> = elements
        .into_iter()
        .map(|v| crate::copied(req, v))
        .collect();
    let Some(class) = php_ext::call::lookup_class(req, b"SplFixedArray")? else {
        return Ok(Value::null());
    };
    let object = new_object(req, class)?;
    with(req, object, |a| a.elements = elements)?;
    Ok(object.as_value())
}

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
fn set_size(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let size = crate::long(req, 1)?;
    if size < 0 {
        return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
    }
    let size = usize::try_from(size).unwrap_or(usize::MAX);
    reserve(req, size)?;
    let cut = with(req, this, |a| {
        if size < a.elements.len() {
            a.elements.split_off(size)
        } else {
            a.elements.resize(size, Value::null());
            Vec::new()
        }
    })?;
    for value in cut.into_iter().rev() {
        req.release(value);
    }
    Ok(Value::bool(true))
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
fn offset_exists(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let offset = crate::value(req, 1);
    Ok(Value::bool(has(req, this, &offset, false)?))
}

/// `offsetGet($index): mixed`; the witness is `offsetExists`'s.
fn offset_get(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let offset = crate::value(req, 1);
    read(req, this, Some(&offset))
}

/// `offsetSet($index, mixed $value): void`.
fn offset_set(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 2, 2)?;
    let this = crate::this(req)?;
    let offset = crate::value(req, 1);
    let value = crate::value(req, 2);
    let offset = (!matches!(offset.as_ref(), ValueRef::Null)).then_some(offset);
    write(req, this, offset.as_ref(), value)?;
    Ok(Value::null())
}

/// `offsetUnset($index): void`, the element set back to null.
fn offset_unset(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let this = crate::this(req)?;
    let offset = crate::value(req, 1);
    unset(req, this, &offset)?;
    Ok(Value::null())
}

/// `getIterator(): Iterator`, an `InternalIterator` over the elements;
/// the witness is the class's.
fn get_iterator(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let this = crate::this(req)?;
    Ok(internal_iterator(req, &this)?.as_value())
}

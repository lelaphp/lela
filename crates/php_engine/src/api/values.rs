//! The value group of the native face (design §4.4): Mahout's vocabulary
//! over Zend's layouts. What a native holds is a [`Value`]; the layout
//! knowledge stays in `php_heap`. This skeleton keeps values as owned Rust
//! data behind the same names, so every signature compiles and behaves
//! trivially; the heap-backed representation lands with Lane A of design
//! §11 behind these signatures.

use std::any::Any;
use std::cmp::Ordering;
use std::sync::Arc;

use crate::api::call::Function;

/// The type of a value, php's `zend_zval_type_name` set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TypeTag {
    Undef,
    Null,
    Bool,
    Int,
    Float,
    String,
    Array,
    Object,
    Resource,
    Reference,
}

/// The identity of a linked class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClassId(pub u32);

/// A byte string, refcounted and hash-cached in the heap; bytes, not UTF-8.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct PhpString {
    bytes: Vec<u8>,
}

impl PhpString {
    /// A string of these bytes.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> PhpString {
        PhpString {
            bytes: bytes.to_vec(),
        }
    }

    /// The bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The length in bytes, `strlen`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the length is zero.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Append bytes in place.
    pub fn append(&mut self, bytes: &[u8]) {
        self.bytes.extend_from_slice(bytes);
    }

    /// A new string of this one followed by `other`.
    #[must_use]
    pub fn concat(&self, other: &PhpString) -> PhpString {
        let mut bytes = self.bytes.clone();
        bytes.extend_from_slice(&other.bytes);
        PhpString { bytes }
    }
}

/// An array key after php's normalisation: an integer, or a string that
/// is not a canonical decimal integer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Int(i64),
    Str(PhpString),
}

impl Key {
    /// The key for these bytes: `Int` when they are a canonical decimal
    /// integer in range (`ZEND_HANDLE_NUMERIC_STR`), else `Str`.
    #[must_use]
    pub fn from_bytes(bytes: &[u8]) -> Key {
        if let Some(n) = canonical_int(bytes) {
            Key::Int(n)
        } else {
            Key::Str(PhpString::from_bytes(bytes))
        }
    }

    /// The key as a property name: an integer key printed in decimal.
    #[must_use]
    pub fn to_prop_name(&self) -> PhpString {
        match self {
            Key::Int(n) => PhpString::from_bytes(n.to_string().as_bytes()),
            Key::Str(s) => s.clone(),
        }
    }
}

/// `ZEND_HANDLE_NUMERIC_STR`: an optional minus, no leading zero unless
/// the number is zero, digits only, and in `i64` range.
fn canonical_int(bytes: &[u8]) -> Option<i64> {
    let (negative, digits) = match bytes {
        [b'-', rest @ ..] => (true, rest),
        _ => (false, bytes),
    };
    if digits.is_empty()
        || digits.len() > 19
        || !digits.iter().all(u8::is_ascii_digit)
        || (digits.len() > 1 && digits[0] == b'0')
        || (negative && digits == b"0")
    {
        return None;
    }
    let text = std::str::from_utf8(bytes).ok()?;
    text.parse().ok()
}

/// A PHP array: insertion-ordered, with php's `nNextFreeElement` and its
/// internal pointer. The heap's `zend_hash` copy replaces this vector.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PhpArray {
    entries: Vec<(Key, Value)>,
    next_free: i64,
    cursor: usize,
}

impl PhpArray {
    /// An empty array.
    #[must_use]
    pub fn new() -> PhpArray {
        PhpArray::default()
    }

    /// The number of elements, `count()`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no elements.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether the keys are `0..len` in order, `array_is_list()`.
    #[must_use]
    pub fn is_list(&self) -> bool {
        self.entries
            .iter()
            .enumerate()
            .all(|(i, (k, _))| matches!(k, Key::Int(n) if usize::try_from(*n) == Ok(i)))
    }

    fn position(&self, key: &Key) -> Option<usize> {
        self.entries.iter().position(|(k, _)| k == key)
    }

    /// The value at `key`.
    #[must_use]
    pub fn get(&self, key: &Key) -> Option<&Value> {
        self.position(key).map(|i| &self.entries[i].1)
    }

    /// Whether `key` is present, `array_key_exists`.
    #[must_use]
    pub fn contains(&self, key: &Key) -> bool {
        self.position(key).is_some()
    }

    /// Write `value` at `key`, in place for a present key and at the end
    /// otherwise; an integer key at or past `nNextFreeElement` moves it.
    pub fn set(&mut self, key: Key, value: Value) {
        if let Some(i) = self.position(&key) {
            self.entries[i].1 = value;
            return;
        }
        if let Key::Int(n) = key
            && n >= self.next_free
        {
            self.next_free = n.saturating_add(1);
        }
        self.entries.push((key, value));
    }

    /// Replace the value at `key` in place; `false` when it is absent.
    pub fn replace(&mut self, key: &Key, value: Value) -> bool {
        match self.position(key) {
            Some(i) => {
                self.entries[i].1 = value;
                true
            }
            None => false,
        }
    }

    /// `$a[] = value`, at `nNextFreeElement`.
    pub fn append(&mut self, value: Value) {
        let _ = self.try_append(value);
    }

    /// `$a[] = value`, or `Err(value)` when `nNextFreeElement` is past
    /// `i64::MAX`, php's "Cannot add element to the array" case.
    pub fn try_append(&mut self, value: Value) -> Result<(), Value> {
        if self.next_free == i64::MAX && self.contains(&Key::Int(i64::MAX)) {
            return Err(value);
        }
        let key = Key::Int(self.next_free);
        self.set(key, value);
        Ok(())
    }

    /// Remove `key`, returning what it held.
    pub fn unset(&mut self, key: &Key) -> Option<Value> {
        let i = self.position(key)?;
        if self.cursor > i {
            self.cursor -= 1;
        }
        Some(self.entries.remove(i).1)
    }

    /// Copy-on-write separation: a no-op while values are owned.
    pub fn separate(&mut self) {}

    /// An iterator over the elements in order.
    #[must_use]
    pub fn iter(&self) -> ArrayIter<'_> {
        ArrayIter {
            array: self,
            at: None,
        }
    }

    /// The elements in order, references followed.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(Key, Value)> {
        self.entries
            .iter()
            .map(|(k, v)| (k.clone(), v.deref().clone()))
            .collect()
    }

    /// The elements in order, references kept as references.
    #[must_use]
    pub fn snapshot_refs(&self) -> Vec<(Key, Value)> {
        self.entries.clone()
    }

    /// Sort the elements in place by `compare`; stable, as `zend_sort` is
    /// for arrays of up to sixteen elements and `sort()` guarantees since 8.0.
    pub fn sort_by(&mut self, mut compare: impl FnMut(&Value, &Value) -> Ordering) {
        self.entries.sort_by(|a, b| compare(&a.1, &b.1));
    }

    /// The array as a value.
    #[must_use]
    pub fn to_value(self) -> Value {
        Value::array(self)
    }

    /// `current()`: the element at the internal pointer.
    #[must_use]
    pub fn cursor_current(&self) -> Option<&Value> {
        self.entries.get(self.cursor).map(|(_, v)| v)
    }

    /// `key()`: the key at the internal pointer.
    #[must_use]
    pub fn cursor_key(&self) -> Option<Key> {
        self.entries.get(self.cursor).map(|(k, _)| k.clone())
    }

    /// `next()`: the internal pointer one element on.
    pub fn cursor_next(&mut self) {
        if self.cursor < self.entries.len() {
            self.cursor += 1;
        }
    }

    /// `prev()`: move the internal pointer back; past the start it points
    /// past the end, as php's does.
    pub fn cursor_prev(&mut self) {
        self.cursor = match self.cursor {
            0 => self.entries.len(),
            n => n - 1,
        };
    }

    /// `reset()`: the internal pointer to the first element.
    pub fn cursor_reset(&mut self) {
        self.cursor = 0;
    }

    /// `end()`: the internal pointer to the last element.
    pub fn cursor_end(&mut self) {
        self.cursor = self.entries.len().saturating_sub(1);
    }
}

/// A position in a [`PhpArray`]: before the first element until
/// [`ArrayIter::advance`] is called.
#[derive(Debug)]
pub struct ArrayIter<'a> {
    array: &'a PhpArray,
    at: Option<usize>,
}

impl ArrayIter<'_> {
    /// Advance one element; `false` past the end.
    pub fn advance(&mut self) -> bool {
        let at = self.at.map_or(0, |i| i + 1);
        self.at = Some(at);
        at < self.array.entries.len()
    }

    /// The key at the position.
    #[must_use]
    pub fn key(&self) -> Option<Key> {
        self.array.entries.get(self.at?).map(|(k, _)| k.clone())
    }

    /// The value at the position.
    #[must_use]
    pub fn value(&self) -> Option<&Value> {
        self.array.entries.get(self.at?).map(|(_, v)| v)
    }
}

impl<'a> IntoIterator for &'a PhpArray {
    type Item = (Key, &'a Value);
    type IntoIter = ArrayIter<'a>;

    fn into_iter(self) -> ArrayIter<'a> {
        self.iter()
    }
}

impl<'a> Iterator for ArrayIter<'a> {
    type Item = (Key, &'a Value);

    fn next(&mut self) -> Option<(Key, &'a Value)> {
        if !self.advance() {
            return None;
        }
        let (key, value) = self.array.entries.get(self.at?)?;
        Some((key.clone(), value))
    }
}

/// What a `Closure` object carries (design §5.5).
#[derive(Debug, Clone)]
pub struct ClosureInfo {
    pub function: Function,
    pub this: Option<PhpObject>,
    pub scope: Option<ClassId>,
}

/// An object (design §5.4): identity, class, declared and dynamic
/// properties, and the typed native payload of an internal class. In the
/// heap an object is a store entry reached by its id; here it is owned.
#[derive(Debug, Clone)]
pub struct PhpObject {
    id: u32,
    class: ClassId,
    props: Vec<(PhpString, Value)>,
    native: Option<Arc<dyn Any + Send + Sync>>,
    lazy: bool,
    closure: Option<Box<ClosureInfo>>,
}

impl PhpObject {
    pub(crate) fn new(id: u32, class: ClassId) -> PhpObject {
        PhpObject {
            id,
            class,
            props: Vec::new(),
            native: None,
            lazy: false,
            closure: None,
        }
    }

    /// The object as a value.
    #[must_use]
    pub fn as_value(&self) -> Value {
        Value::object(self.clone())
    }

    /// The class of the object.
    #[must_use]
    pub fn class(&self) -> ClassId {
        self.class
    }

    /// `spl_object_id()`.
    #[must_use]
    pub fn index(&self) -> u32 {
        self.id
    }

    /// Whether the object is of `class` or a subclass, `instanceof`. The
    /// skeleton knows no hierarchy: an exact class alone.
    #[must_use]
    pub fn instance_of(&self, class: ClassId) -> bool {
        self.class == class
    }

    /// What a `Closure` carries, for a closure object.
    #[must_use]
    pub fn closure(&self) -> Option<&ClosureInfo> {
        self.closure.as_deref()
    }

    /// The declared and dynamic properties in declaration order,
    /// `get_object_vars` from inside the class.
    #[must_use]
    pub fn props(&self) -> &[(PhpString, Value)] {
        &self.props
    }

    /// The properties as array entries, mangled names and all, what
    /// `(array)` produces.
    #[must_use]
    pub fn entries(&self) -> Vec<(Key, Value)> {
        self.props
            .iter()
            .map(|(name, value)| (Key::from_bytes(name.as_bytes()), value.clone()))
            .collect()
    }

    /// One property by name, without a handler in the way.
    #[must_use]
    pub fn prop(&self, name: &[u8]) -> Option<&Value> {
        self.props
            .iter()
            .find(|(n, _)| n.as_bytes() == name)
            .map(|(_, v)| v)
    }

    /// Write one property by name, without a handler in the way.
    pub fn set_prop(&mut self, name: &[u8], value: Value) {
        match self.props.iter_mut().find(|(n, _)| n.as_bytes() == name) {
            Some((_, slot)) => *slot = value,
            None => self.props.push((PhpString::from_bytes(name), value)),
        }
    }

    /// The native payload, as `T`.
    #[must_use]
    pub fn native<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.native.as_deref().and_then(|n| n.downcast_ref::<T>())
    }

    /// The native payload, mutably; `None` while another owner holds it.
    pub fn native_mut<T: Any + Send + Sync>(&mut self) -> Option<&mut T> {
        Arc::get_mut(self.native.as_mut()?).and_then(|n| n.downcast_mut::<T>())
    }

    /// Whether the object is an uninitialised lazy ghost or proxy.
    #[must_use]
    pub fn is_lazy(&self) -> bool {
        self.lazy
    }

    /// Whether `name` is a property whose read would initialise a lazy
    /// object.
    #[must_use]
    pub fn is_lazy_prop(&self, name: &[u8]) -> bool {
        self.lazy && self.prop(name).is_none()
    }

    /// The initializer of a lazy object, while it is uninitialised.
    #[must_use]
    pub fn lazy_initializer(&self) -> Option<&Value> {
        None
    }

    /// The real instance behind an initialised lazy proxy.
    #[must_use]
    pub fn lazy_real(&self) -> Option<&PhpObject> {
        None
    }
}

/// A `&$x` reference: one cell several holders share (design §5.5).
#[derive(Debug, Clone, PartialEq)]
pub struct PhpRef {
    value: Box<Value>,
}

impl PhpRef {
    /// A reference holding `value`.
    #[must_use]
    pub fn new(value: Value) -> PhpRef {
        PhpRef {
            value: Box::new(value),
        }
    }

    /// The value held.
    #[must_use]
    pub fn get(&self) -> &Value {
        &self.value
    }

    /// Replace the value held.
    pub fn set(&mut self, value: Value) {
        *self.value = value;
    }
}

/// A resource: its id, type name and whether it is closed (design §5.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resource {
    pub id: u32,
    pub type_name: PhpString,
    pub closed: bool,
}

/// A borrowed view of a value by type, for matching.
#[derive(Debug, Clone, Copy)]
pub enum ValueRef<'a> {
    Undef,
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(&'a PhpString),
    Array(&'a PhpArray),
    Object(&'a PhpObject),
    Resource(&'a Resource),
    Reference(&'a PhpRef),
}

/// The states a value has; the skeleton constructs `Undef` and
/// `Reference` in tests alone, since no frame slot or `&$x` exists yet.
#[derive(Debug, Clone, PartialEq, Default)]
#[allow(dead_code)]
enum Inner {
    Undef,
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(PhpString),
    Array(PhpArray),
    Object(PhpObject),
    Resource(Resource),
    Reference(PhpRef),
}

impl PartialEq for PhpObject {
    fn eq(&self, other: &PhpObject) -> bool {
        self.id == other.id
    }
}

/// One PHP value. Copying a `Value` is cheap for a scalar and, once the
/// heap backs it, a refcount for anything else.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Value {
    inner: Inner,
}

impl Value {
    const fn of(inner: Inner) -> Value {
        Value { inner }
    }

    /// `null`.
    #[must_use]
    pub const fn null() -> Value {
        Value::of(Inner::Null)
    }

    /// `true` or `false`.
    #[must_use]
    pub const fn bool(b: bool) -> Value {
        Value::of(Inner::Bool(b))
    }

    /// An `int`.
    #[must_use]
    pub const fn int(n: i64) -> Value {
        Value::of(Inner::Int(n))
    }

    /// A `float`.
    #[must_use]
    pub const fn float(f: f64) -> Value {
        Value::of(Inner::Float(f))
    }

    /// A `string`.
    #[must_use]
    pub const fn string(s: PhpString) -> Value {
        Value::of(Inner::Str(s))
    }

    /// An `array`.
    #[must_use]
    pub const fn array(a: PhpArray) -> Value {
        Value::of(Inner::Array(a))
    }

    /// An `object`.
    #[must_use]
    pub const fn object(o: PhpObject) -> Value {
        Value::of(Inner::Object(o))
    }

    /// A `resource`.
    #[must_use]
    pub const fn resource(r: Resource) -> Value {
        Value::of(Inner::Resource(r))
    }

    /// The type.
    #[must_use]
    pub const fn tag(&self) -> TypeTag {
        match &self.inner {
            Inner::Undef => TypeTag::Undef,
            Inner::Null => TypeTag::Null,
            Inner::Bool(_) => TypeTag::Bool,
            Inner::Int(_) => TypeTag::Int,
            Inner::Float(_) => TypeTag::Float,
            Inner::Str(_) => TypeTag::String,
            Inner::Array(_) => TypeTag::Array,
            Inner::Object(_) => TypeTag::Object,
            Inner::Resource(_) => TypeTag::Resource,
            Inner::Reference(_) => TypeTag::Reference,
        }
    }

    /// php's `zend_zval_type_name`: the name `gettype`'s cousin prints in
    /// error text (`int`, `float`, `string`, `array`, `null`, `bool`).
    #[must_use]
    pub fn type_name(&self) -> &'static str {
        match &self.inner {
            Inner::Undef | Inner::Null => "null",
            Inner::Bool(_) => "bool",
            Inner::Int(_) => "int",
            Inner::Float(_) => "float",
            Inner::Str(_) => "string",
            Inner::Array(_) => "array",
            Inner::Object(_) => "object",
            Inner::Resource(r) if r.closed => "resource (closed)",
            Inner::Resource(_) => "resource",
            Inner::Reference(r) => r.get().type_name(),
        }
    }

    /// The value by type, borrowed.
    #[must_use]
    pub fn as_ref(&self) -> ValueRef<'_> {
        match &self.inner {
            Inner::Undef => ValueRef::Undef,
            Inner::Null => ValueRef::Null,
            Inner::Bool(b) => ValueRef::Bool(*b),
            Inner::Int(n) => ValueRef::Int(*n),
            Inner::Float(f) => ValueRef::Float(*f),
            Inner::Str(s) => ValueRef::Str(s),
            Inner::Array(a) => ValueRef::Array(a),
            Inner::Object(o) => ValueRef::Object(o),
            Inner::Resource(r) => ValueRef::Resource(r),
            Inner::Reference(r) => ValueRef::Reference(r),
        }
    }

    /// The value a reference holds, or the value itself, `ZVAL_DEREF`.
    /// Not `Deref`: a value is not a pointer to another value.
    #[must_use]
    #[allow(clippy::should_implement_trait)]
    pub fn deref(&self) -> &Value {
        match &self.inner {
            Inner::Reference(r) => r.get().deref(),
            _ => self,
        }
    }

    /// The refcount of a refcounted value; one for a scalar.
    #[must_use]
    pub fn refcount(&self) -> u32 {
        1
    }

    /// `Some` for a `bool`.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self.deref().inner {
            Inner::Bool(b) => Some(b),
            _ => None,
        }
    }

    /// `Some` for an `int`.
    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self.deref().inner {
            Inner::Int(n) => Some(n),
            _ => None,
        }
    }

    /// `Some` for a `float`.
    #[must_use]
    pub fn as_float(&self) -> Option<f64> {
        match self.deref().inner {
            Inner::Float(f) => Some(f),
            _ => None,
        }
    }

    /// `Some` for a `string`.
    #[must_use]
    pub fn as_php_string(&self) -> Option<&PhpString> {
        match &self.deref().inner {
            Inner::Str(s) => Some(s),
            _ => None,
        }
    }

    /// The bytes of a `string`.
    #[must_use]
    pub fn as_str_bytes(&self) -> Option<&[u8]> {
        self.as_php_string().map(PhpString::as_bytes)
    }

    /// `Some` for an `array`.
    #[must_use]
    pub fn as_array(&self) -> Option<&PhpArray> {
        match &self.deref().inner {
            Inner::Array(a) => Some(a),
            _ => None,
        }
    }

    /// `Some` for an `object`.
    #[must_use]
    pub fn as_object(&self) -> Option<&PhpObject> {
        match &self.deref().inner {
            Inner::Object(o) => Some(o),
            _ => None,
        }
    }

    /// `Some` for a `resource`.
    #[must_use]
    pub fn as_resource(&self) -> Option<&Resource> {
        match &self.deref().inner {
            Inner::Resource(r) => Some(r),
            _ => None,
        }
    }

    /// `Some` for a reference, not followed.
    #[must_use]
    pub fn as_php_ref(&self) -> Option<&PhpRef> {
        match &self.inner {
            Inner::Reference(r) => Some(r),
            _ => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn reference(r: PhpRef) -> Value {
        Value::of(Inner::Reference(r))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_normalise_as_php_does() {
        assert_eq!(Key::from_bytes(b"12"), Key::Int(12));
        assert_eq!(Key::from_bytes(b"-1"), Key::Int(-1));
        assert_eq!(
            Key::from_bytes(b"012"),
            Key::Str(PhpString::from_bytes(b"012"))
        );
        assert_eq!(
            Key::from_bytes(b"-0"),
            Key::Str(PhpString::from_bytes(b"-0"))
        );
        assert_eq!(Key::from_bytes(b""), Key::Str(PhpString::from_bytes(b"")));
        assert_eq!(Key::Int(7).to_prop_name().as_bytes(), b"7");
    }

    #[test]
    fn an_array_keeps_order_and_the_next_free_index() {
        let mut a = PhpArray::new();
        a.set(Key::Int(5), Value::int(1));
        a.append(Value::int(2));
        a.set(Key::from_bytes(b"x"), Value::null());
        assert_eq!(a.len(), 3);
        assert_eq!(a.get(&Key::Int(6)), Some(&Value::int(2)));
        assert!(!a.is_list());
        assert_eq!(a.cursor_key(), Some(Key::Int(5)));
        a.cursor_next();
        assert_eq!(a.cursor_current(), Some(&Value::int(2)));
        let mut it = a.iter();
        assert!(it.advance());
        assert_eq!(it.key(), Some(Key::Int(5)));
        assert_eq!(a.unset(&Key::Int(5)), Some(Value::int(1)));
        assert_eq!(a.cursor_current(), Some(&Value::int(2)));
    }

    #[test]
    fn a_reference_is_followed_by_deref() {
        let r = Value::reference(PhpRef::new(Value::int(3)));
        assert_eq!(r.tag(), TypeTag::Reference);
        assert_eq!(r.as_int(), Some(3));
        assert_eq!(r.type_name(), "int");
    }
}

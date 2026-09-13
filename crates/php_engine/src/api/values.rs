//! The value group of the native face (design §4.4): Mahout's vocabulary
//! over Zend's layouts. What a native holds is a [`Value`]; the layout
//! knowledge stays in `php_heap`.
//!
//! **What stands on the heap, and what does not yet.** A string is
//! `php_heap`'s (design §5.2): [`PhpString`] is the pointer php's
//! `zend_string *` is, sixteen bytes of [`Value`] name it exactly as a
//! zval does, and it is allocated out of the request's heap and nowhere
//! else, so `memory_get_usage()` counts every string a native builds
//! (design §5.13). An array, an object, a resource and a reference are
//! still owned Rust data behind the same names, the stand-ins the rest
//! of Lane A replaces (design §11, days 3–7); a `Value` holding one of
//! those owns it as Rust owns a `Vec`.
//!
//! **Ownership, as design §5.1 decides it.** `PhpString` is `Copy` and
//! has no `Drop`; cloning a `Value` that holds one is php's
//! `ZVAL_COPY_VALUE`, a bitwise copy that touches no refcount. Owners are
//! taken and given up explicitly, by the engine, where php's handlers
//! do it: a value the engine hands a native is the frame's, a value a
//! native hands back is the engine's from the moment it is returned, and
//! a native releases nothing. What a native builds and drops without
//! returning is freed with the request's memory at its end.

use std::any::Any;
use std::cmp::Ordering;
use std::sync::Arc;

use php_heap::Str;

use crate::api::call::Function;
use crate::api::raising::Thrown;
use crate::api::request::Request;

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

/// A byte string: php's `zend_string *`, one allocation of the request's
/// heap holding the refcounted header, the cached hash, the length and
/// the bytes, which are bytes and never UTF-8 (design §5.2). Naming one
/// takes no owner, as php's pointer does not; `Copy` is what makes it a
/// pointer and not a `Vec`.
///
/// A string exists in one of two ways: interned, which is what a
/// compiled program's constants and every native's name are, never
/// refcounted and freed by its table; or the request's own, one owner per
/// [`PhpString::new`], freed by the engine when the last owner gives it
/// up and by the request's end otherwise.
#[derive(Clone, Copy)]
pub struct PhpString(Str);

impl PhpString {
    /// A string of these bytes out of the request's heap, which is where
    /// every string a native returns comes from (design §5.13). php's
    /// `ZVAL_STRINGL_FAST`: the empty string and a string of one byte are
    /// the process's interned ones and allocate nothing, and anything
    /// longer is `zend_string_init`, one allocation counted against the
    /// request. The `Err` is the memory limit, recorded on the request as
    /// php's fatal and ending the native as `zend_mm` bails out.
    pub fn new(req: &mut Request, bytes: &[u8]) -> Result<PhpString, Thrown> {
        req.string(bytes)
    }

    /// A string of `len` bytes written in place by `fill`, php's
    /// `zend_string_alloc` and the `memcpy` a native does into
    /// `ZSTR_VAL` after it. The allocation is asked for, and the memory
    /// limit met, before a byte is written anywhere, which is how
    /// `str_repeat("x", 200000000)` is php's `Allowed memory size` fatal
    /// and not two hundred megabytes built and then refused. `fill` is
    /// given `len` zero bytes to write once.
    pub fn filled(
        req: &mut Request,
        len: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<PhpString, Thrown> {
        req.string_with(len, fill)
    }

    /// A string of `n * m + l` bytes written in place by `fill`, php's
    /// `zend_string_safe_alloc`: the product checked before anything is
    /// asked for, and php's `Possible integer overflow in memory
    /// allocation` fatal where it overflows. Its allocation is the
    /// product plus the aligned header of a string of `l` bytes, which is
    /// more than [`PhpString::filled`] takes for the same length and is
    /// what `memory_get_usage()` moves by after `str_repeat`, `str_pad`
    /// and `implode`, whose php bodies allocate this way.
    pub fn product(
        req: &mut Request,
        n: usize,
        m: usize,
        l: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<PhpString, Thrown> {
        req.string_product(n, m, l, fill)
    }

    /// The face over a heap string the engine already holds: a constant
    /// of an op array, or what a kernel allocated.
    pub(crate) const fn of(s: Str) -> PhpString {
        PhpString(s)
    }

    /// The heap string, for the engine's refcount traffic.
    pub(crate) const fn heap(self) -> Str {
        self.0
    }

    /// The bytes, `ZSTR_VAL` over `ZSTR_LEN`.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.0.bytes()
    }

    /// The length in bytes, `strlen`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the length is zero.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Two strings are equal when their bytes are, `zend_string_equals`.
impl PartialEq for PhpString {
    fn eq(&self, other: &PhpString) -> bool {
        self.0 == other.0
    }
}

impl Eq for PhpString {}

impl std::hash::Hash for PhpString {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_bytes().hash(state);
    }
}

impl PartialOrd for PhpString {
    fn partial_cmp(&self, other: &PhpString) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Ordered by bytes, `zend_binary_strcmp`.
impl Ord for PhpString {
    fn cmp(&self, other: &PhpString) -> Ordering {
        self.as_bytes().cmp(other.as_bytes())
    }
}

impl std::fmt::Debug for PhpString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", String::from_utf8_lossy(self.as_bytes()))
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
    /// The key a string makes: `Int` when its bytes are a canonical
    /// decimal integer in range (`ZEND_HANDLE_NUMERIC_STR`), else the
    /// string itself. The string is the caller's to have allocated, as
    /// php's `zend_symtable_update` is handed a `zend_string *`.
    #[must_use]
    pub fn from_string(s: PhpString) -> Key {
        match canonical_int(s.as_bytes()) {
            Some(n) => Key::Int(n),
            None => Key::Str(s),
        }
    }

    /// The key as a property name: an integer key printed in decimal,
    /// `zend_long_to_str`, a string key as it is.
    pub fn to_prop_name(&self, req: &mut Request) -> Result<PhpString, Thrown> {
        match self {
            Key::Int(n) => PhpString::new(req, n.to_string().as_bytes()),
            Key::Str(s) => Ok(*s),
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
            .map(|(name, value)| (Key::from_string(*name), value.clone()))
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

    /// Write one property by name, without a handler in the way. The
    /// name is a string the caller holds, as `zend_std_write_property`
    /// is handed one; a property already there keeps the name it had.
    pub fn set_prop(&mut self, name: PhpString, value: Value) {
        match self
            .props
            .iter_mut()
            .find(|(n, _)| n.as_bytes() == name.as_bytes())
        {
            Some((_, slot)) => *slot = value,
            None => self.props.push((name, value)),
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

/// The states a value has; the skeleton constructs `Reference` in tests
/// alone, since no `&$x` exists yet, and `Undef` for a frame's slots.
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

/// One PHP value. Cloning one is php's `ZVAL_COPY_VALUE`: sixteen bytes
/// copied for a scalar and a string, no refcount touched, so the clone
/// names what the original names and owns nothing the original did not
/// give up; the engine takes and releases owners where php's handlers
/// do. An array, an object, a resource and a reference are still owned
/// Rust data and are cloned as such until their lane lands.
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

    /// `IS_UNDEF`: a frame slot nothing has written yet, which a CV read
    /// warns of and reads as `null` (`zval_undefined_cv`). The engine's
    /// alone; a native is never handed one.
    #[must_use]
    pub(crate) const fn undef() -> Value {
        Value::of(Inner::Undef)
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
        assert_eq!(canonical_int(b"12"), Some(12));
        assert_eq!(canonical_int(b"-1"), Some(-1));
        assert_eq!(canonical_int(b"012"), None);
        assert_eq!(canonical_int(b"-0"), None);
        assert_eq!(canonical_int(b""), None);
        assert_eq!(canonical_int(b"9223372036854775807"), Some(i64::MAX));
        assert_eq!(canonical_int(b"9223372036854775808"), None);
    }

    #[test]
    fn an_array_keeps_order_and_the_next_free_index() {
        let mut a = PhpArray::new();
        a.set(Key::Int(5), Value::int(1));
        a.append(Value::int(2));
        a.set(Key::Int(-4), Value::null());
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

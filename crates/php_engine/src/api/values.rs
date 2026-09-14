//! The value group of the native face (design §4.4): Mahout's vocabulary
//! over Zend's layouts. What a native holds is a [`Value`]; the layout
//! knowledge stays in `php_heap`.
//!
//! **What stands on the heap, and what does not yet.** A string and an
//! array are `php_heap`'s (design §5.2, §5.3): [`PhpString`] is the
//! pointer php's `zend_string *` is and [`PhpArray`] the pointer php's
//! `zend_array *` is, sixteen bytes of [`Value`] name either exactly as
//! a zval does, and each is allocated out of the request's heap and
//! nowhere else, so `memory_get_usage()` counts every string and every
//! array a native builds (design §5.13). An object, a resource and a
//! reference are still owned Rust data behind the same names, the
//! stand-ins the rest of Lane A replaces (design §11, days 3–7); a
//! `Value` holding one of those owns it as Rust owns a `Vec`, and an
//! array holding one boxes it, the heap counting the box.
//!
//! **Ownership, as design §5.1 decides it.** `PhpString` and `PhpArray`
//! are `Copy` and have no `Drop`; cloning a `Value` that holds one is
//! php's `ZVAL_COPY_VALUE`, a bitwise copy that touches no refcount, and
//! the engine's `addref` beside it is what makes the two `ZVAL_COPY`.
//! Owners are taken and given up explicitly, by the engine, where php's
//! handlers do it: a value the engine hands a native is the frame's, a
//! value a native hands back is the engine's from the moment it is
//! returned, and a native releases nothing. What a native builds and
//! drops without returning is freed with the request's memory at its
//! end.

use std::any::Any;
use std::cmp::Ordering;
use std::sync::Arc;

use php_heap::{Arr, Str, Zval, ZvalType};

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
    /// The heap's key: the integer, or the string the bucket names.
    pub(crate) fn heap(&self) -> php_heap::Key {
        match self {
            Key::Int(n) => php_heap::Key::Long(*n),
            Key::Str(s) => php_heap::Key::Str(s.heap()),
        }
    }

    /// The key a bucket holds, as the face names it.
    pub(crate) fn of(key: php_heap::Key) -> Key {
        match key {
            php_heap::Key::Long(n) => Key::Int(n),
            php_heap::Key::Str(s) => Key::Str(PhpString::of(s)),
        }
    }

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

/// A PHP array: php's `zend_array *`, one allocation of the request's
/// heap for the header and one for the buckets, insertion-ordered, with
/// php's `nNextFreeElement` and its internal pointer (design §5.3).
/// Naming one takes no owner, as php's pointer does not; `Copy` is what
/// makes it a pointer and not a `Vec`, and the engine counts the owners
/// where php's handlers do.
///
/// **Writes separate.** Every write goes through [`PhpArray::set`],
/// [`PhpArray::append`], [`PhpArray::unset`] or [`PhpArray::sort_by`],
/// and each is php's `SEPARATE_ARRAY` first: an array with one owner is
/// written in place, and an array with more is copied once, the copy
/// written, and this name moved to the copy while every other owner
/// keeps what it had. A native holding a `Value` it was handed and a
/// `PhpArray` it took out of it holds two names; writing through the
/// second leaves the first as php leaves the argument.
///
/// **What is handed in is given up.** The value written by a `set` or an
/// `append` is the array's from that moment, as `ZVAL_COPY_VALUE` into
/// the bucket makes it php's, and so is a string key: a native builds a
/// string with [`PhpString::new`], hands it to `set`, and never names it
/// again, which is `add_assoc_*`'s shape. What a read hands back is a
/// view, no owner taken.
#[derive(Clone, Copy)]
pub struct PhpArray(Arr);

impl PhpArray {
    /// An empty array out of the request's heap, php's `array_init`:
    /// fifty-six bytes for the header, nothing for elements until the
    /// first write. The `Err` is the memory limit, ending the native as
    /// `zend_mm` bails out.
    pub fn new(req: &mut Request) -> Result<PhpArray, Thrown> {
        req.array()
    }

    /// The face over a heap array the engine already holds.
    pub(crate) const fn of(a: Arr) -> PhpArray {
        PhpArray(a)
    }

    /// The heap array, for the engine's refcount traffic.
    pub(crate) const fn heap(self) -> Arr {
        self.0
    }

    /// The number of elements, `count()`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether there are no elements.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Whether the keys are `0..len` in order, `array_is_list()`.
    #[must_use]
    pub fn is_list(&self) -> bool {
        self.0
            .iter()
            .enumerate()
            .all(|(i, (k, _))| matches!(k, php_heap::Key::Long(n) if usize::try_from(n) == Ok(i)))
    }

    /// The value at `key`, a view of it.
    #[must_use]
    pub fn get(&self, key: &Key) -> Option<Value> {
        self.0.get(key.heap()).map(Value::of_zval)
    }

    /// Whether `key` is present, `array_key_exists`.
    #[must_use]
    pub fn contains(&self, key: &Key) -> bool {
        self.0.contains(key.heap())
    }

    /// Write `value` at `key`, `zend_hash_update` and
    /// `zend_hash_index_update` after `SEPARATE_ARRAY`: in place for a
    /// present key, its old value released, and at the end otherwise; an
    /// integer key at or past `nNextFreeElement` moves it. The value and
    /// a string key are the array's from here.
    // Taking the key is the point: a string key's owner is given up here.
    #[allow(clippy::needless_pass_by_value)]
    pub fn set(&mut self, req: &mut Request, key: Key, value: Value) -> Result<(), Thrown> {
        self.separate(req)?;
        req.array_update(self.0, key.heap(), value)?;
        if let Key::Str(s) = key {
            req.release(Value::string(s));
        }
        Ok(())
    }

    /// Replace the value at `key` in place; `false`, and `value` given
    /// up, when it is absent.
    pub fn replace(&mut self, req: &mut Request, key: &Key, value: Value) -> Result<bool, Thrown> {
        if !self.contains(key) {
            req.release(value);
            return Ok(false);
        }
        self.separate(req)?;
        req.array_update(self.0, key.heap(), value)?;
        Ok(true)
    }

    /// `$a[] = value`, at `nNextFreeElement`, or php's `Error` "Cannot
    /// add element to the array as the next element is already occupied"
    /// when that is `ZEND_LONG_MAX` and taken (`zend_cannot_add_element`).
    pub fn append(&mut self, req: &mut Request, value: Value) -> Result<(), Thrown> {
        if self.try_append(req, value)? {
            return Ok(());
        }
        Err(crate::api::raising::throw(
            req,
            "Error",
            "Cannot add element to the array as the next element is already occupied",
        ))
    }

    /// `$a[] = value`, or `false`, and `value` given up, when
    /// `nNextFreeElement` is `ZEND_LONG_MAX` and taken, which is what
    /// `add_next_index_*` answers.
    pub fn try_append(&mut self, req: &mut Request, value: Value) -> Result<bool, Thrown> {
        self.separate(req)?;
        req.array_append(self.0, value)
    }

    /// Remove `key`, `zend_hash_del` after `SEPARATE_ARRAY`: `true` when
    /// it was there, its value released.
    pub fn unset(&mut self, req: &mut Request, key: &Key) -> Result<bool, Thrown> {
        if !self.contains(key) {
            return Ok(false);
        }
        self.separate(req)?;
        Ok(req.array_del(self.0, key.heap()))
    }

    /// `SEPARATE_ARRAY`: an array with one owner is itself, and one with
    /// more is copied once (`zend_array_dup`), the copy having one owner
    /// and this name, the original one owner fewer.
    pub fn separate(&mut self, req: &mut Request) -> Result<(), Thrown> {
        self.0 = req.array_separate(self.0)?;
        Ok(())
    }

    /// A copy with one owner, `zend_array_dup`: the same elements, each
    /// one more owner, what `array_merge` and `+` build on.
    pub fn copy(&self, req: &mut Request) -> Result<PhpArray, Thrown> {
        req.array_dup(self.0).map(PhpArray::of)
    }

    /// An iterator over the elements in order.
    #[must_use]
    pub fn iter(&self) -> ArrayIter {
        ArrayIter {
            inner: self.0.iter(),
            at: None,
        }
    }

    /// The elements in order, references followed, views of them.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(Key, Value)> {
        self.iter().map(|(k, v)| (k, v.deref().clone())).collect()
    }

    /// The elements in order, references kept as references.
    #[must_use]
    pub fn snapshot_refs(&self) -> Vec<(Key, Value)> {
        self.iter().collect()
    }

    /// Sort the elements in place by `compare`, keys kept, after
    /// `SEPARATE_ARRAY`: `zend_hash_sort` without renumbering, stable as
    /// `sort()` guarantees since 8.0, the internal pointer reset.
    pub fn sort_by(
        &mut self,
        req: &mut Request,
        mut compare: impl FnMut(&Value, &Value) -> Ordering,
    ) -> Result<(), Thrown> {
        self.separate(req)?;
        req.array_sort(self.0, &mut |x, y| {
            compare(&Value::of_zval(x), &Value::of_zval(y))
        })
    }

    /// The array as a value.
    #[must_use]
    pub const fn to_value(self) -> Value {
        Value::array(self)
    }

    /// `GC_IS_RECURSIVE`: a walk that guards itself with
    /// [`PhpArray::protect`] is inside this array already, which is how
    /// `count()`, `var_dump()` and `serialize()` see an array that holds
    /// itself through a reference.
    #[must_use]
    pub fn is_recursive(&self) -> bool {
        self.0.is_recursive()
    }

    /// `GC_PROTECT_RECURSION`: the walk is inside; nothing for an
    /// immutable array, as php marks none.
    pub fn protect(&self) {
        self.0.protect();
    }

    /// `GC_UNPROTECT_RECURSION`: the walk is out.
    pub fn unprotect(&self) {
        self.0.unprotect();
    }

    /// `current()`: the element at the internal pointer.
    #[must_use]
    pub fn cursor_current(&self) -> Option<Value> {
        self.0
            .entry_at(self.0.position())
            .map(|(_, z)| Value::of_zval(z))
    }

    /// `key()`: the key at the internal pointer.
    #[must_use]
    pub fn cursor_key(&self) -> Option<Key> {
        self.0.entry_at(self.0.position()).map(|(k, _)| Key::of(k))
    }

    /// `next()`: the internal pointer one element on, or past the end.
    pub fn cursor_next(&mut self) {
        self.0.forward();
    }

    /// `prev()`: the internal pointer one element back; from the first
    /// it points past the end, as php's does.
    pub fn cursor_prev(&mut self) {
        self.0.backward();
    }

    /// `reset()`: the internal pointer to the first element.
    pub fn cursor_reset(&mut self) {
        self.0.reset();
    }

    /// `end()`: the internal pointer to the last element.
    pub fn cursor_end(&mut self) {
        self.0.end();
    }
}

/// Two arrays are equal when their elements are, in order: what `==`
/// reads once both sides are arrays (`zend_hash_compare` ordered).
impl PartialEq for PhpArray {
    fn eq(&self, other: &PhpArray) -> bool {
        self.len() == other.len() && self.iter().eq(other.iter())
    }
}

impl std::fmt::Debug for PhpArray {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

/// A position in a [`PhpArray`]: before the first element until
/// [`ArrayIter::advance`] is called.
#[derive(Debug)]
pub struct ArrayIter {
    inner: php_heap::ArrIter,
    at: Option<(Key, Value)>,
}

impl ArrayIter {
    /// Advance one element; `false` past the end.
    pub fn advance(&mut self) -> bool {
        self.at = self
            .inner
            .next()
            .map(|(k, z)| (Key::of(k), Value::of_zval(z)));
        self.at.is_some()
    }

    /// The key at the position.
    #[must_use]
    pub fn key(&self) -> Option<Key> {
        self.at.as_ref().map(|(k, _)| k.clone())
    }

    /// The value at the position.
    #[must_use]
    pub fn value(&self) -> Option<&Value> {
        self.at.as_ref().map(|(_, v)| v)
    }
}

impl IntoIterator for &PhpArray {
    type Item = (Key, Value);
    type IntoIter = ArrayIter;

    fn into_iter(self) -> ArrayIter {
        self.iter()
    }
}

impl Iterator for ArrayIter {
    type Item = (Key, Value);

    fn next(&mut self) -> Option<(Key, Value)> {
        if !self.advance() {
            return None;
        }
        self.at.take()
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
/// copied for a scalar, a string and an array, no refcount touched, so
/// the clone names what the original names and owns nothing the
/// original did not give up; the engine takes and releases owners where
/// php's handlers do. An object, a resource and a reference are still
/// owned Rust data and are cloned as such until their lane lands.
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

    /// The refcount of a refcounted value, `Z_REFCOUNT`; one for a
    /// scalar, an interned string or an immutable array.
    #[must_use]
    pub fn refcount(&self) -> u32 {
        match &self.inner {
            Inner::Str(s) => s.heap().refcount(),
            Inner::Array(a) => a.heap().refcount(),
            _ => 1,
        }
    }

    /// A zval as the face names it, no owner taken: the string or the
    /// array is named and not copied, as php's `ZVAL_COPY_VALUE` names
    /// it, and an owned kind boxed into an array's bucket is cloned out,
    /// as its stand-in is cloned anywhere.
    pub(crate) fn of_zval(zval: Zval) -> Value {
        if let Some(n) = zval.as_long() {
            return Value::int(n);
        }
        if let Some(f) = zval.as_double() {
            return Value::float(f);
        }
        if let Some(b) = zval.as_bool() {
            return Value::bool(b);
        }
        if let Some(s) = zval.as_str() {
            return Value::string(PhpString::of(s));
        }
        if let Some(a) = zval.as_arr() {
            return Value::array(PhpArray::of(a));
        }
        if let Some(o) = zval.as_boxed::<PhpObject>() {
            return Value::object(o.clone());
        }
        if let Some(r) = zval.as_boxed::<Resource>() {
            return Value::resource(r.clone());
        }
        if let Some(r) = zval.as_boxed::<PhpRef>() {
            return Value::of(Inner::Reference(r.clone()));
        }
        match zval.kind() {
            ZvalType::Undef => Value::undef(),
            _ => Value::null(),
        }
    }

    /// The zval this value is, for an array's bucket: the sixteen bytes
    /// a scalar, a string or an array already is, and for an owned kind
    /// the box the heap counts in their place. `UNDEF` is a bucket's
    /// tombstone and never an element, so a value nothing wrote is
    /// `null` here, as php reads one.
    pub(crate) fn into_zval(self, req: &mut Request) -> Zval {
        match self.inner {
            Inner::Undef | Inner::Null => Zval::null(),
            Inner::Bool(b) => Zval::bool(b),
            Inner::Int(n) => Zval::long(n),
            Inner::Float(f) => Zval::double(f),
            Inner::Str(s) => Zval::string(s.heap()),
            Inner::Array(a) => Zval::array(a.heap()),
            Inner::Object(o) => req.heap_mut().boxed(ZvalType::Object, Box::new(o)),
            Inner::Resource(r) => req.heap_mut().boxed(ZvalType::Resource, Box::new(r)),
            Inner::Reference(r) => req.heap_mut().boxed(ZvalType::Reference, Box::new(r)),
        }
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

    /// The array is the heap's: it keeps php's order and its
    /// `nNextFreeElement`, a read is a view, and `count()` falls with
    /// `unset` while the pointer stands.
    #[test]
    fn an_array_keeps_order_and_the_next_free_index() {
        let mut req = crate::test::request();
        let Ok(mut a) = PhpArray::new(&mut req) else {
            return;
        };
        assert!(a.set(&mut req, Key::Int(5), Value::int(1)).is_ok());
        assert!(a.append(&mut req, Value::int(2)).is_ok());
        assert!(a.set(&mut req, Key::Int(-4), Value::null()).is_ok());
        assert_eq!(a.len(), 3);
        assert_eq!(a.get(&Key::Int(6)), Some(Value::int(2)));
        assert!(!a.is_list());
        assert_eq!(a.cursor_key(), Some(Key::Int(5)));
        a.cursor_next();
        assert_eq!(a.cursor_current(), Some(Value::int(2)));
        let mut it = a.iter();
        assert!(it.advance());
        assert_eq!(it.key(), Some(Key::Int(5)));
        assert_eq!(a.unset(&mut req, &Key::Int(5)), Ok(true));
        assert_eq!(a.len(), 2);
        assert_eq!(a.cursor_current(), Some(Value::int(2)));
        req.release(Value::array(a));
    }

    /// Cloning a `Value` that holds an array names the one array
    /// (`ZVAL_COPY_VALUE`), the engine's `addref` is the second owner
    /// (`ZVAL_COPY`), and a write through the second name separates it
    /// as `SEPARATE_ARRAY` does: the first name reads what it read.
    #[test]
    fn a_clone_is_a_refcount_and_a_write_separates() {
        let mut req = crate::test::request();
        let Ok(mut a) = PhpArray::new(&mut req) else {
            return;
        };
        assert!(a.append(&mut req, Value::int(1)).is_ok());
        let first = Value::array(a);
        let second = first.clone();
        assert_eq!(first.refcount(), 1, "a clone touches no refcount");
        req.addref(&second);
        assert_eq!(first.refcount(), 2);
        let Some(mut b) = second.as_array().copied() else {
            return;
        };
        assert!(b.append(&mut req, Value::int(2)).is_ok());
        assert_eq!(first.refcount(), 1, "the write took the second owner away");
        assert_eq!(b.len(), 2);
        assert_eq!(first.as_array().map(PhpArray::len), Some(1));
        assert_ne!(&b, first.as_array().unwrap_or(&b));
        req.release(Value::array(b));
        req.release(first);
        assert_eq!(req.memory_usage(false), 0);
    }

    /// An object in an array is boxed and comes back out as itself.
    #[test]
    fn an_owned_kind_in_an_array_comes_back_as_itself() {
        let mut req = crate::test::request();
        let object = req.new_object_of("stdClass");
        let id = object.index();
        let Ok(mut a) = PhpArray::new(&mut req) else {
            return;
        };
        assert!(a.append(&mut req, Value::object(object)).is_ok());
        assert_eq!(
            a.get(&Key::Int(0))
                .and_then(|v| v.as_object().map(PhpObject::index)),
            Some(id)
        );
        req.release(Value::array(a));
    }

    #[test]
    fn a_reference_is_followed_by_deref() {
        let r = Value::reference(PhpRef::new(Value::int(3)));
        assert_eq!(r.tag(), TypeTag::Reference);
        assert_eq!(r.as_int(), Some(3));
        assert_eq!(r.type_name(), "int");
    }
}

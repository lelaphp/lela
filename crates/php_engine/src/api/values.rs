//! The value group of the native face (design §4.4): Mahout's vocabulary
//! over Zend's layouts. What a native holds is a [`Value`]; the layout
//! knowledge stays in `php_heap`.
//!
//! **Every kind stands on the heap.** A string, an array, an object, a
//! resource and a reference are `php_heap`'s (design §5.2–§5.5):
//! [`PhpString`] is the pointer php's `zend_string *` is, [`PhpArray`]
//! the pointer php's `zend_array *` is, [`PhpObject`] the pointer php's
//! `zend_object *` is, [`Resource`] the pointer php's `zend_resource *`
//! is and [`PhpRef`] the pointer php's `zend_reference *` is; sixteen
//! bytes of [`Value`] name any of them exactly as a zval does, and each
//! is allocated out of the request's heap and nowhere else, so
//! `memory_get_usage()` counts every one a native builds (design
//! §5.13).
//!
//! **Ownership, as design §5.1 decides it.** Every face is `Copy` and
//! has no `Drop`; copying a `Value` is php's `ZVAL_COPY_VALUE`, a
//! bitwise copy that touches no refcount, and the engine's `addref`
//! beside it is what makes the two `ZVAL_COPY`.
//! Owners are taken and given up explicitly, by the engine, where php's
//! handlers do it: a value the engine hands a native is the frame's, a
//! value a native hands back is the engine's from the moment it is
//! returned, and a native releases nothing. What a native builds and
//! drops without returning is freed with the request's memory at its
//! end.

use std::any::Any;
use std::cmp::Ordering;

use php_heap::{Arr, Obj, Ref, Res, Str, Zval, ZvalType};

use crate::api::abi::PropertyPurpose;
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

    /// `ZSTR_IS_VALID_UTF8`: whether a check already found the bytes to
    /// be valid UTF-8, which a string forgets when it is written into.
    /// `false` says only that no check was recorded.
    #[must_use]
    pub fn is_valid_utf8(&self) -> bool {
        self.0.is_valid_utf8()
    }

    /// `GC_ADD_FLAGS(s, IS_STR_VALID_UTF8)`: record that a check found
    /// the bytes to be valid UTF-8, so the next reader need not check
    /// them again, as `pcre` records it after a `/u` match over the whole
    /// subject. A caller that has not checked every byte must not call
    /// this. An interned or persistent string is shared beyond the
    /// request and is left as it is, as php leaves an interned one.
    pub fn mark_valid_utf8(&self) {
        if !self.0.is_interned() && !self.0.is_persistent() {
            self.0.mark_valid_utf8();
        }
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
        match php_heap::canonical_int(s.as_bytes()) {
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

    /// `nNextFreeElement`: the index `$a[] = ...` writes at, one past the
    /// greatest integer key the array has held, `ZEND_LONG_MIN` for an
    /// array that never held one.
    #[must_use]
    pub fn next_free(&self) -> i64 {
        self.0.next_free()
    }

    /// `nNextFreeElement` written after `SEPARATE_ARRAY`, as
    /// `array_pop()` moves it back one where the element it took was the
    /// last one appended.
    pub fn set_next_free(&mut self, req: &mut Request, next: i64) -> Result<(), Thrown> {
        self.separate(req)?;
        self.0.set_next_free(next);
        Ok(())
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

    /// The first live element at or past the bucket `position`, with
    /// the bucket it stands at: what a hash iterator's `pos` reads
    /// (`zend_hash_get_current_data_ex` after
    /// `_zend_hash_get_valid_pos`), so a walk that keeps the bucket
    /// resumes where it stood while the elements before it change. A
    /// view, no owner taken; `None` past the last.
    #[must_use]
    pub fn live_from(&self, position: u32) -> Option<(u32, Key, Value)> {
        self.0
            .live_from(position)
            .map(|(at, key, z)| (at, Key::of(key), Value::of_zval(z)))
    }

    /// The elements in order, references followed, views of them.
    #[must_use]
    pub fn snapshot(&self) -> Vec<(Key, Value)> {
        self.iter().map(|(k, v)| (k, v.deref())).collect()
    }

    /// The elements in order, references kept as references.
    #[must_use]
    pub fn snapshot_refs(&self) -> Vec<(Key, Value)> {
        self.iter().collect()
    }

    /// `zend_hash_sort` after `SEPARATE_ARRAY`, what every sort native
    /// of `array.c` calls: the elements ordered in place by `compare`
    /// with php's hybrid, [`zend_sort`], the internal pointer reset, and
    /// the keys kept or dropped as `renumber` says, `sort()` dropping
    /// them and `asort()` keeping them. Two elements `compare` answers
    /// `Equal` for stay in the order they were in, `RETURN_STABLE_SORT`
    /// over the position each held, which is how php's `sort()` is
    /// stable since 8.0 while its algorithm is not; a comparison that is
    /// not transitive leaves what the algorithm leaves, php's order.
    ///
    /// `compare` is handed each element as its key and its value, the
    /// `Bucket` php's comparators read, so the key comparators of
    /// `ksort()` and `uksort()` stand on the same walk as the value ones.
    ///
    /// `compare` may serve PHP code, a `usort` callback, and so may throw:
    /// the first `Err` is kept and every comparison after it answers
    /// `Equal`, as `zend_call_function` returns without calling while
    /// `EG(exception)` is set, so the sort runs to its end over the
    /// positions alone and the array is left as php leaves it before what
    /// was thrown is raised from here.
    pub fn sort_by(
        &mut self,
        req: &mut Request,
        renumber: bool,
        mut compare: impl FnMut(
            &mut Request,
            (&Key, &Value),
            (&Key, &Value),
        ) -> Result<Ordering, Thrown>,
    ) -> Result<(), Thrown> {
        self.separate(req)?;
        let mut pending: Option<Thrown> = None;
        req.array_sort(self.0, renumber, &mut |req, x, y| {
            let ordering = if pending.is_some() {
                Ordering::Equal
            } else {
                let (xk, xv) = (Key::of(x.key), Value::of_zval(x.val));
                let (yk, yv) = (Key::of(y.key), Value::of_zval(y.val));
                match compare(req, (&xk, &xv), (&yk, &yv)) {
                    Ok(ordering) => ordering,
                    Err(thrown) => {
                        pending = Some(thrown);
                        Ordering::Equal
                    }
                }
            };
            ordering.then(x.position.cmp(&y.position))
        })?;
        pending.map_or(Ok(()), Err)
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

/// What a `Closure` object carries (design §5.5), php's `zend_closure`
/// past its object header: the function, the receiver bound to it and
/// the scope it was made in.
#[derive(Debug, Clone)]
pub struct ClosureInfo {
    pub function: Function,
    pub this: Option<PhpObject>,
    pub scope: Option<ClassId>,
    /// `closure->called_scope`, what `static::` names in the body and
    /// what `zend_closure_get_closure` hands a call: the class of the
    /// receiver where there is one, else the class late binding named when the
    /// closure was made, which for a closure written in a class
    /// method is the class the method was called on and not `scope`.
    pub called_scope: Option<ClassId>,
    /// The closure's own copy of the op array's `static_variables`,
    /// which `zend_create_closure` makes with `zend_array_dup` and
    /// `ZEND_MAP_PTR_INIT`: one slot per entry of
    /// [`php_unit::OpArray::bound_vars`], a `use` variable's undefined
    /// until a `BIND_LEXICAL` beside the declaration fills it and a
    /// `static`'s at the value the table starts it at, and what the
    /// `BIND_STATIC`s of the body read and bind. A clone of this value
    /// is a view of them and takes no owner, as `this` is.
    pub bound: Vec<Value>,
    /// `ZEND_ACC_FAKE_CLOSURE`: whether the closure was made of a
    /// function that already had a name, `zend_create_fake_closure`,
    /// which `Closure::fromCallable` and `Closure::fromCallable`'s kin
    /// make and a closure written in the program never does. `zend_closure_compare`
    /// compares two fake closures and holds every other pair
    /// uncomparable.
    pub fake: bool,
}

/// An object (design §5.4): php's `zend_object *`, one allocation of
/// the request's heap in the object store, the forty-byte header, a
/// zval per declared property and, for an internal class, the native
/// payload as one typed slot. Naming one takes no owner, as php's
/// pointer does not; `Copy` is what makes it a pointer and not a struct,
/// and the engine counts the owners where php's handlers do. The
/// destructor is served when the last owner lets go, through seam 1.
///
/// What is read through the request is read through it because the
/// heap is the request's: a property is a view of the slot or the
/// dynamic table, no owner taken, and a write is `zend_std_write_property`
/// without the handler in the way yet, the value the object's from then
/// on and the name given up as [`PhpArray::set`] gives a key up.
#[derive(Clone, Copy)]
pub struct PhpObject(Obj);

impl PhpObject {
    /// The face over a heap object the engine already holds.
    pub(crate) const fn of(o: Obj) -> PhpObject {
        PhpObject(o)
    }

    /// The heap object, for the engine's refcount traffic.
    pub(crate) const fn heap(self) -> Obj {
        self.0
    }

    /// The object as a value, `ZVAL_OBJ`: named, no owner taken.
    #[must_use]
    pub const fn as_value(&self) -> Value {
        Value::object(*self)
    }

    /// The class of the object.
    #[must_use]
    pub fn class(&self) -> ClassId {
        ClassId(self.0.class())
    }

    /// `spl_object_id()`: the store's index.
    #[must_use]
    pub fn index(&self) -> u32 {
        self.0.id().0
    }

    /// Whether the object is of `class`, of a class that extends it, or
    /// of one that implements it: `instanceof`, `instanceof_function`
    /// over the linked table the request reads.
    #[must_use]
    pub fn instance_of(&self, req: &Request, class: ClassId) -> bool {
        req.instance_of(self.class(), class)
    }

    /// `zend_objects_clone_obj` for a class whose objects carry a
    /// native payload (design §5.4), what that class's own `clone_obj`
    /// answers: a fresh entry of the same class holding what this one
    /// holds, one more owner each, as `zend_objects_clone_members`
    /// copies them, with `payload`, the class's copy of this object's
    /// own, in the new entry's slot, and the class's `__clone` served
    /// on it. The copy is the caller's, one owner, and is given up with
    /// what `__clone` threw. [`Std`](crate::api::abi::Std)'s slot
    /// refuses such an object, since the heap cannot copy a payload it
    /// only knows how to drop; this is the door the class copies it
    /// through.
    ///
    /// ```php
    /// class D extends DateTime { public $n = 1; function __clone() { $this->n = $this->n + 1; } }
    /// $a = new D('2000-01-01 UTC');
    /// $b = clone $a;
    /// var_dump($b->n, $b->format('Y'), $a->n);
    /// ```
    /// ```expect
    /// int(2)
    /// string(4) "2000"
    /// int(1)
    /// ```
    pub fn clone_with<T: Any + Send>(
        &self,
        req: &mut Request,
        payload: T,
    ) -> Result<PhpObject, Thrown> {
        req.clone_members_payload(*self, payload)
    }

    /// What a `Closure` carries, for a closure object: its native
    /// payload, as php's `zend_closure` is the object's own struct.
    #[must_use]
    pub fn closure<'a>(&self, req: &'a Request) -> Option<&'a ClosureInfo> {
        self.native::<ClosureInfo>(req)
    }

    /// The variables a closure bound, by the names its declaration
    /// wrote, in the order the table holds them, which is what
    /// `ReflectionFunctionAbstract::getClosureUsedVariables` walks the
    /// `BIND_STATIC` prologue for: the `use` variables alone and no
    /// `static` of the body, a name whose slot nothing filled left out,
    /// php's `Z_ISUNDEF(bucket->val)`, and a name bound by reference
    /// standing for the cell, as php's bucket holds one. Empty for an
    /// object that is no closure and for a closure that bound nothing.
    /// The values are views, no owner taken; the `Err` is the memory
    /// limit refusing a name.
    pub fn closure_bound(&self, req: &mut Request) -> Result<Vec<(PhpString, Value)>, Thrown> {
        let bound = req.closure_bound(*self, true);
        let mut out = Vec::with_capacity(bound.len());
        for (name, value) in bound {
            out.push((PhpString::new(req, &name)?, value));
        }
        Ok(out)
    }

    /// The declared and dynamic properties in declaration order,
    /// `get_object_vars` from inside the class: views, no owner taken.
    /// A declared name is the interned string php's is, so the `Err` is
    /// the memory limit refusing the intern.
    pub fn props(&self, req: &mut Request) -> Result<Vec<(PhpString, Value)>, Thrown> {
        req.object_entries(self.0)
    }

    /// `zend_get_properties_for(obj, ZEND_PROP_PURPOSE_DEBUG)`'s first
    /// step, `zend_std_get_properties_for`'s call of the class's
    /// `get_debug_info` (`Z/Zend/zend_object_handlers.c:2179`): the
    /// table a class shows in place of its properties, with one owner,
    /// the caller's, which is php's `*is_temp` one and is released once
    /// the rows have been read. `None` for a class that shows its
    /// members, which [`PhpObject::props_for`] answers under the same
    /// purpose; a dumper asks this first and that second, so a typed
    /// property nothing has written still shows as one.
    pub fn debug_info(&self, req: &mut Request) -> Result<Option<PhpArray>, Thrown> {
        req.object_handlers(self.0).get_debug_info(req, *self)
    }

    /// The properties one reader of them sees, `zend_get_properties_for`
    /// under its purpose: the declared slots that hold a value in slot
    /// order and then the dynamic table, each name carrying the prefix
    /// `zend_mangle_property_name` puts before it, `\0*\0p` for a
    /// protected property and `\0C\0p` for a private one, which is what
    /// `print_r` unmangles and `var_export` strips. Views, no owner
    /// taken, and a mangled name is interned as php's is; the `Err` is
    /// the memory limit refusing the intern.
    pub fn props_for(
        &self,
        req: &mut Request,
        purpose: PropertyPurpose,
    ) -> Result<Vec<(PhpString, Value)>, Thrown> {
        crate::ops::properties_for(req, *self, purpose)
    }

    /// The properties as array entries, mangled names and all, what
    /// `(array)` produces: [`PhpObject::props_for`] under
    /// [`PropertyPurpose::ArrayCast`], so the class's
    /// `get_properties_for` slot is asked and a declared name carries
    /// the prefix `zend_mangle_property_name` puts before it, `\0*\0p`
    /// for a protected property and `\0C\0p` for a private one, which
    /// is how `crate::ops::to_array` keys the same rows. A caller that
    /// wants the names as declared, `get_object_vars` from inside the
    /// class, wants [`PhpObject::props`] instead. Views, no owner
    /// taken; the `Err` is the memory limit refusing the intern of a
    /// mangled name.
    ///
    /// Nothing in the engine or the extensions calls this yet, so the
    /// mangling it promises is the one `crate::ops::properties_for`
    /// carries the witness for, `print_r` over a class whose parent
    /// declares a private property of the same name.
    pub fn entries(&self, req: &mut Request) -> Result<Vec<(Key, Value)>, Thrown> {
        Ok(self
            .props_for(req, PropertyPurpose::ArrayCast)?
            .into_iter()
            .map(|(name, value)| (Key::from_string(name), value))
            .collect())
    }

    /// One property by name, without a handler in the way: a view of
    /// it, and `None` for a property that is not there or a declared
    /// one nothing has written.
    #[must_use]
    pub fn prop(&self, req: &Request, name: &[u8]) -> Option<Value> {
        req.object_prop(self.0, name)
    }

    /// Write one property by name through the class's `write_property`
    /// (design §5.4). The value is the object's from here, and so is the name, which is
    /// given up once the slot or the table has taken its own, as
    /// `zend_hash_update` takes a key's. What the property held is
    /// released last, after the write, which is where a destructor is
    /// served (`zend_assign_to_variable`). The `Err` is the memory
    /// limit refusing the dynamic table.
    pub fn set_prop(&self, req: &mut Request, name: PhpString, value: Value) -> Result<(), Thrown> {
        let table = req.object_handlers(self.0);
        table.write_property(req, *self, name, value)
    }

    /// One property written as `unserialize()` writes it,
    /// [`Request::unserialize_property`]: the key as a serialized object
    /// carries it, mangled for a protected or a private property, placed
    /// in the declared slot it names without the class's handlers, or
    /// written into the dynamic table under the key; the value and the
    /// key are the object's from here.
    pub fn unserialize_prop(
        &self,
        req: &mut Request,
        key: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        req.unserialize_property(*self, key, value)
    }

    /// One declared property by slot, `OBJ_PROP_NUM(obj, index)` by
    /// index among the class's declared instance properties, in
    /// declaration order: a view, no owner taken, and `None` for an
    /// index past the declared count or a slot nothing has written.
    #[must_use]
    pub fn slot(&self, index: u32) -> Option<Value> {
        if index >= self.0.slots() {
            return None;
        }
        let z = self.0.slot(index);
        (!z.is_undef()).then(|| Value::of_zval(z))
    }

    /// Write one declared property by slot, `OBJ_PROP_NUM(obj, index)`
    /// with no name in hand, what php's `add_token` does over a fresh
    /// `PhpToken`: no handler, no name interned, no scan of the
    /// declaration. The value is the slot's from here and what the slot
    /// held is released last, after the write, which is where a
    /// destructor is served (`zend_assign_to_variable`). An index past
    /// the declared count is refused as the internal-error fatal, the
    /// value released; a native that knows its class's layout never
    /// reaches it.
    pub fn set_slot(&self, req: &mut Request, index: u32, value: Value) -> Result<(), Thrown> {
        req.write_declared_slot(self.0, index, value)
    }

    /// `GC_IS_RECURSIVE`: whether a walk is already inside this object,
    /// which is what a dumper prints `*RECURSION*` for.
    #[must_use]
    pub fn is_recursive(&self) -> bool {
        self.0.is_recursive()
    }

    /// `GC_PROTECT_RECURSION`: the mark a walk sets before it reads the
    /// object's properties.
    pub fn protect(&self) {
        self.0.protect();
    }

    /// `GC_UNPROTECT_RECURSION`: the mark cleared on the way out, so an
    /// object reached twice side by side is printed in full both times.
    pub fn unprotect(&self) {
        self.0.unprotect();
    }

    /// The native payload, as `T`, borrowed for as long as the request
    /// is: `None` for an object of another payload or of none.
    #[must_use]
    pub fn native<'a, T: Any + Send>(&self, req: &'a Request) -> Option<&'a T> {
        req.native(self.0)
    }

    /// The native payload, mutably, the request held for as long as the
    /// borrow is, so no second name reaches it meanwhile.
    pub fn native_mut<'a, T: Any + Send>(&self, req: &'a mut Request) -> Option<&'a mut T> {
        req.native_mut(self.0)
    }

    /// Whether the object is a lazy ghost or proxy nothing has
    /// initialised, `zend_object_is_lazy(obj) &&
    /// !zend_lazy_object_initialized(obj)`.
    #[must_use]
    pub fn is_lazy(&self) -> bool {
        matches!(self.0.laziness(), Laziness::Ghost | Laziness::Proxy)
    }

    /// `OBJ_EXTRA_FLAGS(obj)`'s two lazy bits as the one state they
    /// encode, what `php_var_dump_object_prefix` reads.
    #[must_use]
    pub fn laziness(&self) -> Laziness {
        self.0.laziness()
    }

    /// `Z_PROP_FLAG_P(OBJ_PROP(obj, offset)) & IS_PROP_LAZY` for the
    /// declared property `name`, on the object the initialised proxies
    /// lead to: whether it still waits for the initializer.
    #[must_use]
    pub fn is_lazy_prop(&self, req: &Request, name: &[u8]) -> bool {
        req.lazy_property(*self, name)
    }

    /// `zend_lazy_object_get_initializer_zv`: the initializer of a lazy
    /// object as it was given, a view, while nothing has initialised it.
    #[must_use]
    pub fn lazy_initializer(&self, req: &Request) -> Option<Value> {
        req.lazy_initializer(self.0)
    }

    /// `zend_lazy_object_get_instance` over an initialised proxy: the
    /// instance its factory returned, a view; `None` for any other
    /// object.
    #[must_use]
    pub fn lazy_real(&self, req: &Request) -> Option<PhpObject> {
        req.lazy_instance(self.0)
    }
}

/// Two names are equal when they name one object, php's pointer
/// equality, which `===` is for objects.
impl PartialEq for PhpObject {
    fn eq(&self, other: &PhpObject) -> bool {
        self.0.is_same(other.0)
    }
}

impl std::fmt::Debug for PhpObject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

/// A `&$x` reference: one cell several holders share (design §5.5),
/// php's `zend_reference *`, thirty-two bytes of the request's heap
/// holding the value and, once typed properties can be referenced, the
/// sources that type it. Naming one takes no owner, as php's pointer
/// does not; `Copy` is what makes it a pointer and not a cell of its
/// own, and the engine counts the owners where php's handlers do.
#[derive(Clone, Copy)]
pub struct PhpRef(Ref);

impl PhpRef {
    /// The face over a heap reference the engine already holds.
    pub(crate) const fn of(r: Ref) -> PhpRef {
        PhpRef(r)
    }

    /// The heap reference, for the engine's refcount traffic.
    pub(crate) const fn heap(self) -> Ref {
        self.0
    }

    /// A reference holding `value`, php's `ZVAL_NEW_REF`: one owner of
    /// the cell, and `value` the cell's from here, so a caller that
    /// keeps a second name of what `value` names takes its own owner.
    /// The `Err` is the memory limit as the fatal that ends the request.
    pub fn new(req: &mut Request, value: Value) -> Result<PhpRef, Thrown> {
        req.reference(value)
    }

    /// The value held, `Z_REFVAL`: a view, no owner taken.
    #[must_use]
    pub fn get(&self) -> Value {
        Value::of_zval(self.0.val())
    }

    /// Replace the value held, the reference's half of
    /// `zend_assign_to_variable`: `value` is the cell's from here, and
    /// what the cell held is released last, after the write, which is
    /// where a destructor is served.
    pub fn set(&self, req: &mut Request, value: Value) {
        req.reference_set(self.0, value);
    }

    /// The reference as a value, `ZVAL_REF`: named, no owner taken.
    #[must_use]
    pub const fn as_value(&self) -> Value {
        Value::of(Inner::Reference(*self))
    }

    /// `Z_PARAM_ARRAY_EX` with `separate` on a by-reference parameter,
    /// `ZVAL_DEREF` and then `SEPARATE_ZVAL_NOREF` on the cell
    /// (`Z/Zend/zend_API.h`): the array the cell holds, written in
    /// place when it has one owner, and otherwise replaced in the cell by
    /// a copy with one, the shared or immutable original one owner fewer
    /// and no destructor served, since an owner remains. What `sort()`
    /// and the rest of the by-reference natives write through, so the
    /// caller reads the sorted array in its variable. `None` for a cell
    /// holding no array, which is the native's `TypeError` to raise.
    pub fn separate_array(&self, req: &mut Request) -> Result<Option<PhpArray>, Thrown> {
        Ok(req.reference_separate(self.0)?.map(PhpArray::of))
    }
}

/// Two names are equal when they name one cell, php's pointer
/// equality.
impl PartialEq for PhpRef {
    fn eq(&self, other: &PhpRef) -> bool {
        self.0.is_same(other.0)
    }
}

impl Eq for PhpRef {}

/// A reference hashes as the cell it names, so two equal names hash
/// alike and a table keyed by references keys them by identity.
impl std::hash::Hash for PhpRef {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl std::fmt::Debug for PhpRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

/// A resource type as the request's table numbers it
/// (`zend_register_list_destructors_ex`): what a resource carries as
/// php's `int type`, naming its type name and its destructor.
pub use php_heap::ResourceType;

/// `zend_sort` over a slice, for a native that sorts an index table of
/// its own rather than an array, `array_unique` and `array_multisort`:
/// the one statement of php's hybrid, whose order under a comparison
/// that is not transitive is what those natives observe. An array is
/// sorted through [`PhpArray::sort_by`], which runs the same function.
pub use php_heap::zend_sort;

/// What an object's two lazy bits say of it (design §5.4): a ghost or a
/// proxy nothing has initialised, a proxy that forwards to its
/// instance, or neither, which a dumper reads to print `lazy ghost ` or
/// `lazy proxy ` before `object(` ([`PhpObject::laziness`]).
pub use php_heap::Laziness;

/// A resource (design §5.5): php's `zend_resource *`, thirty-two bytes
/// of the request's heap in the list `EG(regular_list)` is, its id the
/// one `get_resource_id()` reads, its type the number
/// [`crate::api::request::Resources::register_type`] handed out and, for
/// an internal type, the native payload as one typed slot. Naming one
/// takes no owner, as php's pointer does not; `Copy` is what makes it a
/// pointer and not a struct, and the engine counts the owners where
/// php's handlers do. The per-type destructor is served through seam 1
/// when the last owner lets go, or once at [`Resource::close`].
#[derive(Clone, Copy)]
pub struct Resource(Res);

impl Resource {
    /// The face over a heap resource the engine already holds.
    pub(crate) const fn of(r: Res) -> Resource {
        Resource(r)
    }

    /// The heap resource, for the engine's refcount traffic.
    pub(crate) const fn heap(self) -> Res {
        self.0
    }

    /// A resource of `kind` carrying `payload`, php's
    /// `zend_register_resource`: one owner, in the list under the id
    /// [`Resource::id`] reads, the payload counted with it. The `Err` is
    /// the memory limit as the fatal that ends the request, or php's
    /// `Resource ID space overflow`.
    pub fn new<T: Any + Send>(
        req: &mut Request,
        kind: ResourceType,
        payload: T,
    ) -> Result<Resource, Thrown> {
        req.new_resource(kind, payload)
    }

    /// The resource as a value, `ZVAL_RES`: named, no owner taken.
    #[must_use]
    pub const fn as_value(&self) -> Value {
        Value::of(Inner::Resource(*self))
    }

    /// `get_resource_id()`: the list's index, `Z_RES_HANDLE`.
    #[must_use]
    pub fn id(&self) -> i64 {
        self.0.id()
    }

    /// The type, and `None` once closed.
    #[must_use]
    pub fn kind(&self) -> Option<ResourceType> {
        self.0.resource_type()
    }

    /// Whether [`Resource::close`] ran: `resource (closed)` in every
    /// type name php prints.
    #[must_use]
    pub fn is_closed(&self) -> bool {
        self.0.is_closed()
    }

    /// `zend_rsrc_list_get_rsrc_type`: the type's name as registered,
    /// and `None` once closed or for a type the table does not know.
    #[must_use]
    pub fn type_name<'a>(&self, req: &'a Request) -> Option<&'a [u8]> {
        req.resource_type_name(self.kind()?)
    }

    /// `zend_list_close`: the per-type destructor served once and the
    /// payload dropped, the resource left closed for its owners, which
    /// is what `fclose($f)` leaves `$f` as.
    pub fn close(&self, req: &mut Request) {
        req.close_resource(self.0);
    }

    /// The native payload, as `T`, borrowed for as long as the request
    /// is: `None` for a resource of another payload, of none or closed.
    #[must_use]
    pub fn native<'a, T: Any + Send>(&self, req: &'a Request) -> Option<&'a T> {
        req.resource_native(self.0)
    }

    /// The native payload, mutably, the request held for as long as the
    /// borrow is, so no second name reaches it meanwhile.
    pub fn native_mut<'a, T: Any + Send>(&self, req: &'a mut Request) -> Option<&'a mut T> {
        req.resource_native_mut(self.0)
    }
}

/// Two names are equal when they name one resource, php's pointer
/// equality, which `===` is for resources.
impl PartialEq for Resource {
    fn eq(&self, other: &Resource) -> bool {
        self.0.is_same(other.0)
    }
}

impl std::fmt::Debug for Resource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.0)
    }
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

/// The states a value has, the zval's own set.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
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

/// One PHP value. It is `Copy`, as design §5.1 decides a zval is, and
/// the copy is php's `ZVAL_COPY_VALUE`: sixteen bytes copied whatever
/// the kind, no refcount touched, so the copy names what the original
/// names and owns nothing the original did not give up; the engine's
/// `addref` beside it is `ZVAL_COPY`, and the engine takes and releases
/// owners where php's handlers do.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
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

    /// A reference, not followed.
    #[must_use]
    pub const fn reference(r: PhpRef) -> Value {
        Value::of(Inner::Reference(r))
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

    /// `Z_TYPE`, the zval's type byte as `Z/Zend/zend_types.h:621-629`
    /// numbers it: what a declared type's mask is indexed by, so
    /// `false` and `true` are two codes where [`Value::tag`] has one
    /// `Bool`. A reference is the code of what it holds, as
    /// `zend_check_type` reads it through `Z_REFVAL_P`.
    pub(crate) fn type_code(&self) -> u8 {
        match &self.inner {
            Inner::Undef => 0,
            Inner::Null => 1,
            Inner::Bool(false) => 2,
            Inner::Bool(true) => 3,
            Inner::Int(_) => 4,
            Inner::Float(_) => 5,
            Inner::Str(_) => 6,
            Inner::Array(_) => 7,
            Inner::Object(_) => 8,
            Inner::Resource(_) => 9,
            Inner::Reference(r) => r.get().type_code(),
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
            Inner::Resource(r) if r.is_closed() => "resource (closed)",
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

    /// The value a reference holds, or the value itself, `ZVAL_DEREF`:
    /// a view, no owner taken, since a reference holds one value and
    /// never another reference. Not `Deref`: a value is not a pointer to
    /// another value.
    #[must_use]
    #[allow(clippy::should_implement_trait)]
    pub fn deref(&self) -> Value {
        match &self.inner {
            Inner::Reference(r) => r.get(),
            _ => *self,
        }
    }

    /// The value with `UNDEF` read as `null`, which is how
    /// `zend_zval_value_name` names a slot nothing wrote.
    pub(crate) fn undef_as_null(&self) -> Value {
        match self.inner {
            Inner::Undef => Value::null(),
            _ => *self,
        }
    }

    /// The refcount of a refcounted value, `Z_REFCOUNT`; one for a
    /// scalar, an interned string or an immutable array.
    #[must_use]
    pub fn refcount(&self) -> u32 {
        match &self.inner {
            Inner::Str(s) => s.heap().refcount(),
            Inner::Array(a) => a.heap().refcount(),
            Inner::Object(o) => o.heap().refcount(),
            Inner::Resource(r) => r.heap().refcount(),
            Inner::Reference(r) => r.heap().refcount(),
            _ => 1,
        }
    }

    /// A zval as the face names it, no owner taken: what it names is
    /// named and not copied, as php's `ZVAL_COPY_VALUE` names it.
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
        if let Some(o) = zval.as_obj() {
            return Value::object(PhpObject::of(o));
        }
        if let Some(r) = zval.as_resource() {
            return Value::resource(Resource::of(r));
        }
        if let Some(r) = zval.as_reference() {
            return Value::reference(PhpRef::of(r));
        }
        match zval.kind() {
            ZvalType::Undef => Value::undef(),
            _ => Value::null(),
        }
    }

    /// The zval this value is: the sixteen bytes it names, no owner
    /// taken, `UNDEF` for a slot nothing wrote.
    pub(crate) fn to_zval(self) -> Zval {
        match self.inner {
            Inner::Undef => Zval::UNDEF,
            Inner::Null => Zval::null(),
            Inner::Bool(b) => Zval::bool(b),
            Inner::Int(n) => Zval::long(n),
            Inner::Float(f) => Zval::double(f),
            Inner::Str(s) => Zval::string(s.heap()),
            Inner::Array(a) => Zval::array(a.heap()),
            Inner::Object(o) => Zval::object(o.heap()),
            Inner::Resource(r) => Zval::resource(r.heap()),
            Inner::Reference(r) => Zval::reference(r.heap()),
        }
    }

    /// The zval this value is inside an array's bucket or a property
    /// slot, where `UNDEF` is a tombstone and never an element: a value
    /// nothing wrote is `null` there, as php reads one.
    pub(crate) fn into_zval(self) -> Zval {
        match self.inner {
            Inner::Undef => Zval::null(),
            _ => self.to_zval(),
        }
    }

    /// `Some` for a `bool`. Every `as_*` reads the value as it stands,
    /// as php's `Z_*_P` do: a reference is not followed, and
    /// [`Value::deref`] is the `ZVAL_DEREF` before it.
    #[must_use]
    pub fn as_bool(&self) -> Option<bool> {
        match self.inner {
            Inner::Bool(b) => Some(b),
            _ => None,
        }
    }

    /// `Some` for an `int`.
    #[must_use]
    pub fn as_int(&self) -> Option<i64> {
        match self.inner {
            Inner::Int(n) => Some(n),
            _ => None,
        }
    }

    /// `Some` for a `float`.
    #[must_use]
    pub fn as_float(&self) -> Option<f64> {
        match self.inner {
            Inner::Float(f) => Some(f),
            _ => None,
        }
    }

    /// `Some` for a `string`.
    #[must_use]
    pub fn as_php_string(&self) -> Option<&PhpString> {
        match &self.inner {
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
        match &self.inner {
            Inner::Array(a) => Some(a),
            _ => None,
        }
    }

    /// `Some` for an `object`.
    #[must_use]
    pub fn as_object(&self) -> Option<&PhpObject> {
        match &self.inner {
            Inner::Object(o) => Some(o),
            _ => None,
        }
    }

    /// `Some` for a `resource`.
    #[must_use]
    pub fn as_resource(&self) -> Option<&Resource> {
        match &self.inner {
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
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// Copying a `Value` that holds an array names the one array
    /// (`ZVAL_COPY_VALUE`), the engine's `addref` is the second owner
    /// (`ZVAL_COPY`), and a write through the second name separates it
    /// as `SEPARATE_ARRAY` does: the first name reads what it read.
    #[test]
    fn a_copy_is_a_refcount_and_a_write_separates() {
        let mut req = crate::test::request();
        let start = req.memory_usage(false);
        let Ok(mut a) = PhpArray::new(&mut req) else {
            return;
        };
        assert!(a.append(&mut req, Value::int(1)).is_ok());
        let first = Value::array(a);
        let second = first;
        assert_eq!(first.refcount(), 1, "a copy touches no refcount");
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
        assert_eq!(req.memory_usage(false), start);
    }

    /// An object a native builds is a store entry out of the request's
    /// heap, php's forty bytes for a class of no declared properties,
    /// `spl_object_id()` is its id, copying a `Value` that holds it
    /// is `ZVAL_COPY_VALUE` and the engine's `addref` the second owner;
    /// an array holds it as one more owner and gives it back with the
    /// array, and the last release frees it, the heap reading what it
    /// read before.
    #[test]
    fn an_object_is_a_store_entry_and_a_copy_is_a_refcount() {
        let mut req = crate::test::request();
        let start = req.memory_usage(false);
        let Ok(object) = req.new_object_of("stdClass") else {
            return;
        };
        assert_eq!(req.memory_usage(false) - start, 40);
        assert_eq!(object.index(), 1);
        let first = object.as_value();
        let second = first;
        assert_eq!(first.refcount(), 1, "a copy touches no refcount");
        req.addref(&second);
        assert_eq!(first.refcount(), 2);
        assert_eq!(second.as_object(), Some(&object));
        let Ok(mut a) = PhpArray::new(&mut req) else {
            return;
        };
        assert!(a.append(&mut req, second).is_ok());
        assert_eq!(
            a.get(&Key::Int(0))
                .and_then(|v| v.as_object().map(PhpObject::index)),
            Some(1)
        );
        req.release(Value::array(a));
        assert_eq!(first.refcount(), 1, "the array gave its owner back");
        req.release(first);
        assert_eq!(req.memory_usage(false), start);
        let Ok(next) = req.new_object_of("stdClass") else {
            return;
        };
        assert_eq!(next.index(), 1, "the id came back off the free list");
    }

    /// A property written is the object's: read back as a view, the
    /// name given up once the table took its own, and the value
    /// released with the object.
    #[test]
    fn a_property_is_the_object_s_and_goes_with_it() {
        let mut req = crate::test::request();
        let start = req.memory_usage(false);
        let Ok(object) = req.new_object_of("stdClass") else {
            return;
        };
        let (Ok(name), Ok(text)) = (
            PhpString::new(&mut req, b"message"),
            PhpString::new(&mut req, b"a string the object holds"),
        ) else {
            return;
        };
        assert!(object.set_prop(&mut req, name, Value::string(text)).is_ok());
        assert_eq!(text.heap().refcount(), 1, "the table holds the one owner");
        assert_eq!(
            object
                .prop(&req, b"message")
                .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec)),
            Some(b"a string the object holds".to_vec())
        );
        assert_eq!(object.prop(&req, b"missing"), None);
        assert_eq!(object.entries(&mut req).map(|e| e.len()), Ok(1));
        req.release(object.as_value());
        assert_eq!(
            req.memory_usage(false),
            start,
            "the table and its string went with it"
        );
    }

    /// A reference is php's thirty-two bytes of the request's heap, a
    /// value naming it is followed by `deref` and by every `as_*`, a
    /// write through it releases what the cell held, and the last
    /// release gives the cell back with what it holds.
    #[test]
    fn a_reference_is_a_cell_of_the_heap_and_is_followed_by_deref() {
        let mut req = crate::test::request();
        let start = req.memory_usage(false);
        let Ok(r) = PhpRef::new(&mut req, Value::int(3)) else {
            return;
        };
        assert_eq!(req.memory_usage(false) - start, 32);
        let v = r.as_value();
        assert_eq!(v.tag(), TypeTag::Reference);
        assert_eq!(v.as_int(), None, "not followed, as Z_LVAL_P is not");
        assert_eq!(v.deref().as_int(), Some(3));
        assert_eq!(v.type_name(), "int");
        assert_eq!(v.refcount(), 1);
        let Ok(text) = PhpString::new(&mut req, b"held through the reference") else {
            return;
        };
        r.set(&mut req, Value::string(text));
        assert_eq!(
            v.deref().as_str_bytes().map(<[u8]>::to_vec),
            Some(b"held through the reference".to_vec())
        );
        assert_eq!(text.heap().refcount(), 1, "the cell holds the one owner");
        req.release(v);
        assert_eq!(
            req.memory_usage(false),
            start,
            "the cell and its string went"
        );
    }

    /// A resource is php's thirty-two bytes in the request's list, its
    /// id from one, its type name the table's, a close serves the
    /// destructor once and leaves it `resource (closed)`, and the last
    /// release frees it.
    #[test]
    fn a_resource_is_a_list_entry_closed_once_and_freed_at_the_last_release() {
        fn dtor(req: &mut Request, r: Resource) {
            let seen = r.native::<u32>(req).copied().unwrap_or(0);
            let _ = req.define_constant(b"dtor", Value::int(i64::from(seen)));
        }
        let mut req = crate::test::request();
        let start = req.memory_usage(false);
        let kind = req.resources().register_type(b"stream", Some(dtor));
        let Ok(r) = Resource::new(&mut req, kind, 7u32) else {
            return;
        };
        // The resource, its payload, and the list's own packed table,
        // 160, which the request's first resource lays down.
        assert_eq!(req.memory_usage(false) - start, 160 + 32 + 16);
        assert_eq!(r.id(), 1);
        assert_eq!(r.type_name(&req), Some(&b"stream"[..]));
        assert_eq!(r.native::<u32>(&req), Some(&7));
        let v = r.as_value();
        assert_eq!(v.type_name(), "resource");
        assert_eq!(req.resource(1), Some(r));
        r.close(&mut req);
        r.close(&mut req);
        assert_eq!(v.type_name(), "resource (closed)");
        assert_eq!(r.type_name(&req), None);
        assert_eq!(r.native::<u32>(&req), None);
        assert_eq!(req.resource(1), Some(r), "closed, still listed");
        assert_eq!(
            req.constant_named(b"dtor").and_then(|v| v.as_int()),
            Some(7),
            "the destructor ran once, reading the payload"
        );
        assert_eq!(req.memory_usage(false) - start, 160 + 32);
        req.release(v);
        assert_eq!(req.resource(1), None);
        assert_eq!(
            req.memory_usage(false),
            start + 160,
            "the list's table stays until the request ends"
        );
        let Ok(next) = Resource::new(&mut req, kind, 0u8) else {
            return;
        };
        assert_eq!(next.id(), 2, "an id is never given twice");
    }
}

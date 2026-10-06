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
//! **Ownership, as design §5.1 decides it, in the words Mitchell settled
//! for it on #1980: a native gives up exactly the owners it took and did
//! not hand on.** The kinds here are views:
//! each is `Copy` and has no `Drop`, and copying a `Value` is php's
//! `ZVAL_COPY_VALUE`, a bitwise copy that touches no refcount and
//! carries no owner, good while an owner elsewhere keeps the value
//! alive. An owner is an [`Owner`], one counted claim on a value,
//! neither `Copy` nor `Clone`: [`Owner::take`] is the one conversion
//! from a view, `Z_TRY_ADDREF`, and an owner leaves a native in one of
//! two ways, handed to a sink, a call that takes it from its caller,
//! the engine taking a native's return among them, or given up with
//! [`Owner::give_up`], `zval_ptr_dtor`. Only the engine names an owner
//! it did not take. What the engine hands a native is the frame's, a
//! view the native takes an owner of where it keeps the value past the
//! call or hands it on. A native that drops an owner unconsumed is
//! wrong: a debug build reports it with the line that made it, and a
//! release build runs nothing on drop, so what it kept is freed with
//! the request's memory at its end.
//!
//! The constructors and sinks move to [`Owner`] one sink at a time
//! (#1980): until a constructor returns one, the view it returns is the
//! one owner the value was made with, and a sink that takes a view, or
//! a native that returns one, takes that owner with it.

use std::any::Any;
use std::cmp::Ordering;

use php_heap::{Arr, Obj, Ref, Res, Str, Zval, ZvalType};

use crate::api::abi::{ClassRef, PropertyPurpose};
use crate::api::call::Function;
use crate::api::raising::Thrown;
use crate::api::request::Request;

mod emalloc;
mod fresh;
mod owner;
mod smart_str;
pub mod walk;
pub use emalloc::Emalloc;
pub use fresh::FreshString;
pub use owner::{Owner, View};
pub use smart_str::SmartStr;

/// The type of a value, php's `zend_zval_type_name` set, and the one
/// pseudo type php's `cast_object` is asked for beside them.
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
    /// `_IS_NUMBER`: no value's type, but what arithmetic asks an object's
    /// `cast_object` for, an `Int` or a `Float`
    /// (`zendi_try_convert_scalar_to_number`).
    Number,
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

    /// `zend_string_init_interned`: the one interned string for these
    /// bytes, never refcounted, what php hands out for a name a
    /// declaration made (`ce->name`, `fptr->common.function_name`,
    /// `prop_info->name`): the engine's permanent string where the engine
    /// or the bundle declared the name, which costs the request nothing,
    /// else the request's own, allocated on its first ask and shared by
    /// every later one. The `Err` is the memory limit.
    ///
    /// ```php
    /// $r = new ReflectionClass('stdClass');
    /// $m = memory_get_usage();
    /// $s = new ReflectionClass('stdClass');
    /// var_dump(memory_get_usage() - $m);
    /// ```
    /// ```expect
    /// int(96)
    /// ```
    pub fn interned(req: &mut Request, bytes: &[u8]) -> Result<PhpString, Thrown> {
        req.interned(bytes)
    }

    /// `zend_string_init_existing_interned`: the interned string for
    /// these bytes where the engine or the request holds one already, a
    /// name or a constant some unit declared, which costs nothing and
    /// interns nothing new; a new string of the request's otherwise, as
    /// [`PhpString::new`] makes it. What php's unserializer makes of an
    /// array's or an object's key. The `Err` is the memory limit.
    pub fn existing_interned(req: &mut Request, bytes: &[u8]) -> Result<PhpString, Thrown> {
        req.existing_interned(bytes)
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

    /// The key as a property name with one owner, the caller's: an
    /// integer key printed in decimal, `zend_long_to_str`, and a string
    /// key with an owner taken, `zend_string_copy`, so either arm can be
    /// handed to [`PhpObject::set_prop`] or a `write_property`, which
    /// give the name up once the object has taken its own.
    pub fn to_prop_name(&self, req: &mut Request) -> Result<PhpString, Thrown> {
        match self {
            Key::Int(n) => PhpString::new(req, n.to_string().as_bytes()),
            Key::Str(s) => {
                req.addref(&Value::string(*s));
                Ok(*s)
            }
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

/// How a table [`PhpArray::sized`] lays down is laid out: php's packed
/// array, a list of values indexed from zero, or its hash, the buckets
/// and their hash slots, `zend_hash_real_init_packed` and
/// `zend_hash_real_init_mixed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Packed,
    Hash,
}

impl PhpArray {
    /// An empty array out of the request's heap, php's `array_init`:
    /// fifty-six bytes for the header, nothing for elements until the
    /// first write. The `Err` is the memory limit, ending the native as
    /// `zend_mm` bails out.
    pub fn new(req: &mut Request) -> Result<PhpArray, Thrown> {
        req.array()
    }

    /// `zend_new_array(size)` and `zend_hash_init(ht, size, ...)`, an
    /// empty array whose first write lays its table down for `size`
    /// elements, the size held to `zend_hash_check_size`: the header
    /// alone until then, as [`PhpArray::new`] is, where
    /// [`PhpArray::sized`] lays the table down at once. The `Err` is the
    /// memory limit.
    pub fn init_size(req: &mut Request, size: u32) -> Result<PhpArray, Thrown> {
        req.array_sized(size, true)
    }

    /// `RETURN_EMPTY_ARRAY`, `ZVAL_EMPTY_ARRAY`: the process's one
    /// immutable empty array, php's `zend_empty_array`, which costs the
    /// request nothing and is what a native answers where php's body
    /// answers an array with nothing in it. No owner is counted on it;
    /// a write separates it into an array of the request's own, as it
    /// separates any immutable array, so a native that will fill an
    /// array builds it with [`PhpArray::new`].
    ///
    /// ```php
    /// $m = memory_get_usage();
    /// $a = array_slice([1], 1);
    /// var_dump(memory_get_usage() - $m);
    /// debug_zval_dump($a);
    /// $a[] = 1;
    /// var_dump($a, array_slice([1], 1));
    /// ```
    /// ```expect
    /// int(0)
    /// array(0) interned {
    /// }
    /// array(1) {
    ///   [0]=>
    ///   int(1)
    /// }
    /// array(0) {
    /// }
    /// ```
    #[must_use]
    pub fn empty() -> PhpArray {
        PhpArray(Arr::empty())
    }

    /// `array_init_size(size)`, `zend_new_array(size)`: an empty array
    /// sized for `size` elements, the size held to
    /// `zend_hash_check_size`, whose `Possible integer overflow in memory
    /// allocation` fatal ends the request past `HT_MAX_SIZE` before any
    /// element is written, and laid down at once as `layout` says,
    /// `zend_hash_real_init_packed` or `zend_hash_real_init_mixed`, which
    /// `debug_zval_dump` tells apart. The elements' memory is asked for
    /// here, so a table php real-inits and leaves empty costs what php's
    /// does.
    ///
    /// ```php
    /// var_dump(count(array_fill(5, 3, 'x')));
    /// $a = array_fill(0, 2147483647, 1);
    /// ```
    /// ```expect exit=255
    /// int(3)
    ///
    /// Fatal error: Possible integer overflow in memory allocation (2147483647 * 32 + 32) in %s on line 3
    /// ```
    ///
    /// The unserializer lays every array down as a hash, and `array_fill`
    /// one that does not start inside its count.
    ///
    /// ```php
    /// $u = unserialize('a:2:{i:0;i:1;i:1;i:2;}'); $h = array_fill(5, 2, 0); $p = array_fill(0, 2, 0);
    /// debug_zval_dump($u, $h, $p);
    /// ```
    /// ```expect
    /// array(2) refcount(2){
    ///   [0]=>
    ///   int(1)
    ///   [1]=>
    ///   int(2)
    /// }
    /// array(2) refcount(2){
    ///   [5]=>
    ///   int(0)
    ///   [6]=>
    ///   int(0)
    /// }
    /// array(2) packed refcount(2){
    ///   [0]=>
    ///   int(0)
    ///   [1]=>
    ///   int(0)
    /// }
    /// ```
    pub fn sized(req: &mut Request, size: u32, layout: Layout) -> Result<PhpArray, Thrown> {
        let laid = match layout {
            Layout::Packed => req.heap_mut().array_packed(size),
            Layout::Hash => req.heap_mut().array_mixed(size),
        };
        req.fatal_of(laid).map(PhpArray::of)
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

    /// `HT_IS_PACKED`: whether the table is php's packed list, its slots
    /// the integer keys from zero with holes where elements were
    /// removed, and not a hash, which is what `debug_zval_dump` prints
    /// `packed` for; a list of keys `0..len` can be either.
    #[must_use]
    pub fn is_packed(&self) -> bool {
        self.0.is_packed()
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

    /// The value under the string key of these bytes, a view of it:
    /// php's `zend_hash_str_find`, which hashes the bytes and walks the
    /// chain and so allocates nothing, where a [`Key::Str`] would be a
    /// request string made for the lookup alone. The bytes are the key
    /// as written, an option's or a field's name; `"1"` finds the
    /// string key `"1"`, which no array holds, as php's does.
    ///
    /// ```php
    /// $s = serialize([1]);
    /// unserialize($s, ['max_depth' => 4, 'allowed_classes' => false]);
    /// $m = memory_get_usage();
    /// unserialize($s, ['max_depth' => 4, 'allowed_classes' => false]);
    /// var_dump(memory_get_usage() - $m);
    /// ```
    /// ```expect
    /// int(0)
    /// ```
    #[must_use]
    pub fn find(&self, name: &[u8]) -> Option<Value> {
        self.0.str_find(name).map(Value::of_zval)
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

    /// Lay `value` down at `key` as `zend_hash_add_new` and
    /// `zend_symtable_add_new` do after `SEPARATE_ARRAY`: a string key
    /// appended without the lookup, so a key the array holds already is
    /// held twice and a lookup finds this row. The value and a string key
    /// are the array's from here.
    #[allow(clippy::needless_pass_by_value)]
    pub(crate) fn add_new(
        &mut self,
        req: &mut Request,
        key: Key,
        value: Value,
    ) -> Result<(), Thrown> {
        self.separate(req)?;
        req.array_add_new(self.0, key.heap(), value)?;
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
        Err(crate::api::raising::throw_error(
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

    /// The value at `position`, a view, and where it stands: php's
    /// `(zval *)((char *)ht->arData + offset)`, how `ZEND_BIND_STATIC`
    /// reaches a slot of a `static` table.
    pub(crate) fn at_position(self, position: u32) -> Option<(Value, Place)> {
        let at = self.0.value_ref_at(position)?;
        Some((Value::of_zval(at.read()), Place(at)))
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

    /// Where the value at `key` stands, `zend_hash_find` answering the
    /// bucket's `zval *`; `None` where the array has no `key`. The array
    /// is written through the place, so a caller separates it first.
    #[must_use]
    pub fn place(&self, key: &Key) -> Option<Place> {
        self.0.value_ref(key.heap()).map(Place)
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
    ///
    /// The table is held one owner more while it is sorted,
    /// `zend_array_sort_ex`'s `GC_ADDREF(ht)`, so a comparison whose code
    /// writes the array separates it: the variable takes the copy with the
    /// write, and the table sorted is given up after, destroyed where
    /// nothing else holds it.
    ///
    /// ```php
    /// function resize_arr() { global $arr; for ($i = 0; $i < 4; $i++) { $arr[$i] = $i; } }
    /// class C { function __toString() { resize_arr(); return '3'; } }
    /// $arr = ['a' => '1', '3' => new C, '2' => '2'];
    /// asort($arr);
    /// var_dump($arr);
    /// ```
    /// ```expect
    /// array(5) {
    ///   ["a"]=>
    ///   string(1) "1"
    ///   [3]=>
    ///   int(3)
    ///   [2]=>
    ///   int(2)
    ///   [0]=>
    ///   int(0)
    ///   [1]=>
    ///   int(1)
    /// }
    /// ```
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
        let held = Owner::take(req, self.to_value());
        let mut pending: Option<Thrown> = None;
        let sorted = req.array_sort(self.0, renumber, &mut |req, x, y| {
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
        });
        held.give_up(req);
        sorted?;
        pending.map_or(Ok(()), Err)
    }

    /// The array as a value.
    #[must_use]
    pub const fn to_value(self) -> Value {
        Value::array(self)
    }

    /// Whether `other` names the one table this does, php's
    /// `Z_ARR_P(a) == Z_ARR_P(b)`: the identity, where `==` reads the
    /// elements.
    #[must_use]
    pub fn is_same(&self, other: PhpArray) -> bool {
        self.0.is_same(other.0)
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

/// The table `zend_get_properties_for` hands a reader, held until
/// `zend_release_properties`: [`PhpObject::props_held`]'s rows, each one
/// more owner of its value where the class built the rows for the
/// reader, views of the class's debug table while it is held, and views
/// of the object's own rows otherwise.
#[derive(Debug)]
#[must_use = "the rows are held until they are released"]
pub struct HeldProperties {
    rows: Vec<(Key, HeldRow)>,
    held: Held,
    count: usize,
}

/// What [`HeldProperties`] gives back at its release, php's `*is_temp`
/// table and what `zend_release_properties` does to it.
#[derive(Debug)]
enum Held {
    /// The object's own table: nothing taken.
    Own,
    /// Rows the class built: one owner of each row's value.
    Rows,
    /// The class's debug table, one owner of the table, and what the
    /// class left pending while it built it.
    Table(PhpArray, Option<Thrown>),
}

/// One row of [`HeldProperties`] as `zend_hash` walks it: a value, or
/// under [`PropertyPurpose::Debug`] a typed property nothing has
/// written, php's `IS_INDIRECT` to an `IS_UNDEF` slot that
/// `zend_get_typed_property_info_for_slot` finds a type for, which a
/// dumper shows as `uninitialized(T)` with the type's text.
#[derive(Debug, Clone)]
pub enum HeldRow {
    Value(Value),
    Uninitialized(String),
}

impl HeldProperties {
    /// The rows in the table's order.
    #[must_use]
    pub fn rows(&self) -> &[(Key, HeldRow)] {
        &self.rows
    }

    /// `zend_array_count` over the table (`Z/Zend/zend_hash.c:519`), the
    /// number a dumper prints before the rows: an uninitialised row not
    /// counted, and under [`PropertyPurpose::Debug`] a slot `unset`
    /// empties counted still inside the release that empties it.
    #[must_use]
    pub fn count(&self) -> usize {
        self.count
    }

    /// `zend_release_properties`: the owners the rows held given back.
    /// The `Err` is what the class's `get_debug_info` left pending while
    /// it built the table, handed on once the rows have been read, as
    /// `php_var_dump` and `php_debug_zval_dump` leave `EG(exception)`
    /// standing after the dump (`Z/ext/standard/var.c:184`).
    pub fn release(self, req: &mut Request) -> Result<(), Thrown> {
        match self.held {
            Held::Own => Ok(()),
            Held::Rows => {
                for (_, row) in self.rows {
                    if let HeldRow::Value(value) = row {
                        req.release(value);
                    }
                }
                Ok(())
            }
            Held::Table(table, thrown) => {
                req.release(Value::array(table));
                thrown.map_or(Ok(()), Err)
            }
        }
    }
}

/// Where a value stands in a table, php's `zval *` into a bucket, which
/// a dimension fetch for a write lays into its `VAR` as `IS_INDIRECT`
/// so the write, and an `ASSIGN_REF`, reaches the table itself. Naming
/// the place keeps nothing: it is good until the table is next written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Place(php_heap::ZvalRef);

impl Place {
    /// The heap's place.
    pub(crate) const fn heap(self) -> php_heap::ZvalRef {
        self.0
    }

    pub(crate) const fn of(at: php_heap::ZvalRef) -> Place {
        Place(at)
    }
}

/// php's `HashTableIterator`, one slot of `EG(ht_iterators)`: a bucket
/// position registered at a table, which every write that moves the
/// buckets moves with them (`zend_hash_iterators_update` in a rehash
/// and in the packed-to-hash conversion, `zend_hash_iterators_clamp_max`
/// in a delete), and which `zend_array_dup` copies onto the table a
/// write separates. What a walk that outlives one call keeps, php's
/// `spl_array_object::ht_iter`; given up with [`HashIterator::release`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashIterator(u32);

impl HashIterator {
    /// `zend_hash_iterator_add`: an iterator over `array` at the bucket
    /// `position`.
    #[must_use]
    pub fn new(req: &mut Request, array: &PhpArray, position: u32) -> HashIterator {
        HashIterator(req.heap_mut().iterator_add(array.0, position))
    }

    /// `zend_hash_iterator_pos`: the bucket the iterator stands at in
    /// `array`, moved onto it at the copy's position where `array` is a
    /// separated copy of the table it walked, and at its internal
    /// pointer where it is another table altogether.
    pub fn position(self, req: &mut Request, array: &PhpArray) -> u32 {
        req.heap_mut().iterator_pos(self.0, array.0)
    }

    /// `EG(ht_iterators)[idx].pos = position`.
    pub fn set_position(self, req: &mut Request, position: u32) {
        req.heap_mut().iterator_set_pos(self.0, position);
    }

    /// `zend_hash_iterator_del`: the slot given back.
    pub fn release(self, req: &mut Request) {
        req.heap_mut().iterator_del(self.0);
    }
}

impl PhpArray {
    /// `zend_hash_iterators_lower_pos`: the lowest bucket at or after
    /// `start` a hash iterator on the array stands at, the count of
    /// buckets used where none does. What a native that rebuilds the
    /// array walks its buckets ([`PhpArray::live_from`]) against.
    pub fn iterators_lower_pos(&self, req: &mut Request, start: u32) -> u32 {
        req.heap_mut().iterators_lower_pos(self.0, start)
    }

    /// `zend_hash_iterators_update`: every hash iterator on the array at
    /// bucket `from` moved to `to`.
    pub fn iterators_update(&self, req: &mut Request, from: u32, to: u32) {
        req.heap_mut().iterators_update(self.0, from, to);
    }

    /// `zend_hash_iterators_advance`: every hash iterator on the array
    /// `step` buckets on.
    pub fn iterators_advance(&self, req: &mut Request, step: u32) {
        req.heap_mut().iterators_advance(self.0, step);
    }

    /// The hash iterators on the array moved onto `rebuilt`, at the
    /// positions they have: what php's swap of a rebuilt table's data
    /// into the old header keeps (`HT_SET_ITERATORS_COUNT`), so a
    /// `foreach` by reference over the array goes on in `rebuilt`.
    pub fn iterators_carry(&self, req: &mut Request, rebuilt: &PhpArray) {
        req.heap_mut().iterators_carry(self.0, rebuilt.0);
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
    /// `ZEND_MAP_PTR_INIT`, on the request's heap (design §5.13): each
    /// name of [`php_unit::OpArray::bound_vars`] at the position the
    /// compiler recorded, a `use` variable's `null` until a
    /// `BIND_LEXICAL` beside the declaration fills it and a `static`'s
    /// at the value the table starts it at, and what the `BIND_STATIC`s
    /// of the body read and bind. None where the op array binds nothing.
    /// The closure's one owner; a clone of this value is a view of it.
    pub bound: Option<PhpArray>,
    /// `ZEND_ACC_FAKE_CLOSURE`: whether the closure was made of a
    /// function that already had a name, `zend_create_fake_closure`,
    /// which `Closure::fromCallable` and `Closure::fromCallable`'s kin
    /// make and a closure written in the program never does. `zend_closure_compare`
    /// compares two fake closures and holds every other pair
    /// uncomparable.
    pub fake: bool,
}

/// One move of an internal pointer, a pointer native's
/// ([`PhpObject::pointer_move`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerMove {
    /// `reset()`, `zend_hash_internal_pointer_reset`.
    Reset,
    /// `end()`, `zend_hash_internal_pointer_end`.
    End,
    /// `next()`, `zend_hash_move_forward`.
    Forward,
    /// `prev()`, `zend_hash_move_backwards`.
    Backward,
}

/// The scope a property is written in ([`PhpObject::write_prop_in`]),
/// what `zend_update_property_ex` sets `EG(fake_scope)` to for the
/// write, and so which private or protected property the name reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PropertyScope {
    /// `zend_update_property_ex(prop_info->ce, ..)`: the scope of the
    /// class that declares the property, [`PhpObject::set_prop`]'s.
    Declaring,
    /// `zend_update_property_ex(NULL, ..)`: no fake scope, so
    /// `get_fake_or_executed_scope` decides, the executed one.
    Executing,
    /// `zend_update_property_ex(ce, ..)`: the named class's scope.
    Of(ClassRef),
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

    /// The object's handler table, php's `Z_OBJ_HT_P`: its class's, or
    /// [`Std`](crate::api::abi::Std) for a class that declares none, as
    /// the kernels reach it. What a native asks of another class's
    /// object, as `spl_array_set_array` asks whether its properties are
    /// overloaded
    /// ([`ObjectHandlers::properties_overloaded`](crate::api::abi::ObjectHandlers::properties_overloaded)); the
    /// witness is that slot's.
    #[must_use]
    pub fn handlers(&self, req: &Request) -> &'static dyn crate::api::abi::ObjectHandlers {
        req.object_handlers(self.0)
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
    pub fn props(&self, req: &mut Request) -> Result<Vec<(Key, Value)>, Thrown> {
        req.object_entries(self.0)
    }

    /// `zend_get_properties_for(obj, ZEND_PROP_PURPOSE_DEBUG)`'s first
    /// step, `zend_std_get_properties_for`'s call of the class's
    /// `get_debug_info` (`Z/Zend/zend_object_handlers.c:2179`): the
    /// table a class shows in place of its properties, with one owner,
    /// the caller's, which is php's `*is_temp` one and is released once
    /// the rows have been read. `None` for a class that shows its
    /// members. [`PhpObject::props_held`] under the same purpose asks
    /// this first and reads the members where it answers none, so a
    /// typed property nothing has written still shows as one. The throwable
    /// the class left pending while it built the table rides beside it,
    /// for the dumper to hand on after the rows are printed.
    pub fn debug_info(
        &self,
        req: &mut Request,
    ) -> Result<Option<crate::api::abi::DebugInfo>, Thrown> {
        req.object_handlers(self.0).get_debug_info(req, *self)
    }

    /// The properties one reader of them sees, `zend_get_properties_for`
    /// under its purpose: the declared slots that hold a value in slot
    /// order and then the dynamic table, each name a string key carrying
    /// the prefix `zend_mangle_property_name` puts before it, `\0*\0p`
    /// for a protected property and `\0C\0p` for a private one, which is
    /// what `print_r` unmangles and `var_export` strips; or the rows a
    /// class answers for the purpose, under the keys its table holds
    /// them by, an integer one among them. Views, no owner taken, and a
    /// mangled name is interned as php's is; the `Err` is the memory
    /// limit refusing the intern. Under [`PropertyPurpose::Debug`] a
    /// class may answer a table of its own, which no view outlives, so
    /// that purpose is read through [`PhpObject::props_held`] and is an
    /// `InternalError` here where the class answers one.
    pub fn props_for(
        &self,
        req: &mut Request,
        purpose: PropertyPurpose,
    ) -> Result<Vec<(Key, Value)>, Thrown> {
        crate::ops::properties_for(req, *self, purpose)
    }

    /// [`PhpObject::props_for`] for a reader that holds the table while
    /// it reads, `zend_get_properties_for` to `zend_release_properties`:
    /// where the class builds the table for the reader
    /// (`get_properties_for`, php's `*is_temp`), each row is one more
    /// owner of its value until [`HeldProperties::release`], as
    /// `spl_fixedarray_object_get_properties_for`'s `Z_TRY_ADDREF_P` makes
    /// it, so an element that is the object itself counts while the
    /// reader prints its refcount; the object's own table, which php
    /// hands out with `GC_TRY_ADDREF` on the table alone, takes none.
    /// Under [`PropertyPurpose::Debug`] the class's `get_debug_info` is
    /// asked first, as `zend_std_get_properties_for` asks it
    /// (`Z/Zend/zend_object_handlers.c:2179`), and the table it answers
    /// is held whole, its rows views of it, so a value it alone holds
    /// prints one owner.
    ///
    /// ```php
    /// class D { public int $u; public $a = 1; public function __debugInfo(): array { return ['shown' => str_repeat('s', 2)]; } }
    /// class M { public int $u; public $a = 1; }
    /// debug_zval_dump(new ArrayObject(range(1, 1)), new D, new M, new DateTime('2020-01-01', new DateTimeZone('UTC')));
    /// ```
    /// ```expect
    /// object(ArrayObject)#1 (1) refcount(1){
    ///   ["storage":"ArrayObject":private]=>
    ///   array(1) packed refcount(2){
    ///     [0]=>
    ///     int(1)
    ///   }
    /// }
    /// object(D)#2 (1) refcount(1){
    ///   ["shown"]=>
    ///   string(2) "ss" refcount(1)
    /// }
    /// object(M)#3 (1) refcount(1){
    ///   ["u"]=>
    ///   uninitialized(int)
    ///   ["a"]=>
    ///   int(1)
    /// }
    /// object(DateTime)#4 (3) refcount(1){
    ///   ["date"]=>
    ///   string(26) "2020-01-01 00:00:00.000000" refcount(1)
    ///   ["timezone_type"]=>
    ///   int(3)
    ///   ["timezone"]=>
    ///   string(3) "UTC" refcount(1)
    /// }
    /// ```
    pub fn props_held(
        &self,
        req: &mut Request,
        purpose: PropertyPurpose,
    ) -> Result<HeldProperties, Thrown> {
        use crate::ops::Answered;
        let (rows, held) = match crate::ops::properties_answered(req, *self, purpose)? {
            Answered::Own(rows) if purpose == PropertyPurpose::Debug => {
                let (rows, count) = crate::ops::debug_rows(req, *self, rows)?;
                let held = Held::Own;
                return Ok(HeldProperties { rows, held, count });
            }
            Answered::Own(rows) => (rows, Held::Own),
            Answered::Built(rows) => {
                for (_, value) in &rows {
                    req.addref(value);
                }
                (rows, Held::Rows)
            }
            Answered::Debug(info) => (info.table.snapshot(), Held::Table(info.table, info.thrown)),
        };
        let count = rows.len();
        let rows = rows
            .into_iter()
            .map(|(key, value)| (key, HeldRow::Value(value)))
            .collect();
        Ok(HeldProperties { rows, held, count })
    }

    /// The object's property table as `get_properties` holds it,
    /// php's `zobj->properties`: the declared slots that hold a value,
    /// each keyed by the name `zend_mangle_property_name` makes (`\0*\0p`
    /// for a protected property, `\0C\0p` for a private one), then the
    /// dynamic table, a lazy object initialised first as
    /// `zend_std_get_properties` initialises it. Every row's key is the
    /// string it stands under; views, no owner taken. The rows
    /// [`PhpObject::row`], [`PhpObject::set_row`], [`PhpObject::unset_row`]
    /// address by key; the `Err` is what the
    /// lazy initializer threw or the memory limit refusing a name.
    ///
    /// ```php
    /// #[AllowDynamicProperties] class P { private $a = 1; protected $b = 2; public $c = 3; }
    /// $o = new P; $o->d = 4;
    /// foreach (@new ArrayObject($o) as $k => $v) echo str_replace("\0", '\0', $k), "=$v ";
    /// echo count(@new ArrayObject($o)), "\n";
    /// ```
    /// ```expect
    /// c=3 d=4 2
    /// ```
    pub fn entries(&self, req: &mut Request) -> Result<Vec<(Key, Value)>, Thrown> {
        req.table_entries(*self)
    }

    /// `zend_std_get_properties_ex` for what it leaves behind: the
    /// object's property table built where it has none
    /// (`rebuild_object_properties_internal`), as a class's
    /// `create_object` builds it for its objects from the start
    /// (`pdo_dbh_new`), so `memory_get_usage()` counts the table where
    /// php's does. The `Err` is the memory limit.
    ///
    /// ```php
    /// $r = new ReflectionClass('PDO'); $w = $r->newInstanceWithoutConstructor();
    /// $m = memory_get_usage(); $p = $r->newInstanceWithoutConstructor(); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 264
    /// ```
    pub fn rebuild_properties(&self, req: &mut Request) -> Result<(), Thrown> {
        let table = req.heap_mut().object_properties(self.heap());
        req.fatal_of(table).map(|_| ())
    }

    /// `zobj->properties != NULL && GC_REFCOUNT(zobj->properties) > 1`:
    /// whether the object's property table has a holder beside the
    /// object, as the `(array)` cast or `get_object_vars()` leaves one
    /// that hands the table on, so a reader that counts on one owner
    /// reaching a value asks whether a second can.
    ///
    /// ```php
    /// $t = new stdClass; $t->y = $t; $y = (array) $t; unset($t);
    /// echo serialize($y), "\n";
    /// ```
    /// ```expect
    /// a:1:{s:1:"y";O:8:"stdClass":1:{s:1:"y";r:2;}}
    /// ```
    #[must_use]
    pub fn properties_shared(&self) -> bool {
        self.heap()
            .properties()
            .is_some_and(|table| table.refcount() > 1)
    }

    /// `zend_hash_extend(Z_OBJPROP_P(obj), count + extra, ...)`: the
    /// property table built as [`PhpObject::rebuild_properties`] builds
    /// it and grown at once to hold `extra` rows past the declared and
    /// dynamic ones it holds, what a native that is about to write a
    /// known number of properties does first, php's unserializer among
    /// them. The `Err` is the memory limit.
    pub fn extend_properties(&self, req: &mut Request, extra: u32) -> Result<(), Thrown> {
        let table = req.heap_mut().object_properties_extend(self.heap(), extra);
        req.fatal_of(table).map(|_| ())
    }

    /// `reset`, `end`, `next` or `prev` over the internal pointer of the
    /// object's property table as `zend_std_get_properties` holds it, the
    /// table `get_ht_for_iap` hands the pointer natives
    /// (`Z/ext/standard/array.c:993`): separated first where another
    /// holder shares it, laid down where the object has none, and a lazy
    /// object initialised; `false` and nothing moved where the table has
    /// no row. The pointer stays on the table between calls, and a
    /// declared slot is a row to it whether or not it holds a value, as
    /// php's `IS_INDIRECT` is. What a class whose `get_properties`
    /// answers rows of its own holds is not this table
    /// ([`ObjectHandlers::answers_properties`](crate::api::abi::ObjectHandlers::answers_properties)).
    pub fn pointer_move(&self, req: &mut Request, how: PointerMove) -> Result<bool, Thrown> {
        req.pointer_move(*self, how)
    }

    /// The row at the internal pointer of the object's property table,
    /// `php_array_iter_seek_current`: a declared slot nothing holds, an
    /// uninitialised typed property, stepped over back after an `end` or
    /// a `prev` and on after anything else, `after` saying which move
    /// came before, and the pointer left where the step stops. The key
    /// is the table's, a declared property's its mangled name, and the
    /// key and the value are views; `None` past the end.
    pub fn pointer_row(
        &self,
        req: &mut Request,
        after: Option<PointerMove>,
    ) -> Result<Option<(Key, Value)>, Thrown> {
        req.pointer_row(*self, after)
    }

    /// `zobj->properties = table`, how `spl_array_method` puts back the
    /// table it handed a sort: `table` is the object's property table
    /// from here, one owner passed to it, and holds every row itself,
    /// the declared ones among them as copies the slots go on apart from
    /// ([`php_heap::DeclaredRows::Copied`]); the table the object held is
    /// released. A lazy object is initialised first and its instance
    /// takes the table.
    pub fn replace_table(&self, req: &mut Request, table: PhpArray) -> Result<(), Thrown> {
        req.replace_table(*self, table)
    }

    /// One row of the property table by key, `zend_hash_find_ind`
    /// (`zend_hash_index_find` for an integer key): no handler, hook or
    /// visibility in the way, a declared property
    /// under its mangled name, a view; `None` for a key the table has
    /// not and for a declared slot nothing holds. What `ArrayObject`
    /// reads its backing object through.
    ///
    /// ```php
    /// class H { public $x = 1; private $p = 's'; public string $u { set { throw new Exception('hook'); } } public function __get($n) { return "get $n"; } }
    /// $a = @new ArrayObject(new H);
    /// var_dump($a['x'], isset($a['p']), $a["\0H\0p"], isset($a['u']), @$a['nope']);
    /// ```
    /// ```expect
    /// int(1)
    /// bool(false)
    /// string(1) "s"
    /// bool(false)
    /// NULL
    /// ```
    pub fn row(&self, req: &mut Request, key: &Key) -> Result<Option<Value>, Thrown> {
        req.table_row(*self, key)
    }

    /// One row of the property table written by key,
    /// `zend_hash_update_ind`, and `zend_hash_index_update` for an
    /// integer key, which is a row of its own and never the string that
    /// spells it: a declared slot under its mangled name written raw, its declared type, `readonly` and hooks left
    /// unconsulted, and any other key a dynamic property laid down with
    /// no `__set` served and no deprecation raised. `value` is the
    /// table's from here, and what the row held is released after the
    /// write.
    ///
    /// ```php
    /// class M { public $a = 1; public int $n = 0; public function __set($k, $v) { echo "__set $k\n"; } }
    /// $o = new M;
    /// $ao = @new ArrayObject($o);
    /// $ao['a'] = 'changed'; $ao['dynamic'] = 'new'; $ao['n'] = 'not an int';
    /// var_dump($o);
    /// ```
    /// ```expect
    /// object(M)#1 (3) {
    ///   ["a"]=>
    ///   string(7) "changed"
    ///   ["n"]=>
    ///   string(10) "not an int"
    ///   ["dynamic"]=>
    ///   string(3) "new"
    /// }
    /// ```
    pub fn set_row(&self, req: &mut Request, key: &Key, value: Value) -> Result<(), Thrown> {
        req.table_set_row(*self, key, value)
    }

    /// `zend_hash_update` over the property table, where
    /// [`PhpObject::set_row`] is `zend_hash_update_ind`: a declared row
    /// under its mangled name is written raw as there and left a plain
    /// value from then on ([`php_heap::PropBit::PlainRow`]), which
    /// `unserialize()` writes with no declared type to hold it to, while
    /// every other declared row stays the slot's; any other key is
    /// [`PhpObject::set_row`]'s. What `php_zip_get_properties` writes the
    /// six with, a subclass's own properties going on as slots.
    ///
    /// ```php
    /// class Z extends ZipArchive { public $x = 1; public int $n = 0; }
    /// var_dump(unserialize('O:1:"Z":2:{s:8:"filename";a:0:{}s:1:"x";a:1:{i:0;i:1;}}'));
    /// try { unserialize('O:1:"Z":1:{s:1:"n";a:0:{}}'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(Z)#1 (8) {
    ///   ["lastId"]=>
    ///   int(-1)
    ///   ["status"]=>
    ///   int(0)
    ///   ["statusSys"]=>
    ///   int(0)
    ///   ["numFiles"]=>
    ///   int(0)
    ///   ["filename"]=>
    ///   string(0) ""
    ///   ["comment"]=>
    ///   string(0) ""
    ///   ["x"]=>
    ///   array(1) {
    ///     [0]=>
    ///     int(1)
    ///   }
    ///   ["n"]=>
    ///   int(0)
    /// }
    /// Cannot assign array to property Z::$n of type int
    /// ```
    pub fn update_row(&self, req: &mut Request, key: &Key, value: Value) -> Result<(), Thrown> {
        req.table_update_row(*self, key, value)
    }

    /// One row of the property table removed by key,
    /// `zend_hash_del_ind` (`zend_hash_index_del` for an integer key): a
    /// declared slot left uninitialised with no `__unset` served, a
    /// dynamic property removed, what either held released; a key the
    /// table has not is nothing.
    ///
    /// ```php
    /// #[AllowDynamicProperties] class U { public $a = 1; public $b = 2; public function __unset($k) { echo "__unset $k\n"; } }
    /// $o = new U; $o->d = 3;
    /// $ao = @new ArrayObject($o);
    /// unset($ao['a'], $ao['d'], $ao['nope']);
    /// var_dump($o);
    /// ```
    /// ```expect
    /// object(U)#1 (1) {
    ///   ["b"]=>
    ///   int(2)
    /// }
    /// ```
    pub fn unset_row(&self, req: &mut Request, key: &Key) -> Result<(), Thrown> {
        req.table_unset_row(*self, key)
    }

    /// One row of the property table made a reference in place,
    /// `ZVAL_MAKE_REF` over what `zend_hash_find_ind` finds, `null`
    /// laid down where the table has none: the place a write through an
    /// `ArrayObject` over an object reaches. The cell is handed back
    /// with one more owner, the caller's.
    ///
    /// ```php
    /// $o = new stdClass; $o->list = [1];
    /// $ao = @new ArrayObject($o);
    /// $ao['list'][] = 2; $ao['fresh']['k'] = 'v';
    /// var_dump($o);
    /// ```
    /// ```expect
    /// object(stdClass)#1 (2) {
    ///   ["list"]=>
    ///   array(2) {
    ///     [0]=>
    ///     int(1)
    ///     [1]=>
    ///     int(2)
    ///   }
    ///   ["fresh"]=>
    ///   array(1) {
    ///     ["k"]=>
    ///     string(1) "v"
    ///   }
    /// }
    /// ```
    pub fn row_ref(&self, req: &mut Request, key: &Key) -> Result<PhpRef, Thrown> {
        req.table_row_ref(*self, key)
    }

    /// Where the row `key` stands in the object's property table, the
    /// declared slot or the dynamic entry, `zend_hash_find_ind`'s
    /// `zval *` behind the `IS_INDIRECT`; a row the table has none of,
    /// or a slot nothing wrote, laid down as `null` first, as
    /// `spl_array_get_dimension_ptr` does under a write fetch.
    pub fn row_place(&self, req: &mut Request, key: &Key) -> Result<Place, Thrown> {
        req.table_row_place(*self, key).map(Place)
    }

    /// [`PhpObject::row_ref`], with the typed property whose slot the row
    /// is added as a type source of the cell it makes
    /// (`ZEND_REF_ADD_TYPE_SOURCE`), so a write through any name bound to
    /// the cell is held to the property's type, as `php_array_walk` and
    /// `spl_array_it_get_current_data` make an object's row a reference.
    /// A slot holding a cell already keeps the sources it has; a dynamic
    /// row and an untyped slot are `row_ref`'s. Nothing is raised for a
    /// `readonly` property: a caller that refuses one asks first. A typed
    /// slot nothing has written is refused where the type takes no
    /// `null`, as `$r = &$o->p` refuses it.
    ///
    /// ```php
    /// class P { public int $i = 1; public $u = 2; public ?string $s = null; }
    /// $p = new P;
    /// array_walk($p, function (&$v, $k) {
    ///     try { $v = []; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// });
    /// $keep = &$p->s;
    /// array_walk($p, function (&$v) { $v = is_array($v) ? $v : "5"; });
    /// var_dump($p);
    /// ```
    /// ```expect
    /// Cannot assign array to reference held by property P::$i of type int
    /// Cannot assign array to reference held by property P::$s of type ?string
    /// object(P)#1 (3) {
    ///   ["i"]=>
    ///   int(5)
    ///   ["u"]=>
    ///   array(0) {
    ///   }
    ///   ["s"]=>
    ///   &string(1) "5"
    /// }
    /// ```
    pub fn typed_row_ref(&self, req: &mut Request, key: &Key) -> Result<PhpRef, Thrown> {
        req.table_typed_row_ref(*self, key)
    }

    /// One property by name, without a handler in the way: a view of
    /// it, and `None` for a property that is not there or a declared
    /// one nothing has written.
    #[must_use]
    pub fn prop(&self, req: &Request, name: &[u8]) -> Option<Value> {
        req.object_prop(self.0, name)
    }

    /// Write one property by name through the class's `write_property`
    /// (design §5.4), in the scope of the class that declares it, as
    /// `zend_update_property_ex(ce, ...)` writes one
    /// ([`Request::write_property_as_declared`]). The value is the
    /// object's from here, and so is the name, which is
    /// given up once the slot or the table has taken its own, as
    /// `zend_hash_update` takes a key's. What the property held is
    /// released last, after the write, which is where a destructor is
    /// served (`zend_assign_to_variable`). The `Err` is the memory
    /// limit refusing the dynamic table.
    pub fn set_prop(&self, req: &mut Request, name: PhpString, value: Value) -> Result<(), Thrown> {
        req.write_property_as_declared(*self, name, value)
    }

    /// Write one property by name through the class's `write_property`,
    /// as [`PhpObject::set_prop`] does, in the scope `scope` names, as
    /// `zend_update_property_ex(scope, ...)` sets `EG(fake_scope)` for
    /// the write and puts it back after: a private property the scope
    /// cannot see is a dynamic property, and one it may not write is
    /// php's `Error`, the `Err`.
    pub fn write_prop_in(
        &self,
        req: &mut Request,
        scope: PropertyScope,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        match scope {
            PropertyScope::Declaring => req.write_property_as_declared(*self, name, value),
            PropertyScope::Executing => req.write_property_in(*self, None, name, value),
            PropertyScope::Of(ClassRef(class)) => {
                req.write_property_in(*self, Some(class), name, value)
            }
        }
    }

    /// One property written as `unserialize()` writes it,
    /// [`Request::unserialize_property`]: the key as a serialized object
    /// carries it, mangled for a protected or a private property, placed
    /// in the declared slot it names without the class's handlers, or
    /// written into the dynamic table under the key; the value and the
    /// key are the object's from here. `false` is a key naming a virtual
    /// property, which php's `is_property_visibility_changed` refuses
    /// with its warning, and which fails the unserialization.
    pub fn unserialize_prop(
        &self,
        req: &mut Request,
        key: PhpString,
        value: Value,
    ) -> Result<bool, Thrown> {
        req.unserialize_property(*self, key, value)
    }

    /// What `unserialize()` holds a declared typed property to once the
    /// value under `key`, mangled as [`PhpObject::unserialize_prop`]
    /// takes it, is read into it: the type under strict rules, and a
    /// reference to every type its other properties hold it to, which
    /// from here include this one. The `Err` is php's `TypeError`, and
    /// the slot is left uninitialized; an untyped or undeclared key holds
    /// to nothing.
    ///
    /// ```php
    /// class C { public int $a; public string $b; }
    /// class D { public int $a; public float $b; }
    /// var_dump(unserialize('O:1:"C":2:{s:1:"a";i:1;s:1:"b";s:1:"x";}'));
    /// foreach (['O:1:"C":1:{s:1:"a";N;}', 'O:1:"C":2:{s:1:"a";i:1;s:1:"b";R:2;}', 'O:1:"D":2:{s:1:"a";i:1;s:1:"b";R:2;}'] as $s) {
    ///     try { unserialize($s); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// object(C)#1 (2) {
    ///   ["a"]=>
    ///   int(1)
    ///   ["b"]=>
    ///   string(1) "x"
    /// }
    /// Cannot assign null to property C::$a of type int
    /// Cannot assign int to property C::$b of type string
    /// Reference with value of type int held by property D::$a of type int is not compatible with property D::$b of type float
    /// ```
    ///
    /// A readonly property is held to its type and to nothing more: what
    /// the string writes into it, twice or not, stands.
    ///
    /// ```php
    /// class R { public function __construct(public readonly int $p = 1) {} }
    /// var_dump(unserialize('O:1:"R":2:{s:1:"p";i:2;s:1:"p";i:3;}'));
    /// try { unserialize('O:1:"R":1:{s:1:"p";s:1:"x";}'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(R)#1 (1) {
    ///   ["p"]=>
    ///   int(3)
    /// }
    /// Cannot assign string to property R::$p of type int
    /// ```
    pub fn unserialize_verify(&self, req: &mut Request, key: &[u8]) -> Result<(), Thrown> {
        req.unserialize_verify(*self, key)
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

    /// `ZEND_GUARD_OR_GC_IS_RECURSIVE(guard, walk, zobj)`: whether
    /// `walk` is already inside this object. An object whose class
    /// uses guards, one with `__get`, `__set`, `__isset`, `__unset` or
    /// `__debugInfo`, or a `JsonSerializable`, answers for `walk` alone,
    /// so `var_export()` inside a `var_dump()` of it is not a cycle;
    /// any other answers [`PhpObject::is_recursive`], the one
    /// `GC_PROTECTED` bit every walk shares.
    ///
    /// ```php
    /// class D { public $a = 1; public function __debugInfo(): array { return ['e' => var_export($this, true)]; } }
    /// var_dump(new D);
    /// class P { public $p; }
    /// $p = new P; $p->p = $p;
    /// print_r($p); echo "\n";
    /// ```
    /// ```expect
    /// object(D)#1 (1) {
    ///   ["e"]=>
    ///   string(38) "\D::__set_state(array(
    ///    'a' => 1,
    /// ))"
    /// }
    /// P Object
    /// (
    ///     [p] => P Object
    ///  *RECURSION*
    /// )
    /// ```
    #[must_use]
    pub fn is_recursive_in(&self, req: &Request, walk: RecursionGuard) -> bool {
        req.recursion_guarded(*self, walk)
    }

    /// `ZEND_GUARD_OR_GC_PROTECT_RECURSION`: `walk` is inside, on the
    /// guard [`PhpObject::is_recursive_in`] reads.
    pub fn protect_in(&self, req: &mut Request, walk: RecursionGuard) {
        req.recursion_protect(*self, walk);
    }

    /// `ZEND_GUARD_OR_GC_UNPROTECT_RECURSION`: `walk` is out.
    pub fn unprotect_in(&self, req: &mut Request, walk: RecursionGuard) {
        req.recursion_unprotect(*self, walk);
    }

    /// `GC_ADD_FLAGS(obj, IS_OBJ_DESTRUCTOR_CALLED)`: the object is
    /// taken as destructed, so its `__destruct()` never runs, which is
    /// what `unserialize()` does to an object whose `__wakeup()` or
    /// `__unserialize()` failed or was never made.
    pub fn mark_destructor_called(&self) {
        self.0.set_destructor_called(true);
    }

    /// The blocks `create_object` allocated beside the object given back
    /// now rather than with it: the `efree(dbh)` of a PDO switched over
    /// to a persistent `pdo_dbh_t`, its own no longer the request's.
    pub fn free_beside(&self, req: &mut Request) {
        req.free_beside(*self);
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

    /// The native payload, mutably, or the one `InternalError` a method
    /// served on an object without it raises (design §5.4): what
    /// `This::payload_mut` and `Instance::payload_mut` answer, so no class
    /// writes the sentence and no receiver reads the payload twice.
    pub fn native_mut_or_fatal<'a, T: Any + Send>(
        &self,
        req: &'a mut Request,
    ) -> Result<&'a mut T, Thrown> {
        req.native_mut_or_fatal(self.0)
    }

    /// Whether the object is a lazy ghost or proxy nothing has
    /// initialised, `zend_object_is_lazy(obj) &&
    /// !zend_lazy_object_initialized(obj)`.
    #[must_use]
    pub fn is_lazy(&self) -> bool {
        matches!(self.0.laziness(), Laziness::Ghost | Laziness::Proxy)
    }

    /// What the object's property table holds of its declared
    /// properties: rows that stand for the slots, php's `IS_INDIRECT`s,
    /// or copies of them, plain values under their mangled names, once a
    /// table shared while it stood for the slots was separated (an
    /// `ArrayObject` sort, a lazy ghost's initializer). A reader that
    /// tells php's `IS_INDIRECT` from a plain value, as
    /// `spl_array_object_count_elements_helper` does, reads it after
    /// [`PhpObject::entries`] has laid the table down.
    #[must_use]
    pub fn declared_rows(&self) -> DeclaredRows {
        self.0.declared_rows()
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

    /// Assign through the reference, `ZEND_TRY_ASSIGN_REF`
    /// (`Z/Zend/zend_API.h`): a cell a typed property's slot holds is held
    /// to every type its sources declare, under the calling code's
    /// `strict_types`, as `zend_try_assign_typed_ref` holds it, and
    /// coerced to what they agree on, `value` the cell's from here and
    /// what it held released last (`zend_safe_assign_to_variable_noref`);
    /// any other cell takes the value as it is, after `zval_ptr_safe_dtor`
    /// has left it `null` and released what it held, so a destructor that
    /// release serves reads `null` there and not the value. On the
    /// `TypeError` the value is given up and the cell keeps what it held.
    ///
    /// ```php
    /// class T { public int $i = 0; public ?string $s = null; }
    /// $t = new T; $r = &$t->i;
    /// try { settype($r, "array"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// $q = &$t->s;
    /// try { preg_match('/(a)/', 'a', $q); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// settype($r, "string"); extract(['r' => '7']);
    /// var_dump($t);
    /// ```
    /// ```expect
    /// Cannot assign array to reference held by property T::$i of type int
    /// Cannot assign array to reference held by property T::$s of type ?string
    /// object(T)#1 (2) {
    ///   ["i"]=>
    ///   &int(7)
    ///   ["s"]=>
    ///   &NULL
    /// }
    /// ```
    ///
    /// ```php
    /// class Test { function __destruct() { global $box; var_dump($box); } }
    /// $box = [new Test];
    /// getimagesize("dummy", $box);
    /// var_dump($box);
    /// ```
    /// ```expect
    /// NULL
    ///
    /// Warning: getimagesize(dummy): Failed to open stream: No such file or directory in %s on line 4
    /// array(0) {
    /// }
    /// ```
    pub fn set(&self, req: &mut Request, value: Value) -> Result<(), Thrown> {
        if self.0.sources() == 0 {
            req.reference_set(self.0, Value::null());
            req.reference_set(self.0, value);
            return Ok(());
        }
        let strict = req.strict_types();
        req.assign_to_reference(self.0, value, strict)
    }

    /// The value held replaced by what php's `convert_to_*` makes of it
    /// in place (`ZVAL_DEREF` and then the conversion,
    /// `Z/Zend/zend_operators.c`): no type source is consulted, since php's
    /// conversion writes the zval where it stands. What binding a PDO
    /// parameter by reference does; an assignment through the cell is
    /// [`PhpRef::set`]. What the cell held is released after the write.
    pub fn convert(&self, req: &mut Request, value: Value) {
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

/// What an object's property table holds of its declared properties
/// (design §5.4): the slots standing for them, or copies of them the
/// table holds as plain values ([`PhpObject::declared_rows`]).
pub use php_heap::DeclaredRows;

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

    /// [`Resource::new`] with the payload laid inside an allocation of
    /// `size` bytes, the struct php `emalloc`s and hands
    /// `zend_register_resource` (`sizeof(php_stream_context)` for a
    /// `context`), so the request is charged php's size whatever the
    /// payload's is. A payload that does not fit `size` is an internal
    /// error, raised as the fatal it is.
    pub fn sized<T: Any + Send>(
        req: &mut Request,
        kind: ResourceType,
        payload: T,
        size: usize,
    ) -> Result<Resource, Thrown> {
        let allocated = req.heap_mut().resource_sized(kind, payload, size);
        req.fatal_of(allocated).map(Resource::of)
    }

    /// `zend_register_resource(NULL, type)`: a resource of `kind` with no
    /// payload, one owner, under the next id; the `Err` is the memory
    /// limit's fatal.
    pub fn bare(req: &mut Request, kind: ResourceType) -> Result<Resource, Thrown> {
        let allocated = req.heap_mut().resource(kind);
        req.fatal_of(allocated).map(Resource::of)
    }

    /// [`Resource::sized`] for what php `pemalloc`s persistent, as
    /// `_php_stream_alloc` does a persistent stream: the resource on the
    /// request's heap, the `size` bytes off its count.
    pub fn persistent<T: Any + Send>(
        req: &mut Request,
        kind: ResourceType,
        payload: T,
        size: usize,
    ) -> Result<Resource, Thrown> {
        let allocated = req.heap_mut().resource_persistent(kind, payload, size);
        req.fatal_of(allocated).map(Resource::of)
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

/// Which walk an object's recursion guard is held for, php's
/// `ZEND_GUARD_RECURSION_*` (`Z/Zend/zend_types.h:659-661`): each keeps
/// its own mark on an object whose class uses guards
/// ([`PhpObject::is_recursive_in`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecursionGuard {
    /// `var_dump()`, `print_r()` and `debug_zval_dump()`.
    Debug,
    /// `var_export()`.
    Export,
    /// `json_encode()` around a `jsonSerialize()` call.
    Json,
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

    /// An argument left unpassed, the `IS_UNDEF` slot `zend_call_function`
    /// forwards (`ZEND_CALL_MAY_HAVE_UNDEF`): a call served with one fills
    /// it from the parameter's default or raises php's `ArgumentCountError`
    /// that it was not passed, so the callee never reads one. What
    /// `spl_array_read_dimension_ex` hands an overriding `offsetGet` for
    /// `$o[]`.
    #[must_use]
    pub const fn unpassed() -> Value {
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

    /// `Z_REFCOUNTED`: whether the value carries an owner count at all,
    /// which a scalar, an interned string and an immutable array do not.
    #[must_use]
    pub fn is_refcounted(&self) -> bool {
        self.to_zval().is_refcounted()
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
        let Ok(()) = r.set(&mut req, Value::string(text)) else {
            return;
        };
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
            req.define_constant(b"dtor", Value::int(i64::from(seen)))
                .ok();
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

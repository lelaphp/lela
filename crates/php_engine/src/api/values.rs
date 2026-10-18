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
//! Every constructor of the face returns the [`Owner`] a value is made
//! with, as does every call that hands its caller a value of its own (a
//! call's return, a conversion, a copy). A native's return takes either:
//! an owner is moved to the engine, and a view, an argument or an
//! element of one as the native reads it, is the engine's to own, which
//! takes its owner as php's `RETURN_COPY` does. What a native keeps past
//! a call, in its request state, an object's or a resource's payload or
//! a stream's kind, it keeps as an owner, given up on that state's
//! release path. A value's text read once is
//! [`with_string`](crate::api::coerce::with_string), which gives up the
//! string it converted.

use std::any::Any;
use std::cmp::Ordering;

use php_heap::{Arr, Obj, Ref, Res, Str, Zval, ZvalType};

use crate::api::abi::{ClassRef, PropertiesFor, PropertyPurpose};
use crate::api::call::Function;
use crate::api::raising::Thrown;
use crate::api::request::Request;

mod charset;
mod emalloc;
mod fresh;
mod identity;
mod owner;
mod smart_str;
pub mod walk;
pub use charset::Charset;
pub use emalloc::{Emalloc, Pemalloc};
pub use fresh::FreshString;
pub use identity::Identity;
pub use owner::{Owner, ResourceView, View};
pub(crate) use owner::{adopted, adopted_some};
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

/// The identity of a linked class, one word (design §5.14,
/// *Identities*): with its top bit clear, a class every request shares,
/// the engine's own numbered from zero and then the rows of the
/// engine's published arena past them, the preload's among them; with
/// it set, one of the request's own, numbered among its rows. Either
/// stays the class's for as long as the request runs, whatever another
/// request publishes or declares meanwhile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ClassId(pub(crate) u32);

impl ClassId {
    /// The bit that marks a class of the request's own.
    pub(crate) const LOCAL: u32 = 1 << 31;

    /// Whether every request shares the class: one of the engine's, or a
    /// row of its published arena; not one only this request declared.
    #[must_use]
    pub(crate) fn is_published(self) -> bool {
        self.0 & ClassId::LOCAL == 0
    }

    /// The request's own class at `at` among its rows.
    pub(crate) fn local(at: usize) -> Option<ClassId> {
        let at = u32::try_from(at).ok()?;
        (at & ClassId::LOCAL == 0).then_some(ClassId(at | ClassId::LOCAL))
    }

    /// Where the class stands among the request's own rows, if it is one.
    pub(crate) fn local_at(self) -> Option<usize> {
        if self.is_published() {
            return None;
        }
        usize::try_from(self.0 & !ClassId::LOCAL).ok()
    }

    /// The class's number among those every request shares, if it is one
    /// of them: the engine's own below the arena's.
    pub(crate) fn published_number(self) -> Option<u32> {
        self.is_published().then_some(self.0)
    }
}

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
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn new(req: &mut Request, bytes: &[u8]) -> Result<Owner<PhpString>, Thrown> {
        let made = req.string(bytes)?;
        Ok(Owner::adopt(made))
    }

    /// `zend_string_init_interned`: the one interned string for these
    /// bytes, never refcounted, what php hands out for a name a
    /// declaration made (`ce->name`, `fptr->common.function_name`,
    /// `prop_info->name`): the engine's string where its startup or a
    /// published unit interned these bytes, which costs the request
    /// nothing, else the request's own, allocated on its first ask and
    /// shared by every later one. The `Err` is the memory limit.
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
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn existing_interned(req: &mut Request, bytes: &[u8]) -> Result<Owner<PhpString>, Thrown> {
        let made = req.existing_interned(bytes)?;
        Ok(Owner::adopt(made))
    }

    /// A string of `len` bytes written in place by `fill`, php's
    /// `zend_string_alloc` and the `memcpy` a native does into
    /// `ZSTR_VAL` after it. The allocation is asked for, and the memory
    /// limit met, before a byte is written anywhere, which is how
    /// `str_repeat("x", 200000000)` is php's `Allowed memory size` fatal
    /// and not two hundred megabytes built and then refused. `fill` is
    /// given `len` zero bytes to write once.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn filled(
        req: &mut Request,
        len: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<Owner<PhpString>, Thrown> {
        let made = req.string_with(len, fill)?;
        Ok(Owner::adopt(made))
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
    ) -> Result<Owner<PhpString>, Thrown> {
        let made = req.string_product(n, m, l, fill)?;
        Ok(Owner::adopt(made))
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

    /// The key an owned string makes, as `zend_symtable_update` makes it
    /// of the `zend_string *` it is handed: `Int` when the bytes are a
    /// canonical decimal integer in range (`ZEND_HANDLE_NUMERIC_STR`), the
    /// string's owner given up, since no bucket keeps it, and else the
    /// string itself, its owner the key's. What a native builds a key
    /// with out of a string it made; a key read from a string it was
    /// handed is [`Key::lookup`]'s, with [`Owner::take`] where a sink
    /// keeps it.
    ///
    /// The four natives that key an array by its values stay flat over
    /// numeric strings, as php's do:
    ///
    /// ```php
    /// function run($i) {
    ///     $n = [(string) (100 + $i), (string) (200 + $i), "k$i"];
    ///     array_flip($n); array_count_values($n); array_fill_keys($n, 0); array_combine($n, $n);
    /// }
    /// run(0);
    /// $m = memory_get_usage();
    /// for ($i = 1; $i <= 1000; $i++) { run($i); }
    /// var_dump(memory_get_usage() - $m);
    /// ```
    /// ```expect
    /// int(0)
    /// ```
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn from_string(req: &mut Request, s: Owner<PhpString>) -> Owner<Key> {
        match php_heap::canonical_int(s.as_bytes()) {
            Some(n) => {
                s.give_up(req);
                Owner::index(n)
            }
            None => s.into_key(),
        }
    }

    /// The key a string names, a view and no owner taken: `Int` when its
    /// bytes are a canonical decimal integer in range
    /// (`ZEND_HANDLE_NUMERIC_STR`), else the string itself. What
    /// `zend_symtable_find` looks a string up as; a sink that keeps the
    /// key is handed an owner of it, [`Owner::take`], which takes one of
    /// the string on its string arm alone.
    #[must_use]
    pub fn lookup(s: PhpString) -> Key {
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
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn to_prop_name(&self, req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
        match self {
            Key::Int(n) => PhpString::new(req, crate::c_number::Long::of(*n).as_bytes()),
            Key::Str(s) => Ok(Owner::take(req, *s)),
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
/// **The writes are sinks.** `set`, `replace`, `append` and
/// `try_append` take an [`Owner`] of the value they write, and `set` one
/// of the key: the value is the array's from that moment, as
/// `ZVAL_COPY_VALUE` into the bucket makes it php's, and so is a string
/// key. A native adds a row under a name it has as bytes with
/// [`PhpArray::set_str`], which is `add_assoc_*`'s shape and makes the
/// key where php's does; a key that is a string already it hands to
/// `set` and never names again. A value it was handed, a view, it hands
/// on with [`Owner::take`], `Z_TRY_ADDREF` first as php's natives do. A view
/// handed as it stands does not compile:
///
/// ```compile_fail,E0308
/// # use php_engine::api::raising::Thrown;
/// # use php_engine::api::request::Request;
/// # use php_engine::api::values::{Key, Owner, PhpArray, Value};
/// fn kept(req: &mut Request, into: &mut PhpArray, value: Value) -> Result<(), Thrown> {
///     let key = Owner::index(0);
///     into.set(req, key, value)
/// }
/// # fn main() {}
/// ```
///
/// What a read hands back is a view, no owner taken.
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
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn new(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
        let made = req.array()?;
        Ok(Owner::adopt(made))
    }

    /// A new array [`PhpArray::new`] makes and `fill` writes, given up
    /// where a step of `fill` throws, so the throwable is handed on and
    /// nothing is held: php's `array_init` into a return value or a
    /// debug table, which `RETURN_THROWS` leaves to the engine's free.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn filled(
        req: &mut Request,
        fill: impl FnOnce(&mut Request, &mut Owner<PhpArray>) -> Result<(), Thrown>,
    ) -> Result<Owner<PhpArray>, Thrown> {
        let mut table = PhpArray::new(req)?;
        match fill(req, &mut table) {
            Ok(()) => Ok(table),
            Err(thrown) => {
                table.give_up(req);
                Err(thrown)
            }
        }
    }

    /// `zend_new_array(size)` and `zend_hash_init(ht, size, ...)`, an
    /// empty array whose first write lays its table down for `size`
    /// elements, the size held to `zend_hash_check_size`: the header
    /// alone until then, as [`PhpArray::new`] is, where
    /// [`PhpArray::sized`] lays the table down at once. The `Err` is the
    /// memory limit.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn init_size(req: &mut Request, size: u32) -> Result<Owner<PhpArray>, Thrown> {
        let made = req.array_sized(size, true)?;
        Ok(Owner::adopt(made))
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
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn sized(req: &mut Request, size: u32, layout: Layout) -> Result<Owner<PhpArray>, Thrown> {
        let laid = match layout {
            Layout::Packed => req.heap_mut().array_packed(size),
            Layout::Hash => req.heap_mut().array_mixed(size),
        };
        let made = req.fatal_of(laid).map(PhpArray::of)?;
        Ok(Owner::adopt(made))
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

    /// The value under the key these bytes name, a view of it: php's
    /// `zend_symtable_str_find` (`Z/Zend/zend_hash.h:631`), the integer key where the bytes are a
    /// canonical decimal integer in range (`ZEND_HANDLE_NUMERIC_STR`) and
    /// the string key otherwise, allocating nothing. Where [`find`] reads
    /// a name as written, this reads one as a program's `$a["1"]` does:
    /// `"1"` finds the integer key `1`, and `"01"` the string key `"01"`.
    /// `strtr` over pairs with integer keys looks its candidates up so:
    ///
    /// ```php
    /// var_dump(strtr("1 01 -2 -0", [1 => "one", "01" => "zero-one", -2 => "minus-two", 0 => "zero"]));
    /// ```
    /// ```expect
    /// string(28) "one zero-one minus-two -zero"
    /// ```
    ///
    /// [`find`]: PhpArray::find
    #[must_use]
    pub fn find_symtable(&self, name: &[u8]) -> Option<Value> {
        match php_heap::canonical_int(name) {
            Some(n) => self.0.get(php_heap::Key::Long(n)),
            None => self.0.str_find(name),
        }
        .map(Value::of_zval)
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
    /// the key are the array's from here: the bucket takes its own owner
    /// of a string key, and the one handed in is given up.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn set(
        &mut self,
        req: &mut Request,
        key: Owner<Key>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        if let Err(thrown) = self.separate(req) {
            key.give_up(req);
            value.give_up(req);
            return Err(thrown);
        }
        let key = key.hand_on();
        let written = req.array_update(self.0, key.heap(), value.hand_on());
        if let Key::Str(s) = key {
            req.release(Value::string(s));
        }
        written
    }

    /// Write `value` under the string key of these bytes,
    /// `zend_hash_str_update` after `SEPARATE_ARRAY`: the bytes are the
    /// key as written, an option's or a field's name, no integer read out
    /// of them, as [`PhpArray::find`] reads them. A present key keeps its
    /// string and its place, its old value released; an absent one is
    /// made only after the table has room for it, so a write that
    /// doubles the table never holds the new key beside the old data and
    /// the new, as php's never does. `memory_get_peak_usage()` reads
    /// that order, which is why a native adds a row under a name it has
    /// as bytes here, or through [`PhpArray::set_symtable_str`] where
    /// php's is `add_assoc_*`, never through a [`PhpString`] key made
    /// first and handed to [`PhpArray::set`]. The value is the array's
    /// from here.
    ///
    /// ```php
    /// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, "\n"; }
    /// pk(fn () => stat(__FILE__));
    /// pk(fn () => getrusage());
    /// ```
    /// ```expect
    /// 2072
    /// 2632
    /// ```
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn set_str(
        &mut self,
        req: &mut Request,
        name: &[u8],
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        if let Err(thrown) = self.separate(req) {
            value.give_up(req);
            return Err(thrown);
        }
        req.array_str_update(self.0, name, value.hand_on())
    }

    /// Write `value` under the key these bytes name as a symbol table
    /// reads them, `zend_symtable_str_update`, which is what every
    /// `add_assoc_*` writes: an integer key where the bytes are a
    /// canonical decimal integer in range (`ZEND_HANDLE_NUMERIC_STR`),
    /// as [`Key::from_string`] reads a string, and else
    /// [`PhpArray::set_str`]'s write, the key made only after the table
    /// has room. A native adds a row under a name it has as bytes, and
    /// that may be numeric, here; one whose name cannot be numeric, or
    /// whose php source is `zend_hash_str_update`, uses `set_str`.
    ///
    /// `get_html_translation_table` writes each row so, its key a
    /// character or two and its table doubling as it fills:
    ///
    /// ```php
    /// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, "\n"; }
    /// pk(fn () => get_html_translation_table());
    /// pk(fn () => get_html_translation_table(HTML_ENTITIES));
    /// pk(fn () => get_html_translation_table(HTML_ENTITIES, ENT_QUOTES | ENT_HTML5));
    /// var_dump(iconv_mime_decode_headers("7: a\r\n07: b\r\n7: c\r\n"));
    /// ```
    /// ```expect
    /// 696
    /// 29136
    /// 192752
    /// array(2) {
    ///   [7]=>
    ///   string(1) "c"
    ///   ["07"]=>
    ///   string(1) "b"
    /// }
    /// ```
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn set_symtable_str(
        &mut self,
        req: &mut Request,
        name: &[u8],
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        match php_heap::canonical_int(name) {
            Some(n) => self.set(req, Owner::index(n), value),
            None => self.set_str(req, name, value),
        }
    }

    /// Lay `value` down at `key` as `zend_hash_add_new` and
    /// `zend_symtable_add_new` do after `SEPARATE_ARRAY`: a string key
    /// appended without the lookup, so a key the array holds already is
    /// held twice and a lookup finds this row. The value and the key are
    /// the array's from here, as [`PhpArray::set`] takes them.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn add_new(
        &mut self,
        req: &mut Request,
        key: Owner<Key>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        if let Err(thrown) = self.separate(req) {
            key.give_up(req);
            value.give_up(req);
            return Err(thrown);
        }
        let key = key.hand_on();
        let written = req.array_add_new(self.0, key.heap(), value.hand_on());
        if let Key::Str(s) = key {
            req.release(Value::string(s));
        }
        written
    }

    /// Replace the value at `key` in place; `false`, and `value` given
    /// up, when it is absent. The key is the array's already, so only
    /// the value is taken.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn replace(
        &mut self,
        req: &mut Request,
        key: &Key,
        value: Owner<Value>,
    ) -> Result<bool, Thrown> {
        if !self.contains(key) {
            value.give_up(req);
            return Ok(false);
        }
        if let Err(thrown) = self.separate(req) {
            value.give_up(req);
            return Err(thrown);
        }
        req.array_update(self.0, key.heap(), value.hand_on())?;
        Ok(true)
    }

    /// `$a[] = value`, at `nNextFreeElement`, or php's `Error` "Cannot
    /// add element to the array as the next element is already occupied"
    /// when that is `ZEND_LONG_MAX` and taken (`zend_cannot_add_element`),
    /// `value` given up.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn append(&mut self, req: &mut Request, value: Owner<Value>) -> Result<(), Thrown> {
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
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn try_append(&mut self, req: &mut Request, value: Owner<Value>) -> Result<bool, Thrown> {
        if let Err(thrown) = self.separate(req) {
            value.give_up(req);
            return Err(thrown);
        }
        req.array_append(self.0, value.hand_on())
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

    /// `SEPARATE_ARRAY` and `zend_hash_clean` (`Z/Zend/zend_hash.c:1875`),
    /// [`php_heap::Heap::array_clean`]: every row's value destroyed in
    /// place, in order, its destructor served as it goes, then the table
    /// emptied in one pass, its keys given up and `nNextFreeElement` back
    /// to `ZEND_LONG_MIN`. `session_unset` is php's caller.
    pub fn clean(&mut self, req: &mut Request) -> Result<(), Thrown> {
        self.separate(req)?;
        req.array_clean(self.0);
        Ok(())
    }

    /// [`Owner::unset_in_place`]'s delete, `zend_hash_del` without
    /// `SEPARATE_ARRAY`; an immutable table is left as it is.
    pub(super) fn del_in_place(self, req: &mut Request, key: &Key) -> bool {
        !self.0.is_immutable() && self.contains(key) && req.array_del(self.0, key.heap())
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
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn copy(&self, req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
        let made = req.array_dup(self.0).map(PhpArray::of)?;
        Ok(Owner::adopt(made))
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
        self.len() == other.len() && self.0.iter().map(view).eq(other.0.iter().map(view))
    }
}

/// One of a table's rows as `php_heap` reads it, as the views a reader
/// is lent: what the rows walk ([`walk::rows`]), `==` between two
/// arrays and the debug rendering read, none of which runs code of the
/// program.
fn view((key, value): (php_heap::Key, php_heap::Zval)) -> (Key, Value) {
    (Key::of(key), Value::of_zval(value))
}

impl std::fmt::Debug for PhpArray {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.0.iter().map(view)).finish()
    }
}

/// The table `zend_get_properties_for` hands a reader, held until
/// `zend_release_properties`: [`PhpObject::properties_for`]'s answer,
/// the table and the owner php's reader holds of it, the object's own
/// table or one the class keeps by the owner `GC_TRY_ADDREF` takes, or
/// a table built for the reader by its one owner, php's `*is_temp`.
/// [`walk::properties`] walks it; [`HeldProperties::release`] gives it
/// back where php's reader calls `zend_release_properties`, before or
/// after it clears its recursion guard as that reader does.
#[derive(Debug)]
#[must_use = "the table is held until it is released"]
pub struct HeldProperties {
    /// What the walk reads.
    table: walk::Table,
    /// The reader's owner of the table it stands on
    /// ([`walk::Table::array`]), or of the table built for it.
    owner: Owner<PhpArray>,
    /// What the class's `get_debug_info` left pending while it built
    /// the table.
    thrown: Option<Thrown>,
    count: usize,
}

impl HeldProperties {
    /// `zend_array_count` over the table (`Z/Zend/zend_hash.c:519`), the
    /// number a dumper prints before the rows: an uninitialised row not
    /// counted, and under [`PropertyPurpose::Debug`] a slot `unset`
    /// empties counted still inside the release that empties it.
    #[must_use]
    pub fn count(&self) -> usize {
        self.count
    }

    /// The table held, a view, which the hold keeps until its release:
    /// read under the borrowed request ([`walk::rows`]), or guarded as
    /// php guards `prop_ht` ([`walk::Table::array`]).
    #[must_use]
    pub fn table(&self) -> walk::Table {
        self.table.clone()
    }

    /// `zend_release_properties`: the reader's owner given back,
    /// `zend_array_release`, one owner fewer, the table destroyed at the
    /// last, and no possible root of the cycle collector. The `Err` is
    /// what the class's `get_debug_info` left pending while it built
    /// the table, handed on once the rows have been read, as
    /// `php_var_dump` and `php_debug_zval_dump` leave `EG(exception)`
    /// standing after the dump (`Z/ext/standard/var.c:184`).
    pub fn release(self, req: &mut Request) -> Result<(), Thrown> {
        self.owner.give_up_nogc(req);
        self.thrown.map_or(Ok(()), Err)
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

/// The scope a property is written or read in ([`PhpObject::write_prop_in`],
/// [`PhpObject::read_prop_in`]), what `zend_update_property_ex` and
/// `zend_read_property_ex` set `EG(fake_scope)` to for the access, and
/// so which private or protected property the name reaches.
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

    /// php's `zobj->ce = ce`, which only `to_zval_object_ex` makes, on an
    /// object that keeps its layout: whether the object took `class`.
    /// Both classes lay down no property slot and no native payload and
    /// are served by the standard handlers, the object is not lazy and
    /// `class` is one `new` could make; otherwise nothing changes.
    pub fn set_class(&self, req: &Request, class: ClassId) -> bool {
        req.set_object_class(*self, class)
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
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn clone_with<T: Any + Send>(
        &self,
        req: &mut Request,
        payload: T,
    ) -> Result<Owner<PhpObject>, Thrown> {
        let made = req.clone_members_payload(*self, payload);
        adopted(made)
    }

    /// What a `Closure` carries, for a closure object: its native
    /// payload, as php's `zend_closure` is the object's own struct.
    #[must_use]
    pub fn closure<'a>(&self, req: &'a Request) -> Option<&'a ClosureInfo> {
        self.native::<crate::classes::ClosureObject>(req)
            .map(|closure| &closure.info)
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
    pub fn closure_bound(
        &self,
        req: &mut Request,
    ) -> Result<Vec<(Owner<PhpString>, Value)>, Thrown> {
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

    /// `zend_get_properties_for(obj, purpose)`, php's second entry point
    /// to an object's properties, held until
    /// [`HeldProperties::release`], `zend_release_properties`: the table
    /// the class's `get_properties_for` answers the purpose with, and at
    /// `Std` what `zend_std_get_properties_for` answers, the object's
    /// table as `get_properties` lays it down ([`PhpObject::property_table`]),
    /// a lazy one read as it stands where the purpose initialises
    /// nothing, the hooked properties' table for the readers that call
    /// hooks, and under [`PropertyPurpose::Debug`] what the class's
    /// `get_debug_info` answers first (`Z/Zend/zend_object_handlers.c:2611`).
    /// A table the object or the class keeps is held by one more owner,
    /// `GC_TRY_ADDREF`, so a write that code of the program makes while
    /// the reader walks it separates it; one built for the reader,
    /// `*is_temp`, by its one owner, so an element that is the object
    /// itself counts while the reader prints its refcount; php's `NULL`
    /// table is held as the empty one. Each declared row is keyed by the
    /// name `zend_mangle_property_name` makes, `\0*\0p` for a protected
    /// property and `\0C\0p` for a private one. The `Err` is what the
    /// lazy initializer or the class threw, or the memory limit.
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
    pub fn properties_for(
        &self,
        req: &mut Request,
        purpose: PropertyPurpose,
    ) -> Result<HeldProperties, Thrown> {
        let (table, owner, thrown) = match req.properties_answered(*self, purpose)? {
            PropertiesFor::Table(table) => {
                let owner = Owner::take(req, table.array());
                (table, owner, None)
            }
            PropertiesFor::Temporary(owner) => (walk::Table::Array(*owner), owner, None),
            PropertiesFor::Debug(info) => {
                (walk::Table::Array(*info.table), info.table, info.thrown)
            }
            PropertiesFor::Null => {
                let empty = Owner::take(req, PhpArray::empty());
                (walk::Table::Array(*empty), empty, None)
            }
        };
        let count = match &table {
            walk::Table::Object(object) if self.0.laziness() == php_heap::Laziness::Eager => {
                req.property_count(object.owner())
            }
            walk::Table::Object(object) => Ok(object.held()),
            walk::Table::Array(array) => Ok(array.len()),
        };
        let held = HeldProperties {
            table,
            owner,
            thrown,
            count: 0,
        };
        match count {
            Ok(count) => Ok(HeldProperties { count, ..held }),
            Err(thrown) => match held.release(req) {
                Ok(()) => Err(thrown),
                Err(earlier) => Err(earlier.chain(req, thrown)),
            },
        }
    }

    /// php's `Z_OBJPROP_P`, the first of php's two entry points to an
    /// object's properties: the table the class's `get_properties`
    /// answers, a view ([`walk::foreach`], [`walk::rows`]). At `Std`,
    /// `zend_std_get_properties`, it is the object's own property table
    /// laid down, [`walk::Table::Object`], a lazy object initialised
    /// first, so the table is the instance's; a class that writes its
    /// fields into the table at that call (`DateInterval`) has written
    /// them, and a class that keeps a table in its place
    /// (`sxe_get_properties`) answers that one, [`walk::Table::Array`].
    /// The table stays laid down, as php's object keeps the table
    /// `rebuild_object_properties` built, and `memory_get_usage()`
    /// counts it as php's does. The `Err` is what the lazy initializer
    /// or the class threw, or the memory limit refusing a name.
    ///
    /// ```php
    /// class K { public $d = 1; }
    /// $k = new K; $m = memory_get_usage(); $q = http_build_query($k); echo memory_get_usage() - $m, "\n";
    /// echo http_build_query(simplexml_load_string('<a><b>1</b><c>2</c></a>')), "\n";
    /// echo '[', http_build_query(new DateTime('2020-01-01 UTC')), "]\n";
    /// ```
    /// ```expect
    /// 408
    /// b=1&c=2
    /// []
    /// ```
    pub fn property_table(&self, req: &mut Request) -> Result<walk::Table, Thrown> {
        req.object_handlers(self.0).get_properties(req, *self)
    }

    /// php's `obj->ce->num_hooked_props != 0`: whether a class of the
    /// object's chain declares a property with hooks, which
    /// `zend_std_get_properties_for` reads the hooks' table for, and
    /// whose readers guard the object rather than that table.
    #[must_use]
    pub fn has_hooked_properties(&self, req: &Request) -> bool {
        req.class_has_hooks(self.class())
    }

    /// The walk php's encoders take where they do not rebuild the
    /// property table ("Optimized version without rebuilding properties
    /// `HashTable`", `Z/ext/json/json_encoder.c:129`,
    /// `Z/ext/standard/var.c:1155`): over an object that has no table
    /// laid down, is not lazy, and whose class answers neither
    /// `get_properties` nor `get_properties_for` itself, its declared
    /// slots alone, each read in its slot as it stands
    /// (`OBJ_PROP(obj, prop_info->offset)`) under the name
    /// `zend_mangle_property_name` makes, nothing laid down; `None` for
    /// any other object, which the encoder reads through
    /// [`PhpObject::properties_for`]. The `Err` is the memory limit
    /// refusing a name.
    ///
    /// ```php
    /// class K { public $d = 1; }
    /// $k = new K; $m = memory_get_usage(); $j = json_encode($k); echo memory_get_usage() - $m, "\n";
    /// $l = new K; $m = memory_get_usage(); $s = serialize($l); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 32
    /// 56
    /// ```
    pub fn slots_without_table(&self, req: &mut Request) -> Result<Option<walk::Table>, Thrown> {
        let o = self.heap();
        if o.properties().is_some()
            || o.laziness() != php_heap::Laziness::Eager
            || req.object_handlers(o).properties_overloaded()
        {
            return Ok(None);
        }
        let table = o.slot_table();
        req.object_table(*self, table)
            .map(|table| Some(walk::Table::Object(table)))
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
    /// php's `IS_INDIRECT` is. A class whose `get_properties` keeps a
    /// table in place of the object's ([`PhpObject::property_table`]'s
    /// [`walk::Table::Array`]) holds its pointer there, not on this one.
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

    /// `zobj->properties = zend_symtable_to_proptable(table)`, how
    /// `mysqli_fetch_object` lays a row down as the object of a class
    /// that declares no property and no `__set`
    /// (`Z/ext/mysqli/mysqli.c:793-796`): the array itself, one more
    /// owner of it, where its keys are all strings, and otherwise a copy
    /// with each integer key spelt in decimal
    /// ([`Request::symtable_to_proptable`], the rule the `(object)` cast
    /// lays its table down by); one owner of `table` passed and given up,
    /// as php's `zval_ptr_dtor(&dataset)` gives it up after, so a row
    /// nothing else holds is the object's table alone. The table the
    /// object held is released. An object whose class places a declared
    /// property is php's other arm, `zend_merge_properties`, and refused
    /// here as the `InternalError` it would be.
    pub fn table_from_symtable(
        &self,
        req: &mut Request,
        table: Owner<PhpArray>,
    ) -> Result<(), Thrown> {
        if self.heap().slots() > 0 {
            table.give_up(req);
            return Err(crate::api::raising::fatal(
                req,
                "InternalError: an object whose class places a declared property takes no symbol table as its own",
            ));
        }
        let laid = req.properties_from_array(*self, *table);
        table.give_up(req);
        laid
    }

    /// `zobj->properties = table`, how `spl_array_method` puts back the
    /// table it handed a sort: `table` is the object's property table
    /// from here, one owner passed to it, and holds every row itself,
    /// the declared ones among them as copies the slots go on apart from
    /// ([`php_heap::DeclaredRows::Copied`]); the table the object held is
    /// released. A lazy object is initialised first and its instance
    /// takes the table.
    pub fn replace_table(&self, req: &mut Request, table: Owner<PhpArray>) -> Result<(), Thrown> {
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
    /// A sink: the value is an owner, and a view is refused, so a value
    /// the caller holds no owner of never reaches the table.
    ///
    /// ```compile_fail,E0308
    /// # use php_engine::api::raising::Thrown;
    /// # use php_engine::api::request::Request;
    /// # use php_engine::api::values::{Key, PhpObject, Value};
    /// fn viewed(req: &mut Request, object: PhpObject, key: &Key, value: Value) -> Result<(), Thrown> {
    ///     object.set_row(req, key, value)
    /// }
    /// # fn main() {}
    /// ```
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
    pub fn set_row(&self, req: &mut Request, key: &Key, value: Owner<Value>) -> Result<(), Thrown> {
        req.table_set_row(*self, key, value.hand_on())
    }

    /// One row of the property table written under the string key these
    /// bytes spell, `zend_hash_str_update` over what
    /// `zend_std_get_properties` answers, as a `get_properties` handler
    /// writes its fields (`date_interval_object_to_hash`): a declared
    /// slot under its mangled name written raw, as [`PhpObject::set_row`]
    /// writes it, and any other name a dynamic row, a key it holds kept
    /// with its string and one it does not made only after the table has
    /// room, by `zend_string_init`, so a key of one byte is allocated as
    /// php's is and not the interned character. `value` is the table's
    /// from here, and what the row held is released after the write.
    ///
    /// ```php
    /// $m = memory_get_usage(); $o = new stdClass; foreach (['y','m','d','h','i','s','f','invert','days','from_string'] as $k) $o->$k = 1; echo memory_get_usage() - $m, "\n";
    /// $i = new DateInterval('P1D');
    /// $m = memory_get_usage(); $a = get_object_vars($i); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $b = get_object_vars($i); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 736
    /// 1720
    /// 696
    /// ```
    pub fn set_str(
        &self,
        req: &mut Request,
        name: &[u8],
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        req.table_set_str(*self, name, value.hand_on())
    }

    /// One row appended to the property table under its next integer
    /// key, `zend_hash_next_index_insert`: what `$ao[] = $v` writes
    /// through an `ArrayObject` over the object, with no `__set` served
    /// and no deprecation raised. The key is the table's
    /// `nNextFreeElement`, one past the greatest integer key it has
    /// held, which a declared property's or a dynamic one's name never
    /// moves. A sink: `value`'s owner is the table's from here; `false`,
    /// and the owner given up, where the next element is taken, as
    /// [`PhpArray::try_append`] answers. A lazy object is initialised
    /// first, and a table a cast shares is separated before the write.
    ///
    /// ```php
    /// class G { public $a = 1; }
    /// $g = (new ReflectionClass(G::class))->newLazyGhost(function ($g) { echo "init\n"; });
    /// $ag = @new ArrayObject($g); $ag[] = 'x';
    /// $o = new stdClass; $o->k = 'v'; $cast = (array) $o;
    /// $ao = @new ArrayObject($o); $ao[] = 'y';
    /// var_dump($g, $cast, $o);
    /// ```
    /// ```expect
    /// init
    /// object(G)#3 (2) {
    ///   ["a"]=>
    ///   int(1)
    ///   [0]=>
    ///   string(1) "x"
    /// }
    /// array(1) {
    ///   ["k"]=>
    ///   string(1) "v"
    /// }
    /// object(stdClass)#2 (2) {
    ///   ["k"]=>
    ///   string(1) "v"
    ///   [0]=>
    ///   string(1) "y"
    /// }
    /// ```
    pub fn append_row(&self, req: &mut Request, value: Owner<Value>) -> Result<bool, Thrown> {
        req.table_append_row(*self, value.hand_on())
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
    pub fn update_row(
        &self,
        req: &mut Request,
        key: &Key,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        req.table_update_row(*self, key, value.hand_on())
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
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn row_ref(&self, req: &mut Request, key: &Key) -> Result<Owner<PhpRef>, Thrown> {
        let made = req.table_row_ref(*self, key);
        adopted(made)
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
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn typed_row_ref(&self, req: &mut Request, key: &Key) -> Result<Owner<PhpRef>, Thrown> {
        let made = req.table_typed_row_ref(*self, key);
        adopted(made)
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
    pub fn set_prop(
        &self,
        req: &mut Request,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        req.write_property_as_declared(*self, name, value)
    }

    /// Read one property by name through the class's `read_property`
    /// with `BP_VAR_R`, as `zend_read_property_ex(scope, obj, name,
    /// /* silent */ false, &rv)` reads one, `EG(fake_scope)` set to
    /// `scope` for the read: an unset typed property is php's `Error`,
    /// the `Err`, and a name nothing holds php's `Undefined property`
    /// warning and `null`. The answer is the caller's owner.
    ///
    /// ```php
    /// $f = new CURLFile('/etc/hosts'); unset($f->name);
    /// try { curl_setopt(curl_init(), CURLOPT_POSTFIELDS, ['f' => $f]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    ///
    /// Warning: curl_setopt(): Invalid filename for key f in %s on line 3
    /// Typed property CURLFile::$name must not be accessed before initialization
    /// ```
    pub fn read_prop_in(
        &self,
        req: &mut Request,
        scope: PropertyScope,
        name: &[u8],
    ) -> Result<Owner<Value>, Thrown> {
        let table = req.object_handlers(self.heap());
        let object = *self;
        let read = |req: &mut Request| {
            table.read_property(req, object, name, crate::api::abi::PropertyRead::Warning)
        };
        match scope {
            PropertyScope::Declaring => req.in_declared_scope(object.class(), name, read),
            PropertyScope::Executing => req.in_fake_scope(None, read),
            PropertyScope::Of(ClassRef(class)) => req.in_fake_scope(Some(class), read),
        }
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
        name: Owner<PhpString>,
        value: Owner<Value>,
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
    /// with its warning, and which fails the unserialization. A sink of
    /// both owners.
    pub fn unserialize_prop(
        &self,
        req: &mut Request,
        key: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<bool, Thrown> {
        req.unserialize_property(*self, key.hand_on(), value.hand_on())
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
    pub fn set_slot(
        &self,
        req: &mut Request,
        index: u32,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        req.write_declared_slot(self.0, index, value.hand_on())
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
    /// [`PhpObject::property_table`] has laid the table down.
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
    /// the cell, and a sink of `value`, whose owner the cell takes as
    /// `ZVAL_NEW_REF` copies the zval in without a count of its own. The
    /// `Err` is the memory limit as the fatal that ends the request.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn new(req: &mut Request, value: Owner<Value>) -> Result<Owner<PhpRef>, Thrown> {
        let made = req.reference(value.hand_on());
        adopted(made)
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
    /// `TypeError` the value is given up and the cell keeps what it held,
    /// as `zend_try_assign_typed_ref_ex` gives it up.
    ///
    /// A sink: the value is an owner, the cell's from here, and a view is
    /// refused, so a value the caller holds no owner of never reaches the
    /// cell. The pinned code is documentation on the pinned stable rustdoc,
    /// which checks only that the block fails (#2049); it fails with
    /// `E0308`, an `Owner<Value>` expected and a `Value` found.
    ///
    /// ```compile_fail,E0308
    /// # use php_engine::api::raising::Thrown;
    /// # use php_engine::api::request::Request;
    /// # use php_engine::api::values::{PhpRef, Value};
    /// fn viewed(req: &mut Request, cell: PhpRef, value: Value) -> Result<(), Thrown> {
    ///     cell.set(req, value)
    /// }
    /// # fn main() {}
    /// ```
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
    pub fn set(&self, req: &mut Request, value: Owner<Value>) -> Result<(), Thrown> {
        let value = value.hand_on();
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
    /// [`PhpRef::set`]. A sink, as [`PhpRef::set`] is: the cell takes the
    /// value's owner, and what it held is released after the write.
    pub fn convert(&self, req: &mut Request, value: Owner<Value>) {
        req.reference_set(self.0, value.hand_on());
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

/// The engine's one hash of a name as a `BuildHasher`, for a native's
/// own `HashMap` keyed by a name compared byte for byte, as ext/date's
/// `DATEG(tzcache)` is: every table by name hashes a name this way
/// (#2157), and the face hands it on so an extension needs no hash crate
/// of its own, which would be a package for a person to admit.
pub use php_link::NameHasher;

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
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn new<T: Any + Send>(
        req: &mut Request,
        kind: ResourceType,
        payload: T,
    ) -> Result<Owner<Resource>, Thrown> {
        let made = req.new_resource(kind, payload);
        adopted(made)
    }

    /// [`Resource::new`] with the payload laid inside an allocation of
    /// `size` bytes, the struct php `emalloc`s and hands
    /// `zend_register_resource` (`sizeof(php_stream_context)` for a
    /// `context`), so the request is charged php's size whatever the
    /// payload's is. A payload that does not fit `size` is an internal
    /// error, raised as the fatal it is.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn sized<T: Any + Send>(
        req: &mut Request,
        kind: ResourceType,
        payload: T,
        size: usize,
    ) -> Result<Owner<Resource>, Thrown> {
        let allocated = req.heap_mut().resource_sized(kind, payload, size);
        let made = req.fatal_of(allocated).map(Resource::of);
        adopted(made)
    }

    /// `zend_register_resource(NULL, type)`: a resource of `kind` with no
    /// payload, one owner, under the next id; the `Err` is the memory
    /// limit's fatal.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn bare(req: &mut Request, kind: ResourceType) -> Result<Owner<Resource>, Thrown> {
        let made = Self::bare_made(req, kind);
        adopted(made)
    }

    /// What [`bare`] makes, its one owner not yet named.
    pub(crate) fn bare_made(req: &mut Request, kind: ResourceType) -> Result<Resource, Thrown> {
        let allocated = req.heap_mut().resource(kind);
        req.fatal_of(allocated).map(Resource::of)
    }

    /// [`Resource::sized`] for what php `pemalloc`s persistent, as
    /// `_php_stream_alloc` does a persistent stream: the resource on the
    /// request's heap, the `size` bytes off its count.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn persistent<T: Any + Send>(
        req: &mut Request,
        kind: ResourceType,
        payload: T,
        size: usize,
    ) -> Result<Owner<Resource>, Thrown> {
        let made = Self::persistent_made(req, kind, payload, size);
        adopted(made)
    }

    /// What [`persistent`] makes, its one owner not yet named.
    pub(crate) fn persistent_made<T: Any + Send>(
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

    /// `zend_hash_index_del(&EG(regular_list), handle)` with the list's
    /// destructor set aside, as mysqlnd takes the stream it opens out
    /// (`mysqlnd_fixup_regular_list`): the resource left open and its
    /// owners' alone. `get_resources()` no longer lists it, the request's
    /// end does not close it, and its id stays spent; the last owner's
    /// release serves its destructor as for any other.
    pub fn unlist(&self, req: &mut Request) {
        req.heap_mut().resource_unlist(self.0);
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
    #[inline]
    pub(crate) fn type_code(&self) -> u8 {
        match &self.inner {
            Inner::Reference(r) => r.get().own_type_code(),
            _ => self.own_type_code(),
        }
    }

    /// [`Value::type_code`] of the value as it stands, a reference not
    /// followed: `IS_REFERENCE`'s own code there.
    #[inline]
    const fn own_type_code(&self) -> u8 {
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
            Inner::Reference(_) => 10,
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
    #[inline]
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
    #[inline]
    pub(crate) fn of_zval(zval: Zval) -> Value {
        let named = match zval.kind() {
            ZvalType::Undef => Some(Value::undef()),
            ZvalType::False => Some(Value::bool(false)),
            ZvalType::True => Some(Value::bool(true)),
            ZvalType::Long => zval.as_long().map(Value::int),
            ZvalType::Double => zval.as_double().map(Value::float),
            ZvalType::String => zval.as_str().map(|s| Value::string(PhpString::of(s))),
            ZvalType::Array => zval.as_arr().map(|a| Value::array(PhpArray::of(a))),
            ZvalType::Object => zval.as_obj().map(|o| Value::object(PhpObject::of(o))),
            ZvalType::Resource => zval.as_resource().map(|r| Value::resource(Resource::of(r))),
            ZvalType::Reference => zval.as_reference().map(|r| Value::reference(PhpRef::of(r))),
            ZvalType::Null | ZvalType::ConstExpr | ZvalType::Indirect | ZvalType::Ptr => None,
        };
        named.unwrap_or_else(Value::null)
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
        let key = Owner::index(5);
        assert!(a.set(&mut req, key, Owner::int(1)).is_ok());
        assert!(a.append(&mut req, Owner::int(2)).is_ok());
        let key = Owner::index(-4);
        assert!(a.set(&mut req, key, Owner::null()).is_ok());
        assert_eq!(a.len(), 3);
        assert_eq!(a.get(&Key::Int(6)), Some(Value::int(2)));
        assert!(!a.is_list());
        assert_eq!(a.cursor_key(), Some(Key::Int(5)));
        a.cursor_next();
        assert_eq!(a.cursor_current(), Some(Value::int(2)));
        let first = walk::rows(&req, *a).next().map(|(key, _)| key);
        assert_eq!(first, Some(Key::Int(5)));
        assert_eq!(a.unset(&mut req, &Key::Int(5)), Ok(true));
        assert_eq!(a.len(), 2);
        assert_eq!(a.cursor_current(), Some(Value::int(2)));
        a.give_up(&mut req);
    }

    /// Copying a `Value` that holds an array names the one array
    /// (`ZVAL_COPY_VALUE`), the engine's `addref` is the second owner
    /// (`ZVAL_COPY`), and a write through the second name separates it
    /// as `SEPARATE_ARRAY` does: the first name reads what it read.
    #[test]
    fn a_copy_is_a_refcount_and_a_write_separates() {
        let mut req = crate::test::request();
        let start = req.memory_usage(false);
        let Ok(mut a) = PhpArray::new(&mut req).map(Owner::hand_on) else {
            return;
        };
        assert!(a.append(&mut req, Owner::int(1)).is_ok());
        let first = Value::array(a);
        let second = first;
        assert_eq!(first.refcount(), 1, "a copy touches no refcount");
        req.addref(&second);
        assert_eq!(first.refcount(), 2);
        let Some(mut b) = second.as_array().copied() else {
            return;
        };
        assert!(b.append(&mut req, Owner::int(2)).is_ok());
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
        let second = Owner::take(&req, second);
        assert_eq!(first.refcount(), 2);
        assert_eq!(second.as_object(), Some(&object));
        let Ok(mut a) = PhpArray::new(&mut req) else {
            second.give_up(&mut req);
            return;
        };
        assert!(a.append(&mut req, second).is_ok());
        assert_eq!(
            a.get(&Key::Int(0))
                .and_then(|v| v.as_object().map(PhpObject::index)),
            Some(1)
        );
        a.give_up(&mut req);
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
        let text_view = *text;
        assert!(object.set_prop(&mut req, name, text.into_value()).is_ok());
        let text = text_view;
        assert_eq!(text.heap().refcount(), 1, "the table holds the one owner");
        assert_eq!(
            object
                .prop(&req, b"message")
                .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec)),
            Some(b"a string the object holds".to_vec())
        );
        assert_eq!(object.prop(&req, b"missing"), None);
        assert_eq!(object.props(&mut req).map(|e| e.len()), Ok(1));
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
        let Ok(r) = PhpRef::new(&mut req, Owner::int(3)) else {
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
        let string = *text;
        let Ok(()) = r.set(&mut req, text.into_value()) else {
            return;
        };
        assert_eq!(
            v.deref().as_str_bytes().map(<[u8]>::to_vec),
            Some(b"held through the reference".to_vec())
        );
        assert_eq!(string.heap().refcount(), 1, "the cell holds the one owner");
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
        #[expect(
            clippy::unnecessary_wraps,
            reason = "the resource destructor's shape, which a destructor that throws needs"
        )]
        fn dtor(req: &mut Request, r: Resource) -> Result<(), Thrown> {
            let seen = r.native::<u32>(req).copied().unwrap_or(0);
            req.define_constant(b"dtor", Owner::int(i64::from(seen)));
            Ok(())
        }
        let mut req = crate::test::request();
        let start = req.memory_usage(false);
        let kind = req.resources().register_type(b"stream", Some(dtor));
        let Ok(r) = Resource::new(&mut req, kind, 7u32).map(Owner::hand_on) else {
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
        let Ok(next) = Resource::new(&mut req, kind, 0u8).map(Owner::hand_on) else {
            return;
        };
        assert_eq!(next.id(), 2, "an id is never given twice");
    }
}

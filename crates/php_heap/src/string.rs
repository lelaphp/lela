//! Strings (design §5.2): php's `zend_string`, one allocation holding the
//! refcounted header, the lazily cached hash, the length and the bytes,
//! which are bytes and never UTF-8. A [`crate::Zval`] of type `String`
//! names one of these and stays sixteen bytes, because what it holds is
//! the pointer php holds. Reading its bytes is that pointer and the
//! length beside them: no table is consulted and nothing is taken.
//!
//! Two kinds share the one type, as php's `zend_string *` does. An
//! interned string is flagged `IS_STR_INTERNED`, reads as one owner
//! whatever its header says, and is freed by the table it belongs to and
//! never by a release: that is what a `CONST` operand of an op array
//! holds, and what the name of every native is. Every other string
//! belongs to the request that allocated it, is refcounted, and is freed
//! through the same heap it came from.
//!
//! **The three tables** (design §5.2, §5.14), which are php's three:
//!
//! - the frozen table, laid down once for the process and immutable
//!   afterwards, php's `zend_empty_string` and `zend_one_char_string`
//!   (`Z/Zend/zend_string.c:86-120`);
//! - one [`Strings`] per engine, which a unit's constant strings enter
//!   when it is published into the unit cache (design §6.3), php's
//!   `interned_strings_permanent` under opcache's shared table
//!   (`Z/ext/opcache/ZendAccelerator.c:515-575`);
//! - one table per request, held by the [`crate::Heap`] that allocates
//!   its strings and dropped with the rest of the request's memory, php's
//!   `CG(interned_strings)` (`Z/Zend/zend_string.c:346-353`).
//!
//! Every home walks the same ladder, php's: the frozen table first, its
//! own next, a new string last. Nothing is taken to read a string, and
//! nothing here is process-wide but the frozen table, which is written
//! once before anything can name it.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::alloc::Layout;
use std::cell::Cell;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::OnceLock;

use crate::ZvalType;
use crate::gc::{self, Refcounted};
use crate::hooks::Fatal;
use crate::mm::Mm;

/// `zend_string` without its bytes: twenty-four bytes, php's own layout,
/// with `len + 1` bytes following it in the same allocation. The last is
/// the NUL php keeps so that `ZSTR_VAL` may be handed to a C library.
#[repr(C)]
pub(crate) struct StrHeader {
    /// `zend_refcounted_h gc`.
    gc: Refcounted,
    /// `zend_ulong h`: the hash, zero while it has not been taken.
    h: Cell<u64>,
    /// `size_t len`: the bytes, without the NUL.
    len: usize,
}

/// The header's size, php's `_ZSTR_HEADER_SIZE`.
const HEADER: usize = size_of::<StrHeader>();

/// `IS_STR_INTERNED` (`Z/Zend/zend_types.h:849`): the bit a table's
/// string carries whichever table it is in, which is what says it is
/// never refcounted and never freed by a release.
pub(crate) const INTERNED: u32 = gc::IMMUTABLE;

/// What a string outside every request's heap carries beside it:
/// `IS_STR_PERSISTENT`, allocated by `pemalloc`, and `IS_STR_PERMANENT`,
/// which relives the request boundary.
const PERMANENT: u32 = INTERNED | gc::PERSISTENT | gc::PERMANENT;

/// One string: php's `zend_string *`.
///
/// Naming a string does not keep it alive, exactly as php's pointer does
/// not: a request string lives while its refcount says so, and the
/// engine's kernels take and give up owners through
/// [`crate::Heap::addref`] and [`crate::Heap::release`]. An interned one
/// lives as long as the table it was interned into.
#[derive(Clone, Copy)]
pub struct Str(NonNull<StrHeader>);

impl Str {
    /// The bytes one allocation of a string of `len` bytes takes:
    /// php's `ZEND_MM_ALIGNED_SIZE(_ZSTR_STRUCT_SIZE(len))`, which is what
    /// makes `memory_get_usage()` move by what php's moves by.
    pub(crate) const fn allocation_size(len: usize) -> usize {
        (HEADER + len + 1).next_multiple_of(8)
    }

    /// Lay a string of `bytes` down at `at`, which is an allocation of
    /// [`Str::allocation_size`] bytes, with one owner and `flags` set:
    /// php's `zend_string_init` over `zend_string_alloc`.
    ///
    /// # Safety
    ///
    /// `at` is writable for [`Str::allocation_size`] of `bytes.len()`
    /// bytes and holds nothing else.
    pub(crate) unsafe fn init(at: NonNull<u8>, bytes: &[u8], flags: u32) -> Str {
        let header: NonNull<StrHeader> = at.cast();
        // SAFETY: the caller's allocation is at least the header and the
        // bytes, and nothing else holds it.
        unsafe {
            header.as_ptr().write(StrHeader {
                gc: Refcounted::new(ZvalType::String, gc::NOT_COLLECTABLE | flags),
                h: Cell::new(0),
                len: bytes.len(),
            });
        }
        // SAFETY: the bytes sit right after the header, inside the same
        // allocation the caller gave.
        let val = unsafe { at.as_ptr().add(HEADER) };
        // SAFETY: `val` is writable for `bytes.len()` bytes, and a fresh
        // allocation cannot overlap the source.
        unsafe { val.copy_from_nonoverlapping(bytes.as_ptr(), bytes.len()) };
        // SAFETY: the NUL php keeps is the byte after them, still inside
        // the allocation.
        let end = unsafe { val.add(bytes.len()) };
        // SAFETY: `end` is the last byte of the allocation.
        unsafe { end.write(0) };
        Str(header)
    }

    /// Where the string sits, for the heap that frees it.
    pub(crate) const fn address(self) -> NonNull<u8> {
        self.0.cast()
    }

    /// The header.
    fn header(&self) -> &StrHeader {
        // SAFETY: a `Str` names a string that was laid down by `init` and
        // is live while its owner holds it, which is the same contract
        // php's `zend_string *` carries.
        unsafe { self.0.as_ref() }
    }

    /// The bytes, `ZSTR_VAL` over `ZSTR_LEN`, without the NUL.
    ///
    /// This is the whole path from a zval of type `String` to what it
    /// says: the pointer the zval holds, the length beside it, and the
    /// bytes after the header. No table is consulted and nothing is
    /// taken, whichever of the three tables the string came from, which
    /// is what lets one op array's constants be read by every request
    /// thread at once. What the interpreter does with a constant string
    /// on every `ECHO`, the same op array's constant read on every pass of
    /// a loop, counting down on the CV's truth until `<` and `++` are
    /// read (#213):
    ///
    /// ```php
    /// for ($i = 3; $i; $i = $i - 1) { echo 'dax'; }
    /// ```
    /// ```expect exit=0
    /// daxdaxdax
    /// ```
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        let len = self.header().len;
        // SAFETY: the bytes are inside the allocation, right after the
        // header.
        let val = unsafe { self.0.as_ptr().cast::<u8>().add(HEADER) };
        // SAFETY: `init` wrote `len` bytes there and nothing writes them
        // again while the string lives, so the borrow is of initialised,
        // immutable bytes.
        unsafe { std::slice::from_raw_parts(val, len) }
    }

    /// Whether this is the one string `other` is: php's `s1 == s2`, one
    /// allocation and not one content, which is what interning gives and
    /// what says a constant string two requests read was published once.
    #[must_use]
    pub fn is_same(self, other: Str) -> bool {
        self.0 == other.0
    }

    /// The length in bytes, `strlen`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.header().len
    }

    /// Whether the length is zero.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// `zend_string_hash_val`: the cached hash, taken on the first ask.
    /// Zero means unset, which is why php forces the top bit of every
    /// hash it computes (`Z/Zend/zend_string.h:544-547`).
    ///
    /// The cache is written, so the first ask is the owner's to make: a
    /// string about to be shared is hashed before it is published, which
    /// is what php does in `zend_add_interned_string`.
    #[must_use]
    pub fn hash(&self) -> u64 {
        let header = self.header();
        let cached = header.h.get();
        if cached != 0 {
            return cached;
        }
        let hash = hash_func(self.bytes());
        header.h.set(hash);
        hash
    }

    /// `zend_string_forget_hash_val`: the hash and the UTF-8 verdict are
    /// no longer known, which is what a string that was written into says.
    /// Only a string the request owns is written into; php does not call
    /// this on an interned one.
    pub fn forget_hash(&self) {
        self.header().h.set(0);
        self.header().gc.del_flags(gc::VALID_UTF8);
    }

    /// `ZSTR_IS_INTERNED`: one of a table's strings, which is never
    /// refcounted and is freed by that table alone.
    #[must_use]
    pub fn is_interned(&self) -> bool {
        self.header().gc.flags() & gc::IMMUTABLE != 0
    }

    /// `IS_STR_PERSISTENT`: allocated outside any request's heap.
    #[must_use]
    pub fn is_persistent(&self) -> bool {
        self.header().gc.flags() & gc::PERSISTENT != 0
    }

    /// `ZSTR_IS_VALID_UTF8`: the bytes were checked once and are valid
    /// UTF-8. A string that was written into forgets this with its hash.
    #[must_use]
    pub fn is_valid_utf8(&self) -> bool {
        self.header().gc.flags() & gc::VALID_UTF8 != 0
    }

    /// Record that the bytes are valid UTF-8, php's
    /// `GC_ADD_FLAGS(s, IS_STR_VALID_UTF8)`. The flags are written, so
    /// this is the owner's to say, as the hash is.
    pub fn mark_valid_utf8(&self) {
        self.header().gc.add_flags(gc::VALID_UTF8);
    }

    /// `zend_string_refcount`: an interned string reads as one owner
    /// whatever its header says.
    #[must_use]
    pub fn refcount(&self) -> u32 {
        if self.is_interned() {
            return 1;
        }
        self.header().gc.refcount()
    }

    /// `zend_string_addref`: one more owner, and nothing at all for an
    /// interned string.
    pub fn addref(&self) {
        if !self.is_interned() {
            self.header().gc.addref();
        }
    }

    /// `zend_string_delref`: one owner fewer, and what is left. An
    /// interned string keeps its one.
    pub(crate) fn delref(self) -> u32 {
        if self.is_interned() {
            return 1;
        }
        self.header().gc.delref()
    }

    /// The payload word of a zval naming this string. php stores the
    /// pointer; so does this, with its provenance exposed, because a
    /// zval is a plain sixteen bytes that crosses threads inside a
    /// published unit.
    pub(crate) fn payload(self) -> u64 {
        self.0.as_ptr().expose_provenance() as u64
    }

    /// The string a zval's payload word names.
    ///
    /// # Safety
    ///
    /// The word came from [`Str::payload`] for a string that is still
    /// live.
    pub(crate) unsafe fn from_payload(payload: u64) -> Option<Str> {
        let at: *mut StrHeader =
            std::ptr::with_exposed_provenance_mut(usize::try_from(payload).unwrap_or(0));
        NonNull::new(at).map(Str)
    }
}

/// Two strings are the one string when their bytes are, which is php's
/// `zend_string_equals`; identity is what interning is for.
impl PartialEq for Str {
    fn eq(&self, other: &Str) -> bool {
        self.0 == other.0 || self.bytes() == other.bytes()
    }
}

impl Eq for Str {}

impl std::hash::Hash for Str {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.bytes().hash(state);
    }
}

impl std::fmt::Debug for Str {
    /// The bytes as php's `var_dump` would show them, so a heap dump
    /// reads as strings and not as addresses.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Str({:?})", String::from_utf8_lossy(self.bytes()))
    }
}

/// A home for interned strings: php's `zend_new_interned_string` behind
/// its two handlers (`Z/Zend/zend_string.c:363-373`), which the compiler
/// and the engine call without knowing which home answers.
///
/// The two homes differ in one thing, where the bytes are allocated and
/// so when they are freed: [`Strings`], an engine's, on the system
/// allocator for the life of the engine, and [`crate::Heap`], a
/// request's, out of the request's own memory and gone with it. Both
/// answer from the frozen table first, so the empty string and a string
/// of one byte are allocated nowhere at all.
pub trait Intern {
    /// The one string for these bytes in this home, interned on the first
    /// ask, and the fatal the allocator raised when there is no memory
    /// for it.
    fn intern(&mut self, bytes: &[u8]) -> Result<Str, Fatal>;
}

/// The engine's interned strings (design §5.2, §5.14): the table a unit's
/// constant strings enter when it is published into the unit cache
/// (design §6.3), and where the names an engine knows before its first
/// request go as they become strings.
///
/// It is php's `interned_strings_permanent` where php has one process and
/// one engine, and opcache's shared table where php has many: a string in
/// it is immutable, hashed before it is published, never refcounted and
/// never freed while the engine stands, which is what lets every request
/// thread read one with nothing taken. Growing it is the engine's act,
/// which the engine alone serialises; a request never grows it.
pub struct Strings {
    table: Table,
}

impl Default for Strings {
    fn default() -> Strings {
        Strings::new()
    }
}

impl Strings {
    /// An empty table.
    #[must_use]
    pub fn new() -> Strings {
        Strings {
            table: Table::default(),
        }
    }

    /// How many strings the table holds, which is what says the constant
    /// string two published units share was interned once.
    #[must_use]
    pub fn count(&self) -> usize {
        self.table.count()
    }
}

impl Intern for Strings {
    /// php's `zend_string_init_interned_permanent`
    /// (`Z/Zend/zend_string.c:267-280`): the frozen table, then this one,
    /// then a string on the system allocator, hashed and flagged before
    /// it is published.
    fn intern(&mut self, bytes: &[u8]) -> Result<Str, Fatal> {
        if let Some(frozen) = frozen_string(bytes) {
            return Ok(frozen);
        }
        let hash = hash_func(bytes);
        if let Some(held) = self.table.find(hash, bytes) {
            return Ok(held);
        }
        let s = persistent(bytes)?;
        self.table.add(hash, s);
        Ok(s)
    }
}

impl Drop for Strings {
    /// `zend_hash_destroy(&interned_strings_permanent)` with `_str_dtor`
    /// as its destructor (`Z/Zend/zend_string.c:66-70`): every string the
    /// table laid down goes back to the allocator it came from. The
    /// engine outlives every request that can name one.
    fn drop(&mut self) {
        for s in self.table.strings() {
            free_persistent(s);
        }
    }
}

// SAFETY: the table owns its strings and the only other names for them
// are the zvals of the units the engine published, which are read and
// never written; moving the table moves that sole ownership.
unsafe impl Send for Strings {}

impl std::fmt::Debug for Strings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Strings({} interned)", self.count())
    }
}

/// One table of interned strings: php's `HashTable` of `zend_string *`
/// keyed by the string itself (`zend_init_interned_strings_ht`,
/// `Z/Zend/zend_string.c:78-84`). The key here is the hash, and the chain
/// under it is walked comparing the bytes, which is the walk
/// `zend_interned_string_ht_lookup_ex` makes over php's hash slots
/// (`Z/Zend/zend_string.c:132-169`).
#[derive(Default)]
pub(crate) struct Table {
    slots: HashMap<u64, Vec<Interned>>,
}

impl std::fmt::Debug for Table {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Table({} interned)", self.count())
    }
}

impl Table {
    /// The string here whose bytes are `bytes`, if it is here.
    pub(crate) fn find(&self, hash: u64, bytes: &[u8]) -> Option<Str> {
        let chain = self.slots.get(&hash)?;
        chain.iter().map(|held| held.0).find(|s| s.bytes() == bytes)
    }

    /// Publish `s` under `hash`, php's `zend_add_interned_string`
    /// (`Z/Zend/zend_string.c:173-185`). php takes the hash before it
    /// publishes, so that a table's string is immutable from the moment
    /// a second thread can name it; [`Str::hash`] here is that act.
    pub(crate) fn add(&mut self, hash: u64, s: Str) {
        let _ = s.hash();
        self.slots.entry(hash).or_default().push(Interned(s));
    }

    /// How many strings are here.
    pub(crate) fn count(&self) -> usize {
        self.slots.values().map(Vec::len).sum()
    }

    /// Every string here, for the table that frees what it laid down.
    fn strings(&self) -> impl Iterator<Item = Str> {
        self.slots.values().flatten().map(|held| held.0)
    }
}

/// `sizeof(Bucket)` (`Z/Zend/zend_types.h:400-404`): a zval, the hash and
/// the key pointer, thirty-two bytes.
const BUCKET: usize = 32;

/// One `HT_HASH` slot, `sizeof(uint32_t)`. php lays down twice as many of
/// them as there are buckets, which is what `HT_SIZE_TO_MASK` says
/// (`Z/Zend/zend_types.h:486-489`).
const HASH_SLOT: usize = 4;

/// `zend_init_interned_strings_ht(&CG(interned_strings), 0)`'s `nSize`
/// (`Z/Zend/zend_string.c:78-84`, `:346-349`), which
/// `zend_hash_check_size` leaves at 1024 because it is already a power of
/// two.
const REQUEST_TABLE_SIZE: u32 = 1024;

/// The storage one request's interned-string table takes out of that
/// request's own memory (design §5.13): php's bucket array for
/// `CG(interned_strings)`, and not the strings in it.
///
/// php's table is inited with `persistent` 0 and no eager real-init, so
/// the array is `emalloc`ed by the first string the request interns and
/// every byte of it is in `memory_get_usage()`
/// (`zend_hash_real_init_mixed_ex`'s `emalloc` branch,
/// `Z/Zend/zend_hash.c:166-231`, reached from
/// `_zend_hash_add_or_update_i`'s `HASH_FLAG_UNINITIALIZED` arm). One
/// allocation holds the buckets and the hash slots together,
/// `HT_SIZE_EX(nTableSize, HT_SIZE_TO_MASK(nTableSize))`
/// (`Z/Zend/zend_types.h:486-493`): forty bytes to a bucket, so 40960 for
/// php's 1024, ten whole pages of the request's heap.
///
/// It doubles where php's doubles, `ZEND_HASH_IF_FULL_DO_RESIZE` over
/// `zend_hash_do_resize` (`Z/Zend/zend_hash.c:84-87`, `:1147-1173`):
/// nothing is ever taken out of this table, so `nNumUsed` is the count of
/// strings and the rehash-in-place arm is never the one taken. php
/// allocates the larger array before it frees the smaller, which is what
/// the peak reads.
///
/// What is here is that allocation and php's `nTableSize`. The lookup
/// itself is still [`Table`]'s `HashMap` on the system allocator, the
/// stand-in for `zend_hash` until the array of design §5.3 lands; what
/// this type holds is the bytes php's number reports.
pub(crate) struct Buckets {
    /// `nTableSize`: php's 1024 until the table doubles.
    size: u32,
    /// `nNumUsed`, which for this table is the count of strings.
    used: u32,
    /// The allocation `HT_GET_DATA_ADDR` names, once there is one.
    /// `None` is php's `HASH_FLAG_UNINITIALIZED`.
    data: Option<NonNull<u8>>,
}

impl Buckets {
    /// An uninitialised table at php's `nTableSize`, holding nothing:
    /// what `zend_interned_strings_activate` leaves behind at request
    /// start, before any string has been interned.
    pub(crate) const fn new() -> Buckets {
        Buckets {
            size: REQUEST_TABLE_SIZE,
            used: 0,
            data: None,
        }
    }

    /// `HT_SIZE_EX(size, HT_SIZE_TO_MASK(size))`: the buckets and twice
    /// as many hash slots, in one allocation.
    const fn bytes(size: u32) -> usize {
        size as usize * BUCKET + 2 * size as usize * HASH_SLOT
    }

    /// Room for one more string, out of `mm`: php's lazy
    /// `zend_hash_real_init_mixed` on the first add and its doubling
    /// resize on the add that fills the table. The allocation is the
    /// request's own, so `memory_get_usage()` carries it as php's does,
    /// and the request's end takes it with everything else.
    pub(crate) fn add_one(&mut self, mm: &mut Mm) -> Result<(), Fatal> {
        match self.data {
            None => self.data = Some(mm.alloc(Self::bytes(self.size))?),
            Some(old) if self.used >= self.size => {
                let grown = self.size.saturating_mul(2);
                let at = mm.alloc(Self::bytes(grown))?;
                // SAFETY: `old` is this table's storage, handed out by
                // this same heap and not freed since; php frees it here
                // too, once the larger array stands beside it.
                unsafe { mm.free(old) };
                self.size = grown;
                self.data = Some(at);
            }
            Some(_) => {}
        }
        self.used = self.used.saturating_add(1);
        Ok(())
    }
}

impl std::fmt::Debug for Buckets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let bytes = match self.data {
            Some(_) => Self::bytes(self.size),
            None => 0,
        };
        write!(
            f,
            "Buckets({} of {} used, {bytes} bytes)",
            self.used, self.size
        )
    }
}

/// A string in a table. Sharing one across threads is sound because it is
/// immutable from the moment it is published: its bytes never change, its
/// refcount is never touched, its hash was taken before it went in, and
/// it is freed only with the table itself.
struct Interned(Str);

// SAFETY: see [`Interned`]: a table's string is immutable while the table
// stands, and the table outlives every thread that can name one.
unsafe impl Send for Interned {}
// SAFETY: see [`Interned`].
unsafe impl Sync for Interned {}

/// The frozen table (design §5.2): what the process knows before any
/// engine is built, laid down once for the life of the process and
/// immutable afterwards, so that every thread reads it with nothing
/// taken.
///
/// php's are `zend_empty_string` and `zend_one_char_string[256]`, laid
/// down one by one by `zend_interned_strings_init`
/// (`Z/Zend/zend_string.c:86-120`) and answered before any table by
/// `zend_string_init_fast` (`Z/Zend/zend_string.h:196-205`). Zend's
/// `known_strings` and the names of an engine's natives are that
/// engine's vocabulary and not the process's, so they enter its
/// [`Strings`] at startup, which is the same act one level down.
///
/// A string the system allocator refused at startup is simply not here,
/// and the home that asked for it allocates its own: the table answers
/// what it has, and nothing here can fail a request.
struct Frozen {
    /// `zend_empty_string`.
    empty: Option<Str>,
    /// `zend_one_char_string`, the string of each single byte.
    chars: [Option<Str>; 256],
}

impl Frozen {
    /// The frozen strings laid down, hashed and flagged, in the order
    /// `zend_interned_strings_init` lays them: the empty string, then the
    /// 256 of one byte, those below `0x80` known to be valid UTF-8.
    fn new() -> Frozen {
        let empty = froze(b"");
        let mut chars = [None; 256];
        for (i, slot) in chars.iter_mut().enumerate() {
            let byte = [u8::try_from(i).unwrap_or(0)];
            *slot = froze(&byte);
            if byte[0] >= 0x80 {
                continue;
            }
            if let Some(s) = *slot {
                s.mark_valid_utf8();
            }
        }
        Frozen { empty, chars }
    }

    /// The frozen string for these bytes, php's `zend_string_init_fast`
    /// over `ZSTR_EMPTY_ALLOC` and `ZSTR_CHAR`.
    fn find(&self, bytes: &[u8]) -> Option<Str> {
        match bytes {
            [] => self.empty,
            [byte] => self.chars.get(usize::from(*byte)).copied().flatten(),
            _ => None,
        }
    }
}

// SAFETY: a frozen string is written once, before the table is published
// through the `OnceLock` below, and is immutable and alive for the life
// of the process afterwards.
unsafe impl Send for Frozen {}
// SAFETY: see the `Send` above.
unsafe impl Sync for Frozen {}

/// One frozen string, hashed and marked valid UTF-8 as php marks the
/// empty string, before anything can name it. Nothing frees it: the
/// frozen table is never dropped, as php's permanent one stands for as
/// long as a request can be served.
fn froze(bytes: &[u8]) -> Option<Str> {
    let s = persistent(bytes).ok()?;
    let _ = s.hash();
    if bytes.is_empty() {
        s.mark_valid_utf8();
    }
    Some(s)
}

/// The frozen string for these bytes, if the process has one: the first
/// rung of the ladder every home walks. The table is laid down on the
/// first ask and never again.
pub(crate) fn frozen_string(bytes: &[u8]) -> Option<Str> {
    static FROZEN: OnceLock<Frozen> = OnceLock::new();
    FROZEN.get_or_init(Frozen::new).find(bytes)
}

/// `zend_inline_hash_func` (`Z/Zend/zend_string.h:453`): DJBX33A over the
/// bytes, with the top bit forced so that a hash is never zero and the
/// cached-hash convention has a spare value.
pub(crate) fn hash_func(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 5381;
    for byte in bytes {
        hash = hash.wrapping_mul(33).wrapping_add(u64::from(*byte));
    }
    hash | 0x8000_0000_0000_0000
}

/// A string outside every request's heap, on the system allocator, which
/// is php's `pemalloc(size, 1)`: interned, persistent and permanent, so
/// no request's end takes it and only its table frees it.
fn persistent(bytes: &[u8]) -> Result<Str, Fatal> {
    let size = Str::allocation_size(bytes.len());
    let Ok(layout) = Layout::from_size_align(size, align_of::<StrHeader>()) else {
        return Err(Fatal::OutOfMemory(size));
    };
    // SAFETY: the layout's size is the header and the bytes, never zero.
    let at = unsafe { std::alloc::alloc(layout) };
    let Some(at) = NonNull::new(at) else {
        return Err(Fatal::OutOfMemory(size));
    };
    // SAFETY: the allocation is exactly what `init` asks for and holds
    // nothing else.
    Ok(unsafe { Str::init(at, bytes, PERMANENT) })
}

/// `pefree(s, 1)`: give a persistent string back to the system
/// allocator. A frozen one never reaches here, because the frozen table
/// is never dropped.
pub(crate) fn free_persistent(s: Str) {
    let size = Str::allocation_size(s.len());
    let Ok(layout) = Layout::from_size_align(size, align_of::<StrHeader>()) else {
        return;
    };
    // SAFETY: the string came from `persistent` with this layout and the
    // table that owned it is being dropped, so nothing names it.
    unsafe { std::alloc::dealloc(s.address().as_ptr(), layout) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_the_same_bytes_twice_gives_one_string() {
        let mut strings = Strings::new();
        let interned = (
            strings.intern(b"var_dump"),
            strings.intern(b"var_dump"),
            strings.intern(b"extension_loaded"),
        );
        assert!(
            interned.0.is_ok() && interned.1.is_ok() && interned.2.is_ok(),
            "the table would not intern"
        );
        let (Ok(first), Ok(again), Ok(other)) = interned else {
            return;
        };
        assert_eq!(first, again);
        assert_eq!(first.address(), again.address());
        assert_ne!(first, other);
        assert_eq!(first.bytes(), b"var_dump");
        assert_eq!(other.len(), 16);
        assert_eq!(strings.count(), 2, "two strings, asked for three times");
    }

    #[test]
    fn an_interned_string_is_frozen_and_hashed_before_it_is_published() {
        let mut strings = Strings::new();
        let interned = strings.intern(b"php");
        assert!(interned.is_ok(), "the table would not intern");
        let Ok(s) = interned else {
            return;
        };
        assert!(s.is_interned());
        assert!(s.is_persistent());
        assert_eq!(s.refcount(), 1);
        s.addref();
        assert_eq!(s.refcount(), 1);
        // `hash_func` over "php", with php's top bit forced.
        assert_eq!(s.hash(), hash_func(b"php"));
        assert_ne!(s.hash(), 0);
    }

    /// The frozen table answers before any other, so the empty string and
    /// every string of one byte are the process's and are allocated
    /// nowhere: two engines share what php's `zend_string_init_fast`
    /// hands back before it reaches a table.
    #[test]
    fn the_frozen_table_answers_the_empty_string_and_every_single_byte() {
        let mut one = Strings::new();
        let mut another = Strings::new();
        let frozen = (
            one.intern(b""),
            another.intern(b""),
            one.intern(b"a"),
            another.intern(b"a"),
            one.intern(&[0xff]),
        );
        let (Ok(empty), Ok(elsewhere), Ok(a), Ok(also_a), Ok(high)) = frozen else {
            // The frozen table allocates nothing and always answers.
            return;
        };
        assert_eq!(empty.address(), elsewhere.address());
        assert!(empty.is_empty());
        assert!(empty.is_valid_utf8());
        assert_eq!(a.address(), also_a.address());
        assert_eq!(a.bytes(), b"a");
        assert!(a.is_valid_utf8(), "a byte below 0x80 is valid UTF-8");
        assert!(!high.is_valid_utf8(), "0xff alone is not");
        assert_eq!(high.len(), 1);
        assert_eq!(
            (one.count(), another.count()),
            (0, 0),
            "nothing the process froze is interned into an engine's table"
        );
    }

    /// A hash is taken before a string is published, so that no thread
    /// writes one another is reading.
    #[test]
    fn a_published_string_carries_its_hash_already() {
        let mut strings = Strings::new();
        let interned = (strings.intern(b"zend_string"), strings.intern(b"x"));
        assert!(interned.0.is_ok(), "the table would not intern");
        let (Ok(s), Ok(frozen)) = interned else {
            return;
        };
        assert_eq!(s.header().h.get(), hash_func(b"zend_string"));
        assert_eq!(frozen.header().h.get(), hash_func(b"x"));
    }

    #[test]
    fn the_header_is_php_s_and_the_allocation_holds_the_nul() {
        assert_eq!(HEADER, 24);
        assert_eq!(Str::allocation_size(0), 32);
        assert_eq!(Str::allocation_size(4), 32);
        assert_eq!(Str::allocation_size(8), 40);
    }
}

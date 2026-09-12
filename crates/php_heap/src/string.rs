//! Strings (design §5.2): php's `zend_string`, one allocation holding the
//! refcounted header, the lazily cached hash, the length and the bytes,
//! which are bytes and never UTF-8. A [`crate::Zval`] of type `String`
//! names one of these and stays sixteen bytes, because what it holds is
//! the pointer php holds.
//!
//! Two kinds share the one type, as php's `zend_string *` does. An
//! interned string is allocated outside any request, flagged
//! `IS_STR_INTERNED` and `IS_STR_PERSISTENT`, never freed and never
//! refcounted: that is what a `CONST` operand of an op array holds, and
//! what the name of every native is. Every other string belongs to the
//! request that allocated it, is refcounted, and is freed through the
//! same heap it came from. The three-table interning model of §5.2 and
//! §5.14 lands on top of the one table here (#107).

use std::cell::Cell;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::{Mutex, OnceLock};

use crate::ZvalType;
use crate::gc::{self, Refcounted};

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

/// One string: php's `zend_string *`.
///
/// Naming a string does not keep it alive, exactly as php's pointer does
/// not: a request string lives while its refcount says so, and the
/// engine's kernels take and give up owners through
/// [`crate::Heap::addref`] and [`crate::Heap::release`].
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
    pub fn forget_hash(&self) {
        self.header().h.set(0);
        self.header().gc.del_flags(gc::VALID_UTF8);
    }

    /// `ZSTR_IS_INTERNED`: one of the process's frozen strings, which is
    /// never refcounted and never freed.
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
    /// `GC_ADD_FLAGS(s, IS_STR_VALID_UTF8)`.
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

    /// The string for these bytes from the process's frozen table,
    /// interned on the first ask (design §5.2): every native's name and
    /// every constant string of a compiled program is one of these.
    #[must_use]
    pub fn intern(bytes: &[u8]) -> Str {
        let mut table = locked();
        if let Some(interned) = table.get(bytes) {
            return interned.0;
        }
        let s = persistent(bytes);
        // php takes an interned string's hash before it publishes it
        // (`zend_new_interned_string_int`), which is what lets a frozen
        // string be read from every thread without a lock.
        let _ = s.hash();
        table.insert(bytes.into(), Interned(s));
        s
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

/// `zend_inline_hash_func` (`Z/Zend/zend_string.h:453`): DJBX33A over the
/// bytes, with the top bit forced so that a hash is never zero and the
/// cached-hash convention has a spare value.
fn hash_func(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 5381;
    for byte in bytes {
        hash = hash.wrapping_mul(33).wrapping_add(u64::from(*byte));
    }
    hash | 0x8000_0000_0000_0000
}

/// A frozen string in the process's table. Sharing one across threads is
/// sound because it is immutable after it is published: its bytes never
/// change, its refcount is never touched, its hash was taken before it
/// went in, and it is never freed.
struct Interned(Str);

// SAFETY: see [`Interned`]: an interned string is immutable and outlives
// every thread that can name it.
unsafe impl Send for Interned {}
// SAFETY: see [`Interned`].
unsafe impl Sync for Interned {}

/// The one process-wide table. The key repeats the bytes so that a
/// lookup borrows them; php's table keys by the string itself, which
/// wants a raw entry API this does not have.
type Table = HashMap<Box<[u8]>, Interned>;

fn table() -> &'static Mutex<Table> {
    static TABLE: OnceLock<Mutex<Table>> = OnceLock::new();
    TABLE.get_or_init(|| Mutex::new(Table::default()))
}

/// The table, taken as it stands after a thread ended while holding it:
/// what is here is append-only, so it was left consistent.
fn locked() -> std::sync::MutexGuard<'static, Table> {
    match table().lock() {
        Ok(table) => table,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// A string outside every request's heap, on the system allocator, which
/// is php's `pemalloc(size, 1)`: interned, persistent and permanent, so
/// nothing frees it and no request's end takes it.
fn persistent(bytes: &[u8]) -> Str {
    let size = Str::allocation_size(bytes.len());
    let Ok(layout) = std::alloc::Layout::from_size_align(size, align_of::<StrHeader>()) else {
        return empty();
    };
    // SAFETY: the layout's size is the header and the bytes, never zero.
    let at = unsafe { std::alloc::alloc(layout) };
    let Some(at) = NonNull::new(at) else {
        return empty();
    };
    // SAFETY: the allocation is exactly what `init` asks for and holds
    // nothing else.
    unsafe { Str::init(at, bytes, gc::IMMUTABLE | gc::PERSISTENT | gc::PERMANENT) }
}

/// `pefree(s, 1)`: give a persistent string back to the system
/// allocator. An interned one never reaches here, because nothing frees
/// what the process froze.
pub(crate) fn free_persistent(s: Str) {
    let size = Str::allocation_size(s.len());
    let Ok(layout) = std::alloc::Layout::from_size_align(size, align_of::<StrHeader>()) else {
        return;
    };
    // SAFETY: the string came from `persistent` with this layout and its
    // last owner has given it up.
    unsafe { std::alloc::dealloc(s.address().as_ptr(), layout) };
}

/// The one string a machine that cannot allocate can still answer with:
/// php's `zend_empty_string`, laid down once in this crate's own memory.
fn empty() -> Str {
    static EMPTY: OnceLock<Interned> = OnceLock::new();
    EMPTY
        .get_or_init(|| {
            let leaked: &'static mut [u8] =
                Box::leak(vec![0u8; Str::allocation_size(0)].into_boxed_slice());
            let at = NonNull::from(leaked).cast::<u8>();
            // SAFETY: the leaked box is the size `init` asks for, lives
            // for the process, and nothing else holds it.
            let s = unsafe { Str::init(at, b"", gc::IMMUTABLE | gc::PERSISTENT | gc::PERMANENT) };
            Interned(s)
        })
        .0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interning_the_same_bytes_twice_gives_one_string() {
        let first = Str::intern(b"var_dump");
        let again = Str::intern(b"var_dump");
        let other = Str::intern(b"extension_loaded");
        assert_eq!(first, again);
        assert_eq!(first.address(), again.address());
        assert_ne!(first, other);
        assert_eq!(first.bytes(), b"var_dump");
        assert_eq!(other.len(), 16);
        assert!(Str::intern(b"").is_empty());
    }

    #[test]
    fn an_interned_string_is_frozen_and_hashed_before_it_is_published() {
        let s = Str::intern(b"php");
        assert!(s.is_interned());
        assert!(s.is_persistent());
        assert_eq!(s.refcount(), 1);
        s.addref();
        assert_eq!(s.refcount(), 1);
        // `hash_func` over "php", with php's top bit forced.
        assert_eq!(s.hash(), hash_func(b"php"));
        assert_ne!(s.hash(), 0);
    }

    #[test]
    fn the_header_is_php_s_and_the_allocation_holds_the_nul() {
        assert_eq!(HEADER, 24);
        assert_eq!(Str::allocation_size(0), 32);
        assert_eq!(Str::allocation_size(4), 32);
        assert_eq!(Str::allocation_size(8), 40);
    }
}

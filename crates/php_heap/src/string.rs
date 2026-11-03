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
use std::ptr::NonNull;
use std::sync::OnceLock;

use crate::array::{Arr, Key, free_persistent_array, persistent_array};
use crate::bump::Bump;
use crate::gc::{self, Counted, Refcounted};
use crate::hooks::Fatal;
use crate::mm::Mm;
use crate::{Zval, ZvalType};

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

// The header at the block's start, where `Zval::counted` reads it for
// every counted kind without asking which.
const _: () = assert!(std::mem::offset_of!(StrHeader, gc) == 0);

/// The header's size, php's `_ZSTR_HEADER_SIZE`.
const HEADER: usize = size_of::<StrHeader>();

// php's `zend_string`: the length after the refcounted header and the
// hash, the bytes after the length, which compiled code reads at
// `Str::LEN_AT` and `Str::VAL_AT`.
const _: () = assert!(Str::LEN_AT == 16 && Str::VAL_AT == 24);

/// `IS_STR_INTERNED` (`Z/Zend/zend_types.h:849`): the bit a table's
/// string carries whichever table it is in, which is what says it is
/// never refcounted and never freed by a release.
pub(crate) const INTERNED: u32 = gc::IMMUTABLE;

/// What a string outside every request's heap carries beside it:
/// `IS_STR_PERSISTENT`, allocated by `pemalloc`, and `IS_STR_PERMANENT`,
/// which relives the request boundary.
const PERMANENT: u32 = INTERNED | gc::PERSISTENT | gc::PERMANENT;

/// `IS_STR_CLASS_NAME_MAP_PTR` (`Z/Zend/zend_types.h:860`), php's
/// `GC_PROTECTED` on a string: an interned string that names a class,
/// whose count word, which an interned string never counts with, holds
/// the offset of its class cache in every request's map table.
const CLASS_NAME_MAP_PTR: u32 = gc::PROTECTED;

/// A request string fresh from `zend_string_alloc(len, 0)`, before it
/// is handed out: one owner, the holder's, and nothing else naming it,
/// so its bytes are written in place as a native writes `ZSTR_VAL` and
/// cut down as `zend_string_truncate` cuts them. Neither `Copy` nor
/// `Clone`, so the bytes are written through one name; [`FreshStr::finish`] makes it
/// the string it is from then on.
#[derive(Debug)]
pub struct FreshStr(pub(crate) Str);

impl FreshStr {
    /// The length in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the length is zero.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The bytes, `ZSTR_VAL(s)` for its length.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        self.0.bytes()
    }

    /// The bytes, to write: `ZSTR_VAL(s)` for its length.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        let len = self.0.len();
        // SAFETY: the bytes sit right after the header, inside the
        // allocation, and this value is the string's one name, so the
        // borrow of `self` is the one access to them.
        let val = unsafe { self.0.0.as_ptr().cast::<u8>().add(HEADER) };
        // SAFETY: `len` bytes were laid down there, zero or written.
        unsafe { std::slice::from_raw_parts_mut(val, len) }
    }

    /// The string, handed out: one owner, the caller's.
    #[must_use]
    pub fn finish(self) -> Str {
        self.0
    }

    /// `ZSTR_LEN(s) = len` and the NUL after it: the length cut where
    /// the allocation stands, which keeps its size and its bin, as
    /// php's `deflate_add` hands its string out. A `len` past the
    /// string's is its own length.
    pub fn set_len(&mut self, len: usize) {
        let len = len.min(self.len());
        let header = self.0.0;
        // SAFETY: this is the string's one name, so the header is
        // written through it alone.
        unsafe { (*header.as_ptr()).len = len };
        self.0.forget_hash();
        // SAFETY: the byte after `len` is inside the allocation, which
        // holds at least the old length and its NUL.
        let end = unsafe { header.as_ptr().cast::<u8>().add(HEADER + len) };
        // SAFETY: `end` is the allocation's NUL, written once.
        unsafe { end.write(0) };
    }

    /// `zend_string_extend(s, len, 0)` over `mm`: the allocation
    /// `erealloc`ed to a string of `len` bytes, grown where it stands
    /// where its bin or its pages allow and moved otherwise, the bytes
    /// written so far kept and the new ones zero, the NUL after them
    /// written and the hash forgotten. A `len` under the string's is its
    /// own length.
    pub(crate) fn extend(self, mm: &mut Mm, len: usize) -> Result<FreshStr, Fatal> {
        self.grow(mm, len, Str::reallocation_size)
    }

    /// The buffer of a `smart_str` grown to hold `len` bytes,
    /// `smart_str_erealloc`'s `ferealloc2(str->s, len + _ZSTR_HEADER_SIZE + 1, ...)`
    /// (`Z/Zend/zend_smart_str.c:44`): as [`FreshStr::extend`], but asking
    /// for the header and `len` with its NUL, not `zend_string_extend`'s
    /// sum.
    pub(crate) fn erealloc(self, mm: &mut Mm, len: usize) -> Result<FreshStr, Fatal> {
        self.grow(mm, len, Str::allocation_size)
    }

    /// [`FreshStr::extend`] and [`FreshStr::erealloc`], the allocation
    /// asked for as `size` of the new length says.
    fn grow(self, mm: &mut Mm, len: usize, size: fn(usize) -> usize) -> Result<FreshStr, Fatal> {
        let old = self.len();
        let len = len.max(old);
        // SAFETY: this is the string's one name and the request's own
        // allocation, its header and first `old` bytes initialised.
        let at = unsafe { mm.realloc(self.0.address(), HEADER + old, size(len)) }?;
        let header = at.cast::<StrHeader>();
        // SAFETY: the header moved with the bytes, and nothing else names
        // the allocation.
        unsafe { (*header.as_ptr()).len = len };
        let s = Str(header);
        s.forget_hash();
        // SAFETY: the allocation holds the header and `len` bytes, so
        // the byte after the old ones is inside it.
        let grown = unsafe { at.as_ptr().add(HEADER + old) };
        // SAFETY: from `grown` on the allocation holds the new bytes and
        // the NUL, zeroed so every byte the holder may read is
        // initialised.
        unsafe { grown.write_bytes(0, len - old + 1) };
        Ok(FreshStr(s))
    }

    /// `zend_string_truncate(s, len, 0)` over `mm`: the allocation
    /// `erealloc`ed to a string of `len` bytes, in place where the size
    /// stays in its bin and moved where it falls to a smaller one, the
    /// length set, the NUL after it written and the hash forgotten. A
    /// `len` past the string's is its own length.
    pub(crate) fn truncate(self, mm: &mut Mm, len: usize) -> Result<FreshStr, Fatal> {
        let len = len.min(self.len());
        // SAFETY: this is the string's one name and the request's own
        // allocation, its header and first `len` bytes initialised.
        let at =
            unsafe { mm.realloc(self.0.address(), HEADER + len, Str::reallocation_size(len)) }?;
        let header = at.cast::<StrHeader>();
        // SAFETY: the header moved with the bytes, and nothing else names
        // the allocation.
        unsafe { (*header.as_ptr()).len = len };
        let s = Str(header);
        s.forget_hash();
        // SAFETY: the byte after the `len` is inside the allocation.
        let end = unsafe { at.as_ptr().add(HEADER + len) };
        // SAFETY: `end` is the allocation's NUL, php's, written once.
        unsafe { end.write(0) };
        Ok(FreshStr(s))
    }
}

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
    /// Where in `zend_string` its length is, `ZSTR_LEN`: the bytes
    /// without the NUL, a `size_t`. What the compiled tier reads a
    /// string's length through (`jit-design.md` §4.3).
    pub const LEN_AT: usize = std::mem::offset_of!(StrHeader, len);
    /// Where its bytes begin, `ZSTR_VAL`: past the header,
    /// `_ZSTR_HEADER_SIZE`.
    pub const VAL_AT: usize = HEADER;

    /// The bytes one allocation of a string of `len` bytes takes:
    /// php's `ZEND_MM_ALIGNED_SIZE(_ZSTR_STRUCT_SIZE(len))`, which is what
    /// makes `memory_get_usage()` move by what php's moves by.
    ///
    /// A copy of part of a string is `zend_string_init_fast` of that
    /// many bytes, which php-8.5.12 sizes as [`Str::reallocation_size`],
    /// so `substr` moves `memory_get_usage()` by that rounded up to its
    /// bin, a `run` of whole pages past 3072 bytes, and by nothing
    /// for a span of one byte, the interned character, or one that is the
    /// whole string, which is the argument's own (#2661). A constant
    /// string is interned and its copy one more name for it, so neither
    /// moves it; a string a native makes, `strtoupper`'s or an integer's,
    /// is the request's. The program writes its first line
    /// before it measures, since the first byte a request writes holds
    /// the thirty-two of the default mimetype.
    ///
    /// ```php
    /// echo "copied, moved\n"; $big = str_repeat('0123456789', 1000); $s = $t = $u = $v = null;
    /// foreach ([[0, 1], [0, 7], [0, 10], [5, 100], [0, 3000], [0, 5000], [1, 9999], [0, 10000], [-10, null]] as [$o, $l]) {
    ///     $m = memory_get_usage(); $s = substr($big, $o, $l); echo $o, ',', $l ?? 'null', ' ', memory_get_usage() - $m, "\n"; $s = null;
    /// }
    /// $m = memory_get_usage(); $s = 'interned'; $t = $s; echo 'literal ', memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $u = strtoupper($s); echo 'strtoupper ', memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $v = (string) 1234567; echo 'integer ', memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect capture
    /// copied, moved
    /// 0,1 0
    /// 0,7 40
    /// 0,10 48
    /// 5,100 160
    /// 0,3000 3072
    /// 0,5000 8192
    /// 1,9999 12288
    /// 0,10000 0
    /// -10,null 48
    /// literal 0
    /// strtoupper 40
    /// integer 32
    /// ```
    pub(crate) const fn allocation_size(len: usize) -> usize {
        (HEADER + len + 1).next_multiple_of(8)
    }

    /// The bytes a string of the request's grown or cut to `len` bytes
    /// where it stands asks `erealloc` for: php-8.5.12's
    /// `zend_string_realloc`, `_extend` and `_truncate`, which call
    /// `safe_fperealloc(s, 1, len, ZEND_MM_ALIGNED_SIZE(_ZSTR_STRUCT_SIZE(0)))`
    /// (`Z/Zend/zend_string.h:279`, `:311`, `:343`): `len` past the
    /// aligned header of the empty string, up to seven bytes more than
    /// [`Str::allocation_size`] of the same `len`, which the bins then
    /// round. The same sum is `zend_string_finit`'s,
    /// `zend_string_safe_falloc(len, 1, 0)`.
    pub(crate) fn reallocation_size(len: usize) -> usize {
        // A length whose sum overflows asks for everything, which the
        // allocator refuses as php's limit does.
        Sizing::Finit.bytes(len).unwrap_or(usize::MAX)
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

    /// Lay a string of `len` bytes down at `at`, an allocation of
    /// [`Str::allocation_size`] bytes, and hand its bytes to `fill` to
    /// write once: php's `zend_string_alloc` with the `memcpy` or
    /// `memset` the caller does into `ZSTR_VAL`. The bytes are zero
    /// before `fill` sees them, so a filler that writes fewer leaves NULs.
    ///
    /// # Safety
    ///
    /// `at` is writable for [`Str::allocation_size`] of `len` bytes and
    /// holds nothing else.
    pub(crate) unsafe fn init_with(
        at: NonNull<u8>,
        len: usize,
        flags: u32,
        fill: impl FnOnce(&mut [u8]),
    ) -> Str {
        let header: NonNull<StrHeader> = at.cast();
        // SAFETY: the caller's allocation is at least the header and the
        // bytes, and nothing else holds it.
        unsafe {
            header.as_ptr().write(StrHeader {
                gc: Refcounted::new(ZvalType::String, gc::NOT_COLLECTABLE | flags),
                h: Cell::new(0),
                len,
            });
        }
        // SAFETY: the bytes and the NUL sit right after the header,
        // inside the same allocation the caller gave.
        let val = unsafe { at.as_ptr().add(HEADER) };
        // SAFETY: `val` is writable for `len + 1` bytes, the bytes and
        // php's NUL after them.
        unsafe { val.write_bytes(0, len + 1) };
        // SAFETY: the `len` bytes were just written, and nothing else
        // names the allocation while `fill` writes them.
        fill(unsafe { std::slice::from_raw_parts_mut(val, len) });
        Str(header)
    }

    /// `zend_string_extend(s, ZSTR_LEN(s) + ZSTR_LEN(tail), 0)` and the
    /// `memcpy` of `tail` after the old bytes, the arm of
    /// `concat_function` (`Z/Zend/zend_operators.c:2125-2155`) that writes
    /// back into the left operand, and of `ZEND_CONCAT`'s where the left
    /// is a temporary (`Z/Zend/zend_vm_def.h:407-418`). The caller hands
    /// one owner of `s` over and is handed one owner of what comes back.
    ///
    /// A string of the request's with that one owner and nobody else's
    /// is `perealloc`ed ([`Mm::realloc`]), which keeps the slot where
    /// the new size falls in the same bin and the `run` where the pages
    /// after it are free, so a loop of appends copies what it appends
    /// and the old bytes only when a block moves. Any other string,
    /// interned or shared, is `zend_string_alloc` of the new length with
    /// the old bytes copied in, and the caller's owner of it given up
    /// (`GC_DELREF`); the other holders see it as it was, and a string
    /// a walk pins is copied the same way. The hash is
    /// forgotten, and the UTF-8 verdict kept where both sides had it
    /// (`ZSTR_GET_COPYABLE_CONCAT_PROPERTIES_BOTH`). `tail` may be `s`
    /// itself, `$s .= $s`, whose bytes are then read where the realloc
    /// left them. The `Err` is the limit or the system refusing the
    /// size, with `s` standing as it was.
    pub(crate) fn append(mm: &mut Mm, s: Str, tail: Str) -> Result<Str, Fatal> {
        let old = s.len();
        let add = tail.len();
        // `ZEND_CONCAT`'s own check (`Z/Zend/zend_vm_def.h:410-412`);
        // `concat_function` throws before it asks.
        let Some(len) = old.checked_add(add) else {
            return Err(Fatal::error("Integer overflow in memory allocation"));
        };
        let utf8 = s.is_valid_utf8() && tail.is_valid_utf8();
        let same = s.is_same(tail);
        // A pinned string is named by the walk that pinned it, which
        // holds no count (`Heap::pin_block`): it is copied, as a shared
        // one is, and only its bytes wait for the pin, so the block the
        // pin names never moves.
        let grown =
            if !s.is_interned() && !s.is_persistent() && s.refcount() == 1 && !s.gc().pinned() {
                // SAFETY: `s` is the request's, its one owner is the
                // caller's, handed here, and no walk pins it, so nothing
                // names the allocation but this; its header, bytes and NUL
                // are initialised.
                let at = unsafe {
                    mm.realloc(s.address(), HEADER + old + 1, Str::reallocation_size(len))
                }?;
                Str(at.cast())
            } else {
                let at = mm.alloc(Str::allocation_size(len))?;
                // SAFETY: a fresh allocation of the size a string of `len`
                // bytes takes, which the `old` bytes of `s` fit in.
                let copy = unsafe { Str::init(at, s.bytes(), 0) };
                // The last owner of a pinned string given up: its free
                // counted now, as `Heap::give_back` counts it, and its
                // bytes kept for the pin.
                if !s.is_interned() && !s.is_persistent() && s.delref() == 0 {
                    mm.retire(s.address());
                }
                copy
            };
        let from = if same { grown } else { tail };
        // SAFETY: the bytes of a live string sit right after its header.
        let src = unsafe { from.0.as_ptr().cast::<u8>().add(HEADER) };
        // SAFETY: `grown` is an allocation of `len` bytes whose first
        // `old` are the old string's and whose one owner is ours, and
        // `src` is readable for `add` bytes: `tail`'s own, or `grown`'s
        // head, before byte `old`, when the tail was the string that
        // moved.
        unsafe { grown.append_at(old, src, add) };
        if utf8 {
            grown.mark_valid_utf8();
        }
        Ok(grown)
    }

    /// The `memcpy(ZSTR_VAL(str) + old, src, add)` and the NUL after it
    /// of an append, and `ZSTR_LEN(str) = old + add`, with the hash and
    /// the UTF-8 verdict forgotten as `zend_string_forget_hash_val`
    /// forgets them.
    ///
    /// # Safety
    ///
    /// `self` is a string of one owner, the caller's, whose allocation
    /// holds [`Str::allocation_size`] of `old + add` bytes with its first
    /// `old` bytes initialised; `src` is readable for `add` bytes that
    /// lie wholly before byte `old` of this string or in another
    /// allocation.
    unsafe fn append_at(self, old: usize, src: *const u8, add: usize) {
        let header = self.0.as_ptr();
        // SAFETY: the caller's contract: the header is initialised and
        // no borrow of it or of the bytes is live.
        unsafe { (*header).len = old + add };
        self.forget_hash();
        // SAFETY: the bytes sit right after the header, and the tail and
        // its NUL end inside the allocation.
        let val = unsafe { header.cast::<u8>().add(HEADER + old) };
        // SAFETY: `src` is readable for `add` bytes, `val` writable for
        // them, and the two do not overlap by the caller's contract.
        unsafe { val.copy_from_nonoverlapping(src, add) };
        // SAFETY: the NUL php keeps is the byte after the tail, the
        // allocation's last.
        let end = unsafe { val.add(add) };
        // SAFETY: `end` is inside the allocation, as above.
        unsafe { end.write(0) };
    }

    /// Where the string sits, for the heap that frees it.
    pub(crate) const fn address(self) -> NonNull<u8> {
        self.0.cast()
    }

    /// The header.
    const fn header(&self) -> &StrHeader {
        // SAFETY: a `Str` names a string that was laid down by `init` and
        // is live while its owner holds it, which is the same contract
        // php's `zend_string *` carries.
        unsafe { self.0.as_ref() }
    }

    /// `zend_refcounted_h gc`: the count and the flags.
    pub(crate) fn gc(self) -> Counted {
        // SAFETY: the block starts with its header, and stands while
        // its owner holds the handle, as `header` reads it.
        unsafe { Counted::new(self.address()) }
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
    /// php's own loop, the one the bench of #156 runs; the pattern admits
    /// the one string of a thousand `dax` and nothing else:
    ///
    /// ```php
    /// for ($i = 0; $i < 1000; $i++) { echo 'dax'; }
    /// ```
    /// ```expect exit=0
    /// %r(?:dax){1000}%r
    /// ```
    ///
    /// A debug build reads no bytes of a string the heap has freed: the
    /// read is refused, recorded for the line that asked, and answers an
    /// empty slice ([`Str::len`]).
    #[must_use]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the borrow is of this name, which names a live string
        // while its owner holds it, the contract of every `Str`.
        unsafe { self.bytes_held() }
    }

    /// [`Str::bytes`] borrowed for as long as whatever holds this name
    /// is, a zval's slot among them ([`crate::Zval::str_bytes`]).
    ///
    /// # Safety
    ///
    /// The string stays live for `'a`: the bytes live while the string
    /// does, the contract php's `ZSTR_VAL` carries, and not while the
    /// name `self` does.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) unsafe fn bytes_held<'a>(self) -> &'a [u8] {
        let len = self.len();
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
    ///
    /// A debug build reads no length of a string the heap has freed,
    /// whose header's hash and length the freed-block mark stands over:
    /// the read is refused and recorded in the heap's sensed record for
    /// the line that asked, as a refcount move on a freed block is
    /// ([`crate::gc::refuses_freed`]), and answers zero, so a stale read
    /// ends the request at the engine's next sensing point rather than
    /// building a slice of the mark's length (#2051).
    #[must_use]
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn len(&self) -> usize {
        #[cfg(debug_assertions)]
        if crate::gc::refuses_freed(
            self.address(),
            crate::sensed::Use::Read,
            std::panic::Location::caller(),
        ) {
            return 0;
        }
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
        self.cache_hash();
        self.header().h.get()
    }

    /// `zend_inline_hash_func` over bytes no string holds: the hash
    /// [`Str::hash`] caches for a string of these bytes, so a table
    /// filed by it finds a row by a string's cached hash or by the bytes
    /// alike, as `zend_hash_find` and `zend_hash_str_find` find one
    /// bucket.
    #[must_use]
    pub fn hash_of(bytes: &[u8]) -> u64 {
        hash_func(bytes)
    }

    /// The hash written to the cache where it is unset, and none returned:
    /// what php's `zend_add_interned_string` asks of `zend_string_hash_val`
    /// before it publishes a string, so that a string a second thread can
    /// name is never written again.
    pub(crate) fn cache_hash(self) {
        let header = self.header();
        if header.h.get() == 0 {
            header.h.set(hash_func(self.bytes()));
        }
    }

    /// The hash the header caches, zero while none was taken: a read
    /// that writes nothing, which is how a shared table probes its
    /// strings.
    pub(crate) fn cached_hash(self) -> u64 {
        self.header().h.get()
    }

    /// One byte of the string written where it stands, the
    /// `Z_STRVAL_P(str)[offset] = c` of `zend_assign_to_string_offset`
    /// (`Z/Zend/zend_execute.c`): php writes a string offset into the
    /// allocation it already has whenever the string has one owner, and
    /// allocates nothing, which is what `memory_get_peak_usage()` reads.
    /// The hash and the UTF-8 verdict are forgotten with it, as php's
    /// `zend_string_forget_hash_val` there does.
    ///
    /// Only the one owner of a string the request holds may write one:
    /// an interned string is a table's and every thread reads it, a
    /// persistent one is the process's, and a shared one is separated
    /// first (`SEPARATE_STRING`). Nothing happens where `at` is past the
    /// bytes, which is the caller having failed to extend it first.
    pub fn put_byte(self, at: usize, byte: u8) {
        if self.is_interned() || self.is_persistent() || at >= self.len() {
            return;
        }
        self.forget_hash();
        // SAFETY: the bytes sit right after the header, inside the same
        // allocation, and `at` is one of them.
        let slot = unsafe { self.0.as_ptr().cast::<u8>().add(HEADER + at) };
        // SAFETY: the caller holds the string's one owner, so no borrow
        // of [`Str::bytes`] is live across this write.
        unsafe { slot.write(byte) };
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
    pub const fn is_interned(&self) -> bool {
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

    /// `ZSTR_HAS_CE_CACHE` and the offset `GC_REFCOUNT` holds beside it
    /// (`Z/Zend/zend_types.h:885`): where in a request's map table the
    /// class this name last found stands, for a table's string that names
    /// a class ([`Strings::naming_classes`], [`Pending::class_name`]),
    /// and none for any other.
    #[must_use]
    pub fn ce_cache(&self) -> Option<u32> {
        let gc = &self.header().gc;
        (gc.flags() & CLASS_NAME_MAP_PTR != 0).then(|| gc.refcount())
    }

    /// `zend_alloc_ce_cache`'s last two lines (`Z/Zend/zend.c:2151`):
    /// `IS_STR_CLASS_NAME_MAP_PTR` and the offset written, on a string
    /// nothing else names yet,
    /// so no thread reads the header while it is written.
    fn alloc_ce_cache(self, offset: u32) {
        let gc = &self.header().gc;
        gc.set_refcount(offset);
        gc.add_flags(CLASS_NAME_MAP_PTR);
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
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn addref(&self) {
        if !self.is_interned() {
            self.gc().addref();
        }
    }

    /// `zend_string_delref`: one owner fewer, and what is left. An
    /// interned string keeps its one.
    #[cfg_attr(debug_assertions, track_caller)]
    pub(crate) fn delref(self) -> u32 {
        if self.is_interned() {
            return 1;
        }
        self.gc().delref()
    }

    /// The payload word of a zval naming this string: the pointer, as
    /// php stores it, with its provenance.
    pub(crate) const fn payload(self) -> crate::zval::Word {
        crate::zval::Word::pointer(self.0)
    }

    /// The string a zval's payload word names.
    ///
    /// # Safety
    ///
    /// The word came from [`Str::payload`] for a string that is still
    /// live.
    pub(crate) unsafe fn from_payload(payload: crate::zval::Word) -> Option<Str> {
        // SAFETY: the word holds the pointer `payload` stored, by the
        // contract.
        unsafe { payload.as_pointer::<StrHeader>() }.map(Str)
    }
}

// SAFETY: a `Str` is the pointer php's `zend_string *` is: a request's
// string moves with the request that owns it, whole, to the thread that
// serves it (design §5.14), and an interned one is immutable and freed
// by its table alone. What keeps it off `Sync` is the refcount `Cell`,
// which two threads must never touch at once, and that stays.
unsafe impl Send for Str {}

/// Two strings are the one string when their bytes are, which is php's
/// `zend_string_equals`; identity is what interning is for.
impl crate::Heap {
    /// `smart_str_erealloc` of a buffer ([`FreshStr::erealloc`]): grown to
    /// `len` bytes as `erealloc2` grows it, the new bytes zero.
    pub fn string_erealloc(&mut self, s: FreshStr, len: usize) -> Result<FreshStr, Fatal> {
        s.erealloc(&mut self.mm, len)
    }

    /// A request-scoped string of `bytes`, php-8.5.12's
    /// `zend_string_finit(str, len, 0, ...)`, which is
    /// `zend_string_safe_falloc(len, 1, 0)` and the copy
    /// (`Z/Zend/zend_string.h:215-225`): one allocation of
    /// [`Str::reallocation_size`] bytes, `len` past the aligned header of
    /// the empty string, where [`crate::Heap::string`], php's
    /// `zend_string_init`, asks for the header and `len` rounded
    /// together. A string of one byte is allocated here too; the
    /// interned ones `zend_string_init_fast` hands out for the empty
    /// string and one byte are the caller's.
    pub fn string_finit(&mut self, bytes: &[u8]) -> Result<Str, Fatal> {
        self.string_sized(bytes, Sizing::Finit)
    }
}

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

/// Which of php-8.5.12's two sizes a string of the request's is laid at
/// for its length, the one thing about a string php's constructors and
/// its in-place reallocations disagree on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Sizing {
    /// `zend_string_alloc`'s and `zend_string_init`'s: the header, the
    /// bytes and the NUL rounded together,
    /// `ZEND_MM_ALIGNED_SIZE(_ZSTR_STRUCT_SIZE(len))` ([`crate::Heap::string`]).
    #[default]
    Alloc,
    /// `zend_string_finit`'s, and what `zend_string_realloc`, `_extend`
    /// and `_truncate` grow or cut a string to where it stands: `len`
    /// past the empty string's aligned header (`Z/Zend/zend_string.h:217`,
    /// `:279`), up to seven bytes more ([`crate::Heap::string_finit`]). The
    /// parser's `.` of two written values grows its left one so
    /// (`zend_ast_create_concat_op`, `Z/Zend/zend_ast.c:489`), and a name
    /// the compile prints with `zend_strpprintf` is trimmed so by
    /// `smart_str_extract` (`Z/Zend/zend_smart_str.h:119`).
    Finit,
    /// `zend_string_safe_alloc(n, m, l, 0)`'s (`Z/Zend/zend_string.h:177`):
    /// the product `n * m` past the aligned header of a string of the
    /// `l` bytes left, where the string's length is `n * m + l`. That is
    /// not [`Sizing::Alloc`] of the same length: `pdo_mysql`'s quoter asks
    /// for `safe_alloc(2, 1, 3)`, 34 bytes and the 40-byte bin, where a
    /// 5-byte string takes 30 and the 32-byte bin.
    SafeAlloc { n: usize, m: usize },
}

impl Sizing {
    /// The bytes a string of `len` bytes laid at this size asks the
    /// allocator for. [`Sizing::Finit`] is `zend_string_safe_falloc(len,
    /// 1, 0)`, so it is [`Sizing::SafeAlloc`] of `len` and 1 through the
    /// same sum. A product past `len`, which includes one that overflows,
    /// and a sum that overflows are php's `Possible integer overflow in
    /// memory allocation` (`ZEND_MM_SAFE_ADDRESS_GUARDED`), the sentence
    /// the `Err` carries for the heap to refuse or end on before anything
    /// is asked for. Its third number is the aligned header the product is
    /// added to, the empty string's where no `l` is left.
    #[inline] // `_ZSTR_STRUCT_SIZE` and `ZEND_MM_SAFE_ADDRESS_GUARDED`, in the line of every ask
    pub fn bytes(self, len: usize) -> Result<usize, String> {
        let (n, m) = match self {
            Sizing::Alloc => return Ok(Str::allocation_size(len)),
            Sizing::Finit => (len, 1),
            Sizing::SafeAlloc { n, m } => (n, m),
        };
        let product = n.checked_mul(m).filter(|product| *product <= len);
        let header = Str::allocation_size(product.map_or(0, |product| len - product));
        product
            .and_then(|product| product.checked_add(header))
            .ok_or_else(|| overflowed(n, m, header))
    }
}

/// [`Sizing::bytes`]'s refusal, out of the line of every size it answers.
#[cold]
#[inline(never)]
fn overflowed(n: usize, m: usize, header: usize) -> String {
    format!("Possible integer overflow in memory allocation ({n} * {m} + {header})")
}

/// A home for interned strings: php's `zend_new_interned_string` behind
/// its two handlers (`Z/Zend/zend_string.c:363-373`), which the compiler
/// and the engine call without knowing which home answers.
///
/// [`Strings`], an engine's, interns on the system allocator for the life
/// of the engine. A request's compile of a unit no cache keeps installs
/// opcache's handler instead, `accel_new_interned_string_for_php`, which
/// interns nothing and hands back a string out of the request's memory
/// that the unit owns. Both answer from the frozen table first, so the
/// empty string and a string of one byte are allocated nowhere at all.
pub trait Intern {
    /// The one string for these bytes in this home, interned on the first
    /// ask, and the fatal the allocator raised when there is no memory
    /// for it.
    fn intern(&mut self, bytes: &[u8]) -> Result<Str, Fatal> {
        self.intern_marked(bytes, false, Sizing::Alloc)
    }

    /// [`Intern::intern`] of bytes the asker knows to be valid UTF-8 when
    /// `valid_utf8` says so, php's `zend_new_interned_string` over a
    /// string carrying `IS_STR_VALID_UTF8`: a string this ask lays down
    /// is marked before the table takes it, and one the home already
    /// holds is handed back as it stands, with its own flags, as
    /// `zend_interned_string_ht_lookup` finds it. A string laid on a
    /// request's heap is laid at `sizing`'s size; a home that lays none
    /// there has no use for it.
    fn intern_marked(
        &mut self,
        bytes: &[u8],
        valid_utf8: bool,
        sizing: Sizing,
    ) -> Result<Str, Fatal>;

    /// [`Intern::intern`] of a string laid as a string of `room` bytes and
    /// cut to `bytes` where it stands (`ZSTR_LEN(s) = len`), which keeps
    /// the block's bin: `__DIR__`, which `zend_try_ct_eval_magic_const`
    /// lays at the length of the whole file name before `zend_dirname`
    /// shortens it. A home that lays nothing on a request's heap interns
    /// the bytes as any other.
    fn intern_in(&mut self, bytes: &[u8], room: usize) -> Result<Str, Fatal> {
        let _ = room;
        self.intern_marked(bytes, false, Sizing::Alloc)
    }

    /// [`Intern::intern_marked`] of a string the asker alone will hold
    /// and gives back once it has read it ([`Intern::give_back`]): a
    /// string a fold makes that the fold above it consumes, as
    /// `zend_eval_const_expr` destroys a child's zval once the node it
    /// folds into stands. The [`Made`] is the claim; dropped instead of
    /// given back, the string is kept as any other this home made.
    fn intern_made(
        &mut self,
        bytes: &[u8],
        valid_utf8: bool,
        sizing: Sizing,
    ) -> Result<Made, Fatal> {
        (self.intern_marked(bytes, valid_utf8, sizing)).map(|s| Made(Zval::string(s)))
    }

    /// The next of the names a compile hands over once its unit is
    /// whole, in the order of the compiler's walk of what they name
    /// (`php_compile::named`): `name`, the string this home gave for it,
    /// where the compile asked for one. A home that makes a string each
    /// time it is asked keeps it as that declaration's own, so the
    /// declaration costs no string more for it, and what it is the name
    /// of is told by its place in the walk, never by its bytes.
    fn hand_name(&mut self, name: Option<Str>) {
        let _ = name;
    }

    /// `made` read for the last time: the string this home made for the
    /// claim gone, where the home made one for the asker alone, and
    /// nothing where the string is the table's, as an interned string is
    /// never freed by a release. A home that keeps every string it makes
    /// to its own end gives back nothing.
    fn give_back(&mut self, made: Made) {
        let _ = made;
    }

    /// One more owner of `value`, a value another place owns that a
    /// compile hands on where the program wrote something else, kept for
    /// the unit as what this home made is kept: whether this home holds
    /// it. php's compile takes that owner wherever it copies a counted
    /// value it did not make into what it compiles (`ZVAL_COPY_OR_DUP`,
    /// `ZVAL_COPY`): a constant the request defined that
    /// `zend_try_ct_eval_const` substitutes for its name, and an element
    /// a fold reads out of an array the request owns. A value nothing
    /// counts owners of, an interned string or an immutable array, is
    /// held by any home and takes nothing; a counted one only by a home of
    /// the request's whose values it is, and where the home cannot the
    /// asker leaves what was written standing, the fetch or the node.
    fn hold(&mut self, value: Zval) -> bool {
        !value.is_refcounted()
    }

    /// [`Intern::hold`] of a string or an array as a claim the asker
    /// holds alone and gives back once it has read it
    /// ([`Intern::give_back`]), as a fold over the substituted constant,
    /// or over an element it read out of an array, destroys its copy;
    /// none where this home cannot hold it.
    fn hold_made(&mut self, value: Zval) -> Option<Made> {
        self.hold(value).then_some(Made(value))
    }

    /// A constant array of a unit in this home (design §5.3): the
    /// entries in this order, the keys strings this home made or
    /// integers. An engine's is php's `IS_ARRAY_IMMUTABLE`, no owner
    /// counted and living as long as the home does; a request's compile
    /// makes one its unit owns, laid at the `size` php's compile inits it
    /// with (`array_init_size`, `zend_hash_init`), which counts what was
    /// written and not what stands, and which a home that persists the
    /// array has no use for. What a `switch` or a `match` jump table is
    /// (`zend_compile_switch`, `zend_compile_match`).
    fn constant_array(&mut self, entries: &[(Key, Zval)], size: usize) -> Result<Arr, Fatal>;

    /// [`Intern::constant_array`] of an array the asker alone holds and
    /// gives back once it has read it ([`Intern::give_back`]), each entry
    /// one owner the array took over: the array `zend_try_ct_eval_array`
    /// made for a node, which `zend_eval_const_expr` destroys with its
    /// strings once a fold over it stands (`zval_ptr_dtor`).
    fn constant_array_made(&mut self, entries: &[(Key, Zval)], size: usize) -> Result<Made, Fatal> {
        (self.constant_array(entries, size)).map(|a| Made(Zval::array(a)))
    }

    /// Whether what this home lays from here on is interned, php's
    /// `zend_new_interned_string` asked of it, and whether it was until
    /// now: `false` while a compile makes the strings php's scanner and
    /// `zend_eval_const_expr` lay and nothing interns, an array's
    /// elements and keys, a `static`'s initial value, an attribute's
    /// argument and a jump table's key, where a string an opline names,
    /// a declaration's value and a name are interned. A home that interns
    /// every string, a cached unit's, keeps on and answers `true`.
    fn interning(&mut self, on: bool) -> bool {
        let _ = on;
        true
    }
}

/// One string or constant array an [`Intern`] home made for an asker
/// that holds it alone and hands it back with [`Intern::give_back`] once
/// it has read it. Only [`Intern::intern_made`], [`Intern::hold_made`]
/// and [`Intern::constant_array_made`] make one, and it is neither copied
/// nor cloned, so a value another place made, a constant operand's, is
/// never a claim to hand back. Its [`Made::value`] is a view: the asker
/// gives the claim back only once nothing it put the view in reads it
/// again.
#[derive(Debug)]
pub struct Made(Zval);

impl Made {
    /// The string or the array, a view: the claim stays the holder's.
    #[must_use]
    pub fn value(&self) -> Zval {
        self.0
    }
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
    table: Permanent,
    /// Where the strings this table lays down itself are laid, one after
    /// another, as opcache's `accel_copy_permanent_strings` lays php's
    /// permanent ones in its buffer, and not one system allocation each;
    /// a string a compile made ([`Strings::publish`]) stays where the
    /// compile laid it.
    bump: Bump,
    /// The constant arrays the units published into this engine hold,
    /// freed with the table.
    arrays: Vec<Arr>,
    /// Strings a unit holds whose bytes the table held already when the
    /// unit was published ([`Strings::publish`]), kept as long as the
    /// table and not counted: none while a compile holds the table from
    /// its first string to its publication.
    shadows: Vec<Str>,
    /// The strings [`Strings::intern_known`] interned, by position:
    /// php's `zend_known_strings`, which `ZSTR_KNOWN(id)` indexes.
    known: Vec<Str>,
    /// The class caches handed out, the next one's offset: php's
    /// `CG(map_ptr_last)` as `zend_alloc_ce_cache` moves it, counting
    /// from where the table this one was laid beside stopped
    /// ([`Strings::beside`]).
    ce_caches: u32,
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
        Strings::with_capacity(0)
    }

    /// An empty table with room for `strings` strings before its slots
    /// are laid again, as `zend_hash_init` takes the size it starts at:
    /// a table whose strings are known in number before they are
    /// interned is laid once, and their bytes in one chunk of its bump
    /// laid at [`BYTES_PER_STRING`] a string.
    #[must_use]
    pub fn with_capacity(strings: usize) -> Strings {
        Strings {
            table: Permanent::with_capacity(strings),
            bump: Bump::with_capacity(strings.saturating_mul(BYTES_PER_STRING)),
            arrays: Vec::new(),
            shadows: Vec::new(),
            known: Vec::new(),
            ce_caches: 0,
        }
    }

    /// This table, empty, holding each of `classes`, a class's name,
    /// interned first with a class cache of its own ([`Str::ce_cache`]),
    /// which `zend_register_internal_class_ex` allocates for each class
    /// the engine registers at startup (`Z/Zend/zend_API.c:3543`). A name
    /// the frozen table holds takes none, as php's one-byte names are
    /// permanent strings no slot is allocated on once startup is done.
    #[must_use]
    pub fn naming_classes<'a>(self, classes: impl IntoIterator<Item = &'a [u8]>) -> Strings {
        let mut table = self;
        for name in classes {
            if frozen_string(name).is_some() {
                continue;
            }
            let Ok(s) = table.intern(name) else {
                break;
            };
            // Every string of the table was laid down here, so nothing
            // else names it yet.
            if s.ce_cache().is_none() {
                s.alloc_ce_cache(table.ce_caches);
                table.ce_caches += 1;
            }
        }
        table
    }

    /// An empty table whose class caches are numbered past this one's,
    /// so a request's one map table holds both tables' (`CG(map_ptr_last)`
    /// carried on from startup into what opcache persists).
    #[must_use]
    pub fn beside(&self) -> Strings {
        let mut table = Strings::new();
        table.ce_caches = self.ce_caches;
        table
    }

    /// What a compile made for a unit held for every request entering
    /// the table: opcache's `zend_accel_store_interned_string` over each
    /// string the persisted unit holds, and its constant arrays with it.
    /// A string whose bytes the table holds already stays the unit's,
    /// with the table, uncounted.
    ///
    /// A string the compile named a class's ([`Pending::class_name`])
    /// takes a class cache as it enters, `zend_alloc_ce_cache` on
    /// `ce->name` (`Z/Zend/zend_compile.c:9305`); a shadow takes the one
    /// the table's string of its bytes holds, or one of its own.
    pub fn publish(&mut self, mut pending: Pending) {
        for PendingString { s, class_name, .. } in pending.take_strings() {
            let hash = hash_func(s.bytes());
            let held = self.table.find(hash, s.bytes());
            if class_name {
                // `s` is the compile's alone until it enters the table
                // below, so its header is written before anyone reads it.
                let offset = held.and_then(|held| held.ce_cache()).unwrap_or_else(|| {
                    self.ce_caches += 1;
                    self.ce_caches - 1
                });
                s.alloc_ce_cache(offset);
            }
            if held.is_some() {
                self.shadows.push(s);
            } else {
                self.table.add(hash, s);
            }
        }
        (self.arrays).extend(pending.arrays.drain(..).map(|(a, _)| a));
    }

    /// Intern each of `known` and keep it at its position for
    /// [`Strings::known`], `zend_interned_strings_init`'s loop over
    /// `known_strings` into `zend_known_strings`. A string the table
    /// refuses ends the list there, and asking for it or a later one
    /// answers `None`.
    pub fn intern_known(&mut self, known: &[&[u8]]) {
        for bytes in known {
            match self.intern(bytes) {
                Ok(s) => self.known.push(s),
                Err(_) => return,
            }
        }
    }

    /// `ZSTR_KNOWN(at)`: the string [`Strings::intern_known`] interned at
    /// position `at`, read by index with no hash and no compare.
    #[must_use]
    pub fn known(&self, at: usize) -> Option<Str> {
        self.known.get(at).copied()
    }

    /// How many strings the table holds, which is what says the constant
    /// string two published units share was interned once.
    #[must_use]
    pub fn count(&self) -> usize {
        self.table.count()
    }

    /// The string for these bytes if this table or the frozen one holds
    /// it, php's `zend_interned_string_ht_lookup` over
    /// `interned_strings_permanent`, which `zend_string_init_interned_request`
    /// asks before it interns into the request's own table
    /// (`Z/Zend/zend_string.c:297-327`).
    #[must_use]
    pub fn find(&self, bytes: &[u8]) -> Option<Str> {
        if let Some(frozen) = frozen_string(bytes) {
            return Some(frozen);
        }
        self.table.find(hash_func(bytes), bytes)
    }
}

impl Intern for Strings {
    /// php's `zend_string_init_interned_permanent`
    /// (`Z/Zend/zend_string.c:267-280`): the frozen table, then this one,
    /// then a string laid in this table's bump, as opcache's
    /// `accel_new_interned_string` lays it in its buffer, hashed and
    /// flagged before it is published; a string of the engine's, so no
    /// request's size.
    fn intern_marked(&mut self, bytes: &[u8], valid_utf8: bool, _: Sizing) -> Result<Str, Fatal> {
        if let Some(frozen) = frozen_string(bytes) {
            return Ok(frozen);
        }
        let hash = hash_func(bytes);
        if let Some(held) = self.table.find(hash, bytes) {
            return Ok(held);
        }
        let s = laid(&mut self.bump, bytes)?;
        if valid_utf8 {
            s.mark_valid_utf8();
        }
        self.table.add(hash, s);
        Ok(s)
    }

    /// `zend_persist_zval` over a constant array under opcache: one
    /// array on the system allocator, immutable, shared by every request
    /// that serves the unit, freed with the engine.
    fn constant_array(&mut self, entries: &[(Key, Zval)], _: usize) -> Result<Arr, Fatal> {
        let a = persistent_array(entries)?;
        self.arrays.push(a);
        Ok(a)
    }
}

impl Drop for Strings {
    /// `zend_hash_destroy(&interned_strings_permanent)` with `_str_dtor`
    /// as its destructor (`Z/Zend/zend_string.c:66-70`): every string a
    /// compile laid down goes back to the allocator it came from, and the
    /// table's own go with its bump after this. The engine outlives
    /// every request that can name one.
    fn drop(&mut self) {
        for a in self.arrays.drain(..) {
            free_persistent_array(a);
        }
        for s in self.table.strings() {
            if !self.bump.holds(s.address()) {
                free_persistent(s);
            }
        }
        for s in self.shadows.drain(..) {
            free_persistent(s);
        }
    }
}

/// The strings and constant arrays one compile bound for an engine's
/// table has made and not given back, apart from the table until the
/// unit they are for is held ([`Strings::publish`]): opcache's miss
/// compiles on the request and its buffer takes a string only as
/// `zend_accel_store_interned_string` persists the unit that holds it,
/// so a fold's intermediate, freed as the fold goes on, and what a
/// refused compile made never reach it. Each string is the system
/// allocator's, laid down as the table would lay it, and given back
/// once its last claim is and nothing kept it, or with the set where
/// it is dropped unpublished.
#[derive(Default)]
pub struct Pending {
    /// The strings made, one open-addressed table as the engine's is
    /// ([`Permanent`]), each hashed as it is made.
    slots: Slots<PendingString>,
    /// Each constant array beside the claims on it that stand.
    arrays: Vec<(Arr, u32)>,
    /// How many strings the set has made, the next one's place.
    made: u64,
}

/// One string a compile made and the asks it answered that were not
/// given back: a kept ask is never given back, and neither is a claim
/// dropped, so either keeps it.
#[derive(Clone, Copy)]
struct PendingString {
    s: Str,
    asks: u32,
    /// Whether the compile named a class by it ([`Pending::class_name`]).
    class_name: bool,
    /// Its place among the strings the set made, which the table takes
    /// them in ([`Strings::publish`]).
    at: u64,
}

impl Slotted for PendingString {
    fn string(&self) -> Str {
        self.s
    }
}

impl Pending {
    /// The slot of the string of these bytes the set made.
    fn position(&self, hash: u64, bytes: &[u8]) -> Option<usize> {
        (self.slots).position(hash, |held| {
            held.s.cached_hash() == hash && held.s.bytes() == bytes
        })
    }

    /// The string for these bytes the compile made, or one made now,
    /// with one ask more on it.
    pub fn intern(&mut self, bytes: &[u8], valid_utf8: bool) -> Result<Str, Fatal> {
        let hash = hash_func(bytes);
        if let Some(held) = (self.position(hash, bytes)).and_then(|at| self.slots.get_mut(at)) {
            held.asks = held.asks.saturating_add(1);
            return Ok(held.s);
        }
        let s = persistent(bytes)?;
        s.cache_hash();
        if valid_utf8 {
            s.mark_valid_utf8();
        }
        (self.slots).insert(
            hash,
            PendingString {
                s,
                asks: 1,
                class_name: false,
                at: self.made,
            },
        );
        self.made += 1;
        Ok(s)
    }

    /// The string of `name` this set made, where it made one, named a
    /// class's, `zend_alloc_ce_cache(ce->name)` of a class the compile
    /// declared: it takes a class cache as it is published
    /// ([`Strings::publish`]). A name the tables held already takes
    /// none, as a string already shared is never written.
    pub fn class_name(&mut self, name: &[u8]) {
        let held = (self.position(hash_func(name), name)).and_then(|at| self.slots.get_mut(at));
        if let Some(held) = held {
            held.class_name = true;
        }
    }

    /// The slot of `s` where this set made it.
    fn slot_of(&self, s: Str) -> Option<usize> {
        // A string another home made may not carry its hash, and is not
        // written here.
        let hash = match s.cached_hash() {
            0 => hash_func(s.bytes()),
            hash => hash,
        };
        self.slots.position(hash, |held| held.s.is_same(s))
    }

    /// One claim on `z` given back ([`Intern::give_back`]): a string
    /// freed where no other ask stands on it, and an array with each
    /// string and array it holds given back in turn, the one ask on each
    /// it took over as it was laid; nothing for a value this set did not
    /// make.
    pub fn give_back(&mut self, z: Zval) {
        let Some(s) = z.as_str() else {
            let Some(at) =
                (z.as_arr()).and_then(|a| self.arrays.iter().position(|(held, _)| held.is_same(a)))
            else {
                return;
            };
            let asks = self.arrays.get_mut(at).map(|(_, asks)| {
                *asks = asks.saturating_sub(1);
                *asks
            });
            if asks == Some(0) {
                let (a, _) = self.arrays.swap_remove(at);
                for (key, value) in &a {
                    if let Key::Str(key) = key {
                        self.give_back(Zval::string(key));
                    }
                    self.give_back(value);
                }
                free_persistent_array(a);
            }
            return;
        };
        let Some(at) = self.slot_of(s) else {
            return;
        };
        let Some(held) = self.slots.get_mut(at) else {
            return;
        };
        held.asks = held.asks.saturating_sub(1);
        if held.asks == 0 {
            self.slots.remove(at);
            free_persistent(s);
        }
    }

    /// A constant array of the unit (design §5.3), laid as
    /// [`Strings`] lays one and the table's once the unit is published,
    /// with one claim on it: one ask on each string and array this set
    /// made that it holds is the array's from here, as php's array takes
    /// over the zvals `zend_try_ct_eval_array` folded.
    pub fn constant_array(&mut self, entries: &[(Key, Zval)]) -> Result<Arr, Fatal> {
        let a = persistent_array(entries)?;
        if !a.is_same(Arr::empty()) {
            self.arrays.push((a, 1));
        }
        Ok(a)
    }

    /// One ask more on `z` where this set made it ([`Intern::hold`]), a
    /// string or an array a fold hands on out of an array it reads, so
    /// giving that array back leaves it standing.
    pub fn hold(&mut self, z: Zval) {
        if let Some(a) = z.as_arr() {
            if let Some((_, asks)) = self.arrays.iter_mut().find(|(held, _)| held.is_same(a)) {
                *asks = asks.saturating_add(1);
            }
            return;
        }
        let Some(s) = z.as_str() else {
            return;
        };
        if let Some(held) = (self.slot_of(s)).and_then(|at| self.slots.get_mut(at)) {
            held.asks = held.asks.saturating_add(1);
        }
    }

    /// How many strings stand made.
    #[cfg(test)]
    fn count(&self) -> usize {
        self.slots.len()
    }

    /// Every string, taken out of the set in the order it made them,
    /// beside whether it names a class, so a unit's strings take the same
    /// class caches whichever slots the set filed them in.
    fn take_strings(&mut self) -> Vec<PendingString> {
        let mut held = Vec::with_capacity(self.slots.len());
        held.extend(self.slots.take());
        held.sort_unstable_by_key(|held| held.at);
        held
    }
}

impl Drop for Pending {
    /// A set never published: what the compile made, its unit refused,
    /// given back.
    fn drop(&mut self) {
        for (a, _) in self.arrays.drain(..) {
            free_persistent_array(a);
        }
        for held in self.take_strings() {
            free_persistent(held.s);
        }
    }
}

// SAFETY: the table owns its strings and the only other names for them
// are the zvals of the units the engine published, which are read and
// never written; moving the table moves that sole ownership.
unsafe impl Send for Strings {}

// SAFETY: a shared table is only read: `find`, `count` and `known` take
// `&self` and read the slots, the known list and the strings' cached
// hashes and bytes, each string hashed before `Permanent::add` published
// it, so no read writes; growing the table, and laying a string in its
// bump, takes `&mut self`, which excludes every reader.
unsafe impl Sync for Strings {}

impl std::fmt::Debug for Strings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Strings({} interned)", self.count())
    }
}

/// The engine's table of interned strings, php's
/// `interned_strings_permanent` (`Z/Zend/zend_string.c:31`): one
/// `HashTable` of `zend_string *` keyed by the string itself
/// (`zend_init_interned_strings_ht`, `Z/Zend/zend_string.c:78-84`). It is
/// one open-addressed table of slots ([`Slots`]), each the string and
/// nothing beside it, probed from the hash the string's header caches,
/// which is the walk `zend_interned_string_ht_lookup_ex` makes over php's
/// hash slots comparing the hash and then the bytes
/// (`Z/Zend/zend_string.c:132-169`). It is the table opcache holds in
/// shared memory, and nothing counts it, so its slots are on the system
/// allocator and not an [`Arr`], as the request's own [`Table`] is.
#[derive(Default)]
pub(crate) struct Permanent {
    slots: Slots<Str>,
}

impl std::fmt::Debug for Permanent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Permanent({} interned)", self.count())
    }
}

impl Permanent {
    /// A table with room for `strings` before its slots are laid again.
    fn with_capacity(strings: usize) -> Permanent {
        Permanent {
            slots: Slots::with_capacity(strings),
        }
    }

    /// The string here whose bytes are `bytes`, if it is here.
    pub(crate) fn find(&self, hash: u64, bytes: &[u8]) -> Option<Str> {
        (self.slots)
            .position(hash, |s| s.cached_hash() == hash && s.bytes() == bytes)
            .and_then(|at| self.slots.get(at))
    }

    /// Publish `s` under `hash`, php's `zend_add_interned_string`
    /// (`Z/Zend/zend_string.c:173-185`). php takes the hash before it
    /// publishes, so that a table's string is immutable from the moment
    /// a second thread can name it; [`Str::cache_hash`] here is that act.
    pub(crate) fn add(&mut self, hash: u64, s: Str) {
        s.cache_hash();
        self.slots.insert(hash, s);
    }

    /// How many strings are here.
    pub(crate) fn count(&self) -> usize {
        self.slots.len()
    }

    /// Every string here, for the table that frees what it laid down.
    fn strings(&self) -> impl Iterator<Item = Str> {
        self.slots.iter()
    }
}

/// What a slot of [`Slots`] holds: a string, and whatever is kept beside
/// it.
trait Slotted: Copy {
    /// The string the slot is filed under, whose header caches its hash.
    fn string(&self) -> Str;
}

impl Slotted for Str {
    fn string(&self) -> Str {
        *self
    }
}

/// An open-addressed table of strings, probed linearly from the hash
/// each string's header caches: php's `HashTable` without its buckets,
/// as nothing here is ordered by insertion and the key is the value.
/// The slots are one allocation, laid at the size asked for and laid
/// again at twice it only when they would be more than three quarters
/// full; a string leaves by moving the full slots behind it back, so a
/// lookup stops at the first empty slot.
struct Slots<T> {
    slots: Vec<Option<T>>,
    len: usize,
}

impl<T> Default for Slots<T> {
    fn default() -> Slots<T> {
        Slots {
            slots: Vec::new(),
            len: 0,
        }
    }
}

impl<T: Slotted> Slots<T> {
    /// The fewest slots that hold `len` entries at most three quarters
    /// full: a power of two, none for none.
    fn slots_for(len: usize) -> usize {
        if len == 0 {
            return 0;
        }
        len.saturating_add(len.div_ceil(3))
            .next_power_of_two()
            .max(8)
    }

    /// Room for `len` entries before the slots are laid again.
    fn with_capacity(len: usize) -> Slots<T> {
        let mut slots = Vec::new();
        slots.resize_with(Slots::<T>::slots_for(len), || None);
        Slots { slots, len: 0 }
    }

    /// How many entries stand.
    fn len(&self) -> usize {
        self.len
    }

    /// The first slot a hash probes from.
    #[allow(clippy::cast_possible_truncation)] // the mask keeps fewer bits than a word
    fn home(&self, hash: u64) -> usize {
        // The slots are a power of two, so the mask keeps the low bits.
        (hash as usize) & self.slots.len().wrapping_sub(1)
    }

    /// The slot of the entry under `hash` that `matches`, if one is here.
    fn position(&self, hash: u64, matches: impl Fn(T) -> bool) -> Option<usize> {
        if self.slots.is_empty() {
            return None;
        }
        let mask = self.slots.len() - 1;
        let mut at = self.home(hash);
        // Never full, so the probe meets an empty slot.
        while let Some(Some(held)) = self.slots.get(at) {
            if matches(*held) {
                return Some(at);
            }
            at = (at + 1) & mask;
        }
        None
    }

    /// The entry at slot `at`.
    fn get(&self, at: usize) -> Option<T> {
        self.slots.get(at).copied().flatten()
    }

    /// The entry at slot `at`, to change what is kept beside its string.
    fn get_mut(&mut self, at: usize) -> Option<&mut T> {
        self.slots.get_mut(at).and_then(Option::as_mut)
    }

    /// `entry` filed under `hash`, the slots laid again at twice their
    /// number first where it would fill more than three quarters.
    fn insert(&mut self, hash: u64, entry: T) {
        if Slots::<T>::slots_for(self.len + 1) > self.slots.len() {
            self.grow();
        }
        self.place(hash, entry);
        self.len += 1;
    }

    /// `entry` into the first empty slot from its hash's.
    fn place(&mut self, hash: u64, entry: T) {
        let mask = self.slots.len().wrapping_sub(1);
        let mut at = self.home(hash);
        while let Some(slot) = self.slots.get_mut(at) {
            if slot.is_none() {
                *slot = Some(entry);
                return;
            }
            at = (at + 1) & mask;
        }
    }

    /// The slots laid again at twice their number, each entry filed
    /// from its string's cached hash.
    fn grow(&mut self) {
        let doubled = self.slots.len().saturating_mul(2).max(8);
        let mut slots = Vec::new();
        slots.resize_with(doubled, || None);
        let old = std::mem::replace(&mut self.slots, slots);
        for entry in old.into_iter().flatten() {
            self.place(entry.string().cached_hash(), entry);
        }
    }

    /// The entry at slot `at` taken out, and each entry of the full
    /// slots behind it moved back into the gap where a lookup for it
    /// passes it, Knuth's deletion for linear probing (TAOCP 6.4
    /// Algorithm R).
    fn remove(&mut self, at: usize) -> Option<T> {
        let taken = self.slots.get_mut(at)?.take()?;
        self.len -= 1;
        let mask = self.slots.len() - 1;
        let mut gap = at;
        let mut next = (gap + 1) & mask;
        while let Some(Some(entry)) = self.slots.get(next).copied() {
            let home = self.home(entry.string().cached_hash());
            // The entry may fill the gap unless its home lies cyclically
            // in (gap, next], where a probe for it never reaches the gap.
            let stays = if gap <= next {
                gap < home && home <= next
            } else {
                gap < home || home <= next
            };
            if !stays {
                if let Some(slot) = self.slots.get_mut(gap) {
                    *slot = Some(entry);
                }
                if let Some(slot) = self.slots.get_mut(next) {
                    *slot = None;
                }
                gap = next;
            }
            next = (next + 1) & mask;
        }
        Some(taken)
    }

    /// Every entry, in slot order.
    fn iter(&self) -> impl Iterator<Item = T> + '_ {
        self.slots.iter().filter_map(|slot| *slot)
    }

    /// Every entry, taken out, the slots given back.
    fn take(&mut self) -> impl Iterator<Item = T> {
        self.len = 0;
        std::mem::take(&mut self.slots).into_iter().flatten()
    }
}

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
    /// Where the strings are laid, one chunk for the 257, never given
    /// back.
    _bump: Bump,
}

impl Frozen {
    /// The frozen strings laid down, hashed and flagged, in the order
    /// `zend_interned_strings_init` lays them: the empty string, then the
    /// 256 of one byte, those below `0x80` known to be valid UTF-8.
    fn new() -> Frozen {
        // Each string is a header and two bytes rounded to a word.
        let mut bump = Bump::with_capacity(257 * Str::allocation_size(1));
        let empty = froze(&mut bump, b"");
        let mut chars = [None; 256];
        for (i, slot) in chars.iter_mut().enumerate() {
            let byte = [u8::try_from(i).unwrap_or(0)];
            *slot = froze(&mut bump, &byte);
            if byte[0] >= 0x80 {
                continue;
            }
            if let Some(s) = *slot {
                s.mark_valid_utf8();
            }
        }
        Frozen {
            empty,
            chars,
            _bump: bump,
        }
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

/// One frozen string, laid in the frozen table's bump, hashed and
/// marked valid UTF-8 as php marks the empty string, before anything can
/// name it. Nothing frees it: the frozen table is never dropped, as php's
/// permanent one stands for as long as a request can be served.
fn froze(bump: &mut Bump, bytes: &[u8]) -> Option<Str> {
    let s = laid(bump, bytes).ok()?;
    s.cache_hash();
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
        return Err(Fatal::out_of_memory(size));
    };
    // SAFETY: the layout's size is the header and the bytes, never zero.
    let at = unsafe { std::alloc::alloc(layout) };
    let Some(at) = NonNull::new(at) else {
        return Err(Fatal::out_of_memory(size));
    };
    // SAFETY: the allocation is exactly what `init` asks for and holds
    // nothing else.
    Ok(unsafe { Str::init(at, bytes, PERMANENT) })
}

/// What a table lays a string of `bytes` at in its bump, as opcache's
/// `accel_new_interned_string` copies one into its buffer
/// (`Z/ext/opcache/ZendAccelerator.c:515`): interned, persistent and
/// permanent, never freed alone, and gone with the bump.
fn laid(bump: &mut Bump, bytes: &[u8]) -> Result<Str, Fatal> {
    let size = Str::allocation_size(bytes.len());
    let Some(at) = bump.alloc(size) else {
        return Err(Fatal::out_of_memory(size));
    };
    // SAFETY: the bump handed out `size` bytes nobody else holds, aligned
    // to the header's words, which is what `init` asks.
    Ok(unsafe { Str::init(at, bytes, PERMANENT) })
}

/// What [`Strings::with_capacity`] lays its bump's first chunk at for
/// each string it is sized for: the header's 24 bytes and the name, with
/// room past the average for the longer ones and for a count asked short
/// (the bundle's 9,637 startup strings take 427.7 KB at f39c7d0d5, 44
/// bytes each on average). A page of the chunk no string reaches is
/// never touched.
const BYTES_PER_STRING: usize = 80;

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

    /// A string a compile asked for twice stands until both asks are
    /// given back, one asked again after its last is made anew, and what
    /// stands enters the table as the set is published, a string the
    /// table held already left out of its count; a set dropped
    /// unpublished frees what it holds.
    #[test]
    fn a_pending_string_stands_while_an_ask_does_and_enters_the_table_published() {
        let mut table = Strings::new();
        let held = table.intern(b"held already").expect("a table string");
        let mut pending = Pending::default();
        let fold = pending.intern(b"fold", false).expect("a string");
        let again = pending.intern(b"fold", false).expect("the same string");
        assert!(fold.is_same(again));
        pending.give_back(Zval::string(fold));
        assert_eq!(pending.count(), 1);
        pending.give_back(Zval::string(again));
        assert_eq!(pending.count(), 0);
        pending.intern(b"kept", true).expect("a string");
        pending.intern(b"held already", false).expect("a shadow");
        pending.give_back(Zval::string(held));
        table.publish(pending);
        assert_eq!(table.count(), 2);
        assert!(table.find(b"kept").is_some_and(|s| s.is_valid_utf8()));
        assert!(table.find(b"held already").is_some_and(|s| s.is_same(held)));
        let mut refused = Pending::default();
        refused.intern(b"refused", false).expect("a string");
        drop(refused);
        assert!(table.find(b"refused").is_none());
    }

    /// A class's name takes a class cache where the table lays its
    /// string down, the startup table's first and a unit table's
    /// numbered past them; a name already shared takes none, a shadow
    /// takes its table string's, and a string named no class's none.
    #[test]
    fn a_class_name_takes_a_class_cache_where_its_string_is_laid_down() {
        let names =
            Strings::new().naming_classes([&b"stdClass"[..], b"A", b"Exception", b"stdClass"]);
        let std_class = names.find(b"stdClass").and_then(|s| s.ce_cache());
        assert_eq!(std_class, Some(0));
        assert_eq!(names.find(b"A").and_then(|s| s.ce_cache()), None);
        assert_eq!(names.find(b"Exception").and_then(|s| s.ce_cache()), Some(1));
        let mut table = names.beside();
        let shared = table.intern(b"Shared").expect("a table string");
        let mut pending = Pending::default();
        pending.intern(b"Foo", false).expect("a string");
        pending.intern(b"echoed", false).expect("a string");
        pending.intern(b"Shared", false).expect("a shadow");
        pending.class_name(b"Foo");
        pending.class_name(b"Shared");
        pending.class_name(b"Absent");
        table.publish(pending);
        assert_eq!(table.find(b"Foo").and_then(|s| s.ce_cache()), Some(2));
        assert_eq!(table.find(b"echoed").and_then(|s| s.ce_cache()), None);
        assert!(table.find(b"Shared").is_some_and(|s| s.is_same(shared)));
        assert_eq!(shared.ce_cache(), None);
        let mut next = Pending::default();
        next.intern(b"Bar", false).expect("a string");
        next.class_name(b"Bar");
        table.publish(next);
        assert_eq!(table.find(b"Bar").and_then(|s| s.ce_cache()), Some(4));
    }

    /// A pending array takes over the asks on what it holds: given back,
    /// it gives back one ask on each string and array in it, so a string
    /// another ask stands on stays, and one a hold kept for a fold's
    /// value stays too.
    #[test]
    fn a_pending_array_given_back_gives_back_its_strings() {
        let mut pending = Pending::default();
        let key = pending.intern(b"key", false).expect("a string");
        let kept = pending.intern(b"kept", false).expect("a string");
        let value = pending.intern(b"value", false).expect("a string");
        pending.intern(b"kept", false).expect("a second ask");
        let inner = pending
            .constant_array(&[(Key::Long(0), Zval::string(value))])
            .expect("an array");
        let outer = pending
            .constant_array(&[
                (Key::Str(key), Zval::string(kept)),
                (Key::Long(1), Zval::array(inner)),
            ])
            .expect("an array");
        pending.hold(Zval::string(value));
        assert_eq!(pending.count(), 3);
        pending.give_back(Zval::array(outer));
        assert_eq!(pending.count(), 2);
        assert!(pending.arrays.is_empty());
        pending.give_back(Zval::string(kept));
        pending.give_back(Zval::string(value));
        assert_eq!(pending.count(), 0);
    }

    /// `zend_string_alloc` then `zend_string_truncate`: the bytes written
    /// in place survive the cut, the NUL follows them, and the heap
    /// counts the smaller bin the string moved to; a cut that stays in
    /// the bin keeps the allocation where it stands.
    #[test]
    fn a_fresh_string_is_written_in_place_and_cut_to_its_bin() {
        let _alone = crate::serial::alone();
        let mut heap = crate::Heap::new();
        let base = heap.memory_usage(false);
        let mut fresh = heap
            .string_fresh(2103, Sizing::Alloc)
            .expect("a fresh string");
        fresh.bytes_mut()[..5].copy_from_slice(b"hello");
        assert_eq!(heap.memory_usage(false) - base, 2560);
        // php-8.5.12's truncate asks 32 + 5, the 40-byte bin.
        let cut = heap.string_truncate(fresh, 5).expect("a truncated string");
        assert_eq!(heap.memory_usage(false) - base, 40);
        let same = heap.string_truncate(cut, 3).expect("a truncated string");
        let s = same.finish();
        assert_eq!(s.bytes(), b"hel");
        assert_eq!(heap.memory_usage(false) - base, 40);
        heap.release_string(s);
        assert_eq!(heap.memory_usage(false), base);
    }

    /// `zend_string_extend` then `ZSTR_LEN(s) = len`: the bytes written
    /// survive the growth into the larger bin and the new ones are zero;
    /// a length set where the allocation stands keeps its bin.
    #[test]
    fn a_fresh_string_grows_in_place_and_keeps_its_bin_under_a_shorter_length() {
        let _alone = crate::serial::alone();
        let mut heap = crate::Heap::new();
        let base = heap.memory_usage(false);
        let mut fresh = heap.string_fresh(5, Sizing::Alloc).expect("a fresh string");
        fresh.bytes_mut().copy_from_slice(b"hello");
        assert_eq!(heap.memory_usage(false) - base, 32);
        let mut grown = heap.string_extend(fresh, 40).expect("an extended string");
        assert_eq!(heap.memory_usage(false) - base, 80);
        assert_eq!(&grown.bytes_mut()[..7], b"hello\0\0");
        grown.set_len(3);
        let s = grown.finish();
        assert_eq!(s.bytes(), b"hel");
        assert_eq!(heap.memory_usage(false) - base, 80);
        heap.release_string(s);
        assert_eq!(heap.memory_usage(false), base);
    }

    /// Strings whose hashes are one hash, DJBX33A's `Ez` and `FY`
    /// written four ways, and a thousand more, each found as the one
    /// string interned for its bytes however the table has grown.
    #[test]
    fn colliding_and_many_strings_are_each_found_as_the_one_interned() {
        let colliding = [&b"EzEz"[..], b"EzFY", b"FYEz", b"FYFY"];
        assert!(
            colliding
                .iter()
                .all(|bytes| hash_func(bytes) == hash_func(b"EzEz"))
        );
        let mut strings = Strings::new();
        let mut interned = Vec::new();
        for bytes in colliding
            .iter()
            .map(|b| b.to_vec())
            .chain((0..1000).map(|n| format!("name{n}").into_bytes()))
        {
            interned.push((strings.intern(&bytes).expect("a table string"), bytes));
        }
        for (s, bytes) in &interned {
            assert!(strings.find(bytes).is_some_and(|found| found.is_same(*s)));
            assert!(strings.intern(bytes).is_ok_and(|again| again.is_same(*s)));
        }
        assert_eq!(strings.count(), 1004);
        assert!(strings.find(b"FYFZ").is_none());
    }

    /// Strings a compile made under one hash, given back out of the
    /// middle of their slots, leave the others found as they were, and a
    /// string given back is made anew when it is asked for again.
    #[test]
    fn a_pending_string_given_back_from_a_run_leaves_the_rest_found() {
        let mut pending = Pending::default();
        let colliding = [&b"EzEz"[..], b"EzFY", b"FYEz", b"FYFY", b"EzEzEz", b"other"];
        let made: Vec<Str> = (colliding.iter())
            .map(|bytes| pending.intern(bytes, false).expect("a string"))
            .collect();
        pending.give_back(Zval::string(made[1]));
        pending.give_back(Zval::string(made[0]));
        assert_eq!(pending.count(), 4);
        for (bytes, s) in colliding.iter().zip(&made).skip(2) {
            let again = pending.intern(bytes, false).expect("the same string");
            assert!(again.is_same(*s));
            pending.give_back(Zval::string(again));
        }
        let anew = pending.intern(b"EzFY", false).expect("a string made anew");
        assert_eq!(anew.bytes(), b"EzFY");
        assert_eq!(pending.count(), 5);
        let mut table = Strings::new();
        table.publish(pending);
        assert_eq!(table.count(), 5);
        assert!(table.find(b"EzEz").is_none());
        assert!(table.find(b"FYFY").is_some_and(|s| s.is_same(made[3])));
        let mut many = Pending::default();
        let names: Vec<Vec<u8>> = (0..500).map(|n| format!("n{n}").into_bytes()).collect();
        let made: Vec<Str> = (names.iter())
            .map(|bytes| many.intern(bytes, false).expect("a string"))
            .collect();
        for s in made.iter().step_by(3) {
            many.give_back(Zval::string(*s));
        }
        assert_eq!(many.count(), 333);
        for (n, (bytes, s)) in names
            .iter()
            .zip(&made)
            .enumerate()
            .filter(|(n, _)| n % 3 != 0)
        {
            let again = many.intern(bytes, false).expect("the same string");
            assert!(again.is_same(*s), "string {n} is found where it was made");
        }
    }

    /// A table sized for its strings lays them in its bump, as opcache's
    /// `accel_copy_permanent_strings` lays php's permanent ones in its
    /// buffer: a thousand names take one allocation between them, each
    /// laid right after the last, and a string a compile published stays
    /// the compile's, freed alone as the table drops with the bump.
    #[test]
    fn a_table_s_strings_are_laid_in_one_bump_and_not_one_allocation_each() {
        let names: Vec<Vec<u8>> = (0..1000)
            .map(|at| format!("startup_name_{at}").into_bytes())
            .collect();
        let mut table = Strings::with_capacity(names.len());
        let mut last: Option<Str> = None;
        for name in &names {
            let s = table.intern(name).expect("a string");
            if let Some(last) = last {
                let after = last.address().as_ptr().addr() + Str::allocation_size(last.len());
                assert_eq!(s.address().as_ptr().addr(), after, "laid after the last");
            }
            last = Some(s);
        }
        assert_eq!(table.bump.chunks(), 1, "one allocation for 1000 strings");
        let mut pending = Pending::default();
        pending.intern(b"published", false).expect("a string");
        table.publish(pending);
        let published = table.find(b"published").expect("published");
        assert!(!table.bump.holds(published.address()));
        assert!(names.iter().all(|name| table.find(name).is_some()));
        assert_eq!(table.count(), 1001);
    }

    #[test]
    fn interning_the_same_bytes_twice_gives_one_string() {
        let mut strings = Strings::new();
        let first = strings.intern(b"var_dump").expect("intern \"var_dump\"");
        let again = strings.intern(b"var_dump").expect("intern \"var_dump\"");
        let other = strings
            .intern(b"extension_loaded")
            .expect("intern \"extension_loaded\"");
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
        let s = strings.intern(b"php").expect("intern \"php\"");
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
        // The frozen table allocates nothing and always answers.
        let answered = "the frozen table answers";
        let empty = one.intern(b"").expect(answered);
        let elsewhere = another.intern(b"").expect(answered);
        let a = one.intern(b"a").expect(answered);
        let also_a = another.intern(b"a").expect(answered);
        let high = one.intern(&[0xff]).expect(answered);
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
        let s = strings
            .intern(b"zend_string")
            .expect("intern \"zend_string\"");
        let frozen = strings.intern(b"x").expect("the frozen table answers");
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

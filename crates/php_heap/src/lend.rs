//! A request's heap lent to C for the length of one call (ADR-0014,
//! design §5.14). php builds a C library whose allocator macros take no
//! argument naming a heap with those macros pointed at `emalloc` (timelib's
//! `timelib_config.h`, `Z/ext/date/config0.m4`), so what the library
//! allocates while it works is the request's and `memory_get_usage()`
//! and `memory_get_peak_usage()` count it. Here the library is compiled
//! against the six `extern "C"` functions below, php's names, and they
//! reach the request's heap through one per-thread slot that
//! [`Heap::lend`] fills for exactly the length of one call and empties
//! when the call returns, on every path out of it. Nothing outlives the
//! call, so a pooled thread carries no heap from one request to the
//! next (revision 50's reason, which still rejects a heap that outlives
//! a call).
//!
//! Outside a lend there is no heap: `emalloc`, `ecalloc`, `erealloc`,
//! `estrdup` and `estrndup` fall back to the system allocator, never to a
//! stale request. Under a lend `efree` and `erealloc` tell the heap's
//! blocks from the system's by asking the heap ([`Mm::owns`], php's
//! `chunk->heap == heap`), so a block made outside a lend goes back to
//! the system allocator and one made under it to the heap. Outside a
//! lend nothing can tell a heap's block from the system's without
//! reading memory that may already be gone, so `efree` frees nothing
//! there: a block of a heap goes back with that heap at the request's
//! end, and the caller keeps every free of a heap's block under a lend
//! of the same request (ADR-0014).
//!
//! The memory limit cannot leave a C library as php's `zend_bailout`
//! leaves it, by a jump over its frames: the allocation the heap refuses is served
//! by the system allocator, so the library runs to its return, and the
//! refusal is what [`Heap::lend`] answers, for the caller to end the
//! request in.
//!
//! A library whose allocator callbacks take an argument of their own
//! (zlib's `zalloc`, uriparser's memory manager, libtidy's allocator) is
//! pointed at four of the six from Rust as php points it at its own:
//! [`emalloc`], [`ecalloc`], [`erealloc`] and [`efree`] are public, and
//! the extension makes each call into the library under a lend. They
//! zero exactly where php's do: `emalloc` and `erealloc` leave the bytes
//! as they find them (`Z/Zend/zend_alloc.c:2776`, `:2797`), and
//! `ecalloc` alone writes zeroes (`Z/Zend/zend_alloc.c:2842-2850`).
//!
//! The symbols resolve at link time, the library's undefined references
//! against this crate's definitions, with no edge between the two
//! crates (seams.toml's edges). [`enter`] and [`leave`] are never
//! inlined and sit in this file with the six, so the object that
//! defines them is the one every lend's caller pulls into the link,
//! whichever order the linker reads the archives in.

use std::cell::Cell;
use std::ffi::{c_char, c_void};
use std::ptr::NonNull;

use crate::Heap;
use crate::hooks::Fatal;
use crate::mm::Mm;

/// What one lend hands C: the heap's allocator, and the first refusal
/// its limit made while the call ran.
struct Lending {
    mm: NonNull<Mm>,
    refused: Option<Fatal>,
}

// The crate's root carries the macro's expectation: clippy 1.95 reports
// `disallowed_macros` for this invocation at the crate, past any attribute
// nearer to it; the type's is the static's own.
std::thread_local! {
    /// The lend in force on this thread, none outside one.
    #[expect(
        clippy::disallowed_types,
        reason = "ADR-0014's lend: one slot per thread for the length of one C call"
    )]
    static LENT: Cell<Option<NonNull<Lending>>> = const { Cell::new(None) };
}

/// The lend in force before this one put back, on every path out of
/// [`Heap::lend`], unwinding among them.
struct Restore(Option<NonNull<Lending>>);

impl Drop for Restore {
    fn drop(&mut self) {
        leave(self.0);
    }
}

/// Whether a lend stands on this thread: what a debug build asks as it
/// makes a heap (`Mm`'s birth), since a heap made while another's lend
/// stands would have C served by that other heap outside its own lends.
#[cfg(debug_assertions)]
pub(crate) fn standing() -> bool {
    LENT.get().is_some()
}

/// `lending` put in the slot, and what stood there answered.
#[inline(never)]
fn enter(lending: NonNull<Lending>) -> Option<NonNull<Lending>> {
    LENT.replace(Some(lending))
}

/// `previous` put back in the slot.
#[inline(never)]
fn leave(previous: Option<NonNull<Lending>>) {
    LENT.set(previous);
}

impl Heap {
    /// `call` served with this heap lent to C (ADR-0014): what a C
    /// library allocates through `emalloc` and its kin while `call` runs
    /// is this heap's, counted in `memory_get_usage()` and its peak as
    /// php counts its own library's, and what it frees through `efree`
    /// goes back where it came from. The heap is borrowed for the length
    /// of `call`, so nothing in Rust reaches it meanwhile, and the lend
    /// ends when `call` returns. The `Err` is the memory limit's fatal,
    /// the first the heap made while `call` ran, whose allocation the
    /// system allocator served instead; what `call` answered is then
    /// dropped, and what it held is the request's end's.
    ///
    /// # Errors
    ///
    /// The limit's fatal, for the caller to end the request in.
    pub fn lend<R>(&mut self, call: impl FnOnce() -> R) -> Result<R, Fatal> {
        let mut lending = Lending {
            mm: NonNull::from(&mut self.mm),
            refused: None,
        };
        let answered = {
            let _restore = Restore(enter(NonNull::from(&mut lending)));
            call()
        };
        match lending.refused {
            None => Ok(answered),
            Some(fatal) => Err(fatal),
        }
    }
}

/// The lend in force, as one exclusive borrow for the length of one
/// allocator entry, or none outside a lend.
fn lending<'a>() -> Option<&'a mut Lending> {
    let lending = LENT.get()?;
    // SAFETY: the slot holds a `Lending` on the stack of a `Heap::lend`
    // still running on this thread, whose heap that lend borrows
    // uniquely; C calls one entry at a time and no entry calls another,
    // so this is the one reference to it while the entry runs.
    Some(unsafe { &mut *lending.as_ptr() })
}

impl Lending {
    /// Whether `block` is the lent heap's ([`Mm::owns`]).
    fn owns(&self, block: NonNull<u8>) -> bool {
        // SAFETY: the heap `Heap::lend` borrowed uniquely for the length
        // of the call this entry runs inside, read here.
        unsafe { self.mm.as_ref() }.owns(block)
    }

    fn mm(&mut self) -> &mut Mm {
        // SAFETY: the heap `Heap::lend` borrowed uniquely for the length
        // of the call this entry runs inside.
        unsafe { self.mm.as_mut() }
    }

    /// The heap's `block` resized to `size`, or moved to the system
    /// allocator where the limit refused the size, the refusal kept.
    ///
    /// # Safety
    ///
    /// `block` is live in the lent heap.
    unsafe fn realloc(&mut self, block: NonNull<u8>, size: usize) -> *mut c_void {
        // SAFETY: live in this heap, by the contract.
        let copy = unsafe { self.mm().usable(block) };
        // SAFETY: a live block of this heap holding `copy` bytes.
        let fatal = match unsafe { self.mm().realloc(block, copy, size) } {
            Ok(moved) => return moved.as_ptr().cast(),
            Err(fatal) => fatal,
        };
        self.refuse(fatal);
        let moved = system_alloc(size);
        if !moved.is_null() {
            let (from, to) = (block.as_ptr(), moved.cast::<u8>());
            // SAFETY: both blocks hold the shorter length, and the heap's
            // still stands, a refused resize having left it.
            unsafe { from.copy_to_nonoverlapping(to, copy.min(size)) };
            // SAFETY: the heap's block, copied out above and named by
            // nothing the caller keeps.
            unsafe { self.mm().free(block) };
        }
        moved
    }

    /// The limit's refusal kept, the first of the call's.
    fn refuse(&mut self, fatal: Fatal) {
        if self.refused.is_none() {
            self.refused = Some(fatal);
        }
    }

    /// `size` bytes of the heap, or of the system allocator where the
    /// limit refused them, the refusal kept.
    fn alloc(&mut self, size: usize) -> *mut c_void {
        match self.mm().alloc(size) {
            Ok(at) => at.as_ptr().cast(),
            Err(fatal) => {
                self.refuse(fatal);
                system_alloc(size)
            }
        }
    }
}

/// `malloc(size)`, at least one byte, as `emalloc(0)` answers a block.
fn system_alloc(size: usize) -> *mut c_void {
    // SAFETY: `malloc` takes a size and answers memory of its own or null.
    unsafe { libc::malloc(size.max(1)) }
}

/// `emalloc(size)`: `size` bytes of the lent heap, counted by the bin,
/// `run` or huge block the size falls in and left as the heap finds them;
/// the system allocator's outside a lend, and where the memory limit
/// refused them, whose fatal the lend answers.
#[unsafe(no_mangle)]
pub extern "C" fn emalloc(size: usize) -> *mut c_void {
    match lending() {
        Some(lending) => lending.alloc(size),
        None => system_alloc(size),
    }
}

/// `ecalloc(count, size)`: [`emalloc`] of the product, zeroed; a product
/// past the address space is the system's `calloc`'s to refuse.
#[unsafe(no_mangle)]
pub extern "C" fn ecalloc(count: usize, size: usize) -> *mut c_void {
    let Some(total) = count.checked_mul(size) else {
        // SAFETY: `calloc` takes two sizes and answers null for this one.
        return unsafe { libc::calloc(count, size) };
    };
    let at = emalloc(total);
    if !at.is_null() {
        // SAFETY: `at` is a fresh block of at least `total` bytes.
        unsafe { at.cast::<u8>().write_bytes(0, total) };
    }
    at
}

/// `erealloc(at, size)`: a block of the lent heap resized there, any
/// other one by the system allocator, the bytes past the old length left
/// as they are found; null is [`emalloc`].
///
/// # Safety
///
/// `at` is null or a live block one of these entries answered, and
/// outside a lend one they answered outside a lend: the caller resizes
/// a heap's block only under a lend of that heap.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn erealloc(at: *mut c_void, size: usize) -> *mut c_void {
    let Some(block) = NonNull::new(at.cast::<u8>()) else {
        return emalloc(size);
    };
    match lending() {
        // SAFETY: the heap owns the block, and it is live by the caller's
        // contract.
        Some(lending) if lending.owns(block) => unsafe { lending.realloc(block, size) },
        // SAFETY: a block the system allocator answered, by the contract.
        _ => unsafe { libc::realloc(at, size.max(1)) },
    }
}

/// `estrdup(s)`: the string and its terminator in an [`emalloc`] block.
///
/// # Safety
///
/// `s` is a NUL-terminated string.
#[unsafe(no_mangle)]
unsafe extern "C" fn estrdup(s: *const c_char) -> *mut c_char {
    // SAFETY: NUL-terminated by the caller's contract.
    let len = unsafe { libc::strlen(s) };
    // SAFETY: `len` bytes of `s` are readable, as above.
    unsafe { estrndup(s, len) }
}

/// `estrndup(s, len)`: `len` bytes of `s` and a terminator in an
/// [`emalloc`] block, as php's `_estrndup` copies them.
///
/// # Safety
///
/// `len` bytes at `s` are readable.
#[unsafe(no_mangle)]
unsafe extern "C" fn estrndup(s: *const c_char, len: usize) -> *mut c_char {
    let at = emalloc(len.saturating_add(1)).cast::<c_char>();
    if at.is_null() {
        return at;
    }
    // SAFETY: `len` readable bytes by the contract, and `at` a fresh
    // block of `len + 1`.
    unsafe { s.copy_to_nonoverlapping(at, len) };
    // SAFETY: `len` is inside the block of `len + 1`.
    let end = unsafe { at.add(len) };
    // SAFETY: the block's last byte.
    unsafe { end.write(0) };
    at
}

/// `efree(at)`: a block of the lent heap back to it, any other one back
/// to the system allocator; outside a lend, nothing (this file's
/// paragraph on telling the two apart).
///
/// # Safety
///
/// `at` is null or a live block one of these entries answered.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn efree(at: *mut c_void) {
    let Some(block) = NonNull::new(at.cast::<u8>()) else {
        return;
    };
    let Some(lending) = lending() else {
        return;
    };
    if lending.owns(block) {
        // SAFETY: the heap's block, live by the caller's contract.
        unsafe { lending.mm().free(block) };
    } else {
        // SAFETY: not the heap's, so the system allocator's, live by the
        // contract.
        unsafe { libc::free(at) };
    }
}

#[cfg(test)]
mod tests;

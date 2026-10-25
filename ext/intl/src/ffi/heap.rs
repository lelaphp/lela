//! Blocks of the request's heap, what php's intl asks `emalloc` and its
//! kin for while it works: the UTF-16 copy `intl_convert_utf8_to_utf16`
//! makes with `eumalloc`, `normalizer_normalize`'s destination and
//! `grapheme_levenshtein`'s two rows. Each is asked of the heap under a
//! lend (ADR-0014), so the memory limit meets it as php's `zend_mm` meets
//! php's, its size and its order, and the delayed OOM it records is the
//! one the interrupt check after the call raises. A block is given back
//! under a lend of the same request with [`Block::give_back`], where php
//! frees it; it has no drop, since outside a lend `efree` frees nothing.

use std::marker::PhantomData;
use std::ptr::NonNull;

use php_ext::raising::Thrown;
use php_ext::request::{Request, efree, emalloc};

/// A value whose every byte may be zero, what a block holds.
pub(crate) trait Unit: Copy {}

impl Unit for u16 {}
impl Unit for i64 {}

/// `len` values of `T` in one block of the request's heap.
pub(crate) struct Block<T: Unit> {
    at: NonNull<T>,
    len: usize,
    held: PhantomData<T>,
}

impl<T: Unit> Block<T> {
    /// `safe_emalloc(len, sizeof(T), 0)`, and `safe_femalloc(..., true)`
    /// where `fallible`: `None` where the memory limit refused a fallible
    /// ask, php's `NULL`, with the delayed OOM recorded. An ask that is
    /// not fallible is served past the soft limit as php serves it, and
    /// the `Err` is the hard limit's end of the request. The values are
    /// zero, never read before they are written.
    pub(crate) fn ask(
        req: &mut Request,
        len: usize,
        fallible: bool,
    ) -> Result<Option<Self>, Thrown> {
        let unit = size_of::<T>();
        let Some(size) = len.checked_mul(unit) else {
            let message =
                format!("Possible integer overflow in memory allocation ({len} * {unit} + 0)");
            return Err(php_ext::raising::fatal(req, &message));
        };
        let at = if fallible {
            req.memory_mut()
                .fallible(|req| req.memory_mut().lend_heap(|| emalloc(size)))?
                .unwrap_or(std::ptr::null_mut())
        } else {
            req.memory_mut().lend_heap(|| emalloc(size))?
        };
        let Some(at) = NonNull::new(at.cast::<T>()) else {
            return Ok(None);
        };
        // SAFETY: a fresh block of `size` bytes the heap answered.
        unsafe { at.as_ptr().cast::<u8>().write_bytes(0, size) };
        Ok(Some(Block {
            at,
            len,
            held: PhantomData,
        }))
    }

    pub(crate) fn as_slice(&self) -> &[T] {
        // SAFETY: the block holds `len` initialised values until it is
        // given back, which takes it by value.
        unsafe { std::slice::from_raw_parts(self.at.as_ptr(), self.len) }
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [T] {
        // SAFETY: as `as_slice`, borrowed uniquely through `self`.
        unsafe { std::slice::from_raw_parts_mut(self.at.as_ptr(), self.len) }
    }

    /// `efree`: the block back to the heap that made it.
    pub(crate) fn give_back(self, req: &mut Request) {
        let at = self.at.as_ptr().cast();
        // SAFETY: a live block of this request's heap, freed once, under
        // a lend of it.
        let freed = req.memory_mut().lend_heap(|| unsafe { efree(at) });
        // A free asks nothing of the limit.
        drop(freed);
    }
}

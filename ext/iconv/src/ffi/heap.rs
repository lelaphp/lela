//! A block of the request's heap, what `_php_iconv_mime_encode` asks
//! `safe_femalloc` for to convert one encoded word into: asked under a
//! lend (ADR-0014) inside the memory limit's fallible scope, so the limit
//! refuses it as php 8.5.12's `zend_mm` refuses php's, its size and its
//! order, and given back under a lend with [`Block::give_back`] where php
//! frees it. It has no drop, since outside a lend `efree` frees nothing.

use std::ptr::NonNull;

use php_ext::raising::Thrown;
use php_ext::request::{Request, efree, emalloc};

/// `len` bytes in one block of the request's heap.
pub(crate) struct Block {
    at: NonNull<u8>,
    len: usize,
}

impl Block {
    /// `safe_femalloc(1, len, 0, true)`: `None` where the memory limit
    /// refused it, php's `NULL`, with the delayed OOM recorded. The bytes
    /// are zero.
    pub(crate) fn ask_fallibly(req: &mut Request, len: usize) -> Result<Option<Block>, Thrown> {
        let at = req
            .memory_mut()
            .fallible(|req| req.memory_mut().lend_heap(|| emalloc(len)))?
            .unwrap_or(std::ptr::null_mut());
        let Some(at) = NonNull::new(at.cast::<u8>()) else {
            return Ok(None);
        };
        // SAFETY: a fresh block of `len` bytes the heap answered.
        unsafe { at.as_ptr().write_bytes(0, len) };
        Ok(Some(Block { at, len }))
    }

    pub(crate) fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: the block holds `len` initialised bytes until it is given
        // back, which takes it by value; borrowed uniquely through `self`.
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

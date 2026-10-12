//! The request leak sensor's walk (design §5.1, §10): php's
//! `zend_mm_check_leaks` (`Z/Zend/zend_alloc.c:2311`), which a debug
//! build of php runs from `zend_mm_shutdown` once the request's state is
//! freed. Every block the heap still holds there is one nothing freed,
//! and the walk names them through the heap's sensed record
//! ([`crate::sensed`]), the one path a debug sensor reports by.
//!
//! php reads a block's state from the debug trailer every allocation of
//! its debug heap carries. This heap carries none, so a bin slot is held
//! where it is on no free list, and a large `run` or a huge block is held
//! while it stands at all. The walk sweeps first (`zend_mm_gc`), so a
//! `run` wholly free is given back before it is read, and a slot a debug
//! build keeps back from reuse is on its free list again when it is.

use std::collections::HashSet;
use std::ptr::NonNull;

use super::origins;
use super::{
    BIN_DATA_SIZE, BIN_ELEMENTS, BIN_PAGES, FIRST_PAGE, IS_SRUN, LRUN_PAGES_MASK, Mm, PAGE_SIZE,
    SRUN_BIN_NUM_MASK, bitset_is_set,
};
use crate::sensed::{Held, Origin, Sensed};

impl Mm {
    /// `zend_mm_check_leaks`: every block this heap still holds, recorded
    /// as one [`Sensed::Leak`] naming how many there are, the bytes the
    /// accounting counts in them and the first the walk met, in php's
    /// order: the huge blocks, then each chunk from its first page up;
    /// the first with what `name` calls the origin it was allocated for
    /// and, where allocations are traced, the frames that allocated it.
    /// Nothing is recorded for a heap that holds nothing.
    pub(crate) fn sense_leaks(&mut self, name: &dyn Fn(Origin) -> String) {
        self.gc_heap();
        let mut free = HashSet::new();
        for &head in &self.free_slot {
            let mut next = NonNull::new(head);
            while let Some(slot) = next {
                free.insert(slot.addr().get());
                // SAFETY: a slot on a free list is free memory of this
                // heap, whose first word is the next free slot.
                next = NonNull::new(unsafe { slot.as_ref() }.next);
            }
        }
        let mut blocks = 0;
        let mut first = None;
        let mut held = |at: usize, size: usize, origin: Origin| {
            blocks += 1;
            first.get_or_insert((Held { at, size }, origin));
        };
        for huge in &self.huge {
            // The node is the block's own and goes with it, as php's walk
            // frees it with the block (`zend_mm_check_leaks`).
            free.insert(huge.node.addr().get());
            held(huge.at.addr().get(), huge.size, huge.origin);
        }
        for &chunk in &self.chunks {
            // SAFETY: every chunk on the list is one this heap mapped and
            // initialised, and nothing writes its header while it is read.
            let header = unsafe { &*chunk.as_ptr() };
            let base = chunk.addr().get();
            let mut page_num = FIRST_PAGE;
            while page_num < header.free_tail {
                let info = header.map[page_num as usize];
                if !bitset_is_set(&header.free_map, page_num) {
                    page_num += 1;
                    continue;
                }
                let run = base + page_num as usize * PAGE_SIZE;
                if info & IS_SRUN == 0 {
                    let pages = (info & LRUN_PAGES_MASK).max(1);
                    held(run, pages as usize * PAGE_SIZE, origins::read(header, run));
                    page_num += pages;
                    continue;
                }
                let bin_num = (info & SRUN_BIN_NUM_MASK) as usize;
                let size = BIN_DATA_SIZE[bin_num];
                for slot in (0..BIN_ELEMENTS[bin_num]).map(|i| run + i * size) {
                    if !free.contains(&slot) {
                        held(slot, size, origins::read(header, slot));
                    }
                }
                page_num += BIN_PAGES[bin_num];
            }
        }
        if let Some((first, origin)) = first {
            self.sensed.record(Sensed::Leak {
                bytes: self.size,
                blocks,
                first,
                named: name(origin),
                trace: self.trace_of(first.at),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::serial::alone;
    use crate::{Fatal, FatalReason, Heap, Origin};

    /// The name the walk is handed for each origin, as the engine names an
    /// extension by its number.
    fn named(origin: Origin) -> String {
        format!("ext{}", origin.number())
    }

    /// What the heap sensed, as the message of the fatal it ends in.
    fn sensed(heap: &mut Heap) -> String {
        match heap.take_sensed().map(Fatal::into_reason) {
            Some(FatalReason::Error(message)) => message,
            other => format!("{other:?}"),
        }
    }

    /// A block left held is named with the origin the heap allocated it
    /// for, the one set where it was made and not the one standing where
    /// the request ends; a small slot, a `run` and a huge block alike.
    #[test]
    fn a_leak_names_the_origin_its_block_was_allocated_for() {
        let _alone = alone();
        for size in [7, 5000, 3 * 1024 * 1024] {
            let mut heap = Heap::new();
            let outer = heap.set_origin(Origin::new(7));
            assert_eq!(
                outer,
                Origin::default(),
                "Core is the origin a heap starts at"
            );
            let leaked = heap.string_with(size, |_| {});
            assert!(leaked.is_ok());
            assert_eq!(heap.set_origin(outer), Origin::new(7));
            heap.sense_leaks(&named);
            let message = sensed(&mut heap);
            assert!(
                message.ends_with(", allocated by ext7"),
                "{size}: {message}"
            );
        }
    }

    /// With allocations traced, the leak names the line that allocated the
    /// block, past the heap's own frames.
    #[test]
    #[cfg_attr(
        miri,
        ignore = "the traced backtrace resolves its frames' file names through `getcwd`, which Miri's isolation refuses"
    )]
    fn a_traced_leak_names_the_line_that_allocated_it() {
        let _alone = alone();
        let mut heap = Heap::new();
        heap.trace_allocations();
        let line = line!() + 1;
        let leaked = heap.string(b"leaked");
        assert!(leaked.is_ok());
        heap.sense_leaks(&named);
        let message = sensed(&mut heap);
        assert!(
            message.contains(&format!("leaks.rs:{line}:")),
            "line {line}: {message}"
        );
        assert!(message.contains(", allocated by ext0 at:\n"), "{message}");
    }
}

//! Allocation observation (design §7.1, as #1780 ruled): what stands in
//! the place of php's `zend_mm_set_custom_handlers`, which no extension
//! is given. A profiler is told of each allocation, reallocation and
//! free the heap serves, and of what `gc_mem_caches()` gave back, after
//! the heap served it; it is never asked to serve one. The heap that
//! serves them is the request's own, so `memory_get_usage()`,
//! `memory_get_peak_usage()` and the limit read what they read with no
//! observer at all, which a replacement allocator would not leave them
//! reading (principle 2).
//!
//! php's custom heap is tested on every `_emalloc`, `_efree` and
//! `_erealloc` (`UNEXPECTED(AG(mm_heap)->use_custom_heap)`,
//! `Z/Zend/zend_alloc.c:2779-2811`), and so is this list: one test of
//! whether it is empty, on the heap the allocation is served from, and
//! the call past it out of line. The list is the heap's and not the
//! engine's, because php's own user of the hooks, `zend_test`, turns
//! them on and off while a request is served, from an ini entry's
//! `on_modify` (`Z/ext/zend_test/zend_mm_custom_handlers.c`).
//!
//! What is told is what php's custom handlers are handed, at the same
//! entries: the size the caller asked for, before the heap rounds it to
//! its bin, and the addresses the heap answered. An allocation, a
//! reallocation or a free the heap makes on its own behalf, a
//! reallocation's move and the collection a limit's growth asks for, is
//! no entry's, and php's custom handlers are not handed those either,
//! since they are made on the heap beneath (`zend_mm_realloc_slow`,
//! `zend_mm_gc` from `zend_mm_alloc_pages`). An allocation the limit
//! refuses is not told, as php's fatal leaves the handler before it
//! prints. The request's end gives the chunks back whole and tells no
//! one; `zend_test` removes its handlers in its `RSHUTDOWN`, before php's
//! heap is shut down, so its shutdown handler is never handed that
//! either.

use std::any::Any;

/// What the heap served, as an observer is told of it: an address is
/// the block's, as php's `%p` prints it, and says nothing past the
/// block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allocation {
    /// `_emalloc(size)` answered `at`.
    Allocated { at: usize, size: usize },
    /// `_erealloc(from, size)` answered `to`, which is `from` where the
    /// block stayed where it stood.
    Reallocated { from: usize, to: usize, size: usize },
    /// `_efree(at)`.
    Freed { at: usize },
    /// `zend_mm_gc`, what `gc_mem_caches()` calls, gave `freed` bytes
    /// back.
    Collected { freed: usize },
}

/// A profiler of a request's allocations: told of each, in the order the
/// heap served them, while it is in the heap's list
/// ([`crate::Heap::observe_allocations`]).
///
/// It is told while the heap is mid-allocation, so it is handed
/// nothing of the request: what it keeps is its own, and what it
/// allocates is the process's, never the request heap's. It is `Any` so
/// that the extension that put it in takes back its own type when it is
/// removed ([`crate::Heap::unobserve_allocations`]).
pub trait ObserveAllocations: Any + Send {
    /// Told of one allocation the heap served.
    fn observe(&mut self, what: Allocation);
}

/// One observer's place in a heap's list, which removes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AllocationObserverId(u32);

/// A heap's observers, in the order they were put in.
#[derive(Default)]
pub(crate) struct Observers {
    list: Vec<(AllocationObserverId, Box<dyn ObserveAllocations>)>,
    next: u32,
}

impl Observers {
    /// Whether no observer is in the list, which the heap reads into its
    /// `use_custom_heap` as the list changes, so an allocation pays one
    /// test for observation and the tracked allocator together.
    #[inline]
    pub(crate) fn is_empty(&self) -> bool {
        self.list.is_empty()
    }

    /// `observer` put in last, and its place.
    pub(crate) fn add(&mut self, observer: Box<dyn ObserveAllocations>) -> AllocationObserverId {
        let at = AllocationObserverId(self.next);
        self.next = self.next.wrapping_add(1);
        self.list.push((at, observer));
        at
    }

    /// The observer at `at` taken out, where it is still in.
    pub(crate) fn remove(
        &mut self,
        at: AllocationObserverId,
    ) -> Option<Box<dyn ObserveAllocations>> {
        let index = self.list.iter().position(|(held, _)| *held == at)?;
        Some(self.list.remove(index).1)
    }

    /// Every observer told of `what`, in the order they were put in; out
    /// of line, so the allocation that finds the list empty carries no
    /// more than the test.
    #[cold]
    #[inline(never)]
    pub(crate) fn tell(&mut self, what: Allocation) {
        for (_, observer) in &mut self.list {
            observer.observe(what);
        }
    }
}

impl std::fmt::Debug for Observers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Observers")
            .field("observers", &self.list.len())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::Heap;
    use crate::serial::alone;
    use crate::string::Str;

    /// An observer that keeps what it is told, taken back by its type.
    #[derive(Default)]
    struct Recorder(Vec<Allocation>);

    impl ObserveAllocations for Recorder {
        fn observe(&mut self, what: Allocation) {
            self.0.push(what);
        }
    }

    /// What `observer` was told, taken out of `heap`.
    fn taken(heap: &mut Heap, observer: AllocationObserverId) -> Vec<Allocation> {
        let back: Box<dyn Any> = heap
            .unobserve_allocations(observer)
            .expect("the observer is handed back");
        back.downcast::<Recorder>()
            .expect("the observer is the test's Recorder")
            .0
    }

    /// One act of each entry: a string allocated and given back, a block
    /// grown where it cannot stand and so moved, and the bin caches
    /// collected.
    fn serve(heap: &mut Heap) {
        let s = heap
            .string(b"hello world")
            .expect("the string \"hello world\"");
        heap.release_string(s);
        let block = heap.block(100).expect("a block");
        let grown = heap.block_resize(block, 5000).expect("a resized block");
        heap.free_block(grown);
        heap.gc_mem_caches();
    }

    /// Each entry the heap serves is told once, after it is served, with
    /// the size the caller asked for and the addresses the heap answered:
    /// a block moved by its growth is one reallocation, not the
    /// allocation and the free the heap makes on its own behalf to move
    /// it, as php's custom handlers see one `_erealloc`.
    #[test]
    fn an_observer_is_told_each_entry_the_heap_served() {
        let _alone = alone();
        let mut heap = Heap::new();
        let observer = heap.observe_allocations(Box::new(Recorder::default()));
        serve(&mut heap);
        let seen = taken(&mut heap, observer);
        let (s, b, to, freed) = match seen.as_slice() {
            [
                Allocation::Allocated { at: s, .. },
                _,
                Allocation::Allocated { at: b, .. },
                Allocation::Reallocated { to, .. },
                _,
                Allocation::Collected { freed },
            ] => (*s, *b, *to, *freed),
            _ => (0, 0, 0, 0),
        };
        assert_ne!(b, to, "a block grown past its `run` moves");
        assert_eq!(
            seen,
            [
                Allocation::Allocated {
                    at: s,
                    size: Str::allocation_size(11)
                },
                Allocation::Freed { at: s },
                Allocation::Allocated { at: b, size: 100 },
                Allocation::Reallocated {
                    from: b,
                    to,
                    size: 5000
                },
                Allocation::Freed { at: to },
                Allocation::Collected { freed },
            ]
        );
    }

    /// An observer taken out is told nothing more, and two are told in
    /// the order they were put in.
    #[test]
    fn an_observer_taken_out_is_told_nothing_more() {
        let _alone = alone();
        let mut heap = Heap::new();
        let first = heap.observe_allocations(Box::new(Recorder::default()));
        let second = heap.observe_allocations(Box::new(Recorder::default()));
        assert_ne!(first, second);
        serve(&mut heap);
        let told_first = taken(&mut heap, first);
        assert_eq!(told_first.len(), 6);
        assert!(heap.unobserve_allocations(first).is_none());
        serve(&mut heap);
        let told_second = taken(&mut heap, second);
        assert_eq!(told_second.len(), 12);
        assert_eq!(told_second.get(..6), Some(told_first.as_slice()));
    }

    /// With no observer, with one in, and with one put in and taken out,
    /// the heap reads the same: `memory_get_usage()` and its peak are
    /// the heap's own accounting, which observing leaves alone.
    #[test]
    fn observing_leaves_what_the_heap_reads_unchanged() {
        let _alone = alone();
        let read = |observe: Option<bool>| {
            let mut heap = Heap::new();
            let observer = observe.map(|_| heap.observe_allocations(Box::new(Recorder::default())));
            if let (Some(false), Some(observer)) = (observe, observer) {
                heap.unobserve_allocations(observer);
            }
            serve(&mut heap);
            let s = heap.string(&vec![b'x'; 3_000_000]);
            assert!(s.is_ok());
            [
                heap.memory_usage(false),
                heap.memory_usage(true),
                heap.memory_peak(),
                heap.memory_real_peak(),
            ]
        };
        let plain = read(None);
        assert_eq!(read(Some(true)), plain);
        assert_eq!(read(Some(false)), plain);
    }
}

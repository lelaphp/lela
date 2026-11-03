//! The system allocator with a count of the allocations it serves to
//! one thread: what a test reads to hold a path to allocating nothing past
//! the request's own heap, as a native's variadic tail allocates
//! nothing, `Z_PARAM_VARIADIC` pointing into the frame (#4014). A debug
//! build's alone, and a test binary's global allocator where it is one.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// The thread whose allocations are counted, `pthread_self()` as a
/// word; zero, no thread, until one is watched. A word beside the count
/// rather than a slot per thread, as the manifest's
/// `state.no-thread-local` holds a test's state to.
static WATCHED: AtomicUsize = AtomicUsize::new(0);

/// The allocations served to the watched thread since it was watched.
static ALLOCATIONS: AtomicU64 = AtomicU64::new(0);

/// The calling thread, `pthread_self()` as a word.
fn this_thread() -> usize {
    // SAFETY: `pthread_self` has no precondition and touches no memory.
    let thread = unsafe { libc::pthread_self() };
    thread as usize
}

/// [`System`], each allocation it serves to the watched thread counted,
/// a grown one among them; a free is not counted, nor what another
/// thread asks for.
#[derive(Debug, Clone, Copy, Default)]
pub struct CountingSystem;

impl CountingSystem {
    /// The calling thread watched from here, its count from zero.
    pub fn watch() {
        WATCHED.store(this_thread(), Ordering::SeqCst);
        ALLOCATIONS.store(0, Ordering::SeqCst);
    }

    /// The allocations served to the watched thread since it was watched.
    #[must_use]
    pub fn allocations() -> u64 {
        ALLOCATIONS.load(Ordering::SeqCst)
    }

    fn count() {
        if WATCHED.load(Ordering::Relaxed) == this_thread() {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// SAFETY: every call is `System`'s own, with the caller's layout and
// pointer as they came; the count beside it touches no memory the
// allocator hands out.
unsafe impl GlobalAlloc for CountingSystem {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        CountingSystem::count();
        // SAFETY: the caller's contract for `alloc`, handed on.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        CountingSystem::count();
        // SAFETY: the caller's contract for `alloc_zeroed`, handed on.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract for `dealloc`, handed on: `ptr`
        // came from this allocator, which is `System`'s.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        CountingSystem::count();
        // SAFETY: the caller's contract for `realloc`, handed on: `ptr`
        // came from this allocator, which is `System`'s.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

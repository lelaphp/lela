//! A debug build's origin of every block the heap hands out
//! ([`crate::sensed::Origin`]), which the request leak sensor's walk
//! ([`super::leaks`]) names a block still held by, and the backtrace of
//! every block where allocations are traced.
//!
//! The origin is kept per block and not per `run` or page, because a bin
//! `run` serves every extension whose blocks fall in its bin, and the
//! leaks the sensor finds are mostly small strings. It is kept beside the
//! chunk and not in it: php's debug heap widens every block by its
//! `zend_mm_debug_info`, and the oracle is a release build, so a block
//! here takes the bytes a release build's does and `memory_get_usage()`
//! reads php's. Each chunk's header names one table of [`GRAINS`] origins,
//! one per eight bytes, which every block of the chunk starts on, since a
//! bin's size is a multiple of eight; a block's origin is one store where
//! it is handed out, and nothing where it is freed, since the walk reads
//! only blocks still held. The table is zeroed by the system's allocator
//! and so is touched only where blocks stand, and a huge block, which has
//! no chunk, keeps its origin in its own record.
//!
//! Tracing is opt-in and costs what `Backtrace::force_capture`
//! costs, on every allocation: a map from each block the heap has out to
//! the frames that allocated it, kept from the entry that hands it out
//! and dropped at the entry that frees it.

use std::alloc::{Layout, alloc_zeroed, dealloc};
use std::ptr::NonNull;

use super::{CHUNK_SIZE, Chunk, Mm, chunk_of};
use crate::sensed::Origin;

/// The bytes one origin covers: every block of a chunk starts on one.
const GRAIN: usize = 8;
/// The origins of one chunk's table.
const GRAINS: usize = CHUNK_SIZE / GRAIN;

/// One chunk's table, zeroed, so every block reads as `Core`'s until it
/// is handed out; none where the system could not give it, and the
/// chunk's blocks then read as `Core`'s.
pub(super) fn table() -> Option<NonNull<Origin>> {
    let layout = Layout::array::<Origin>(GRAINS).ok()?;
    // SAFETY: the layout has a non-zero size.
    NonNull::new(unsafe { alloc_zeroed(layout) }.cast())
}

/// The table of `chunk` given back, as the chunk leaves its heap's list.
pub(super) fn give_back(chunk: NonNull<Chunk>) {
    // SAFETY: the chunk is one its heap initialised, and nothing else
    // reads its header while the heap gives it up.
    let header = unsafe { &mut *chunk.as_ptr() };
    if let (Some(origins), Ok(layout)) = (header.origins.take(), Layout::array::<Origin>(GRAINS)) {
        // SAFETY: the table came from `table` with this layout, and the
        // header named it alone.
        unsafe { dealloc(origins.as_ptr().cast(), layout) };
    }
}

/// Where the origin of the block at address `at` of the chunk `header`
/// heads stands in the chunk's table.
fn slot(header: &Chunk, at: usize) -> Option<NonNull<Origin>> {
    let grain = (at & (CHUNK_SIZE - 1)) / GRAIN;
    // SAFETY: a block's offset in its chunk is under `CHUNK_SIZE`, so its
    // grain is inside the table.
    header.origins.map(|origins| unsafe { origins.add(grain) })
}

/// The block at `at`, in a chunk, allocated for `origin`.
pub(super) fn write(at: NonNull<u8>, origin: Origin) {
    // SAFETY: `at` is a block of a chunk its heap holds, whose page zero
    // is the header `chunk_init` wrote, and nothing writes it meanwhile.
    let header = unsafe { &*chunk_of(at).as_ptr() };
    if let Some(slot) = slot(header, at.as_ptr().addr()) {
        // SAFETY: the slot is the block's in its chunk's table.
        unsafe { slot.write(origin) };
    }
}

/// The origin the block at address `at` of the chunk `header` heads was
/// allocated for.
pub(super) fn read(header: &Chunk, at: usize) -> Origin {
    // SAFETY: the slot is the block's in its chunk's table.
    slot(header, at).map_or_else(Origin::default, |slot| unsafe { slot.read() })
}

impl Mm {
    /// What the heap allocates from here on is for `origin`, and the
    /// origin it replaces, for the engine to put back.
    pub(crate) fn set_origin(&mut self, origin: Origin) -> Origin {
        std::mem::replace(&mut self.origin, origin)
    }

    /// Every allocation from here on keeps the backtrace that made it.
    pub(crate) fn trace_allocations(&mut self) {
        self.traces.get_or_insert_default();
    }

    /// The block at `to` handed out by an entry, in place of the one at
    /// `from` where it was a reallocation, traced where allocations are.
    pub(super) fn trace(&mut self, from: Option<NonNull<u8>>, to: NonNull<u8>) {
        if let Some(traces) = &mut self.traces {
            if let Some(from) = from {
                traces.remove(&from.as_ptr().addr());
            }
            traces.insert(
                to.as_ptr().addr(),
                std::backtrace::Backtrace::force_capture(),
            );
        }
    }

    /// The frames that allocated the block at `at`, where allocations
    /// are traced, from the heap's entry that handed it out: the capture's
    /// own frames are std's and say nothing.
    pub(super) fn trace_of(&self, at: usize) -> Option<String> {
        let trace = self.traces.as_ref()?.get(&at)?.to_string();
        Some(past_the_capture(&trace))
    }
}

/// A rendered backtrace from its first frame outside `std::backtrace`'s
/// own. A frame is its numbered line and the `at` lines under it.
fn past_the_capture(trace: &str) -> String {
    let mut frames: Vec<String> = Vec::new();
    for line in trace.lines() {
        let numbered = line
            .trim_start()
            .split_once(": ")
            .is_some_and(|(number, _)| number.bytes().all(|b| b.is_ascii_digit()));
        match frames.last_mut() {
            Some(frame) if !numbered => {
                frame.push('\n');
                frame.push_str(line);
            }
            _ => frames.push(line.to_owned()),
        }
    }
    let capture = |frame: &String| {
        frame
            .lines()
            .next()
            .is_some_and(|symbol| symbol.contains("std::backtrace"))
    };
    let past = frames.iter().position(|frame| !capture(frame)).unwrap_or(0);
    frames.into_iter().skip(past).collect::<Vec<_>>().join("\n")
}

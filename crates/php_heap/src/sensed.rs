//! What a debug build's sensors in this crate found (design §10): the
//! one place a defect the heap senses waits until the engine asks, at
//! its next sensing point and at the request's end
//! ([`crate::Heap::take_sensed`]). A sensor records here and goes on;
//! it never decides how the request ends, and nothing it records is
//! dropped unasked.
//!
//! The record is the request's heap's, a [`Record`] its allocator owns,
//! and not the thread's: no engine state is per thread (§5.14). The
//! sensor that writes it most is the refcounted header's increment and
//! decrement, which every path reaches and which cannot name its heap,
//! so the allocator writes the record's address into the header of
//! every chunk it lays down, and a sensor reaches it from the block it
//! found through the block's chunk (`mm::freed::record`), as php reaches
//! a block's heap through `chunk->heap` (`Z/Zend/zend_alloc.c:1454`).
//!
//! A value that names no block of a chunk, or none at all, reaches the
//! record through a [`Sensor`], a way to it the engine takes from
//! the heap where the value is made: an owner the engine hands a native
//! carries one, so the owner dropped unconsumed is recorded whatever it
//! owns, a huge string, an interned one or an integer among them.
//!
//! The first defect is kept and a later one is not: the first ends the
//! request where the engine next asks, and what follows it is read
//! through memory that defect has already made untrustworthy. A release
//! build senses nothing and keeps nothing, and compiles none of this.

use crate::Fatal;

/// What a refcount operation was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefOp {
    Addref,
    Release,
}

/// One defect a debug build sensed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sensed {
    /// A refcount operation refused because the block it named was one
    /// the heap had freed: the block, the operation and the line that
    /// asked for it.
    FreedUse {
        at: usize,
        op: RefOp,
        site: &'static std::panic::Location<'static>,
    },
    /// A write into a freed bin slot while its bin's quarantine held it
    /// (`crate::mm`), found as the slot left: the block and its bin's
    /// size. Nothing names the line that wrote.
    FreedWrite { at: usize, size: usize },
    /// An owner dropped without being handed to a sink or given up
    /// (design §5.1): the line that made it.
    Unconsumed {
        site: &'static std::panic::Location<'static>,
    },
    /// Blocks the heap still held where the request ended, once every
    /// block of the request's lifetime was freed by name: how many, the
    /// bytes its accounting counted in them, the first its walk met
    /// (`zend_mm_check_leaks`), the name of the [`Origin`] it was
    /// allocated for, and where allocations are traced, the frames that
    /// allocated it (`zend_leak_info`'s `filename` and `lineno`).
    Leak {
        bytes: usize,
        blocks: usize,
        first: Held,
        by: String,
        trace: Option<String>,
    },
}

/// One block a heap held where the request ended: where it stands, the
/// bytes it takes and the origin it was allocated for, what php's
/// `zend_leak_info` names first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Held {
    pub(crate) at: usize,
    pub(crate) size: usize,
    pub(crate) origin: Origin,
}

/// Whose code a debug build's heap allocates for: a number the engine
/// gives the heap where an extension's native or lifecycle hook begins
/// and takes back where it ends, its `module_number`, and which the heap
/// keeps beside every block it hands out, so a block still held where
/// the request ends is named with the extension that allocated it. It is
/// what php's debug heap keeps as `__zend_filename` and `__zend_lineno`
/// in each block's `zend_mm_debug_info` (`Z/Zend/zend_alloc.c:2311`),
/// kept per extension rather than per line, and beside the block rather
/// than in it, so `memory_get_usage()` reads what a release build's
/// does, which is what the oracle's release build reads. The default is
/// zero, `Core`'s number, which the engine's own code allocates under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
pub struct Origin(u16);

impl Origin {
    /// The origin numbered `number`.
    #[must_use]
    pub const fn new(number: u16) -> Origin {
        Origin(number)
    }

    /// Its number.
    #[must_use]
    pub const fn number(self) -> u16 {
        self.0
    }
}

impl Sensed {
    /// The `InternalError` fatal that ends the request, naming what was
    /// sensed and where.
    fn into_fatal(self) -> Fatal {
        match self {
            Sensed::FreedUse { at, op, site } => {
                let op = match op {
                    RefOp::Addref => "an addref",
                    RefOp::Release => "a release",
                };
                Fatal::Error(format!(
                    "InternalError: {op} of the block at {at:#x}, which the heap has freed, at {site}"
                ))
            }
            Sensed::FreedWrite { at, size } => Fatal::Error(format!(
                "InternalError: a write into the {size}-byte block at {at:#x} after the heap freed it, found as it left the quarantine"
            )),
            Sensed::Unconsumed { site } => Fatal::Error(format!(
                "InternalError: the owner made at {site} was dropped without being handed to a sink or given up"
            )),
            Sensed::Leak {
                bytes,
                blocks,
                first,
                by,
                trace,
            } => Fatal::Error(format!(
                "InternalError: the request ended with {blocks} {} of its heap never freed, {bytes} bytes, the first of {} bytes at {:#x}, allocated by {by}{}",
                if blocks == 1 { "block" } else { "blocks" },
                first.size,
                first.at,
                trace
                    .map(|trace| format!(" at:\n{trace}"))
                    .unwrap_or_default()
            )),
        }
    }
}

/// The first defect a debug build sensed in one heap and the engine has
/// not yet taken. Written through a shared pointer, from a chunk's
/// header, and from a [`Sensor`] wherever the owner carrying one is
/// dropped, so the defect sits in a `Mutex`, and the `AtomicBool`
/// beside it lets the engine's question after every handler be one load
/// while nothing is kept.
#[derive(Default)]
pub(crate) struct Record {
    kept: std::sync::atomic::AtomicBool,
    defect: std::sync::Mutex<Option<Sensed>>,
}

impl std::fmt::Debug for Record {
    /// Nothing of the defect: reading it would take it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Record").finish_non_exhaustive()
    }
}

impl Record {
    /// `sensed` kept for the engine's next question, unless a defect is
    /// kept already.
    pub(crate) fn record(&self, sensed: Sensed) {
        let mut defect = self
            .defect
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if defect.is_none() {
            *defect = Some(sensed);
            self.kept.store(true, std::sync::atomic::Ordering::Release);
        }
    }

    /// The defect kept, as the `InternalError` fatal that ends the
    /// request, and taken, so it is raised once.
    pub(crate) fn take(&self) -> Option<Fatal> {
        if !self.kept.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        let mut defect = self
            .defect
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.kept.store(false, std::sync::atomic::Ordering::Release);
        defect.take().map(Sensed::into_fatal)
    }
}

/// A debug build's way to one heap's sensed record, for a value that
/// cannot reach it through a chunk: what the engine's owner type carries
/// so its `Drop` can record an owner dropped unconsumed. Cloning it
/// shares the record; the record stands while any sensor does, so a
/// sensor that outlives its heap records into a record nobody takes and
/// never into freed memory.
///
/// The record is shared by count across threads, so a sensor moved off
/// the request's thread with the owner carrying it, and dropped there,
/// is sound: what it can do is record a defect for the engine to raise.
#[derive(Clone)]
pub struct Sensor(std::sync::Arc<Record>);

impl std::fmt::Debug for Sensor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sensor").finish_non_exhaustive()
    }
}

impl Sensor {
    /// An owner made at `site` dropped without being handed to a sink
    /// or given up, kept for the engine's next question.
    pub fn unconsumed(&self, site: &'static std::panic::Location<'static>) {
        self.0.record(Sensed::Unconsumed { site });
    }

    /// A record of its own, for a new heap.
    pub(crate) fn new() -> Sensor {
        Sensor(std::sync::Arc::default())
    }

    /// The record, for a chunk's header to name.
    pub(crate) fn record_at(&self) -> *const Record {
        std::sync::Arc::as_ptr(&self.0)
    }

    /// `sensed` kept, as [`Record::record`] keeps it.
    pub(crate) fn record(&self, sensed: Sensed) {
        self.0.record(sensed);
    }

    /// The defect kept, as [`Record::take`] gives it.
    pub(crate) fn take(&self) -> Option<Fatal> {
        self.0.take()
    }
}

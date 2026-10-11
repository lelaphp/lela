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
//! An owner the engine's owner type holds reaches it the same way, from
//! the block it owns, and so does the table of the lines that made the
//! owners standing ([`crate::owners`]), which the record keeps.
//!
//! The first defect is kept and a later one is not: the first ends the
//! request where the engine next asks, and what follows it is read
//! through memory that defect has already made untrustworthy. A release
//! build senses nothing and keeps nothing, and compiles none of this.

use crate::Fatal;

/// What was asked of a block: a move of its refcount, or a read of a
/// string's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Use {
    Addref,
    Release,
    Read,
}

/// One defect a debug build sensed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Sensed {
    /// A refcount operation or a read of a string's bytes refused
    /// because the block it named was one the heap had freed: the block,
    /// what was asked and the line that asked for it.
    FreedUse {
        at: usize,
        op: Use,
        site: &'static std::panic::Location<'static>,
    },
    /// A write into a freed bin slot while its bin's quarantine held it
    /// (`crate::mm`), found as the slot left: the block and its bin's
    /// size. Nothing names the line that wrote.
    FreedWrite { at: usize, size: usize },
    /// A free of a bin slot its bin's quarantine held already (`crate::mm`),
    /// refused: the block and its bin's size.
    FreedTwice { at: usize, size: usize },
    /// An owner dropped without being handed to a sink or given up
    /// (design §5.1): the block it owned, the line that made it, which
    /// is the latest made of that block's owners standing, and the lines
    /// of the others ([`crate::owners`]).
    Unconsumed {
        at: usize,
        site: Option<&'static std::panic::Location<'static>>,
        others: Vec<&'static std::panic::Location<'static>>,
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
        named: String,
        trace: Option<String>,
    },
}

/// One block a heap held where the request ended: where it stands and the
/// bytes it takes, what php's `zend_leak_info` names first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Held {
    pub(crate) at: usize,
    pub(crate) size: usize,
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
/// One `u16`, so a chunk's zeroed table of them reads as `Core`'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Hash)]
#[repr(transparent)]
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
                    Use::Addref => "an addref",
                    Use::Release => "a release",
                    Use::Read => "a read",
                };
                Fatal::error(format!(
                    "InternalError: {op} of the block at {at:#x}, which the heap has freed, at {site}"
                ))
            }
            Sensed::FreedWrite { at, size } => Fatal::error(format!(
                "InternalError: a write into the {size}-byte block at {at:#x} after the heap freed it, found as it left the quarantine"
            )),
            Sensed::FreedTwice { at, size } => Fatal::error(format!(
                "InternalError: a free of the {size}-byte block at {at:#x}, which the heap has freed"
            )),
            Sensed::Unconsumed { at, site, others } => {
                let made = site.map_or_else(
                    || format!("an owner of the block at {at:#x}"),
                    |site| format!("the owner made at {site}"),
                );
                let others = if others.iter().all(|other| Some(*other) == site) {
                    String::new()
                } else {
                    let lines: Vec<String> = others.iter().map(ToString::to_string).collect();
                    format!(
                        ", or one of the other owners of its block, made at {}",
                        lines.join(", ")
                    )
                };
                Fatal::error(format!(
                    "InternalError: {made} was dropped without being handed to a sink or given up{others}"
                ))
            }
            Sensed::Leak {
                bytes,
                blocks,
                first,
                named,
                trace,
            } => Fatal::error(format!(
                "InternalError: the request ended with {blocks} {} of its heap never freed, {bytes} bytes, the first of {} bytes at {:#x}, allocated by {named}{}",
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
/// not yet taken, and the lines that made the owners standing
/// ([`crate::owners`]). Written through a shared pointer, from a chunk's
/// header, wherever a block of the chunk is used or an owner of one is
/// dropped, so each sits in a `Mutex`, and the `AtomicBool` beside the
/// defect lets the engine's question after every handler be one load
/// while nothing is kept.
///
/// While a fatal leaves the request (`zend_bailout`), an owner dropped
/// unconsumed is not a defect: php's `longjmp` leaves a native's locals
/// for the heap to discard, and the native that drops its owners as it
/// returns the fatal's error is doing what php's jump does. A refused
/// refcount change is a defect on every path, the fatal's included.
#[derive(Default)]
pub(crate) struct Record {
    kept: std::sync::atomic::AtomicBool,
    defect: std::sync::Mutex<Option<Sensed>>,
    fatal_leaving: std::sync::atomic::AtomicBool,
    owners: std::sync::Mutex<crate::owners::Table>,
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

    /// Whether a fatal is leaving the request, so an owner dropped
    /// unconsumed is not a defect.
    pub(crate) fn fatal_is_leaving(&self) -> bool {
        self.fatal_leaving
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// The table of the lines that made the owners standing.
    pub(crate) fn owners(&self) -> std::sync::MutexGuard<'_, crate::owners::Table> {
        self.owners
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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

/// A debug build's hold on one heap's sensed record, the heap's own:
/// shared, so the address every chunk's header names stands while the
/// heap moves.
pub(crate) struct Sensor(std::sync::Arc<Record>);

impl std::fmt::Debug for Sensor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sensor").finish_non_exhaustive()
    }
}

impl Sensor {
    /// Whether a fatal is leaving the request, as the engine says where
    /// `zend_bailout` begins and where its `zend_try` catches it.
    pub(crate) fn fatal_leaving(&self, leaving: bool) {
        self.0
            .fatal_leaving
            .store(leaving, std::sync::atomic::Ordering::Release);
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

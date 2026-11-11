//! The owner of every published trace (`jit-design.md` §4.5): one
//! [`TraceTable`] per epoch, one space of [`TraceId`]s for roots and side
//! traces, php's three bounds, slots written once and read with nothing
//! taken, and the code arena, `code`, the one place a trace's bytes are
//! written.
//!
//! **One id space, php's.** A root and a side trace are numbered from one
//! number, from one, as php's `ZEND_JIT_TRACE_NUM` numbers
//! `zend_jit_traces`, and an id is never reused while the table stands.
//! **php's three bounds**, kept as counts under the compile mutex: no trace
//! once the next id reaches `opcache.jit_max_root_traces`
//! (`TOO_MANY_TRACES`), no side once its root has
//! `opcache.jit_max_side_traces` (`TOO_MANY_CHILDREN`), and no trace whose
//! exits would bring the epoch's to `opcache.jit_max_exit_counters`
//! (`TOO_MANY_EXITS`), each checked as `zend_jit_compile_root_trace` and
//! `zend_jit_compile_side_trace` check it
//! (`Z/ext/opcache/jit/zend_jit_trace.c:7499`, `:8337-8340`, `:7314`).
//! **Slots written once.** The read side is a fixed table of segments of
//! [`SEGMENT`] slots, a segment laid down under the mutex the first time an
//! id reaches it and a slot written once by the publish that numbered it,
//! so [`TraceTable::get`] is two acquire loads and borrows the table.
//!
//! **The table is the epoch's** ([`of`]): made the first time a request
//! of the epoch asks for it, dropped with the epoch, so a restart's epoch
//! starts with none, as `zend_jit_restart` empties php's buffer, and no
//! trace is freed alone. A table whose next id reached its bound is full
//! ([`TraceTable::full`]): the next hot root is refused with
//! `TOO_MANY_TRACES` and stops it ([`TraceTable::stop`]), after which no
//! word of the epoch is claimed until the restart, as php's
//! `zend_jit_trace_hot_root` refuses one root and
//! `zend_jit_stop_counter_handlers` stops its counters
//! (`Z/ext/opcache/jit/zend_jit_trace.c:8164-8167`). A full arena stops
//! nothing: each publish that finds no room is [`Full::Code`], php's
//! `COMPILER_ERROR`, and the next may find room for a smaller body
//! (`jit-design.md` §4.5, #3015's ruling).
//!
//! **php's `PERF` and `PERF_DUMP` writers** ([`Perf`]) are the table's,
//! written under the mutex by the publish that places the code.
//!
//! **A fork's child takes its own code** (`jit-design.md` §4.4): every
//! table is entered among what a fork holds ([`tier::Inherited`]), which
//! takes its compile mutex for the fork and copies its arena's chunks in
//! use, and the child maps the copy over its arena at the same addresses.
//! Where the copy or the mapping is refused, the child's table keeps no
//! arena and publishes no more code, so neither process writes a page the
//! other runs: the one that asked writes only into chunks and tails that
//! were free at the fork, which the child, publishing nothing, never
//! enters, and no chunk of a standing table is freed before the table.
//! The copy is the asking process's to pay, at each fork, for the chunks
//! in use alone.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};

use php_engine::tier::{self, Epoch, FuncRef, MapPtrRaw, PlainCall, Request, Word};
use php_unit::OplineIdx;

use crate::lower::{
    Compiled, Door, Entrance, Escapes, Fuzzed, Linked, Mark, Numbered, Stands, engine_offset,
};
use crate::policy::{Place, Policy};
use crate::record::{Entered, Start, Stop};
use crate::types::{ExitId, Grows, Landing};

#[allow(
    unsafe_code,
    reason = "seams.toml rule unsafe.homes: php_jit::table::code"
)]
pub(crate) mod code;
mod perf;
#[cfg(test)]
mod tests;

use code::{Arena, Code, NullTable, Pages};
pub(crate) use perf::{Outputs, Perf, Usage, Used};

/// The `Pages` adapter a trace's code is mapped through on this target
/// (`jit-design.md` §4.4): `MAP_JIT` on macOS aarch64, one anonymous
/// mapping and its `mach_vm_remap` alias on macOS x86-64, one sealed
/// `memfd` mapped twice on Linux, on x86-64 and aarch64; under Miri the
/// heap adapter, the only one it runs; elsewhere, where `build.rs` sets no
/// `php_jit_arena`, none, so a table maps no arena and numbers each trace
/// it publishes with no code.
#[cfg(miri)]
pub(crate) type Native = code::HeapPages;
#[cfg(all(not(miri), php_jit_arena, target_os = "macos", target_arch = "aarch64"))]
pub(crate) type Native = code::MapJit;
#[cfg(all(not(miri), php_jit_arena, target_os = "macos", target_arch = "x86_64"))]
pub(crate) type Native = code::Remapped;
#[cfg(all(not(miri), php_jit_arena, target_os = "linux"))]
pub(crate) type Native = code::Memfd;
#[cfg(all(not(miri), not(php_jit_arena)))]
pub(crate) type Native = code::Unmapped;

/// The table an epoch keeps: over this target's pages.
pub(crate) type Table = TraceTable<Native>;

/// How many slots one segment of the read side holds.
pub(crate) const SEGMENT: usize = 1024;

/// One published trace's number (`jit-design.md` §3.2, §4.5): one space
/// for roots and side traces, whichever word holds it, below `2^24`, the
/// width a word keeps it in. Zero is no trace, as php's trace numbers
/// count from one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct TraceId(u32);

impl TraceId {
    /// The first number past the ones a word can hold.
    pub(crate) const LIMIT: u32 = 1 << 24;

    /// Trace number `n`; none for zero and past [`TraceId::LIMIT`].
    pub(crate) fn new(n: u32) -> Option<TraceId> {
        (n != 0 && n < TraceId::LIMIT).then_some(TraceId(n))
    }

    /// The number.
    pub(crate) fn get(self) -> u32 {
        self.0
    }

    /// Its place in a table, and in a request's rows of a table's traces
    /// (`enter::Rows`): the segment, and the slot in it.
    pub(crate) fn slot(self) -> (usize, usize) {
        let n = self.0 as usize;
        (n / SEGMENT, n % SEGMENT)
    }

    /// Its place among the rows of [`Counts`], which start at one.
    fn row(self) -> usize {
        self.0 as usize - 1
    }
}

/// What a profiler names a trace by, but its number: php's
/// `zend_jit_trace_name` (`Z/ext/opcache/jit/zend_jit_trace.c:237-252`)
/// over the op array the trace starts in, `Class::method`, the function,
/// or for a file's own code the file, and the line of its first opline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TraceName {
    pub(crate) at: Box<[u8]>,
    pub(crate) line: u32,
}

impl TraceName {
    /// The name of a trace that starts where `place` stands, the place
    /// its `TRACE start` line names, whose line is php's first opline's.
    pub(crate) fn of(place: &Place<'_>) -> TraceName {
        let at = match (place.scope, place.function) {
            (Some(scope), Some(function)) => [scope, b"::", function].concat(),
            (None, Some(function)) => function.to_vec(),
            (_, None) => place.file.to_vec(),
        };
        TraceName {
            at: at.into_boxed_slice(),
            line: place.line,
        }
    }
}

/// What a trace is (`jit-design.md` §4.5): today one recorded path; what
/// month three builds over hot traces is a second kind, entered from the
/// same word by the same id through the same entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Trace,
}

/// One published trace (`jit-design.md` §4.5): its code, final, and what
/// the crossing and the dispatch read of it, none of it written again.
pub(crate) struct Trace<P: Pages = Native> {
    pub(crate) code: Code<P>,
    entry_offset: usize,
    body_offset: usize,
    /// How an escape enters it where the request's door does not lead
    /// here: its body and its number (`lower::Escapes`).
    pub(crate) entrance: Entrance,
    /// The ways its code hands on to another trace, whose words stand in
    /// the request's addresses of it (`lower::Door`).
    pub(crate) doors: Box<[Door]>,
    /// Its number as its answers write it, which the publish writes once
    /// and its code reads (`lower::Numbered`).
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "read by the trace's code alone")
    )]
    pub(crate) number: Numbered,
    pub(crate) id: TraceId,
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "a region over hot traces is month three's kind")
    )]
    pub(crate) kind: Kind,
    pub(crate) start: Start,
    /// The root it grew from: itself for a root.
    pub(crate) root: TraceId,
    pub(crate) exits: Box<[Exit]>,
    /// The exit it ends through where it does not loop.
    pub(crate) leaves: Option<ExitId>,
    /// Whether a side trace's link to it tests the interrupt first
    /// (`lower::Compiled::checks_interrupt`).
    pub(crate) checks_interrupt: bool,
    /// Where a loop whose interrupt test takes php's stub stands its
    /// frame as it answers `lower::INTERRUPTED`: its header's opline.
    pub(crate) stub: Option<OplineIdx>,
    /// The op array and opline its first recorded opline stands at.
    pub(crate) opens: Option<(FuncRef, OplineIdx)>,
    /// Whether its code reads where the frame stands as it enters, which
    /// `enter` hands it as a kernel reports it (`lower::Compiled::entry_stood`).
    pub(crate) entry_stood: bool,
    /// Where it is entered at a loop's body too, the opline its back edge
    /// lands on.
    pub(crate) body: Option<OplineIdx>,
    /// The runtime-cache slots its code reads, which `enter` touches on a
    /// request's first entry of it.
    pub(crate) map_ptrs: Box<[MapPtrRaw]>,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "the disassembly reads them; no host prints one yet"
        )
    )]
    pub(crate) marks: Box<[Mark]>,
    /// The bytes its code fails each guard once by, under exit-fuzz:
    /// held for as long as the code that names them.
    #[expect(dead_code, reason = "read and written by the trace's code alone")]
    pub(crate) fuzzed: Fuzzed,
    /// The plain calls its code pushes in place, whose headers `enter`
    /// lays out in the request ([`lower::Compiled::calls`]).
    pub(crate) calls: Box<[PlainCall]>,
    /// The callees of the calls its code makes in place whose caches
    /// `enter` asks are laid down (`lower::Compiled::callee_caches`).
    pub(crate) callee_caches: Box<[FuncRef]>,
    /// Every trace an exit links to, each once, ascending.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "what a disassembly and the PERF_DUMP walk read")
    )]
    pub(crate) links: Box<[TraceId]>,
}

impl<P: Pages> Trace<P> {
    /// The address the trace is entered at, in the executable view: its
    /// prologue, which `enter` calls.
    pub(crate) fn entry(&self) -> std::ptr::NonNull<u8> {
        self.code
            .entry()
            .map_addr(|addr| addr.saturating_add(self.entry_offset))
    }

    /// The address of its body, in the executable view, which its prologue
    /// calls and a jump from another trace enters.
    pub(crate) fn body(&self) -> std::ptr::NonNull<u8> {
        self.code
            .entry()
            .map_addr(|addr| addr.saturating_add(self.body_offset))
    }

    /// Where the words of `door` stand among the request's addresses of
    /// the trace, past its calls' headers and its cache slots; none for a
    /// door its code does not hold.
    pub(crate) fn door_words(&self, door: Door) -> Option<usize> {
        let at = self.doors.iter().position(|made| *made == door)?;
        (at.checked_mul(crate::lower::DOOR_WORDS)?)
            .checked_add(self.calls.len())?
            .checked_add(self.map_ptrs.len())
    }

    /// What a link to it jumps to (`lower::Linked`), from a trace that
    /// `start` grew: its number, and whether the jump tests the
    /// interrupt first, which php's does where a side trace links to its
    /// own root and the root loops other than by recursive returns
    /// (`zend_jit_trace.c:7209-7212`, `:7285-7303`).
    pub(crate) fn linked(&self, from_root: Option<TraceId>) -> Linked {
        let opens = self.opens.and_then(|(func, opline)| {
            Some(Stands {
                func: engine_offset(func)?,
                opline: opline.0,
            })
        });
        Linked {
            trace: self.id.get(),
            interrupt: from_root == Some(self.id) && self.checks_interrupt,
            opens,
        }
    }
}

impl<P: Pages> std::fmt::Debug for Trace<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Trace")
            .field("id", &self.id)
            .field("start", &self.start)
            .field("root", &self.root)
            .field("exits", &self.exits.len())
            .finish_non_exhaustive()
    }
}

/// One exit of a published trace (the glossary's Exit): where the
/// interpreter goes on, what the trampoline does first, the trace it
/// links to, and its own word, zero at publication, which grows a side
/// trace by the machine that grew the root.
#[derive(Debug)]
pub(crate) struct Exit {
    pub(crate) opline: Option<OplineIdx>,
    pub(crate) landing: Landing,
    pub(crate) link: Option<TraceId>,
    /// Whether a side trace may grow from it, php's `flags`.
    pub(crate) grows: Grows,
    /// What it knows of the slots of the frame it lands in, php's stack
    /// map, which a side trace grown from it enters with.
    pub(crate) entered: Box<[Entered]>,
    pub(crate) word: Word,
}

/// Which bound a publish met (`jit-design.md` §4.5): php's
/// `TOO_MANY_TRACES`, `TOO_MANY_CHILDREN` and `TOO_MANY_EXITS`, and a code
/// arena with no room for the body, php's full buffer, where its emit
/// fails with `IR_ERROR_CODE_MEM_OVERFLOW`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Full {
    Traces,
    Children,
    Exits,
    Code,
}

impl Full {
    /// The stop php settles the word with: a bound's, which blacklists
    /// it, or for a full arena `COMPILER_ERROR`, which counts one attempt
    /// (`zend_jit_trace.c:7612-7622`, `:8452-8461`).
    pub(crate) fn stop(self) -> Stop {
        match self {
            Full::Traces => Stop::TooManyTraces,
            Full::Code => Stop::CompilerError,
            Full::Children => Stop::TooManyChildren,
            Full::Exits => Stop::TooManyExits,
        }
    }
}

/// Why a publish published nothing: a bound, or a [`Compiled`] that does
/// not hold together, which is the `Backend`'s mistake and php's
/// `COMPILER_ERROR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unpublished {
    Full(Full),
    Malformed(&'static str),
}

impl From<Full> for Unpublished {
    fn from(full: Full) -> Unpublished {
        Unpublished::Full(full)
    }
}

/// The table's bounds, read once from the `Policy`.
#[derive(Debug, Clone, Copy)]
struct Bounds {
    /// No id is this or past it: `opcache.jit_max_root_traces`, at most
    /// the ids a word holds.
    traces: u32,
    sides: u32,
    exits: u32,
}

/// What a publish counts, php's `ZEND_JIT_EXIT_COUNTERS` and each root's
/// `child_count`, beside the root of every id numbered.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Counts {
    exits: u32,
    /// By id, from one: the trace's root, and for a root how many side
    /// traces it has.
    rows: Vec<Row>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Row {
    root: TraceId,
    sides: u32,
}

/// A trace the bounds admit: its id and its root.
#[derive(Debug, Clone, Copy)]
struct Admitted {
    id: TraceId,
    root: TraceId,
}

impl Counts {
    /// Whether a trace of `start` with `exits` exits fits the bounds as
    /// the trace numbered `next`, held in php's order.
    fn admit(
        &self,
        bounds: Bounds,
        next: u32,
        start: Start,
        exits: u32,
    ) -> Result<Admitted, Unpublished> {
        let id = TraceId::new(next)
            .filter(|id| id.get() < bounds.traces)
            .ok_or(Full::Traces)?;
        let root = match start {
            Start::Side { parent, .. } => {
                let root = (parent < id)
                    .then(|| self.rows.get(parent.row()))
                    .flatten()
                    .map(|row| row.root)
                    .ok_or(Unpublished::Malformed("a side trace of no trace numbered"))?;
                let sides = self.rows.get(root.row()).map_or(0, |row| row.sides);
                if sides >= bounds.sides {
                    return Err(Full::Children.into());
                }
                root
            }
            Start::Loop | Start::Enter | Start::Return => id,
        };
        if self.exits.saturating_add(exits) >= bounds.exits {
            return Err(Full::Exits.into());
        }
        Ok(Admitted { id, root })
    }

    /// The admitted trace counted: its row, its root's side, its exits.
    fn count(&mut self, admitted: Admitted, exits: u32) {
        self.exits = self.exits.saturating_add(exits);
        self.rows.push(Row {
            root: admitted.root,
            sides: 0,
        });
        if admitted.root != admitted.id
            && let Some(root) = self.rows.get_mut(admitted.root.row())
        {
            root.sides = root.sides.saturating_add(1);
        }
    }
}

/// One segment of the read side: [`SEGMENT`] slots, each written once.
type Segment<P> = Box<[OnceLock<Box<Trace<P>>>]>;

/// What the compile mutex guards: the arena, none where it could not
/// be mapped, the counts and where the `PERF` lines go.
struct Writer<P: Pages> {
    arena: Option<Arena<P>>,
    counts: Counts,
    perf: Perf,
}

/// The epoch's traces (`jit-design.md` §4.5): a read side of segments of
/// slots, each written once and read with nothing taken, and a write side
/// behind the one mutex a publish takes.
pub(crate) struct TraceTable<P: Pages = Native> {
    segments: Box<[OnceLock<Segment<P>>]>,
    /// The entrance of each trace by its number, php's `zend_jit_traces`
    /// as a trace's escape reads it where its door does not lead to the
    /// landing it reached (`lower::Escapes`): null until the publish that
    /// numbered the trace placed it, then the trace's [`Entrance`], written
    /// once with release. Unwritten at its length, as php's table is.
    entrances: NullTable<Entrance>,
    /// The id the next trace takes, php's `ZEND_JIT_TRACE_NUM`: written
    /// under the mutex after the trace's slot, read with nothing taken.
    next: AtomicU32,
    /// Whether a hot root found the table full: php's
    /// `ZCSG(jit_counters_stopped)`.
    stopped: AtomicBool,
    bounds: Bounds,
    compile: Mutex<Writer<P>>,
    /// What the published traces take, the counts of the `Perf` behind
    /// the compile mutex, read with nothing taken, as php reads `*dasm_ptr`.
    used: Arc<Used>,
    /// php's analysis of each op array a trace was built through.
    analyses: crate::analysis::Analyses,
}

impl<P: Pages> std::fmt::Debug for TraceTable<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TraceTable")
            .field("next", &self.next)
            .field("bounds", &self.bounds)
            .finish_non_exhaustive()
    }
}

impl<P: Pages> TraceTable<P> {
    /// An empty table under `policy`'s bounds, its arena as many bytes as
    /// php's buffer, `opcache.jit_buffer_size` rounded up to the page
    /// ([`code::capacity`], #3112's ruling), its `PERF` lines and `PERF_DUMP` records written as
    /// the engine's `outputs` started them. An arena that cannot be
    /// mapped is none: each publish numbers its trace and keeps no code
    /// (revision 27).
    ///
    /// A buffer smaller than a chunk holds what php's holds, so a trace
    /// that fits php's fits this one:
    ///
    /// ```php
    /// function twice($n) { return $n * 2; }
    /// echo twice(21), "\n";
    /// ```
    /// ```expect exit=0 regime=jit-small-buffer process capture
    /// 42
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:3
    /// ---- TRACE 1 stop (return)
    /// ---- TRACE 1 compiled
    /// ```
    ///
    /// A loop php's buffer cannot hold is a compile that finds no room,
    /// php's `COMPILER_ERROR`, at each of its sixteen attempts, which
    /// blacklist it (#3015's and #3112's rulings):
    ///
    /// ```php
    /// $s = 0;
    /// for ($i = 0; $i < 100; $i++) {
    ///     $s += $i * 3; $s += $i * 4; $s += $i * 5; $s += $i * 6; $s += $i * 7;
    ///     $s += $i * 8; $s += $i * 9; $s += $i * 10; $s += $i * 11; $s += $i * 12;
    ///     $s += $i * 13; $s += $i * 14; $s += $i * 15; $s += $i * 16; $s += $i * 17;
    ///     $s += $i * 18; $s += $i * 19; $s += $i * 20; $s += $i * 21; $s += $i * 22;
    ///     $s += $i * 23; $s += $i * 24; $s += $i * 25; $s += $i * 26; $s += $i * 27;
    ///     $s += $i * 28; $s += $i * 29; $s += $i * 30; $s += $i * 31; $s += $i * 32;
    ///     $s += $i * 33; $s += $i * 34; $s += $i * 35; $s += $i * 36; $s += $i * 37;
    ///     $s += $i * 38; $s += $i * 39; $s += $i * 40; $s += $i * 41; $s += $i * 42;
    ///     $s += $i * 43; $s += $i * 44; $s += $i * 45; $s += $i * 46; $s += $i * 47;
    ///     $s += $i * 48; $s += $i * 49; $s += $i * 50; $s += $i * 51; $s += $i * 52;
    ///     $s += $i * 53; $s += $i * 54; $s += $i * 55; $s += $i * 56; $s += $i * 57;
    ///     $s += $i * 58; $s += $i * 59; $s += $i * 60; $s += $i * 61; $s += $i * 62;
    ///     $s += $i * 63; $s += $i * 64; $s += $i * 65; $s += $i * 66; $s += $i * 67;
    ///     $s += $i * 68; $s += $i * 69; $s += $i * 70; $s += $i * 71; $s += $i * 72;
    ///     $s += $i * 73; $s += $i * 74; $s += $i * 75; $s += $i * 76; $s += $i * 77;
    ///     $s += $i * 78; $s += $i * 79; $s += $i * 80; $s += $i * 81; $s += $i * 82;
    ///     $s += $i * 83; $s += $i * 84; $s += $i * 85; $s += $i * 86; $s += $i * 87;
    ///     $s += $i * 88; $s += $i * 89; $s += $i * 90; $s += $i * 91; $s += $i * 92;
    ///     $s += $i * 93; $s += $i * 94; $s += $i * 95; $s += $i * 96; $s += $i * 97;
    ///     $s += $i * 98; $s += $i * 99; $s += $i * 100; $s += $i * 101; $s += $i * 102;
    /// }
    /// echo $s, "\n";
    /// ```
    /// ```expect regime=jit-small-buffer process
    /// 25987500
    /// ```
    /// ```stderr
    /// ---- TRACE 1 start (enter) $main() %s:2
    /// ---- TRACE 1 abort (inner loop)
    /// ---- TRACE 1 start (loop) $main() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 abort (JIT compilation error)
    ///
    /// ---- TRACE 1 start (loop) $main() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 abort (JIT compilation error)
    ///
    /// ---- TRACE 1 start (loop) $main() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 abort (JIT compilation error)
    ///
    /// ---- TRACE 1 start (loop) $main() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 abort (JIT compilation error)
    ///
    /// ---- TRACE 1 start (loop) $main() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 abort (JIT compilation error)
    ///
    /// ---- TRACE 1 start (loop) $main() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 abort (JIT compilation error)
    ///
    /// ---- TRACE 1 start (loop) $main() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 abort (JIT compilation error)
    ///
    /// ---- TRACE 1 start (loop) $main() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 abort (JIT compilation error)
    ///
    /// ---- TRACE 1 start (loop) $main() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 abort (JIT compilation error)
    ///
    /// ---- TRACE 1 start (loop) $main() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 abort (JIT compilation error)
    ///
    /// ---- TRACE 1 start (loop) $main() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 abort (JIT compilation error)
    ///
    /// ---- TRACE 1 start (loop) $main() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 abort (JIT compilation error)
    ///
    /// ---- TRACE 1 start (loop) $main() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 abort (JIT compilation error)
    ///
    /// ---- TRACE 1 start (loop) $main() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 abort (JIT compilation error)
    ///
    /// ---- TRACE 1 start (loop) $main() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 abort (JIT compilation error)
    ///
    /// ---- TRACE 1 start (loop) $main() %s:3
    /// ---- TRACE 1 stop (loop)
    /// ---- TRACE 1 abort (JIT compilation error)
    /// ---- TRACE 1 blacklisted
    ///
    /// ```
    pub(crate) fn new(policy: &Policy, outputs: &Outputs) -> TraceTable<P> {
        TraceTable::with_perf(policy, Perf::of(outputs))
    }

    /// [`TraceTable::new`] with the writers given.
    pub(crate) fn with_perf(policy: &Policy, perf: Perf) -> TraceTable<P> {
        let arena = Arena::new(code::capacity(policy.buffer_size())).ok();
        let bounds = Bounds {
            traces: policy.max_root_traces().min(TraceId::LIMIT),
            sides: policy.max_side_traces(),
            exits: policy.max_exit_counters(),
        };
        let segments = (bounds.traces as usize).div_ceil(SEGMENT);
        let used = perf.used();
        TraceTable {
            segments: (0..segments).map(|_| OnceLock::new()).collect(),
            entrances: NullTable::new(bounds.traces as usize),
            next: AtomicU32::new(1),
            stopped: AtomicBool::new(false),
            bounds,
            compile: Mutex::new(Writer {
                arena,
                counts: Counts::default(),
                perf,
            }),
            used,
            analyses: crate::analysis::Analyses::default(),
        }
    }

    /// The analyses of the op arrays the epoch's traces were built
    /// through ([`crate::analysis`]).
    pub(crate) fn analyses(&self) -> &crate::analysis::Analyses {
        &self.analyses
    }

    /// What the traces published into this epoch's table so far take
    /// (#2961), each trace counted last in its publish: what
    /// `opcache_get_status()['jit']` reads of the buffer
    /// ([`crate::policy::Policy::status`]); atomics read with nothing
    /// taken, so a reader never waits on a compile.
    pub(crate) fn usage(&self) -> Usage {
        self.used.now()
    }

    /// The trace `id` names: two acquire loads, nothing taken. None for
    /// an id no publish wrote, which is "not compiled" to every reader,
    /// the window `jit-design.md` §3.2 names included.
    pub(crate) fn get(&self, id: TraceId) -> Option<&Trace<P>> {
        let (segment, slot) = id.slot();
        let slots = self.segments.get(segment)?.get()?;
        slots.get(slot)?.get().map(Box::as_ref)
    }

    /// The number the next trace published takes, which a `TRACE start`
    /// line names before the recording, as php's names
    /// `ZEND_JIT_TRACE_NUM`; none past the numbers a word holds.
    pub(crate) fn next(&self) -> Option<TraceId> {
        TraceId::new(self.next.load(Ordering::Relaxed))
    }

    /// Whether no trace more can be published: the next id reached
    /// `opcache.jit_max_root_traces`, php's `ZEND_JIT_TRACE_NUM >=
    /// max_root_traces` at a hot root. A full arena is not this.
    pub(crate) fn full(&self) -> bool {
        self.next.load(Ordering::Relaxed) >= self.bounds.traces
    }

    /// The table full, its counters stopped: php's
    /// `zend_jit_stop_counter_handlers`, which the hot root that found it
    /// full calls, so no word of the epoch is claimed until the restart.
    pub(crate) fn stop(&self) {
        self.stopped.store(true, Ordering::Relaxed);
    }

    /// Whether the root `parent` grew from, or `parent` itself where it
    /// is a root, has `opcache.jit_max_side_traces` side traces, so a side
    /// of it is refused with `TOO_MANY_CHILDREN`, as php's
    /// `zend_jit_trace_hot_side` refuses one before it records
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:8521-8525`).
    pub(crate) fn children_full(&self, parent: TraceId) -> bool {
        let writer = self.writer();
        let rows = &writer.counts.rows;
        let root = rows.get(parent.row()).map_or(parent, |row| row.root);
        rows.get(root.row())
            .is_some_and(|row| row.sides >= self.bounds.sides)
    }

    /// Whether [`TraceTable::stop`] stopped it.
    pub(crate) fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Relaxed)
    }

    /// The compile mutex, taken as a thread that panicked holding it left
    /// it: a publish writes what the mutex guards only once nothing it
    /// does after can fail or panic.
    fn writer(&self) -> std::sync::MutexGuard<'_, Writer<P>> {
        self.compile.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `compiled` published as the trace `start` grew (`jit-design.md`
    /// §4.5): under the compile mutex, the bounds held in php's order, one
    /// [`Exit`] built per exit with a zero word, each link held to an id
    /// this table numbered, the bytes copied into the arena, the `PERF`
    /// line and `PERF_DUMP` record written where `opcache.jit_debug`
    /// asks, and the trace published by the release store that writes
    /// its slot, then the next id's. Transactional: the counts, the slot
    /// and the next id are written last, after everything that can fail
    /// or panic, so a publish that does either leaves them as they were.
    /// Where the table has no arena, the trace is numbered and counted
    /// and its slot left empty, so its word reads compiled and every
    /// reader takes it as not compiled (`jit-design.md` §3.2).
    pub(crate) fn publish(&self, compiled: Compiled, start: Start) -> Result<TraceId, Unpublished> {
        let mut writer = self.writer();
        let exits = u32::try_from(compiled.exits.len())
            .ok()
            .filter(|n| *n <= u32::from(u16::MAX) + 1)
            .ok_or(Unpublished::Malformed("more exits than an ExitId names"))?;
        let next = self.next.load(Ordering::Relaxed);
        let admitted = writer.counts.admit(self.bounds, next, start, exits)?;
        if compiled.entry_offset >= compiled.bytes.len()
            || compiled.body_offset >= compiled.bytes.len()
        {
            return Err(Unpublished::Malformed("an entry past the trace's bytes"));
        }
        if compiled
            .leaves
            .is_some_and(|exit| usize::from(exit.0) >= compiled.exits.len())
        {
            return Err(Unpublished::Malformed("an end past the trace's exits"));
        }
        if compiled.bytes.len() > code::CHUNK || !compiled.align.is_power_of_two() {
            return Err(Unpublished::Malformed(
                "a body no chunk holds, or an alignment no power of two",
            ));
        }
        let mut links: Vec<TraceId> = compiled.exits.iter().filter_map(|e| e.link).collect();
        if links.iter().any(|link| *link >= admitted.id) {
            return Err(Unpublished::Malformed("a link to no trace numbered"));
        }
        links.sort_unstable();
        links.dedup();
        let built = (compiled.exits.iter())
            .map(|spec| Exit {
                opline: spec.opline,
                landing: spec.landing,
                link: spec.link,
                grows: spec.grows,
                entered: (spec.known.iter())
                    .map(|known| Entered {
                        var: known.slot.var,
                        tag: known.tag,
                    })
                    .collect(),
                word: Word::default(),
            })
            .collect();
        let Some(arena) = writer.arena.as_mut() else {
            // An arena this target cannot map yet (revision 27): the trace
            // numbered and counted as published, its slot left empty, so
            // its word reads compiled and every reader takes it as not.
            writer.counts.count(admitted, exits);
            self.next
                .store(admitted.id.get().saturating_add(1), Ordering::Release);
            return Ok(admitted.id);
        };
        let code = (arena.publish(&compiled.bytes, compiled.align)).map_err(|_| Full::Code)?;
        let (id, entry) = (admitted.id, code.entry());
        if !self.vacant(id) {
            return Err(Unpublished::Malformed("a slot written twice"));
        }
        writer
            .perf
            .published(id, &compiled.name, entry, &compiled.bytes);
        let (bytes, oplines) = (compiled.bytes.len(), compiled.marks.len());
        let body = entry
            .as_ptr()
            .expose_provenance()
            .saturating_add(compiled.body_offset);
        // The number its answers write, written before anything can enter
        // it: the slot below is what makes it enterable.
        compiled.number.number(id.get());
        let trace = Box::new(Trace {
            code,
            entry_offset: compiled.entry_offset,
            body_offset: compiled.body_offset,
            entrance: Entrance {
                body,
                trace: id.get(),
            },
            doors: compiled.doors,
            number: compiled.number,
            id,
            kind: Kind::Trace,
            start,
            root: admitted.root,
            exits: built,
            leaves: compiled.leaves,
            checks_interrupt: compiled.checks_interrupt,
            stub: compiled.stub,
            opens: compiled.opens,
            entry_stood: compiled.entry_stood,
            body: compiled.body,
            map_ptrs: compiled.map_ptrs,
            marks: compiled.marks,
            fuzzed: compiled.fuzzed,
            calls: compiled.calls,
            callee_caches: compiled.callee_caches,
            links: links.into_boxed_slice(),
        });
        self.fill(id, trace)?;
        self.enterable(id);
        writer.counts.count(admitted, exits);
        writer.perf.counted(bytes, oplines);
        self.next
            .store(id.get().saturating_add(1), Ordering::Release);
        Ok(id)
    }

    /// The trace `id` names entered through its entrance, written with
    /// release once the trace is in its slot, where an escape to a
    /// landing whose word names it reads it (`lower::Escapes`), as php's
    /// root publish writes the landing opline's handler
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:7595`).
    fn enterable(&self, id: TraceId) {
        let slot = usize::try_from(id.get())
            .ok()
            .and_then(|at| self.entrances.get(at));
        if let (Some(slot), Some(trace)) = (slot, self.get(id)) {
            slot.store(
                std::ptr::from_ref(&trace.entrance).cast_mut(),
                Ordering::Release,
            );
        }
    }

    /// Where the code of a trace published into this table, the epoch's,
    /// reads the trace compiled at a landing it escapes to: the epoch's
    /// landing index and this table's entrances.
    pub(crate) fn escapes(&self, epoch: &Epoch) -> Escapes {
        Escapes {
            landings: epoch.landing_index(),
            entrances: self.entrances.as_ptr().expose_provenance(),
            traces: u32::try_from(self.entrances.len()).unwrap_or(u32::MAX),
        }
    }

    /// How many segments of slots the table holds, which a request's rows
    /// of its traces hold as many of (`enter::Rows`).
    pub(crate) fn segments(&self) -> usize {
        self.segments.len()
    }

    /// What the link of a trace `start` grows, linking to `link`, jumps
    /// to: none where `link` has no code.
    pub(crate) fn linked(&self, link: TraceId, start: Start) -> Option<Linked> {
        let to = self.get(link)?;
        let root = start
            .grew()
            .and_then(|(parent, _)| self.get(parent))
            .map(|parent| parent.root);
        Some(to.linked(root))
    }

    /// Whether the slot `id` names stands unwritten, so [`TraceTable::fill`]
    /// will write it: asked before a publish writes its `PERF` line.
    fn vacant(&self, id: TraceId) -> bool {
        let (segment, slot) = id.slot();
        self.segments.get(segment).is_some_and(|slots| {
            slots
                .get()
                .is_none_or(|slots| slots.get(slot).is_some_and(|slot| slot.get().is_none()))
        })
    }

    /// `trace` written to the slot `id` names, its segment laid down the
    /// first time an id reaches it: the release store that publishes it.
    /// Under the compile mutex, so no slot is written twice.
    fn fill(&self, id: TraceId, trace: Box<Trace<P>>) -> Result<(), Unpublished> {
        let (segment, slot) = id.slot();
        let slots = (self.segments.get(segment))
            .ok_or(Unpublished::Full(Full::Traces))?
            .get_or_init(|| (0..SEGMENT).map(|_| OnceLock::new()).collect());
        (slots.get(slot))
            .and_then(|slot| slot.set(trace).ok())
            .ok_or(Unpublished::Malformed("a slot written twice"))
    }
}

impl<P: Pages + 'static> tier::Inherited for TraceTable<P> {
    /// The compile mutex, held for the fork, and the arena's chunks in use
    /// copied for the child ([`Arena::inheritance`]); in the child the copy
    /// is mapped over the arena, or, where either was refused, the arena
    /// let go, so the child writes no page its parent maps.
    fn hold(&self) -> Box<dyn FnOnce() + '_> {
        let mut writer = self.writer();
        let inheritance = writer.arena.as_ref().map(Arena::inheritance);
        Box::new(move || {
            let adopted = match (writer.arena.as_mut(), inheritance) {
                (Some(arena), Some(Ok(inheritance))) => arena.adopt(inheritance).is_ok(),
                _ => false,
            };
            if !adopted {
                writer.arena = None;
            }
        })
    }
}

/// An epoch's table under `policy`, shared, and entered among what every
/// fork holds and its child makes its own (`tier::inherit`).
pub(crate) fn made(policy: &Policy, outputs: &Outputs) -> Arc<Table> {
    let table = Arc::new(Table::new(policy, outputs));
    let entry: Weak<Table> = Arc::downgrade(&table);
    tier::inherit(entry);
    table
}

/// The trace table of the epoch `req` started in, made the first time a
/// request of it asks, over `policy`'s bounds ([`made`]); none only where
/// the epoch's slot holds another tier's value. The slot holds it shared,
/// so a request can keep it for as long as it serves (`enter::lent`).
pub(crate) fn of<'r>(req: &'r Request, policy: &Policy, outputs: &Outputs) -> Option<&'r Table> {
    tier::epoch_state(req, || made(policy, outputs)).map(Arc::as_ref)
}

/// A hold on the epoch `req` started in, its trace table made as [`of`]
/// makes it: for a caller that reads the table while it goes on to serve
/// the request, which a borrow of the request's own hold forbids.
pub(crate) fn holding(req: &Request, policy: &Policy, outputs: &Outputs) -> Epoch {
    of(req, policy, outputs);
    tier::epoch(req)
}

/// The trace table `epoch` keeps, where a request of it made one.
pub(crate) fn of_epoch(epoch: &Epoch) -> Option<&Table> {
    shared_of_epoch(epoch).map(Arc::as_ref)
}

/// [`of_epoch`], shared, for a request that keeps it.
pub(crate) fn shared_of_epoch(epoch: &Epoch) -> Option<&Arc<Table>> {
    epoch.tier_state::<Arc<Table>>()
}

/// The word of exit `exit` of trace `trace` in `epoch`'s table; the
/// sealed word, which nothing claims, where the table holds no such exit.
pub(crate) fn exit_word(epoch: &Epoch, trace: TraceId, exit: ExitId) -> &Word {
    static SEALED: Word = Word::sealed();
    of_epoch(epoch)
        .and_then(|table| table.get(trace))
        .and_then(|trace| trace.exits.get(usize::from(exit.0)))
        .map_or(&SEALED, |exit| &exit.word)
}

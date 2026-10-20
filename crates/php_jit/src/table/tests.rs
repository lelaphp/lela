//! The table at its seam (`jit-design.md` §4.5, §7's `table` row): under
//! Miri with `HeapPages`, publish and get, ids across a segment boundary,
//! `Full` for each of php's three bounds and for the arena, which stops
//! nothing, a table that stops compiling, a new epoch with no table, a publish that panics and
//! leaves the counts as they were, and a dropped table that returns its
//! chunks; natively, `mov; ret` through `publish` and `get`; and one
//! trace's `PERF` map line against php's.

use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use php_engine::tier::Epoch;
use php_unit::{OpArray, OplineIdx, Unit};

use super::code::{CHUNK, HeapPages, Unmapped};
use super::perf::{self, Captured, Loaded};
use super::{Full, Perf, TraceId, TraceName, TraceTable, Unpublished};
use crate::lower::Compiled;
use crate::policy::Place;
use crate::record::Start;
use crate::testing;
use crate::types::{ExitId, ExitSpec, Frame, Landing};

type Heap = TraceTable<HeapPages>;

/// A table over the heap adapter under `entries`, with one chunk of
/// code unless they say otherwise, writing to `perf`.
fn table_to(entries: &[(&str, &str)], perf: Perf) -> Heap {
    let mut all = entries.to_vec();
    all.push(("opcache.jit_buffer_size", "64K"));
    TraceTable::with_perf(&testing::policy(&all), perf)
}

fn table(entries: &[(&str, &str)]) -> Heap {
    table_to(entries, Perf::none())
}

fn trace(n: u32) -> TraceId {
    TraceId(n)
}

/// An exit that lands at opline 1, linking where asked.
fn exit(link: Option<TraceId>) -> ExitSpec {
    ExitSpec {
        frame: Frame::ROOT,
        opline: Some(OplineIdx(1)),
        landing: Landing::Plain,
        stores: Vec::new(),
        link,
    }
}

/// `bytes` with `exits`, entered at their start, named `f` at line 3.
fn compiled(bytes: &[u8], exits: Vec<ExitSpec>) -> Compiled {
    Compiled {
        bytes: bytes.to_vec(),
        entry_offset: 0,
        align: 4,
        exits: exits.into(),
        map_ptrs: Box::new([]),
        marks: Box::new([]),
        name: TraceName {
            at: b"f".to_vec().into_boxed_slice(),
            line: 3,
        },
    }
}

fn side(parent: u32) -> Start {
    Start::Side {
        parent: trace(parent),
        exit: ExitId(0),
    }
}

/// Two traces published and read back: numbered from one, each exit
/// built with a zero word and its link, the links listed, the bytes
/// where the code says; an id not yet published reads as none.
#[test]
fn a_published_trace_is_got_by_its_id_with_its_exits() -> Result<(), String> {
    let table = table(&[]);
    assert_eq!(table.next(), Some(trace(1)));
    let first = table.publish(compiled(&[1, 2, 3, 4], vec![exit(None)]), Start::Loop);
    assert_eq!(first, Ok(trace(1)));
    let links = vec![exit(Some(trace(1))), exit(None), exit(Some(trace(1)))];
    let second = table.publish(compiled(&[5, 6, 7, 8], links), side(1));
    assert_eq!(second, Ok(trace(2)));
    assert_eq!(table.next(), Some(trace(3)));
    let got = table.get(trace(2)).ok_or("trace 2 is published")?;
    assert_eq!((got.id, got.root, got.start), (trace(2), trace(1), side(1)));
    assert_eq!(got.exits.len(), 3);
    assert!(got.exits.iter().all(|e| {
        e.word
            .halves()
            .iter()
            .all(|h| h.load(Ordering::Relaxed) == 0)
    }));
    assert_eq!(got.exits[0].link, Some(trace(1)));
    let first = &got.exits[1];
    assert_eq!(
        (first.opline, first.landing),
        (Some(OplineIdx(1)), Landing::Plain)
    );
    assert_eq!(got.kind, super::Kind::Trace);
    assert!(got.map_ptrs.is_empty() && got.marks.is_empty());
    assert_eq!(&*got.links, &[trace(1)]);
    assert_eq!(got.code.len(), 4);
    assert_eq!(got.entry(), got.code.entry());
    assert!(
        table.get(trace(3)).is_none(),
        "numbered next, not published"
    );
    assert!(table.get(trace(0)).is_none());
    Ok(())
}

/// Ids go on across the first segment into the second, each read back
/// from its own.
#[test]
fn ids_run_across_a_segment_boundary() {
    let table = table(&[("opcache.jit_max_root_traces", "2000")]);
    for n in 1..=1025 {
        assert_eq!(
            table.publish(compiled(&[0; 4], vec![]), Start::Loop),
            Ok(trace(n))
        );
    }
    for n in 1022..=1025 {
        assert_eq!(table.get(trace(n)).map(|t| t.id), Some(trace(n)));
    }
    assert_eq!(trace(1024).slot(), (1, 0));
}

/// `opcache.jit_max_root_traces` bounds every id, sides as well, as php's
/// `ZEND_JIT_TRACE_NUM` does: no id reaches it, and the table that met it
/// is full, publishing nothing more; the hot root that finds
/// it so stops its counters, which stand stopped.
#[test]
fn the_ids_bound_is_full_of_traces_and_stops_the_table() {
    let table = table(&[("opcache.jit_max_root_traces", "3")]);
    assert_eq!(
        table.publish(compiled(&[0; 4], vec![]), Start::Enter),
        Ok(trace(1))
    );
    assert!(!table.full());
    assert_eq!(
        table.publish(compiled(&[0; 4], vec![]), side(1)),
        Ok(trace(2))
    );
    assert!(table.full(), "the next id is the bound");
    let full = Err(Unpublished::Full(Full::Traces));
    assert_eq!(table.publish(compiled(&[0; 4], vec![]), Start::Loop), full);
    assert_eq!(table.next(), Some(trace(3)));
    assert_eq!(Full::Traces.stop(), crate::record::Stop::TooManyTraces);
    assert!(!table.stopped(), "until a hot root finds it full");
    table.stop();
    assert!(table.stopped());
}

/// `opcache.jit_max_side_traces` bounds the sides of each root, counted
/// at the root whichever trace a side grew from; another root's sides
/// still publish.
#[test]
fn a_roots_sides_are_bounded_at_the_root() {
    let table = table(&[("opcache.jit_max_side_traces", "2")]);
    assert_eq!(
        table.publish(compiled(&[0; 4], vec![]), Start::Loop),
        Ok(trace(1))
    );
    assert_eq!(
        table.publish(compiled(&[0; 4], vec![]), side(1)),
        Ok(trace(2))
    );
    assert_eq!(
        table.publish(compiled(&[0; 4], vec![]), side(2)),
        Ok(trace(3))
    );
    let full = Err(Unpublished::Full(Full::Children));
    assert_eq!(table.publish(compiled(&[0; 4], vec![]), side(3)), full);
    assert_eq!(table.publish(compiled(&[0; 4], vec![]), side(1)), full);
    assert_eq!(
        table.publish(compiled(&[0; 4], vec![]), Start::Loop),
        Ok(trace(4))
    );
    assert_eq!(
        table.publish(compiled(&[0; 4], vec![]), side(4)),
        Ok(trace(5))
    );
    assert!(!table.full(), "a root's bound stops no other root");
    assert_eq!(
        table.publish(compiled(&[0; 4], vec![]), side(9)),
        Err(Unpublished::Malformed("a side trace of no trace numbered"))
    );
}

/// `opcache.jit_max_exit_counters` bounds the epoch's exits as php's
/// `ZEND_JIT_EXIT_COUNTERS + exit_count >= max` does: the trace whose
/// exits would reach it is refused, and one with none still publishes.
#[test]
fn the_epochs_exits_are_bounded() {
    let table = table(&[("opcache.jit_max_exit_counters", "3")]);
    let two = vec![exit(None), exit(None)];
    assert_eq!(
        table.publish(compiled(&[0; 4], two), Start::Loop),
        Ok(trace(1))
    );
    assert_eq!(
        table.publish(compiled(&[0; 4], vec![exit(None)]), Start::Loop),
        Err(Unpublished::Full(Full::Exits))
    );
    assert_eq!(
        table.publish(compiled(&[0; 4], vec![]), Start::Loop),
        Ok(trace(2))
    );
    assert!(!table.full());
}

/// An arena with no room for a body is `Full::Code`, php's
/// `COMPILER_ERROR`, an attempt the word recovers from, and the table is
/// not full: the next id stands, and nothing stops (#3015's ruling).
#[test]
fn a_full_arena_is_a_compiler_error_and_stops_nothing() {
    let table = table(&[]);
    assert_eq!(
        table.publish(compiled(&vec![0; CHUNK - 8], vec![]), Start::Loop),
        Ok(trace(1))
    );
    let full = Err(Unpublished::Full(Full::Code));
    assert_eq!(table.publish(compiled(&[0; 16], vec![]), Start::Loop), full);
    assert_eq!(table.next(), Some(trace(2)), "no id was taken");
    assert!(!table.full(), "a full arena is no bound");
    assert_eq!(Full::Code.stop(), crate::record::Stop::CompilerError);
    assert!(Full::Code.stop().recovers());
}

/// A table whose arena could not be mapped, as on a target with no
/// adapter yet, numbers each trace it publishes under the same bounds
/// and keeps no code: its slot stays empty.
#[test]
fn a_table_with_no_arena_numbers_and_keeps_no_code() {
    let policy = testing::policy(&[
        ("opcache.jit_buffer_size", "64K"),
        ("opcache.jit_max_root_traces", "2"),
    ]);
    let table: TraceTable<Unmapped> = TraceTable::with_perf(&policy, Perf::none());
    assert_eq!(
        table.publish(compiled(&[0; 4], vec![]), Start::Loop),
        Ok(trace(1))
    );
    assert!(table.get(trace(1)).is_none(), "numbered with no code");
    assert_eq!((table.next(), table.full()), (Some(trace(2)), true));
    assert_eq!(
        table.publish(compiled(&[0; 4], vec![]), Start::Loop),
        Err(Unpublished::Full(Full::Traces))
    );
}

/// What does not hold together is refused before anything is counted.
#[test]
fn a_malformed_trace_is_refused_and_counts_nothing() {
    let table = table(&[]);
    let mut past = compiled(&[0; 4], vec![]);
    past.entry_offset = 4;
    assert_eq!(
        table.publish(past, Start::Loop),
        Err(Unpublished::Malformed("an entry past the trace's bytes"))
    );
    assert_eq!(
        table.publish(compiled(&[0; 4], vec![exit(Some(trace(1)))]), Start::Loop),
        Err(Unpublished::Malformed("a link to no trace numbered"))
    );
    let mut huge = compiled(&vec![0; CHUNK + 1], vec![]);
    let odd = Compiled {
        align: 3,
        ..compiled(&[0; 4], vec![])
    };
    for refused in [huge.clone(), odd] {
        assert!(matches!(
            table.publish(refused, Start::Loop),
            Err(Unpublished::Malformed(_))
        ));
    }
    huge.bytes.truncate(CHUNK);
    assert!(!table.full(), "a body the backend made wrong fills nothing");
    assert_eq!(table.next(), Some(trace(1)));
    assert_eq!(table.writer().counts, super::Counts::default());
}

/// A new epoch keeps no table until a request of it asks for one, and a
/// new table holds nothing: a restart's epoch starts empty.
#[test]
fn a_new_epochs_table_starts_empty() {
    let (epoch, _) = Epoch::fixture(Unit {
        main: OpArray::default(),
        ..Unit::default()
    });
    assert!(super::of_epoch(&epoch).is_none());
    let exit_word = super::exit_word(&epoch, trace(1), ExitId(0));
    assert!(crate::policy::read(exit_word) == crate::policy::State::Blacklisted);
    let table = table(&[]);
    assert_eq!((table.next(), table.full()), (Some(trace(1)), false));
    assert!(table.get(trace(1)).is_none());
}

/// A `Write` that panics while armed.
struct Tripwire(Arc<AtomicBool>);

impl Write for Tripwire {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        assert!(!self.0.load(Ordering::Relaxed), "the PERF writer panicked");
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A publish that panics after its bytes are placed leaves the counts,
/// the slots and the next id as they were, and the mutex it held is taken
/// again: the next publish takes the id the panicked one would have.
#[test]
fn a_publish_that_panics_leaves_the_counts_as_they_were() {
    let armed = Arc::new(AtomicBool::new(false));
    let perf = Perf::to(Some(Box::new(Tripwire(Arc::clone(&armed)))), None);
    let table = table_to(&[("opcache.jit_max_side_traces", "1")], perf);
    assert_eq!(
        table.publish(compiled(&[0; 4], vec![exit(None)]), Start::Loop),
        Ok(trace(1))
    );
    let before = table.writer().counts.clone();
    armed.store(true, Ordering::Relaxed);
    let panicked = catch_unwind(AssertUnwindSafe(|| {
        table.publish(compiled(&[0; 4], vec![exit(None)]), side(1))
    }));
    assert!(panicked.is_err());
    assert_eq!(table.writer().counts, before);
    assert_eq!(table.next(), Some(trace(2)));
    assert!(table.get(trace(2)).is_none());
    armed.store(false, Ordering::Relaxed);
    assert_eq!(
        table.publish(compiled(&[0; 4], vec![exit(None)]), side(1)),
        Ok(trace(2))
    );
    assert_eq!(
        table.publish(compiled(&[0; 4], vec![]), side(1)),
        Err(Unpublished::Full(Full::Children)),
        "the side the panicked publish did not count is counted once"
    );
}

/// No trace is freed alone: the chunks a table's traces hold stay in
/// use while it stands, and the table dropped, as its epoch drops it,
/// returns them, the pages going with the last.
#[test]
fn a_dropped_table_returns_its_chunks() -> Result<(), String> {
    let table = table(&[("opcache.jit_buffer_size", "128K")]);
    // A whole chunk fits only from the chunk's first byte, which the heap
    // adapter's allocation under Miri need not place at any alignment.
    let whole = Compiled {
        align: 1,
        ..compiled(&vec![0; CHUNK], vec![])
    };
    assert_eq!(table.publish(whole, Start::Loop), Ok(trace(1)));
    assert_eq!(
        table.publish(compiled(&[0; 8], vec![]), Start::Loop),
        Ok(trace(2))
    );
    let watch = table
        .writer()
        .arena
        .as_ref()
        .map(super::code::Arena::watch)
        .ok_or("an arena")?;
    assert_eq!(watch.chunks_in_use(), Some(2));
    drop(table);
    assert_eq!(watch.chunks_in_use(), None);
    Ok(())
}

/// What php's `zend_jit_trace_name` names a trace by: the method with its
/// class, the function, or for a file's own code the file.
#[test]
fn a_trace_is_named_as_php_names_it() {
    let place = |scope, function| Place {
        scope,
        function,
        file: b"/w/main.php",
        line: 7,
    };
    let named = |place: &Place<'_>| TraceName::of(place).at.to_vec();
    assert_eq!(named(&place(Some(b"C"), Some(b"m"))), b"C::m");
    assert_eq!(named(&place(None, Some(b"f"))), b"f");
    assert_eq!(named(&place(None, None)), b"/w/main.php");
    assert_eq!(TraceName::of(&place(None, None)).line, 7);
}

/// The `PERF` map line php wrote for trace 1 of the loop witness
/// `function f($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += $i; }
/// return $s; } echo f(10);`, its function on line 3, served by the
/// oracle under the recipe with `opcache.jit_debug=0x3F010`: the
/// `TRACE 1 start (loop) f() …:3` line's trace, `/tmp/perf-<pid>.map`.
const PHP_PERF_LINE: &str = "148000e90 a4 TRACE-1$f$3\n";

/// A map line with its start and size, which differ by nature, written
/// `%x`, after holding each to lowercase hex.
fn masked(line: &str) -> Option<String> {
    let mut fields = line.splitn(3, ' ');
    let hex = |field: &str| {
        !field.is_empty()
            && field
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    };
    let (start, size, rest) = (fields.next()?, fields.next()?, fields.next()?);
    (hex(start) && hex(size)).then(|| format!("%x %x {rest}"))
}

/// The witness's trace 1 published, named from the place its
/// `TRACE start` line names (the function `f`, its loop on line 3), writes
/// php's map line: the same shape and name, the start the code's own and
/// the size its bytes'.
#[test]
fn a_traces_perf_line_is_phps() -> Result<(), String> {
    let map = Captured::default();
    let table = table_to(&[], Perf::to(Some(Box::new(map.clone())), None));
    let mut loop_f = compiled(&[0; 0xa4], vec![]);
    loop_f.name = TraceName::of(&Place {
        scope: None,
        function: Some(b"f"),
        file: b"/w/main.php",
        line: 3,
    });
    assert_eq!(table.publish(loop_f, Start::Loop), Ok(trace(1)));
    let ours = String::from_utf8(map.taken()).map_err(|e| e.to_string())?;
    let code = table.get(trace(1)).ok_or("published")?;
    assert_eq!(
        ours,
        format!("{:x} a4 TRACE-1$f$3\n", code.code.entry().as_ptr().addr())
    );
    assert_eq!(masked(&ours), masked(PHP_PERF_LINE));
    assert!(masked(PHP_PERF_LINE).is_some());
    Ok(())
}

/// The jitdump's header and `JIT_CODE_LOAD` record as php's
/// `ir_perf_jitdump_open` and `_register` lay them: the magic, the
/// version, each size, and the name with its NUL before the bytes; and
/// `ir_perf_jitdump_close`'s `JIT_CODE_CLOSE`, a header alone.
#[test]
fn a_jitdump_record_is_laid_as_phps() {
    let word = |bytes: &[u8], at: usize| {
        u32::from_ne_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    };
    let header = perf::header(42, 7);
    assert_eq!(header.len(), 40);
    assert_eq!(
        (word(&header, 0), word(&header, 4), word(&header, 8)),
        (0x4A69_5444, 1, 40)
    );
    assert_eq!(word(&header, 20), 42);
    let at = Loaded {
        pid: 42,
        tid: 9,
        time: 7,
        code_id: 1,
    };
    let record = perf::load_record(&at, b"TRACE-1$f$3", 0x1000, &[0xC3; 5]);
    assert_eq!(record.len(), 56 + 12 + 5);
    assert_eq!((word(&record, 0), word(&record, 4)), (0, 73));
    assert_eq!((word(&record, 16), word(&record, 20)), (42, 9));
    assert_eq!(&record[56..68], b"TRACE-1$f$3\0");
    assert_eq!(&record[68..], &[0xC3; 5]);
    let close = perf::close_record(7);
    assert_eq!((close.len(), word(&close, 0), word(&close, 4)), (16, 3, 16));
}

/// On the target itself (`jit-design.md` §4.5): `mov w0, #7; ret`
/// published into a table over `MAP_JIT` and entered through `get`.
#[cfg(all(target_os = "macos", target_arch = "aarch64", not(miri)))]
mod native {
    use super::super::code::MapJit;
    use super::super::{TraceTable, Unpublished};
    use super::{compiled, trace};
    use crate::record::Start;
    use crate::table::Perf;
    use crate::testing;

    /// `mov w0, #value; ret`.
    fn returning(value: u16) -> [u8; 8] {
        let mov = 0x5280_0000_u32 | (u32::from(value) << 5);
        let ret = 0xD65F_03C0_u32;
        let mut bytes = [0; 8];
        bytes[..4].copy_from_slice(&mov.to_le_bytes());
        bytes[4..].copy_from_slice(&ret.to_le_bytes());
        bytes
    }

    /// The code at `entry` as a function of no arguments.
    fn call(entry: std::ptr::NonNull<u8>) -> u32 {
        let entered = crate::table::code::callable(entry);
        entered()
    }

    #[test]
    fn mov_ret_runs_through_publish_and_get() -> Result<(), String> {
        let policy = testing::policy(&[("opcache.jit_buffer_size", "64K")]);
        let table: TraceTable<MapJit> = TraceTable::with_perf(&policy, Perf::none());
        let published = table.publish(compiled(&returning(7), vec![]), Start::Loop);
        assert_eq!(published, Ok(trace(1)));
        let got = table.get(trace(1)).ok_or("published")?;
        assert_eq!(call(got.code.entry()), 7);
        let mut offset = compiled(&[returning(1), returning(9)].concat(), vec![]);
        offset.entry_offset = 8;
        assert_eq!(
            table.publish(offset, Start::Loop),
            Ok::<_, Unpublished>(trace(2))
        );
        let second = table.get(trace(2)).ok_or("published")?;
        assert_eq!((call(second.code.entry()), call(second.entry())), (1, 9));
        Ok(())
    }
}

/// The table is the epoch's, on the engine itself: each request's first
/// activation publishes one trace with one exit into the table of the
/// epoch it started in and claims that exit's word through an exit
/// holder, which reads it through the epoch; the second request of the
/// epoch finds the first's trace and takes the next id.
#[cfg(all(target_os = "macos", target_arch = "aarch64", not(miri)))]
mod epoch {
    use php_engine::tier::{
        self, Extension, ExtensionBuilder, Flow, FrameRef, Request, Site, Tier,
    };

    use std::fmt::Write as _;

    use super::{compiled, exit, side, trace};
    use crate::policy::{self, Holder, State};
    use crate::record::Start;
    use crate::table::{self, TraceId, Unpublished};
    use crate::testing;
    use crate::types::ExitId;

    /// What each request did, kept for the test.
    #[derive(Debug, Default)]
    struct Seen {
        published: Option<Result<TraceId, Unpublished>>,
        before: Option<bool>,
        claimed: bool,
        held_twice: bool,
    }

    /// The tier that publishes at a request's first activation.
    struct Publishing;

    impl Tier for Publishing {
        fn safepoint(req: &mut Request, _: FrameRef, site: Site) -> Flow {
            if site != Site::Entry || req.state::<Seen>().published.is_some() {
                return Flow::Continue;
            }
            let policy = testing::policy(&[("opcache.jit_buffer_size", "64K")]);
            let (published, before) = match table::of(req, &policy, &table::Outputs::none()) {
                Some(table) => {
                    let before = table.get(trace(1)).is_some();
                    (
                        table.publish(compiled(&[0; 4], vec![exit(None)]), Start::Loop),
                        before,
                    )
                }
                None => return Flow::Continue,
            };
            let (claimed, held_twice) = match published {
                Ok(id) => {
                    let holder = || Holder::Exit {
                        epoch: tier::epoch(req),
                        trace: id,
                        exit: ExitId(0),
                    };
                    let start = side(id.get());
                    let first = policy::claim(holder(), &policy, start);
                    let second = policy::claim(holder(), &policy, start);
                    let read = policy::read(holder().word());
                    (
                        first.is_some() && read == State::Recording,
                        second.is_some(),
                    )
                }
                Err(_) => (false, false),
            };
            let seen = req.state::<Seen>();
            seen.published = Some(published);
            seen.before = Some(before);
            (seen.claimed, seen.held_twice) = (claimed, held_twice);
            Flow::Continue
        }

        fn extension() -> Option<Extension> {
            Some(
                ExtensionBuilder::default()
                    .request_state::<Seen>()
                    .build("publishing"),
            )
        }
    }

    #[test]
    fn a_table_is_its_epochs_and_an_exit_holder_reads_its_word() -> Result<(), String> {
        let engine = testing::engine::<Publishing>(&[]);
        let scratch = testing::Scratch::new();
        let (_, first) = scratch.serve_kept::<Seen>(&engine, "function f() {} f();");
        let first = first.ok_or("the first request's state")?;
        assert_eq!(first.published, Some(Ok(trace(1))));
        assert_eq!(first.before, Some(false));
        assert!(first.claimed && !first.held_twice);
        let (_, second) = scratch.serve_kept::<Seen>(&engine, "function f() {} f();");
        let second = second.ok_or("the second request's state")?;
        assert_eq!(second.published, Some(Ok(trace(2))));
        assert_eq!(second.before, Some(true), "the epoch's table stands");
        Ok(())
    }

    /// `Lane` with the epoch's arena spent before its first site is
    /// asked, as a spent `opcache.jit_buffer_size` leaves it: a body of
    /// the whole arena published, and one more that finds no room.
    struct Spent;

    impl Tier for Spent {
        fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
            if let Some(engine) = req.engine::<crate::JitEngine>() {
                let shared = &engine.shared;
                let table = table::of(req, &shared.policy, &shared.perf);
                if let Some(table) = table.filter(|table| table.next() == Some(trace(1))) {
                    let whole =
                        table.publish(compiled(&vec![0; super::CHUNK], vec![]), Start::Loop);
                    let more = table.publish(compiled(&[0; 16], vec![]), Start::Loop);
                    debug_assert!(whole.is_ok() && more.is_err(), "{whole:?} {more:?}");
                }
            }
            crate::dispatch::safepoint(req, frame, site)
        }

        fn extension() -> Option<Extension> {
            <testing::Lane as Tier>::extension()
        }
    }

    /// A spent arena stops nothing (#3015's ruling): the hot root after
    /// it is recorded, not refused with `too many traces`, and aborts as
    /// php's `COMPILER_ERROR`, `TRACE 2 abort (JIT compilation error)`,
    /// one attempt counted; the next request's visit records it again,
    /// the table never stopped.
    #[cfg_attr(
        not(all(target_os = "macos", target_arch = "aarch64")),
        ignore = "no code arena on this target until #2943"
    )]
    #[test]
    fn a_spent_arena_refuses_no_hot_root() {
        let engine = testing::engine::<Spent>(&[
            ("jit.mode", "force"),
            ("opcache.jit_debug", "258048"),
            ("opcache.jit_buffer_size", "64K"),
        ]);
        let scratch = testing::Scratch::new();
        let program = "function f() { return 1; } echo f();";
        let aborted = "---- TRACE 2 start (enter) $main() ";
        for request in 0..2 {
            let served = scratch.serve(&engine, program);
            let lines = String::from_utf8_lossy(&served.stderr).into_owned();
            assert!(
                lines.contains(aborted)
                    && lines.ends_with(
                        "\n---- TRACE 2 stop (return)\n---- TRACE 2 abort (JIT compilation error)\n\n"
                    ),
                "request {request}: {lines}"
            );
            assert_eq!(testing::stdout(&served), "1");
        }
    }

    /// The arena filled by the traces a program compiles: once a body
    /// finds no room each root after it aborts as `JIT compilation error`
    /// and none is refused or blacklisted, as php's do under a small
    /// `opcache.jit_buffer_size` (`zend_jit_trace.c:7612-7622`,
    /// `:8164-8167`).
    #[cfg_attr(
        not(all(target_os = "macos", target_arch = "aarch64")),
        ignore = "no code arena on this target until #2943"
    )]
    #[test]
    fn traces_that_fill_the_arena_abort_the_roots_after() {
        let engine = testing::engine::<testing::Lane>(&[
            ("jit.mode", "force"),
            ("opcache.jit_debug", "258048"),
            ("opcache.jit_buffer_size", "64K"),
        ]);
        let functions = 400;
        let mut program = String::new();
        for k in 0..functions {
            writeln!(program, "function f{k}($a) {{ return $a * {k} + 1; }}").unwrap_or_default();
        }
        write!(
            program,
            "$t = 0; for ($k = 0; $k < {functions}; $k++) {{ $f = \"f$k\"; $t += $f(2); }} echo $t;"
        )
        .unwrap_or_default();
        let served = testing::Scratch::new().serve(&engine, &program);
        let lines = String::from_utf8_lossy(&served.stderr).into_owned();
        let compiled = lines.matches(" compiled\n").count();
        let aborted = lines.matches(" abort (JIT compilation error)\n").count();
        assert!(
            compiled > 0 && aborted > 0,
            "{compiled} compiled, {aborted} aborted"
        );
        let first = lines.find(" abort (JIT compilation error)").unwrap_or(0);
        assert!(
            !lines[first..].contains(" compiled\n"),
            "nothing compiles once the arena is spent"
        );
        assert!(
            !lines.contains("too many") && !lines.contains("blacklisted"),
            "{lines}"
        );
        let sum: i64 = (0..functions).map(|k| 2 * k + 1).sum();
        assert_eq!(testing::stdout(&served), sum.to_string());
    }
}

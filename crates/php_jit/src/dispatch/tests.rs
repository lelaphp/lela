//! `dispatch` at its seam (`jit-design.md` §4.9 *Tested*, §7): `visit` as
//! a table over `(State, Start)` on a fixture epoch's word and on the
//! sealed word, and with the running execution recording; then the
//! machine on real requests through `Lane`, the tier as the `jit` build
//! binds it: one `TRACE start` for a destructor's hot loop inside a
//! recording and a second root on the next request, the request's
//! exclusion clear again after a recording a panic unwound, a fiber
//! suspended inside a recording that keeps its resumer from recording
//! until the fiber is destroyed, and no word written under `Off`.

use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::atomic::Ordering;

use php_engine::tier::{
    self, Epoch, Extension, ExtensionBuilder, Fatal, Flow, FrameRef, FuncRef, Request,
    SafepointIdx, Site, Tier, TraceNum, Word,
};
use php_unit::{OpArray, OplineIdx, Unit};

use super::{Visited, visit};
use crate::policy::{self, Holder, Outcome, State};
use crate::record::{self, Start, Stop};
use crate::table::TraceId;
use crate::testing::{self, Lane, function_of};
use crate::types::ExitId;

/// An epoch holding one unit whose main op array has two safepoints.
fn fixture() -> (Epoch, FuncRef) {
    Epoch::fixture(Unit {
        main: OpArray {
            safepoints: vec![OplineIdx(0), OplineIdx(1)].into(),
            ..OpArray::default()
        },
        ..Unit::default()
    })
}

fn trace(n: u32) -> TraceId {
    TraceId::new(n).unwrap_or_else(|| {
        assert!(
            n != 0 && n < TraceId::LIMIT,
            "trace {n} is a number a word holds"
        );
        trace(1)
    })
}

fn heat(word: &Word) -> u32 {
    word.halves()[0].load(Ordering::Relaxed)
}

/// The holder of safepoint `at` of the fixture's op array, as a home
/// hands one to `visit`.
#[expect(
    clippy::unnecessary_wraps,
    reason = "visit takes the holder a home may not have"
)]
fn root(epoch: &Epoch, func: FuncRef, at: u32) -> Option<Holder> {
    Some(Holder::Root {
        epoch: epoch.clone(),
        func,
        at: SafepointIdx(at),
    })
}

/// What a visit decided, by name, a claim released again.
fn decided(visited: Visited) -> String {
    match visited {
        Visited::Continue => "continue".to_owned(),
        Visited::Enter(id) => format!("enter {}", id.get()),
        Visited::Hot(claim) => {
            let start = claim.start();
            policy::settle(claim, &Outcome::Released);
            format!("hot {}", start.describe())
        }
    }
}

/// `visit` as a table over `(State, Start)`: a cold word is bumped and
/// left below its threshold and claimed at it, a compiled one entered,
/// a recording or blacklisted one left alone; an `opcache.jit_hot_*` of zero
/// bumps and claims nothing; the sealed word is never claimed.
#[test]
fn visit_as_a_table_over_state_and_start() {
    let tracing = testing::policy(&[("opcache.jit", "tracing"), ("opcache.jit_hot_loop", "2")]);
    let side = Start::Side {
        parent: trace(1),
        exit: ExitId(0),
    };
    for start in [Start::Loop, Start::Enter, Start::Return, side] {
        let (epoch, func) = fixture();
        let word = epoch.word(func, SafepointIdx(1));
        let threshold = tracing.threshold(start).unwrap_or(0);
        for visits in 1..threshold {
            let visited = visit(word, &tracing, start, true, || root(&epoch, func, 1));
            assert_eq!(decided(visited), "continue");
            assert_eq!(heat(word), visits, "{start:?} bumped");
        }
        let visited = visit(word, &tracing, start, true, || root(&epoch, func, 1));
        assert_eq!(decided(visited), format!("hot {}", start.describe()));
    }
    let (epoch, func) = fixture();
    let compiled = epoch.word(func, SafepointIdx(0));
    if let Some(claim) =
        root(&epoch, func, 0).and_then(|h| policy::claim(h, &tracing, Start::Enter))
    {
        policy::settle(claim, &Outcome::Published(trace(5)));
    }
    let visited = visit(compiled, &tracing, Start::Enter, true, || {
        root(&epoch, func, 0)
    });
    assert_eq!(decided(visited), "enter 5");
    let recording = epoch.word(func, SafepointIdx(1));
    let held = root(&epoch, func, 1).and_then(|h| policy::claim(h, &tracing, Start::Loop));
    assert_eq!(policy::read(recording), State::Recording);
    let visited = visit(recording, &tracing, Start::Loop, true, || {
        root(&epoch, func, 1)
    });
    assert_eq!(
        (decided(visited), heat(recording)),
        ("continue".to_owned(), 0)
    );
    if let Some(held) = held {
        policy::settle(held, &Outcome::Blacklist(Stop::TooManyTraces));
    }
    let visited = visit(recording, &tracing, Start::Loop, true, || {
        root(&epoch, func, 1)
    });
    assert_eq!(
        (decided(visited), heat(recording)),
        ("continue".to_owned(), 0)
    );
    let no_returns =
        testing::policy(&[("opcache.jit", "tracing"), ("opcache.jit_hot_return", "0")]);
    let (epoch, func) = fixture();
    let word = epoch.word(func, SafepointIdx(1));
    for _ in 0..300 {
        let visited = visit(word, &no_returns, Start::Return, true, || {
            root(&epoch, func, 1)
        });
        assert_eq!(decided(visited), "continue");
    }
    assert_eq!(heat(word), 0);
    let sealed = Word::sealed();
    for start in [Start::Loop, Start::Enter, Start::Return, side] {
        for _ in 0..300 {
            let visited = visit(&sealed, &tracing, start, true, || root(&epoch, func, 1));
            assert_eq!(decided(visited), "continue");
        }
    }
    assert_eq!(policy::read(&sealed), State::Blacklisted);
}

/// With the request recording, a visit that crosses the
/// threshold answers `Continue` and leaves the word bumped and cold, as
/// php's `zend_jit_loop_trace_counter_handler` bumps and returns while
/// `JIT_G(tracing)`.
#[test]
fn a_visit_while_recording_bumps_and_does_not_claim() {
    let policy = testing::policy(&[("jit.mode", "force")]);
    let (epoch, func) = fixture();
    let word = epoch.word(func, SafepointIdx(1));
    for visits in 1..=3 {
        let visited = visit(word, &policy, Start::Loop, false, || root(&epoch, func, 1));
        assert_eq!(decided(visited), "continue");
        assert_eq!((policy::read(word), heat(word)), (State::Cold, visits));
    }
    let visited = visit(word, &policy, Start::Loop, true, || root(&epoch, func, 1));
    assert_eq!(decided(visited), "hot loop");
}

/// The trace-dump lanes' ini: `force`, inline, and php's `TRACE` lines.
const DUMPED: [(&str, &str); 3] = [
    ("jit.mode", "force"),
    ("jit.compile", "inline"),
    ("opcache.jit_debug", "258048"),
];

/// The lines a request wrote to its error stream, its file named by its
/// name alone.
fn lines(served: &php_engine::test::Served) -> Vec<String> {
    String::from_utf8_lossy(&served.stderr)
        .lines()
        .map(|line| match (line.find(" /"), line.rfind("/body.php")) {
            (Some(from), Some(to)) => format!("{} {}", &line[..from], &line[to + 1..]),
            _ => line.to_owned(),
        })
        .collect()
}

/// A destructor that runs a hot loop inside a recording: the loop is
/// bumped and not recorded, so one `TRACE start` is written, the root's;
/// the next request records a root of its own, the function the first
/// recorded inside its root.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_destructors_hot_loop_inside_a_recording_starts_no_second_trace() {
    let scratch = testing::Scratch::new();
    let engine = testing::engine::<Lane>(&DUMPED);
    let program = "class D { function __destruct() { $s = 0; for ($i = 0; $i < 3; $i++) { $s += $i; } echo \"d $s\\n\"; } }
        function f() { $d = new D; $d = null; return 1; }
        echo f(), \"\\n\";";
    let first = scratch.serve(&engine, program);
    assert_eq!(testing::stdout(&first), "d 3\n1\n");
    assert_eq!(
        lines(&first),
        [
            "JIT: jit.mode=force",
            "---- TRACE 1 start (enter) $main() body.php:4",
            "---- TRACE 1 stop (return)",
            "---- TRACE 1 compiled",
            "",
        ]
    );
    let second = scratch.serve(&engine, program);
    assert_eq!(testing::stdout(&second), "d 3\n1\n");
    assert_eq!(
        lines(&second),
        [
            "---- TRACE 2 start (enter) f() body.php:3",
            "---- TRACE 2 stop (return)",
            "---- TRACE 2 compiled",
            "",
        ]
    );
}

/// A full table, as php's `opcache.jit_max_root_traces` fills it: the
/// first root hot after it is refused with `too many traces` and
/// blacklisted, and the counters stop, so no later root starts, as
/// `zend_jit_trace_hot_root` refuses one and
/// `zend_jit_stop_counter_handlers` stops the rest. php under the
/// recipe's hot counters with `opcache.jit_max_root_traces=3` writes the
/// same for this program's `b`, its third trace, a side trace of `a`'s
/// loop taking the second, which no `Backend` compiles here yet:
/// `---- TRACE 3 start (enter) b() …:3`, `---- TRACE 3 abort (too many
/// traces)`, `---- TRACE 3 blacklisted`, and nothing for `c` or `d`.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_full_table_refuses_one_root_and_stops_the_counters() {
    let mut ini = DUMPED.to_vec();
    ini.push(("opcache.jit_max_root_traces", "2"));
    let engine = testing::engine::<Lane>(&ini);
    let program = "function a($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += $i; } return $s; }
        function b($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s -= $i; } return $s; }
        function c($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += 2; } return $s; }
        function d($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += 3; } return $s; }
        echo a(5), b(5), c(5), d(5), \"\\n\";";
    let served = testing::Scratch::new().serve(&engine, program);
    assert_eq!(testing::stdout(&served), "10-101015\n");
    assert_eq!(
        lines(&served),
        [
            "JIT: jit.mode=force",
            "---- TRACE 1 start (enter) $main() body.php:6",
            "---- TRACE 1 abort (inner loop)",
            "---- TRACE 1 start (loop) a() body.php:2",
            "---- TRACE 1 stop (loop)",
            "---- TRACE 1 compiled",
            "",
            "---- TRACE 2 start (enter) b() body.php:3",
            "---- TRACE 2 abort (too many traces)",
            "---- TRACE 2 blacklisted",
            "",
        ]
    );
}

/// What the panicking tier saw after the panic: whether the request
/// recorded, the running execution's trace number, and the state of the
/// word it claimed.
#[derive(Debug, Default)]
struct Unwound {
    after: Option<bool>,
    trace: Option<TraceNum>,
    word: Option<State>,
}

/// `Lane` at the activation of `f` alone, the recording there made under
/// a catch of a panic, and a site a nested `execute` asks at while the
/// request records panicking.
struct Panics;

impl Tier for Panics {
    fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
        if record::recording(req) {
            resume_unwind(Box::new("a kernel panicked under a recording"));
        }
        if site != Site::Entry || function_of(req, frame).as_deref() != Some(b"f") {
            return Flow::Continue;
        }
        let caught = catch_unwind(AssertUnwindSafe(|| super::safepoint(req, frame, site)));
        let word = policy::read(tier::word(req, frame, SafepointIdx::ENTRY));
        let after = record::recording(req);
        let trace = tier::trace_num(req);
        let unwound = req.state::<Unwound>();
        unwound.after = Some(after);
        unwound.trace = Some(trace);
        unwound.word = Some(word);
        match caught {
            Ok(flow) => flow,
            Err(_) => Flow::Bail(Fatal::error("the recording panicked")),
        }
    }

    fn extension() -> Option<Extension> {
        let mut ini = crate::policy::ini();
        ini.extend(testing::declared(&testing::OPCACHE_JIT));
        Some(
            ExtensionBuilder::default()
                .ini(ini)
                .engine_state(crate::JitEngine::new)
                .request_state::<Unwound>()
                .build("jit"),
        )
    }
}

/// A recording a panic unwound out of leaves the request's exclusion
/// clear and the trace number zero, which a recording never writes, and
/// its claim settled itself as a failed attempt: the word is cold again.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn the_exclusion_is_clear_after_a_recording_a_panic_unwound() {
    let scratch = testing::Scratch::new();
    let engine = testing::engine::<Panics>(&DUMPED);
    let (served, unwound) = scratch.serve_kept::<Unwound>(
        &engine,
        "class D { function __destruct() { for ($i = 0; $i < 3; $i++) {} } }
        function f() { $d = new D; $d = null; return 1; }
        echo f();",
    );
    let fatal = served.fatal.unwrap_or_default();
    assert!(fatal.contains("the recording panicked"), "{fatal}");
    let unwound = unwound.unwrap_or_default();
    assert_eq!(unwound.after, Some(false));
    assert_eq!(unwound.trace, Some(TraceNum::NONE));
    assert_eq!(unwound.word, Some(State::Cold));
}

/// A fiber suspended inside a recording: the request records, as php's
/// `JIT_G(tracing)` stays set while the fiber's recorder is suspended, so
/// the resumer's hot `work()` records no root; destroying the fiber
/// unwinds the recording and clears the exclusion, so the next hot
/// `work()` is recorded (revision 30). The first request records
/// `$main`, inside which nothing else roots; the second finds `$main`
/// numbered and roots in the fiber.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_fiber_suspended_inside_a_recording_keeps_its_resumer_from_recording_until_destroyed() {
    let scratch = testing::Scratch::new();
    let engine = testing::engine::<Lane>(&DUMPED);
    let program = "function body() { Fiber::suspend(1); echo \"never\\n\"; }
        function work() { return 2; }
        $f = new Fiber('body');
        $f->start();
        echo work(), \"\\n\";
        $f = null;
        echo work(), \"\\n\";";
    let first = scratch.serve(&engine, program);
    assert_eq!(testing::stdout(&first), "2\n2\n", "{:?}", lines(&first));
    let second = scratch.serve(&engine, program);
    assert_eq!(testing::stdout(&second), "2\n2\n");
    assert_eq!(
        lines(&second),
        [
            "---- TRACE 2 start (enter) body() body.php:2",
            "---- TRACE 2 abort (exception)",
            "",
            "---- TRACE 2 start (enter) work() body.php:3",
            "---- TRACE 2 stop (return)",
            "---- TRACE 2 compiled",
            "",
        ],
        "after {:?}",
        lines(&first)
    );
}

/// What the word spy saw over a serving.
#[derive(Debug, Default)]
struct Spied {
    sites: u32,
    written: u32,
}

/// `Lane` with every site's word read before and after the tier is
/// asked.
struct Spy;

impl Tier for Spy {
    fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
        let halves = |req: &Request| {
            let word = tier::word(req, frame, site.idx());
            [0, 1].map(|half| word.halves()[half].load(Ordering::Relaxed))
        };
        let before = halves(req);
        let flow = super::safepoint(req, frame, site);
        let after = halves(req);
        let spied = req.state::<Spied>();
        spied.sites += 1;
        spied.written += u32::from(before != after);
        flow
    }

    fn extension() -> Option<Extension> {
        let mut ini = crate::policy::ini();
        ini.extend(testing::declared(&testing::OPCACHE_JIT));
        Some(
            ExtensionBuilder::default()
                .ini(ini)
                .engine_state(crate::JitEngine::new)
                .request_state::<Spied>()
                .build("jit"),
        )
    }
}

/// Under `Off`, php's default, the tier is asked at every site of a
/// program with loops, calls and returns and no word is written; the same
/// program
/// under `force` writes words.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn under_off_no_word_is_written() {
    let program = "function f($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += $i; } return $s; }
        for ($j = 0; $j < 5; $j++) { echo f($j); }";
    let off = testing::Scratch::new();
    let (served, spied) = off.serve_kept::<Spied>(&testing::engine::<Spy>(&[]), program);
    assert_eq!(testing::stdout(&served), "00136");
    let spied = spied.unwrap_or_default();
    assert!(spied.sites > 20, "{spied:?}");
    assert_eq!(spied.written, 0);
    let force = testing::Scratch::new();
    let (_, spied) = force.serve_kept::<Spied>(&testing::engine::<Spy>(&DUMPED), program);
    assert!(spied.unwrap_or_default().written > 0);
}

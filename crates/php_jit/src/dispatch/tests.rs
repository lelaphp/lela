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

use php_engine::api::abi::PostStartup;
use php_engine::api::request::TierStatus;
use php_engine::tier::{
    self, Epoch, Extension, ExtensionBuilder, Fatal, Flow, FrameRef, FuncRef, Request,
    SafepointIdx, Site, Tier, TraceNum, Word,
};
use php_unit::{OpArray, OplineIdx, Unit};

use super::{GoesOn, Submitted, Visited, quieted, visit};
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

/// A loop root's word, visited where a return or an exit goes on at the
/// root, is entered where compiled and otherwise neither bumped nor
/// claimed, however hot: the loop's back edge counts its heat once a
/// turn, as php's header counts each arrival once, so a body that ends in
/// a call is not counted twice a turn (#3318).
#[test]
fn a_loop_roots_word_is_entered_from_a_landing_and_never_counted_there() {
    let tracing = testing::policy(&[("opcache.jit", "tracing"), ("opcache.jit_hot_loop", "2")]);
    let (epoch, func) = fixture();
    let word = epoch.word(func, SafepointIdx(1));
    for _ in 0..300 {
        let visited = super::Goes::Header.visit(word, &tracing, true, || root(&epoch, func, 1));
        assert_eq!(decided(visited), "continue");
    }
    assert_eq!((policy::read(word), heat(word)), (State::Cold, 0));
    if let Some(claim) = root(&epoch, func, 1).and_then(|h| policy::claim(h, &tracing, Start::Loop))
    {
        policy::settle(claim, &Outcome::Published(trace(3)));
    }
    let visited = super::Goes::Header.visit(word, &tracing, true, || root(&epoch, func, 1));
    assert_eq!(decided(visited), "enter 3");
}

/// A loop whose body ends in a call counts its heat once a turn, as one
/// whose body does not: under `opcache.jit_hot_loop=8` neither roots in
/// seven turns and each roots in eight, where a return landing on the
/// condition that counted too rooted the first in four (#3318's review).
/// php counts the `JMP` into the loop as an arrival as well and roots both
/// in seven, which this engine, with no site at the condition (revision
/// 70), does not see.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_loop_whose_body_ends_in_a_call_counts_once_a_turn() {
    let ini = [
        ("opcache.jit", "tracing"),
        ("jit.compile", "inline"),
        ("opcache.jit_debug", "258048"),
        ("opcache.jit_hot_loop", "8"),
        ("opcache.jit_hot_func", "255"),
    ];
    let scratch = testing::Scratch::new();
    let rooted = |body: &str, turns: u32| {
        let engine = testing::engine::<Lane>(&ini);
        let program = format!("function g() {{}} $i = 0; while ($i < {turns}) {{ {body} }}");
        let served = scratch.serve(&engine, &program);
        (lines(&served).iter())
            .filter(|line| line.contains("start (loop)"))
            .count()
    };
    for body in ["$i++; g();", "g(); $i++;"] {
        assert_eq!((rooted(body, 7), rooted(body, 8)), (0, 1), "{body}");
    }
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

/// A request that submitted a loop's trace closed at its header claims
/// nothing while the job is under way, nor once it is published until it
/// visits a loop or a return site, where php's interpreter would have
/// entered the trace already: a call's entry the body makes meanwhile is
/// bumped and left. Then it claims again.
#[test]
fn a_request_claims_nothing_until_it_reaches_the_trace_it_submitted() {
    let policy = testing::policy(&[("jit.mode", "force")]);
    let (epoch, func) = fixture();
    let word = epoch.word(func, SafepointIdx(1));
    let holder = Holder::Root {
        epoch: epoch.clone(),
        func,
        at: SafepointIdx(1),
    };
    let claim = policy::claim(holder.clone(), &policy, Start::Loop);
    assert_eq!(policy::read(word), State::Recording);
    let state = crate::JitRequest::recorder(&policy);
    state.submitted.set(Some(Box::new(Submitted {
        word: holder,
        goes_on: GoesOn::Short,
    })));
    let at = |state: &crate::JitRequest, site| quieted(state, Some(site));
    assert!(at(&state, Site::Entry), "under way");
    assert!(at(&state, Site::Loop(SafepointIdx(1))), "under way");
    if let Some(claim) = claim {
        policy::settle(claim, &Outcome::Published(trace(1)));
    }
    assert_eq!(policy::read(word), State::Compiled(trace(1)));
    assert!(at(&state, Site::Entry), "short of the trace");
    assert!(quieted(&state, None), "short of the trace");
    assert!(!at(&state, Site::Return(SafepointIdx(0))), "reached");
    assert!(state.submitted.take().is_none());
    assert!(!at(&state, Site::Entry));
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
/// the next request enters that root, which makes its call to `f` with no
/// exit, as php's `INIT_FCALL` of a user function takes none (#3230), and
/// the destructor's loop, hot where nothing records, roots a trace of its
/// own, whose exit past the loop grows a side trace to the destructor's
/// return, as php's `zend_jit_trace_hot_side` records one.
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
            "---- TRACE 2 start (enter) D::__destruct() body.php:2",
            "---- TRACE 2 abort (inner loop)",
            "---- TRACE 2 start (loop) D::__destruct() body.php:2",
            "---- TRACE 2 stop (loop)",
            "---- TRACE 2 compiled",
            "",
            "     TRACE 2 exit 2 D::__destruct() body.php:2",
            "---- TRACE 3 start (side trace 2/2) D::__destruct() body.php:2",
            "---- TRACE 3 stop (return)",
            "---- TRACE 3 compiled",
            "",
        ]
    );
}

/// A full table, as php's `opcache.jit_max_root_traces` fills it: the
/// first root hot after it is refused with `too many traces` and
/// blacklisted, and the counters stop, so no later root starts, as
/// `zend_jit_trace_hot_root` refuses one and
/// `zend_jit_stop_counter_handlers` stops the rest. Before it, `a`'s
/// compiled loop leaves through php's `exit 3`, the trace started at the
/// loop's condition, holding the three variables' types at entry and
/// meeting the jump's direction before the body (#3134), and the hot exit
/// starts a side trace that the full table refuses before it records, as
/// php's `zend_jit_trace_hot_side` does, blacklisting the exit. The pinned
/// php under the recipe's hot counters with the same bound writes these
/// lines, line for line.
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
            "     TRACE 1 exit 3 a() body.php:2",
            "---- TRACE 2 start (side trace 1/3) a() body.php:2",
            "---- TRACE 2 abort (too many traces)",
            "---- EXIT 1/3 blacklisted",
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
    type State = crate::JitRequest;

    fn lay(started: &tier::Started<'_>) -> Option<crate::JitRequest> {
        crate::JitRequest::laid(*started)
    }

    fn safepoint(req: &mut Request, frame: FrameRef, site: Site, held: tier::Held<'_>) -> Flow {
        if record::recording(req) {
            resume_unwind(Box::new("a kernel panicked under a recording"));
        }
        if site != Site::Entry || function_of(req, frame).as_deref() != Some(b"f") {
            return Flow::Continue;
        }
        let caught = catch_unwind(AssertUnwindSafe(|| {
            super::safepoint(req, frame, site, held)
        }));
        let word = policy::read(held.word(req, SafepointIdx::ENTRY));
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
        ini.extend(testing::declared(&testing::OPCACHE));
        Some(
            testing::switched(ExtensionBuilder::default())
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
/// the resumer's hot `W::__toString()`, which the echo's cast serves from
/// a native, outside `$main`'s compiled code, records no root; destroying
/// the fiber unwinds the recording and clears the exclusion, so the next
/// hot `W::__toString()` is recorded (revision 30). The first request
/// records `$main`, inside which nothing else roots; the second finds
/// `$main` numbered and roots in the fiber, and its last cast enters the
/// trace the one before compiled, whose end writes no exit line, as php's
/// return to the interpreter at a trace's end writes none.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_fiber_suspended_inside_a_recording_keeps_its_resumer_from_recording_until_destroyed() {
    let scratch = testing::Scratch::new();
    let engine = testing::engine::<Lane>(&DUMPED);
    let program = "function body() { Fiber::suspend(1); echo \"never\\n\"; }
        class W { function __toString(): string { return '2'; } }
        $f = new Fiber('body');
        $f->start();
        echo new W, \"\\n\";
        $f = null;
        echo new W, \"\\n\";";
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
            "---- TRACE 2 start (enter) W::__toString() body.php:3",
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
    type State = crate::JitRequest;

    fn lay(started: &tier::Started<'_>) -> Option<crate::JitRequest> {
        crate::JitRequest::laid(*started)
    }

    fn safepoint(req: &mut Request, frame: FrameRef, site: Site, held: tier::Held<'_>) -> Flow {
        let halves = |req: &Request| {
            let word = held.word(req, site.idx());
            [0, 1].map(|half| word.halves()[half].load(Ordering::Relaxed))
        };
        let before = halves(req);
        let flow = super::safepoint(req, frame, site, held);
        let after = halves(req);
        let spied = req.state::<Spied>();
        spied.sites += 1;
        spied.written += u32::from(before != after);
        flow
    }

    fn extension() -> Option<Extension> {
        let mut ini = crate::policy::ini();
        ini.extend(testing::declared(&testing::OPCACHE));
        Some(
            testing::switched(ExtensionBuilder::default())
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

/// What the loop asked a tier bound as `Jit` is over a serving: each
/// site it was asked at, and the back edges whose word the visit wrote.
#[derive(Debug, Default, PartialEq, Eq)]
struct Asked {
    loops: u32,
    entries: u32,
    returns: u32,
    loops_written: u32,
}

/// `Lane` bound as the `jit` build binds `Jit` ([`Tier::asked`]), with
/// every site it is asked at counted and a back edge's word read before
/// and after its visit.
struct Counted;

impl Tier for Counted {
    type State = crate::JitRequest;

    fn lay(started: &tier::Started<'_>) -> Option<crate::JitRequest> {
        crate::JitRequest::laid(*started)
    }

    fn safepoint(req: &mut Request, frame: FrameRef, site: Site, held: tier::Held<'_>) -> Flow {
        let halves = |req: &Request| {
            let word = held.word(req, site.idx());
            [0, 1].map(|half| word.halves()[half].load(Ordering::Relaxed))
        };
        let before = halves(req);
        let flow = super::safepoint(req, frame, site, held);
        let written = before != halves(req);
        let asked = req.state::<Asked>();
        match site {
            Site::Loop(_) => {
                asked.loops += 1;
                asked.loops_written += u32::from(written);
            }
            Site::Entry => asked.entries += 1,
            Site::Return(_) => asked.returns += 1,
        }
        flow
    }

    fn bound(started: &tier::Started<'_>) -> tier::Bound {
        <crate::Jit as Tier>::bound(started)
    }

    fn laid(started: &tier::Started<'_>, req: Option<&Request>) -> tier::Laid {
        <crate::Jit as Tier>::laid(started, req)
    }

    fn setting(started: &tier::Started<'_>, startup: TierStatus) -> TierStatus {
        <crate::Jit as Tier>::setting(started, startup)
    }

    fn extension() -> Option<Extension> {
        let mut ini = crate::policy::ini();
        ini.extend(testing::declared(&testing::OPCACHE));
        Some(
            testing::switched(ExtensionBuilder::default())
                .ini(ini)
                .engine_state(crate::JitEngine::new)
                .request_state::<Asked>()
                .build("jit"),
        )
    }
}

/// A tier that is `Off` is never asked (#3133): the engine binds `Nop`'s
/// loop, so no loop's back edge reads its word or bumps its heat, and
/// no call or return asks either, under a zero buffer, G1's setting, as
/// under `opcache.jit=disable` and on the command line without
/// `opcache.enable_cli`. Under tracing the same program asks at every
/// back edge and bumps the header's heat.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_tier_that_is_off_is_asked_at_no_back_edge() {
    let program = "function f($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += $i; } return $s; }
        for ($j = 0; $j < 5; $j++) { echo f($j); }";
    for (host, off) in [
        (
            "",
            &[("opcache.jit", "tracing"), ("opcache.jit_buffer_size", "0")][..],
        ),
        ("", &[("opcache.jit", "disable")]),
        ("cli", &[("opcache.jit", "tracing")]),
    ] {
        let engine = testing::engine_on::<Counted>(host, off);
        let (served, asked) = testing::Scratch::new().serve_kept::<Asked>(&engine, program);
        assert_eq!(testing::stdout(&served), "00136", "{off:?} on {host:?}");
        assert_eq!(
            asked.unwrap_or_default(),
            Asked::default(),
            "{off:?} on {host:?}"
        );
    }
    let on = [("opcache.jit", "tracing"), ("opcache.jit_hot_loop", "255")];
    let scratch = testing::Scratch::new();
    let (served, asked) = scratch.serve_kept::<Asked>(&testing::engine::<Counted>(&on), program);
    assert_eq!(testing::stdout(&served), "00136");
    let asked = asked.unwrap_or_default();
    // The back edges of `f`'s loop, ten over its five calls, and of
    // `$main`'s, five, each bumping its header's heat.
    assert_eq!((asked.loops, asked.loops_written), (15, 15), "{asked:?}");
    assert!(asked.entries >= 6 && asked.returns >= 5, "{asked:?}");
}

/// php gives an op array its counters as it persists it while its tier
/// is on (#3139), and the loop that serves it follows (#3362). Set up
/// and off, a request starts in `Nop`'s loop, which asks the tier at no
/// site; one that turns the tier on and includes a file publishes it
/// with counting words, and its loop hands the included frame to the
/// tier's at the activation's check, where the file's entry and loop
/// are asked, its primed words claimed at their first arrivals however
/// high the threshold (#3363). On from the start, a unit's words
/// count from cold, each back edge bumping its heat, and a unit
/// published after a request turns the tier off is sealed, asked at
/// every back edge and never written.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_unit_counts_as_its_request_had_the_tier_when_it_was_published() {
    let looped = "function g() { $s = 0; for ($i = 0; $i < 4; $i++) { $s += $i; } return $s; }";
    let program = |setting: Option<&str>| {
        let switch = setting.map_or(String::new(), |s| format!("ini_set('opcache.jit', '{s}');"));
        format!("{switch}\necho include __DIR__ . '/loop.php';")
    };
    let hot = ("opcache.jit_hot_loop", "255");
    let serve = |start: &str, switch: Option<&str>| {
        let engine = testing::engine::<Counted>(&[("opcache.jit", start), hot]);
        let scratch = testing::Scratch::new();
        let loop_file = format!("<?php {looped} return g();");
        std::fs::write(scratch.0.join("loop.php"), loop_file).unwrap_or_default();
        let (served, asked) = scratch.serve_kept::<Asked>(&engine, &program(switch));
        assert_eq!(testing::stdout(&served), "6", "{start} then {switch:?}");
        asked.unwrap_or_default()
    };
    assert_eq!(
        serve("off", None),
        Asked::default(),
        "Nop's loop throughout"
    );
    let switched = serve("off", Some("tracing"));
    // The included file's primed entry roots at its first arrival, and
    // its recording roots `g`'s loop as it aborts there, so no visit the
    // loop asks writes the loop's word (the `TRACE` lines are
    // `Policy::laid`'s witness).
    assert!(
        switched.entries > 0 && switched.loops > 0,
        "handed to the tier's loop: {switched:?}"
    );
    let tracing = serve("tracing", None);
    assert_eq!(
        (tracing.loops, tracing.loops_written),
        (4, 4),
        "{tracing:?}"
    );
    let sealed = serve("tracing", Some("off"));
    assert_eq!((sealed.loops, sealed.loops_written), (4, 0), "{sealed:?}");
}

/// A loop in `$main` that no call activates is a root of its own: the
/// entry's recording aborts at the throwable `t()` throws, and the
/// loop's header claims when its back edge visits it, the recording
/// handed back through the jump's `Resume`, `TRACE 1 start (loop)`; its
/// compiled loop leaves through php's `TRACE 1 exit 1 …:6`, the line past
/// the loop the jump goes to, and grows php's side trace there, which
/// goes on to the file's return. The pinned php under the recipe writes
/// these lines, line for line.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_main_loop_no_call_activates_is_a_root_at_its_header() {
    let engine = testing::engine::<Lane>(&DUMPED);
    let program = "function t() { throw new Exception(\"x\"); }\ntry { t(); } catch (Exception $e) {}\n$s = 0;\nfor ($i = 0; $i < 3; $i++) { $s += $i; }\necho $s, \"\\n\";";
    let served = testing::Scratch::new().serve(&engine, program);
    assert_eq!(testing::stdout(&served), "3\n");
    assert_eq!(
        lines(&served),
        [
            "JIT: jit.mode=force",
            "---- TRACE 1 start (enter) $main() body.php:3",
            "---- TRACE 1 abort (exception)",
            "",
            "---- TRACE 1 start (loop) $main() body.php:5",
            "---- TRACE 1 stop (loop)",
            "---- TRACE 1 compiled",
            "",
            "     TRACE 1 exit 1 $main() body.php:6",
            "---- TRACE 2 start (side trace 1/1) $main() body.php:6",
            "---- TRACE 2 stop (return)",
            "---- TRACE 2 compiled",
            "",
        ]
    );
}

/// A return root is the caller's landing past the call, armed where a
/// compiled trace hands on to the interpreter at that call (`jit-design.md`
/// §2.2, #3036): the entry's trace stops at the `eval`, which arms
/// `$main`'s landing past it, as php's `zend_jit_trace_setup_ret_counter`
/// does, and the `eval`'d code's return lands there and roots `TRACE 2
/// start (return) $main() …:3`, whose recording aborts at the loop php
/// records next, `TRACE 2 start (loop) $main() …:4`. The pinned php under
/// the recipe writes these lines, line for line.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_return_roots_at_the_landing_past_the_eval_its_trace_stopped_at() {
    let engine = testing::engine::<Lane>(&DUMPED);
    let program = "eval(\"\\$x = 1;\");\n$s = 0;\nfor ($i = 0; $i < 3; $i++) { $s += $i; }\necho $s, \"\\n\";";
    let served = testing::Scratch::new().serve(&engine, program);
    assert_eq!(testing::stdout(&served), "3\n");
    assert_eq!(
        lines(&served),
        [
            "JIT: jit.mode=force",
            "---- TRACE 1 start (enter) $main() body.php:2",
            "---- TRACE 1 stop (exit to VM interpreter)",
            "---- TRACE 1 compiled",
            "",
            "---- TRACE 2 start (return) $main() body.php:3",
            "---- TRACE 2 abort (inner loop)",
            "---- TRACE 2 start (loop) $main() body.php:4",
            "---- TRACE 2 stop (loop)",
            "---- TRACE 2 compiled",
            "",
            "     TRACE 2 exit 1 $main() body.php:5",
            "---- TRACE 3 start (side trace 2/1) $main() body.php:5",
            "---- TRACE 3 stop (return)",
            "---- TRACE 3 compiled",
            "",
        ]
    );
}

/// Under `jit.mode=exit-fuzz` every guard a compiled trace holds fails
/// once, the loop's interrupt test among them, and the interpreter goes
/// on at each landing to print what it prints under `force`: `a`'s three
/// entry guards, `$n`, `$s` and `$i` in their slots' order, leave through
/// exits of their own, each knowing the types those before it held, as
/// php's stack map does; the interrupt test through its own, numbered
/// last and growing no side trace, php's `ZEND_JIT_EXIT_TO_VM`; the
/// `+=`'s overflow guard through its own, where php holds the guards of
/// the loop's phis (#3135), and the increment none, since php's
/// `zend_may_overflow` reads `$i` below the bound the loop's `$i < $n`
/// leaves it as the increment is compiled (#3898), as php's exit info
/// lists `exit_5` for the interrupt test and none past the `PRE_INC`;
/// then the loop runs compiled to its end, through php's `exit 3`, the trace starting at the
/// loop's condition and meeting the jump's direction before the body
/// (#3134). Each exit but the interrupt test's, taken once, grows a side
/// trace, as php's does at its first exit under the recipe's
/// `opcache.jit_hot_side_exit=1`, so under `force` the loop's end is
/// written once and the second call's trace, linking into the loop,
/// leaves through the side its end grew, writing nothing. A jump's
/// direction is never failed: its exit lands where the jump it did not
/// expect went.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn under_exit_fuzz_every_guard_fails_once_and_the_output_is_forces() {
    let program = "function a($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += $i; } return $s; }
        echo a(10), a(20), \"\\n\";";
    let exits = |served: &php_engine::test::Served| -> Vec<String> {
        lines(served)
            .into_iter()
            .filter(|line| line.starts_with("     TRACE"))
            .collect()
    };
    let force = testing::serve::<Lane>(&DUMPED, program);
    assert_eq!(testing::stdout(&force), "45190\n");
    assert_eq!(exits(&force), ["     TRACE 1 exit 3 a() body.php:2"]);
    let mut fuzzed = DUMPED.to_vec();
    fuzzed[0] = ("jit.mode", "exit-fuzz");
    let fuzz = testing::serve::<Lane>(&fuzzed, program);
    assert_eq!(testing::stdout(&fuzz), "45190\n");
    assert_eq!(
        exits(&fuzz),
        [0, 1, 2, 5, 4, 3].map(|exit| format!("     TRACE 1 exit {exit} a() body.php:2"))
    );
}

/// A loop's trace entered at its body from its back edge
/// (`types::BodyEntry`) holds the frame to what it knows there: under
/// exit-fuzz every guard fails once, and the interpreter, going on at
/// each landing, comes to the back edge with the frame as it left it;
/// where `$s` turned a string the entry declines, nothing ran, and the
/// trace is entered at its condition, whose guards take the exits. What
/// it prints is the interpreter's under force, exit-fuzz and tracing.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_loop_entered_at_its_body_holds_the_frame_to_what_it_knows_there() {
    let program = "function f($n) { $s = 0; for ($i = 0; $i < $n; $i++) {
            $s = $i < 4 ? $s + $i : \"s$i\"; } return $s; }
        echo f(8), f(3), f(8), \"\\n\";";
    for mode in ["force", "exit-fuzz", "tracing"] {
        let mut ini = DUMPED.to_vec();
        ini[0] = ("jit.mode", mode);
        let served = testing::serve::<Lane>(&ini, program);
        assert_eq!(
            (testing::stdout(&served), served.fatal),
            ("s73s7\n".to_owned(), None),
            "{mode}"
        );
    }
}

/// `run` from trace to trace (`jit-design.md` §4.9 *Tested*), on an
/// aarch64 target whose arena maps code, the bodies' instruction set: two
/// traces assembled by hand, each
/// serving one opline through a kernel that keeps the number of the
/// trace that called it.
#[cfg(all(php_jit_arena, target_arch = "aarch64", not(miri)))]
mod ran {
    use std::sync::Arc;
    use std::sync::atomic::Ordering;

    use php_engine::tier::{
        self, Extension, ExtensionBuilder, Fatal, Flow, FrameRef, Request, Site, Tier,
    };

    use super::super::run_lent;
    use crate::enter::tests::entered::numbered;
    use crate::policy::{self, Holder, Outcome, State};
    use crate::record::Start;
    use crate::table;
    use crate::testing::bodies::{Numbered, answering, calling, compiled, landing};
    use crate::testing::records::trace;
    use crate::testing::{self, Scratch, function_of, stdout};
    use crate::types::{ExitId, Landing};

    /// What `run` answered at `f`'s activation, and what became of the
    /// cold exit's word.
    #[derive(Debug, Default)]
    struct Ran {
        /// The two traces, by number: the first entered, the one it links to.
        traces: Option<(u32, u32)>,
        /// The numbers of the traces the kernel was called by, in order.
        entered: Vec<u32>,
        /// Where the flow `run` answered resumes: the frame was `f`'s, and
        /// the opline it stands at.
        resumed: Option<(bool, u32)>,
        /// The cold exit word's state and heat after it was visited.
        cold: Option<(State, u32)>,
    }

    /// The tier that, at `f`'s activation, publishes two traces and runs
    /// the first: `linking` serves `f`'s opline 0 and leaves through an
    /// exit landing at opline 1 that links to `cold`, which serves opline
    /// 1 and leaves through an exit landing at opline 2 whose word is
    /// cold.
    struct Running;

    impl Tier for Running {
        type State = crate::JitRequest;

        fn lay(started: &tier::Started<'_>) -> Option<crate::JitRequest> {
            crate::JitRequest::laid(*started)
        }

        fn safepoint(req: &mut Request, frame: FrameRef, site: Site, _: tier::Held<'_>) -> Flow {
            if site != Site::Entry || function_of(req, frame).as_deref() != Some(b"f") {
                return Flow::Continue;
            }
            let Some(shared) = req
                .engine::<crate::JitEngine>()
                .map(|engine| Arc::clone(&engine.shared))
            else {
                return Flow::Bail(Fatal::error("no engine state"));
            };
            let epoch = table::holding(req, &shared.policy, &shared.perf);
            let Some(table) = table::of_epoch(&epoch) else {
                return Flow::Bail(Fatal::error("no table"));
            };
            let number = (numbered::number as *const ()).addr();
            let body = |at: u32| answering(calling(number, at, &[], true), 0);
            let cold = compiled(&body(1), vec![landing(2, Landing::PLAIN)]);
            let Ok(cold) = table.publish(cold, Start::Enter) else {
                return Flow::Bail(Fatal::error("the cold trace was not published"));
            };
            let mut linked = landing(1, Landing::PLAIN);
            linked.link = Some(cold);
            let Ok(linking) = table.publish(compiled(&body(0), vec![linked]), Start::Enter) else {
                return Flow::Bail(Fatal::error("the linking trace was not published"));
            };
            let flow = run_lent(req, frame, linking);
            let resumed = match flow {
                Flow::Resume(at) => tier::frame(req, at).map(|view| (at == frame, view.opline().0)),
                _ => None,
            };
            let word = table
                .get(cold)
                .and_then(|trace| trace.exits.first())
                .map(|exit| {
                    let heat = exit.word.halves()[0].load(Ordering::Relaxed);
                    (policy::read(&exit.word), heat)
                });
            let entered = std::mem::take(&mut req.state::<Numbered>().0);
            let ran = req.state::<Ran>();
            ran.entered = entered;
            ran.traces = Some((linking.get(), cold.get()));
            ran.resumed = resumed;
            ran.cold = word;
            flow
        }

        fn extension() -> Option<Extension> {
            let mut ini = crate::policy::ini();
            ini.extend(testing::declared(&testing::OPCACHE));
            Some(
                testing::switched(ExtensionBuilder::default())
                    .ini(ini)
                    .engine_state(crate::JitEngine::new)
                    .request_state::<Ran>()
                    .request_state::<Numbered>()
                    .build("jit"),
            )
        }
    }

    /// The linking trace is entered, then the one it links to on the
    /// frame its exit landed, with no word read between; that one's exit
    /// lands at a word nothing compiled, which is bumped and claimed by
    /// nothing, php's `TRACE n exit m` line written for it, and `run`
    /// answers `Resume` of the frame it landed, `f`'s at opline 2, where
    /// the interpreter goes on.
    #[test]
    fn run_follows_a_link_and_resumes_the_frame_a_cold_exit_landed() {
        let engine = testing::engine::<Running>(&[
            ("opcache.jit", "tracing"),
            ("opcache.jit_debug", "32768"),
        ]);
        let program = r#"function f() { echo "a"; echo "b"; echo "c"; echo "d"; } f();"#;
        let scratch = Scratch::new();
        let (served, ran) = scratch.serve_kept::<Ran>(&engine, program);
        assert_eq!(stdout(&served), "abcd");
        let ran = ran.unwrap_or_default();
        assert_eq!(ran.traces, Some((2, 1)));
        assert_eq!(ran.entered, [2, 1]);
        assert_eq!(ran.resumed, Some((true, 2)));
        assert_eq!(ran.cold, Some((State::Cold, 1)));
        let stderr = String::from_utf8_lossy(&served.stderr);
        let line = stderr.lines().next().unwrap_or_default();
        assert!(
            line.starts_with("     TRACE 1 exit 0 f() ") && line.ends_with("body.php:2"),
            "{stderr}"
        );
    }

    /// What `run` did from a root whose exit's word names a side trace.
    #[derive(Debug, Default)]
    struct Sided {
        entered: Vec<u32>,
        resumed: Option<(bool, u32)>,
        /// The side's own exit word's state and heat after it was visited.
        cold: Option<(State, u32)>,
    }

    /// At `f`'s activation: a root serving `f`'s opline 0 that leaves
    /// through an exit landing at opline 1, whose word is settled compiled
    /// as a side's, published where `published`, serving opline 1 and
    /// leaving through a cold exit landing at opline 2; or naming trace
    /// 99, which no publish wrote; then `run` from the root.
    fn sided(req: &mut Request, frame: FrameRef, published: bool) -> Flow {
        let Some(shared) = req
            .engine::<crate::JitEngine>()
            .map(|engine| Arc::clone(&engine.shared))
        else {
            return Flow::Bail(Fatal::error("no engine state"));
        };
        let epoch = table::holding(req, &shared.policy, &shared.perf);
        let Some(table) = table::of_epoch(&epoch) else {
            return Flow::Bail(Fatal::error("no table"));
        };
        let number = (numbered::number as *const ()).addr();
        let body = |at: u32| answering(calling(number, at, &[], true), 0);
        let root = compiled(&body(0), vec![landing(1, Landing::PLAIN)]);
        let Ok(root) = table.publish(root, Start::Enter) else {
            return Flow::Bail(Fatal::error("the root was not published"));
        };
        let grew = Start::Side {
            parent: root,
            exit: ExitId(0),
        };
        let side = if published {
            let side = compiled(&body(1), vec![landing(2, Landing::PLAIN)]);
            match table.publish(side, grew) {
                Ok(side) => side,
                Err(_) => return Flow::Bail(Fatal::error("the side was not published")),
            }
        } else {
            trace(99)
        };
        let holder = Holder::Exit {
            epoch: tier::epoch(req),
            trace: root,
            exit: ExitId(0),
        };
        let Some(claim) = policy::claim(holder, &shared.policy, grew) else {
            return Flow::Bail(Fatal::error("the root's exit was not claimed"));
        };
        policy::settle(claim, &Outcome::Published(side));
        let flow = run_lent(req, frame, root);
        let resumed = match flow {
            Flow::Resume(at) => tier::frame(req, at).map(|view| (at == frame, view.opline().0)),
            _ => None,
        };
        let cold = (table.get(side))
            .and_then(|trace| trace.exits.first())
            .map(|exit| {
                let heat = exit.word.halves()[0].load(Ordering::Relaxed);
                (policy::read(&exit.word), heat)
            });
        let entered = std::mem::take(&mut req.state::<Numbered>().0);
        let kept = req.state::<Sided>();
        kept.entered = entered;
        kept.resumed = resumed;
        kept.cold = cold;
        flow
    }

    /// [`sided`] with the side published, and with none.
    struct SideRuns<const PUBLISHED: bool>;

    impl<const PUBLISHED: bool> Tier for SideRuns<PUBLISHED> {
        type State = crate::JitRequest;

        fn lay(started: &tier::Started<'_>) -> Option<crate::JitRequest> {
            crate::JitRequest::laid(*started)
        }

        fn safepoint(req: &mut Request, frame: FrameRef, site: Site, _: tier::Held<'_>) -> Flow {
            if site != Site::Entry || function_of(req, frame).as_deref() != Some(b"f") {
                return Flow::Continue;
            }
            sided(req, frame, PUBLISHED)
        }

        fn extension() -> Option<Extension> {
            let mut ini = crate::policy::ini();
            ini.extend(testing::declared(&testing::OPCACHE));
            Some(
                testing::switched(ExtensionBuilder::default())
                    .ini(ini)
                    .engine_state(crate::JitEngine::new)
                    .request_state::<Sided>()
                    .request_state::<Numbered>()
                    .build("jit"),
            )
        }
    }

    /// A side trace's exit that lands at a word nothing compiled is a
    /// `Resume` of the frame it landed (`jit-design.md` §4.9): the root is
    /// entered, its exit's word names the side, which is entered on the
    /// frame the root's exit landed with no line written, and the side's
    /// exit lands at its cold word, which is bumped, so `run` answers
    /// `Resume` of `f`'s frame at opline 2.
    #[test]
    fn a_side_traces_exit_that_lands_cold_resumes_the_frame_it_landed() {
        let engine = testing::engine::<SideRuns<true>>(&[
            ("opcache.jit", "tracing"),
            ("opcache.jit_debug", "32768"),
        ]);
        let program = r#"function f() { echo "a"; echo "b"; echo "c"; echo "d"; } f();"#;
        let (served, kept) = Scratch::new().serve_kept::<Sided>(&engine, program);
        assert_eq!(stdout(&served), "abcd");
        let kept = kept.unwrap_or_default();
        assert_eq!(kept.entered, [1, 2]);
        assert_eq!(kept.resumed, Some((true, 2)));
        assert_eq!(kept.cold, Some((State::Cold, 1)));
        let stderr = String::from_utf8_lossy(&served.stderr);
        let lines: Vec<&str> = stderr.lines().collect();
        assert!(
            lines.len() == 1 && lines[0].starts_with("     TRACE 2 exit 0 f() "),
            "the side's exit alone is written: {stderr}"
        );
    }

    /// An exit whose word names a side trace no publish wrote, the
    /// window §3.2 names, is a `Resume` of the frame it landed: the root
    /// ran and its exit landed `f`'s frame at opline 1, where the
    /// interpreter goes on.
    #[test]
    fn a_missing_side_target_resumes_the_frame_its_exit_landed() {
        let engine = testing::engine::<SideRuns<false>>(&[
            ("opcache.jit", "tracing"),
            ("opcache.jit_debug", "32768"),
        ]);
        let program = r#"function f() { echo "a"; echo "b"; echo "c"; echo "d"; } f();"#;
        let (served, kept) = Scratch::new().serve_kept::<Sided>(&engine, program);
        assert_eq!(stdout(&served), "abcd");
        let kept = kept.unwrap_or_default();
        assert_eq!(kept.entered, [1]);
        assert_eq!(kept.resumed, Some((true, 1)));
        assert_eq!(kept.cold, None);
        assert_eq!(String::from_utf8_lossy(&served.stderr), "");
    }
}

/// A frame the tier moved at its activation goes on where the tier left
/// it, on a call the loop made before as on its first. The closure's
/// trace, which stops at the call php's
/// `opcache.jit_max_polymorphic_calls=0` leaves to the interpreter, is
/// entered at the closure's activation and leaves its frame standing at
/// that call. A closure's push stands its frame at its entry safepoint,
/// so the loop took the frame up again where the push had put it, served
/// `echo "a"` and the call's `INIT_DYNAMIC_CALL` a second time, and left
/// a call being built on the stack; `gh22443.phpt` died past the end of
/// an op array (#3077).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_call_the_tier_moved_at_its_activation_goes_on_where_the_tier_left_it() {
    let program = "function g() { return 1; }
        $f = function ($c) { echo \"a\"; return $c(); };
        for ($i = 0; $i < 3; $i++) { echo $f('g'), \"\\n\"; }";
    for mode in ["force", "tracing"] {
        let served = testing::serve::<Lane>(
            &[
                ("jit.mode", mode),
                ("jit.compile", "inline"),
                ("opcache.jit_hot_func", "1"),
                ("opcache.jit_hot_loop", "1"),
                ("opcache.jit_max_polymorphic_calls", "0"),
            ],
            program,
        );
        assert_eq!(
            (testing::stdout(&served), served.fatal),
            ("a1\na1\na1\n".to_owned(), None),
            "{mode}"
        );
    }
}

/// A destructor that throws while a compiled loop's assignment frees the
/// object it overwrites: the loop leaves through its unwind, which takes
/// no exit and writes no `TRACE 1 exit` line, as php's compiled code
/// jumps to its stub for a throwable, with the sum it kept in a register
/// stored, so the `catch` in the loop's own frame reads it.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_throw_in_a_compiled_loop_unwinds_through_no_exit() {
    let program = "class D { public $i; function __construct($i) { $this->i = $i; }
            function __destruct() { if ($this->i == 5) { throw new Exception(\"d\"); } } }
        function f() { $s = 0; for ($i = 0; $i < 10; $i++) { $s += $i; $o = new D($i);
            try { $o = 1; } catch (Exception $e) { return $s; } } return -1; }
        echo f(), \"\\n\";";
    let served = testing::serve::<Lane>(&DUMPED, program);
    assert_eq!(testing::stdout(&served), "15\n");
    let lines = lines(&served);
    assert!(
        lines.iter().any(|line| line == "---- TRACE 1 compiled"),
        "{lines:?}"
    );
    assert!(
        !lines
            .iter()
            .any(|line| line.starts_with("     TRACE 1 exit")),
        "{lines:?}"
    );
}

/// A loop's back edge lands on its body, and its trace is recorded and
/// entered at its condition, php's header (`record::headed`): a body that
/// leaves the loop on the way there, a `break`, records nothing and goes
/// on where it went, and one that gets there records from the condition,
/// where php's trace takes its heat.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_body_that_leaves_on_the_way_to_the_condition_goes_on_where_it_went() {
    let program =
        "function f($n) { for ($i = 0; $i < 10; $i++) { if ($i == $n) { break; } } return $i; }
        echo f(1), f(5), \"\\n\";";
    let served = testing::serve::<Lane>(&DUMPED, program);
    assert_eq!(testing::stdout(&served), "15\n");
    let starts: Vec<String> = (lines(&served).into_iter())
        .filter(|line| line.contains("start (loop)"))
        .collect();
    assert_eq!(starts, ["---- TRACE 1 start (loop) f() body.php:2"]);
}

/// How many escapes the crossing went on from into a landing's trace
/// in the request, how many times an exit or an escape visited the
/// opline it left a frame at ([`crate::JitRequest`]'s counts), and the
/// landings the request's open escape doors name, with the return root
/// compiled at each, as [`Escaping`] keeps them.
#[derive(Debug, Default)]
struct Crossed(u32, u32, Vec<(usize, bool)>);

/// `Lane`, which keeps the request's count of escapes the crossing went
/// on from in [`Crossed`] after each site; with `ELSEWHERE`, each escape's
/// open door made to name a landing no escape reaches, so every escape
/// takes the way past its door.
struct Escaping<const ELSEWHERE: bool = false>;

impl<const ELSEWHERE: bool> Tier for Escaping<ELSEWHERE> {
    type State = crate::JitRequest;

    fn lay(started: &tier::Started<'_>) -> Option<crate::JitRequest> {
        crate::JitRequest::laid(*started)
    }

    fn safepoint(req: &mut Request, frame: FrameRef, site: Site, held: tier::Held<'_>) -> Flow {
        let flow = super::safepoint(req, frame, site, held);
        let (crossed, visited) = tier::state::<crate::JitRequest>(req).map_or((0, 0), |state| {
            (state.escapes_crossed.get(), state.landings_visited.get())
        });
        let doors = escape_doors(req, ELSEWHERE);
        *req.state::<Crossed>() = Crossed(crossed, visited, doors);
        flow
    }

    fn bound(started: &tier::Started<'_>) -> tier::Bound {
        <Lane as Tier>::bound(started)
    }

    fn laid(started: &tier::Started<'_>, req: Option<&Request>) -> tier::Laid {
        <Lane as Tier>::laid(started, req)
    }

    fn setting(started: &tier::Started<'_>, startup: TierStatus) -> TierStatus {
        <Lane as Tier>::setting(started, startup)
    }

    /// `Lane`'s, which preloads the file `opcache.preload` names, as
    /// opcache's `accel_post_startup` does.
    fn extension() -> Option<Extension> {
        let mut ini = crate::policy::ini();
        ini.extend(testing::declared(&testing::OPCACHE));
        ini.extend(testing::declared(&["opcache.preload"]));
        Some(
            testing::switched(ExtensionBuilder::default())
                .ini(ini)
                .engine_state(crate::JitEngine::new)
                .rinit(crate::JitEngine::request_started)
                .request_state::<Crossed>()
                .post_startup(|req, _| match req.ini().get(b"opcache.preload") {
                    Some(script) if !script.is_empty() => PostStartup::Preload {
                        script: script.to_vec(),
                        thread: None,
                    },
                    _ => PostStartup::Serve,
                })
                .build("jit"),
        )
    }
}

/// The landing each of the request's open escape doors names, and
/// whether the body it enters is that of a return root compiled there;
/// `elsewhere`, each made to name a landing no escape reaches.
fn escape_doors(req: &mut Request, elsewhere: bool) -> Vec<(usize, bool)> {
    let doors = crate::enter::lent(req, |_, lent| {
        let Some(table) = crate::table::of_epoch(lent.epoch) else {
            return Vec::new();
        };
        let traces = || (1..).map_while(|id| table.get(TraceId::new(id)?));
        let doors = traces().filter_map(|trace| {
            let door = crate::lower::Door::Escape;
            lent.state.headers.door(trace, door, elsewhere)
        });
        doors
            .filter(|[_, body, _]| *body != 0)
            .map(|[key, body, _]| {
                let enters = traces().any(|trace| {
                    let opens = trace.opens.and_then(|(func, opline)| {
                        let func = crate::lower::engine_offset(func)?;
                        let stands = crate::lower::Stands {
                            func,
                            opline: opline.0,
                        };
                        usize::try_from(stands.word()).ok()
                    });
                    trace.start == Start::Return
                        && trace.body().as_ptr().addr() == body
                        && opens == Some(key)
                });
                (key, enters)
            })
            .collect()
    });
    doors.unwrap_or_default()
}

/// fib's side traces escape past fib's first call before a trace is
/// compiled at that landing, and that trace is first entered from the
/// interpreter's return, not from an escape: the entry through `enter`
/// that follows the escape's opens the escape's door, which names the
/// landing that trace was compiled at (#4108's ruling, `jit-design.md`
/// §4.9), so from there on the escape enters it through the door. Nothing
/// opens a door to a landing no escape reached.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn an_escapes_door_names_the_landing_it_reached() {
    let ini = [("opcache.jit", "tracing"), ("jit.compile", "inline")];
    let engine = testing::engine::<Escaping>(&ini);
    let program = "function fib($n) { if ($n < 2) { return $n; } return fib($n - 1) + fib($n - 2); }\necho fib(20), \"\\n\";";
    let (served, kept) = testing::Scratch::new().serve_kept::<Crossed>(&engine, program);
    assert_eq!(testing::stdout(&served), "6765\n");
    let doors = kept.map(|kept| kept.2).unwrap_or_default();
    assert!(!doors.is_empty(), "no escape's door opened");
    assert!(doors.iter().all(|(_, enters)| *enters), "{doors:?}");
}

/// [`an_escapes_door_names_the_landing_it_reached`] where every count is
/// one, so the trace at the landing is compiled, and entered from the
/// interpreter's return, before the first escape that reaches it: that
/// escape's code enters it through the landing's word, with no crossing
/// to open the door, and leaves its number in the trace's row, so the
/// next entry through `enter` opens the door.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn an_escape_that_found_its_landing_compiled_has_its_door_opened() {
    let ini = [
        ("opcache.jit", "tracing"),
        ("jit.compile", "inline"),
        ("opcache.jit_hot_func", "1"),
        ("opcache.jit_hot_return", "1"),
        ("opcache.jit_hot_side_exit", "1"),
    ];
    let engine = testing::engine::<Escaping>(&ini);
    let program = "function fib($n) { if ($n < 2) { return $n; } return fib($n - 1) + fib($n - 2); }\necho fib(20), \"\\n\";";
    let (served, kept) = testing::Scratch::new().serve_kept::<Crossed>(&engine, program);
    assert_eq!(testing::stdout(&served), "6765\n");
    let doors = kept.map(|kept| kept.2).unwrap_or_default();
    assert!(!doors.is_empty(), "no escape's door opened");
    assert!(doors.iter().all(|(_, enters)| *enters), "{doors:?}");
}

/// A door that names another landing is not a way out: the escape reads
/// the landing's word, and a trace compiled there is entered in code,
/// never answered `ESCAPED` (#4108's ruling, `jit-design.md` §4.9), so the
/// crossing goes on from no more escapes than where the door leads there.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn an_escape_whose_door_names_another_landing_reads_the_landings_word() {
    let ini = [("opcache.jit", "tracing"), ("jit.compile", "inline")];
    let engine = testing::engine::<Escaping<true>>(&ini);
    let program = "function fib($n) { if ($n < 2) { return $n; } return fib($n - 1) + fib($n - 2); }\necho fib(20), \"\\n\";";
    let (served, kept) = testing::Scratch::new().serve_kept::<Crossed>(&engine, program);
    assert_eq!(testing::stdout(&served), "6765\n");
    let crossed = kept.map(|kept| kept.0);
    assert!(
        crossed.is_some_and(|crossed| (1..=4).contains(&crossed)),
        "the crossing went on from {crossed:?} escapes"
    );
}

/// Where a trace escapes to a landing whose word reads compiled, its own
/// code reads the word and goes on into the trace compiled there, as
/// php's escape stub runs that opline's handler (#3299's ruling,
/// `jit-design.md` §4.9): fib's side traces escape at every return that
/// lands past a call no trace goes on at, thousands of times in fib(20),
/// and the crossing goes on into the landing's trace only where the code
/// found that trace's row unready, the first escape to it in the request,
/// which lays the row, so at least once; every other escape enters it in
/// code, where with the code's lookup taken out the crossing goes on from
/// thousands. The traces
/// themselves keep nothing of the landings they escape to.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn an_escape_enters_the_trace_compiled_at_its_landing_in_code() {
    let ini = [("opcache.jit", "tracing"), ("jit.compile", "inline")];
    let engine = testing::engine::<Escaping>(&ini);
    let program = "function fib($n) { if ($n < 2) { return $n; } return fib($n - 1) + fib($n - 2); }\necho fib(20), \"\\n\";";
    let (served, kept) = testing::Scratch::new().serve_kept::<Crossed>(&engine, program);
    assert_eq!(testing::stdout(&served), "6765\n");
    let crossed = kept.map(|kept| kept.0);
    assert!(
        crossed.is_some_and(|crossed| (1..=4).contains(&crossed)),
        "the crossing went on from {crossed:?} escapes"
    );
}

/// An escape to a landing no trace armed, fib's past its second call,
/// where php's interpreter goes on through the opline's own handler,
/// is handed back to the loop with no visit of the landing
/// ([`super::unarmed`], #4100): of the thousands of escapes there in
/// fib(20), only those before the request kept the landing's words
/// visit it. With [`super::unarmed`] fib(20) visits 8, without it 41;
/// the bound of 16 is twice the 8, room for the order the traces are
/// compiled in, and well short of the 41.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn an_escape_to_an_unarmed_landing_visits_nothing() {
    let ini = [("opcache.jit", "tracing"), ("jit.compile", "inline")];
    let engine = testing::engine::<Escaping>(&ini);
    let program = "function fib($n) { if ($n < 2) { return $n; } return fib($n - 1) + fib($n - 2); }\necho fib(20), \"\\n\";";
    let (served, kept) = testing::Scratch::new().serve_kept::<Crossed>(&engine, program);
    assert_eq!(testing::stdout(&served), "6765\n");
    let visited = kept.map(|kept| kept.1);
    assert!(
        visited.is_some_and(|visited| visited <= 16),
        "exits and escapes visited {visited:?} landings"
    );
}

/// `Lane` that keeps the request's count of the returns that visited
/// their site ([`super::return_visit`]) in [`Returned`] after each
/// serving's sites.
struct Testing;

/// How many returns visited their site.
#[derive(Debug, Default)]
struct Returned(u32);

impl Tier for Testing {
    type State = crate::JitRequest;

    fn lay(started: &tier::Started<'_>) -> Option<crate::JitRequest> {
        crate::JitRequest::laid(*started)
    }

    fn safepoint(req: &mut Request, frame: FrameRef, site: Site, held: tier::Held<'_>) -> Flow {
        let flow = super::safepoint(req, frame, site, held);
        let visited =
            tier::state::<crate::JitRequest>(req).map(|state| state.returns_visited.get());
        req.state::<Returned>().0 = visited.unwrap_or(0);
        flow
    }

    fn bound(started: &tier::Started<'_>) -> tier::Bound {
        <Lane as Tier>::bound(started)
    }

    fn laid(started: &tier::Started<'_>, req: Option<&Request>) -> tier::Laid {
        <Lane as Tier>::laid(started, req)
    }

    fn setting(started: &tier::Started<'_>, startup: TierStatus) -> TierStatus {
        <Lane as Tier>::setting(started, startup)
    }

    /// `Lane`'s, with [`Returned`].
    fn extension() -> Option<Extension> {
        let mut ini = crate::policy::ini();
        ini.extend(testing::declared(&testing::OPCACHE));
        Some(
            testing::switched(ExtensionBuilder::default())
                .ini(ini)
                .engine_state(crate::JitEngine::new)
                .rinit(crate::JitEngine::request_started)
                .request_state::<Returned>()
                .build("jit"),
        )
    }
}

/// An interpreted `RETURN` to a landing no trace armed makes no call into
/// `dispatch` (#4105's ruling, `jit-design.md` §2.2): `f`'s return lands
/// fifty times in a loop too cold to count, and the visits [`Testing`]
/// keeps stay none, where a tier with no test visits one a return.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_return_to_an_unarmed_landing_visits_nothing() {
    let ini = [("opcache.jit", "tracing"), ("jit.compile", "inline")];
    let program = "function f($x) { return $x + 1; }\n$s = 0; for ($i = 0; $i < 50; $i++) { $s = f($s); }\necho $s, \"\\n\";";
    let engine = testing::engine::<Testing>(&ini);
    let (served, kept) = testing::Scratch::new().serve_kept::<Returned>(&engine, program);
    assert_eq!(testing::stdout(&served), "50\n");
    assert_eq!(
        kept.map_or(0, |kept| kept.0),
        0,
        "a return visited its site"
    );
}

/// A return to a landing a trace armed does visit: fib's second call's
/// landing roots a return trace, so where its first landing is armed
/// [`super::return_site`] lets the returns through that enter it.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_return_to_an_armed_landing_visits_its_site() {
    let ini = [("opcache.jit", "tracing"), ("jit.compile", "inline")];
    let engine = testing::engine::<Testing>(&ini);
    let program = "function fib($n) { if ($n < 2) { return $n; } return fib($n - 1) + fib($n - 2); }\necho fib(20), \"\\n\";";
    let (served, kept) = testing::Scratch::new().serve_kept::<Returned>(&engine, program);
    assert_eq!(testing::stdout(&served), "6765\n");
    assert!(
        kept.is_some_and(|kept| kept.0 > 0),
        "no return visited its site"
    );
}

/// [`an_escape_enters_the_trace_compiled_at_its_landing_in_code`] with
/// fib preloaded (#3568): a preloaded op array has its words and its
/// landings in every epoch (`php_engine::arena::Base`), and the epoch's
/// landing index names them as it names the ones its cache holds, so an
/// escape to a landing compiled in fib enters its trace in code, as php's
/// escape stub runs the landing's handler whatever file the op array
/// is of; the crossing goes on from at most the first escape to each.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn an_escape_enters_the_trace_compiled_at_a_preloaded_landing_in_code() {
    let scratch = testing::Scratch::new();
    let preload = scratch.0.join("preload.php");
    let fib =
        "<?php function fib($n) { if ($n < 2) { return $n; } return fib($n - 1) + fib($n - 2); }";
    std::fs::write(&preload, fib).expect("the preload's file");
    let script = preload.to_string_lossy().into_owned();
    let ini = [
        ("opcache.jit", "tracing"),
        ("jit.compile", "inline"),
        ("opcache.preload", script.as_str()),
    ];
    let engine = testing::engine::<Escaping>(&ini);
    let (served, kept) = scratch.serve_kept::<Crossed>(&engine, "echo fib(20), \"\\n\";");
    assert_eq!(testing::stdout(&served), "6765\n");
    let crossed = kept.map(|kept| kept.0);
    assert!(
        crossed.is_some_and(|crossed| (1..=4).contains(&crossed)),
        "the crossing went on from {crossed:?} escapes"
    );
}

/// `Lane` that counts, in [`Landed`], each return the tier is asked
/// about whose caller serves an op array an epoch holds, and each of
/// those whose caller does not stand past a call ([`record::lands_past`]),
/// before it asks [`super::safepoint`]. A caller whose op array no epoch
/// holds reads only sealed landings ([`tier::landing_stood`]).
struct Landing;

/// The returns [`Landing`] saw, and how many of them landed past no call.
#[derive(Debug, Default)]
struct Landed {
    held: u32,
    past_no_call: u32,
}

impl Tier for Landing {
    type State = crate::JitRequest;

    fn lay(started: &tier::Started<'_>) -> Option<crate::JitRequest> {
        crate::JitRequest::laid(*started)
    }

    fn safepoint(req: &mut Request, frame: FrameRef, site: Site, held: tier::Held<'_>) -> Flow {
        if let Site::Return(_) = site
            && let Some((at, code)) = tier::frame(req, frame)
                .map(|view| view.opline())
                .zip(tier::served(req, frame))
        {
            let before = at.0.checked_sub(1);
            let past_a_call = before
                .and_then(|before| code.code().oplines.get(before as usize))
                .is_some_and(|op| record::lands_past(op.opcode));
            let landed = req.state::<Landed>();
            landed.held += 1;
            landed.past_no_call += u32::from(!past_a_call);
        }
        super::safepoint(req, frame, site, held)
    }

    fn bound(started: &tier::Started<'_>) -> tier::Bound {
        <Lane as Tier>::bound(started)
    }

    fn laid(started: &tier::Started<'_>, req: Option<&Request>) -> tier::Laid {
        <Lane as Tier>::laid(started, req)
    }

    fn setting(started: &tier::Started<'_>, startup: TierStatus) -> TierStatus {
        <Lane as Tier>::setting(started, startup)
    }

    /// `Lane`'s, with [`Landed`].
    fn extension() -> Option<Extension> {
        let mut ini = crate::policy::ini();
        ini.extend(testing::declared(&testing::OPCACHE));
        Some(
            testing::switched(ExtensionBuilder::default())
                .ini(ini)
                .engine_state(crate::JitEngine::new)
                .rinit(crate::JitEngine::request_started)
                .request_state::<Landed>()
                .build("jit"),
        )
    }
}

/// A return reads a landing only past a call (#4156): php arms a
/// landing at the opline past a `DO_*CALL`, a `YIELD*` or an
/// `INCLUDE_OR_EVAL` (`zend_jit_trace_setup_ret_counter`), so a
/// destructor or a `__toString` returning from inside a
/// `FRAMELESS_ICALL_2` returns to no landing. Here they return from the
/// entry frame of a serving of their own, which asks no site, and every
/// return the tier is asked about, with `E_ALL` in a loop traces serve,
/// under plain tracing and exit-fuzz, has its caller past a call, the
/// interpreted returns plain tracing leaves among them; the
/// frameless call is made once a turn, printing no `Deprecated` of a
/// cleared operand.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_return_to_no_calls_successor_reads_no_landing() {
    let program = r#"class T { static $boom = []; function __construct(public $n) {} function __destruct() { if (isset(self::$boom[$this->n])) { throw new Exception("dtor{$this->n}"); } } function __toString(): string { return "T"; } }
function g($n) { $k = 0; for ($i = 0; $i < $n; $i++) { try { $k += property_exists(new T(10 * $i + 1), "n") ? 1 : 0; $k += class_exists(new T(10 * $i + 2), false) ? 1 : 0; } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; } } return $k; }
T::$boom = [61 => 1, 92 => 1];
for ($j = 0; $j < 3; $j++) { echo g(12), "\n"; }"#;
    let mut held = 0;
    for mode in ["tracing", "exit-fuzz"] {
        let ini = [
            ("opcache.jit", "tracing"),
            ("jit.compile", "inline"),
            ("jit.mode", mode),
            ("opcache.jit_hot_func", "2"),
            ("opcache.jit_hot_return", "2"),
            ("error_reporting", "32767"),
        ];
        let engine = testing::engine::<Landing>(&ini);
        let (served, kept) = testing::Scratch::new().serve_kept::<Landed>(&engine, program);
        assert_eq!(
            testing::stdout(&served),
            "caught dtor61\ncaught dtor92\n21\n".repeat(3),
            "under {mode}"
        );
        let landed = kept.unwrap_or_default();
        assert_eq!(landed.past_no_call, 0, "under {mode}: {landed:?}");
        held += landed.held;
    }
    assert!(held > 0, "no return was asked about");
}

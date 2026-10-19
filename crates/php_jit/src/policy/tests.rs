//! `policy` at its seam with no engine (`jit-design.md` §4.7, §7): php's
//! `zend_jit_parse_config_num` cases, the state machine as a table over a fixture epoch's
//! word through a `Holder::Root`, the sealed word, a claim settled on
//! another thread, and the dump's lines.

use std::sync::atomic::Ordering;

use php_engine::tier::{Epoch, FuncRef, SafepointIdx, StartupIni, Word};
use php_unit::{OpArray, OplineIdx, Unit};

use super::{
    Claim, Compile, Holder, JitDebug, Mode, Note, OffBecause, Outcome, Place, Policy, Setting,
    State, TraceEvent, bad_stop, bump, claim, read, settle,
};
use crate::record::{Start, Stop};
use crate::table::TraceId;
use crate::types::ExitId;

fn ini(entries: &[(&str, &str)]) -> StartupIni {
    StartupIni {
        entries: entries
            .iter()
            .map(|(name, value)| (name.as_bytes().to_vec(), Some(value.as_bytes().to_vec())))
            .collect(),
    }
}

/// The `Policy` `entries` read to, with the lines it logged.
fn read_ini(entries: &[(&str, &str)]) -> (Policy, Vec<Note>) {
    let mut notes = Vec::new();
    let policy = Policy::from_ini(&ini(entries), &mut notes);
    (policy, notes)
}

/// An epoch holding one unit whose main op array has two safepoints,
/// and the op array's name: its words are zero, `Word::default()`'s.
fn fixture() -> (Epoch, FuncRef) {
    Epoch::fixture(Unit {
        main: OpArray {
            safepoints: vec![OplineIdx(0), OplineIdx(1)].into(),
            ..OpArray::default()
        },
        ..Unit::default()
    })
}

fn root(epoch: &Epoch, func: FuncRef, at: u32) -> Holder {
    Holder::Root {
        epoch: epoch.clone(),
        func,
        at: SafepointIdx(at),
    }
}

fn trace(n: u32) -> TraceId {
    TraceId::new(n).unwrap_or_else(|| unreachable_trace(n))
}

#[track_caller]
fn unreachable_trace(n: u32) -> TraceId {
    assert!(
        n != 0 && n < TraceId::LIMIT,
        "trace {n} is a number a word holds"
    );
    TraceId::new(1).unwrap_or_else(|| unreachable_trace(1))
}

fn side() -> Start {
    Start::Side {
        parent: trace(1),
        exit: ExitId(3),
    }
}

fn attempts(word: &Word) -> u32 {
    (word.halves()[1].load(Ordering::Relaxed) >> 2) & 0x3F
}

fn heat(word: &Word) -> u32 {
    word.halves()[0].load(Ordering::Relaxed)
}

/// `zend_jit_config` and `zend_jit_parse_config_num`: `on` and
/// `tracing` are 1254, `function` 1205, `disable` and `off` are off, a
/// number is read digit by digit and one out of range is php's refusal.
#[test]
fn opcache_jit_is_read_as_php_reads_it() {
    assert_eq!(Setting::of(b"on"), Setting::On { crto: 1254 });
    assert_eq!(Setting::of(b"TRACING"), Setting::On { crto: 1254 });
    assert_eq!(Setting::of(b"function"), Setting::On { crto: 1205 });
    assert_eq!(Setting::of(b"disable"), Setting::Disable);
    assert_eq!(Setting::of(b"off"), Setting::Off);
    assert_eq!(Setting::of(b"0"), Setting::Off);
    assert_eq!(Setting::of(b"1235"), Setting::On { crto: 1235 });
    assert_eq!(
        Setting::of(b"1245"),
        Setting::Invalid,
        "trigger 4 is php's refusal"
    );
    assert_eq!(Setting::of(b"1250"), Setting::Invalid, "opt level 0");
    assert_eq!(Setting::of(b"11255"), Setting::Invalid, "five digits");
    assert_eq!(Setting::of(b"sideways"), Setting::Invalid);
}

/// `function` and any trigger but `HOT_TRACE` trace as tracing does and say so
/// once; `HOT_TRACE` says nothing; `disable`, `off`, a refused setting and
/// a zero buffer are `Off` with one line each.
#[test]
fn a_setting_maps_to_its_mode_with_one_line_where_it_is_not_tracing_as_asked() {
    assert_eq!(read_ini(&[("opcache.jit", "tracing")]), {
        let (policy, _) = read_ini(&[("opcache.jit", "1254")]);
        (policy, Vec::new())
    });
    let (policy, notes) = read_ini(&[("opcache.jit", "function")]);
    assert_eq!(policy.mode(), Mode::Tracing);
    assert_eq!(notes, [Note::AsTracing { crto: 1205 }]);
    let (policy, notes) = read_ini(&[("opcache.jit", "1235")]);
    assert_eq!(policy.mode(), Mode::Tracing);
    assert_eq!(notes, [Note::AsTracing { crto: 1235 }]);
    for (setting, why) in [
        ("disable", OffBecause::Disabled),
        ("off", OffBecause::NotOn),
        ("1245", OffBecause::Invalid),
    ] {
        let (policy, notes) = read_ini(&[("opcache.jit", setting)]);
        assert_eq!(
            (policy.mode(), notes),
            (Mode::Off, vec![Note::Off(why)]),
            "{setting}"
        );
    }
    let (policy, notes) = read_ini(&[("opcache.jit", "on"), ("opcache.jit_buffer_size", "0")]);
    assert_eq!(
        (policy.mode(), notes),
        (Mode::Off, vec![Note::Off(OffBecause::NoBuffer)])
    );
}

/// php's defaults with nothing set; its bounds keep the default for a
/// value outside them; `jit.mode` names the `Mode` whatever `opcache.jit`
/// says, and its lanes compile inline.
#[test]
fn the_directives_take_phps_defaults_and_bounds_and_jit_mode_names_the_lane() {
    let (policy, _) = read_ini(&[]);
    assert_eq!(policy.mode(), Mode::Off);
    assert_eq!(policy.compile(), Compile::Thread);
    assert_eq!(policy.threshold(Start::Loop), Some(61));
    assert_eq!(policy.threshold(Start::Enter), Some(127));
    assert_eq!(policy.threshold(Start::Return), Some(8));
    assert_eq!(policy.threshold(side()), Some(8));
    assert_eq!(
        (policy.attempts(Start::Loop), policy.attempts(side())),
        (16, 8)
    );
    assert_eq!(policy.buffer_size, 64 << 20);
    assert_eq!(
        (
            policy.max_root_traces,
            policy.max_side_traces,
            policy.max_exit_counters
        ),
        (1024, 128, 8192)
    );
    assert_eq!(policy.max_trace_length, 1024);
    let (policy, _) = read_ini(&[
        ("opcache.jit_hot_loop", "256"),
        ("opcache.jit_hot_func", "0"),
        ("opcache.jit_max_loop_unrolls", "0"),
        ("opcache.jit_max_recursive_returns", "4"),
        ("opcache.jit_max_trace_length", "3"),
        ("opcache.jit_debug", "0x3F000"),
    ]);
    assert_eq!(
        policy.threshold(Start::Loop),
        Some(61),
        "past 255: the default"
    );
    assert_eq!(
        policy.threshold(Start::Enter),
        None,
        "zero installs no counter"
    );
    assert_eq!(
        (policy.max_loop_unrolls, policy.max_recursive_returns),
        (8, 2)
    );
    assert_eq!(policy.max_trace_length, 1024);
    assert!(policy.debug.any(JitDebug::TRACE_START) && policy.debug.any(JitDebug::TRACE_BLACKLIST));
    let (policy, notes) = read_ini(&[("jit.mode", "force")]);
    assert_eq!(
        (policy.mode(), policy.compile()),
        (Mode::Force, Compile::Inline)
    );
    assert_eq!(notes, [Note::Lane(Mode::Force)]);
    let (policy, _) = read_ini(&[("jit.mode", "tracing"), ("jit.compile", "inline")]);
    assert_eq!(
        (policy.mode(), policy.compile()),
        (Mode::Tracing, Compile::Inline)
    );
}

/// One row of the state machine: a word of `start`'s kind, cold, heated
/// three times, claimed and settled with `outcome`, and what it reads.
fn row(start: Start, outcome: Outcome) -> (State, u32, u32) {
    let (policy, _) = read_ini(&[]);
    let (epoch, func) = fixture();
    let holder = root(&epoch, func, 1);
    let word = holder.word();
    for _ in 0..3 {
        bump(word);
    }
    let claimed = claim(holder.clone(), &policy, start);
    assert_eq!(read(word), State::Recording);
    if let Some(claimed) = claimed {
        assert_eq!(claimed.start(), start);
        settle(claimed, &outcome);
    }
    (read(word), attempts(word), heat(word))
}

/// The machine as a table, one row per `(State, Start, Outcome)` from
/// cold: a publish compiles, an abort is cold again with one more
/// attempt and no heat, a blacklist is at once; claim is `None` on a
/// word that is not cold.
#[test]
fn the_state_machine_as_a_table() {
    let rows = [
        (
            Start::Loop,
            Outcome::Published(trace(7)),
            (State::Compiled(trace(7)), 0, 3),
        ),
        (
            Start::Enter,
            Outcome::Published(trace(9)),
            (State::Compiled(trace(9)), 0, 3),
        ),
        (
            Start::Return,
            Outcome::Abort(Stop::NotSupported),
            (State::Cold, 1, 0),
        ),
        (side(), Outcome::Abort(Stop::TooLong), (State::Cold, 1, 0)),
        (
            Start::Loop,
            Outcome::Blacklist(Stop::TooManyTraces),
            (State::Blacklisted, 0, 3),
        ),
        (
            side(),
            Outcome::Blacklist(Stop::TooManyExits),
            (State::Blacklisted, 0, 3),
        ),
    ];
    for (start, outcome, expected) in rows {
        assert_eq!(row(start, outcome), expected, "{start:?} {outcome:?}");
    }
    let (policy, _) = read_ini(&[]);
    let (epoch, func) = fixture();
    let first = claim(root(&epoch, func, 0), &policy, Start::Loop);
    assert!(first.is_some());
    assert!(
        claim(root(&epoch, func, 0), &policy, Start::Loop).is_none(),
        "recording"
    );
    if let Some(first) = first {
        settle(first, &Outcome::Published(trace(2)));
    }
    assert!(
        claim(root(&epoch, func, 0), &policy, Start::Loop).is_none(),
        "compiled"
    );
}

/// Sixteen aborts blacklist a root and eight a side, each abort a claim
/// of its own after the word is cold again.
#[test]
fn sixteen_aborts_blacklist_a_root_and_eight_a_side() {
    let (policy, _) = read_ini(&[]);
    for (start, limit) in [(Start::Loop, 16), (side(), 8)] {
        let (epoch, func) = fixture();
        let holder = root(&epoch, func, 0);
        for attempt in 1..=limit {
            assert_eq!(
                read(holder.word()),
                State::Cold,
                "{start:?} before attempt {attempt}"
            );
            let claimed = claim(holder.clone(), &policy, start);
            assert!(claimed.is_some());
            if let Some(claimed) = claimed {
                settle(claimed, &Outcome::Abort(Stop::LoopExit));
            }
        }
        assert_eq!(read(holder.word()), State::Blacklisted, "{start:?}");
        assert!(claim(holder, &policy, start).is_none());
    }
}

/// A claim moved to another thread and dropped there unsettled settles
/// itself as `Abort(CompilerError)`: cold, one more attempt, no heat.
#[test]
fn a_claim_dropped_on_another_thread_leaves_the_word_cold_with_one_more_attempt() {
    let (policy, _) = read_ini(&[]);
    let (epoch, func) = fixture();
    let holder = root(&epoch, func, 1);
    bump(holder.word());
    let claimed: Option<Claim> = claim(holder.clone(), &policy, Start::Loop);
    assert!(claimed.is_some());
    std::thread::spawn(move || drop(claimed)).join().ok();
    let word = holder.word();
    assert_eq!(
        (read(word), attempts(word), heat(word)),
        (State::Cold, 1, 0)
    );
}

/// The layout invariant: the sealed word, all ones, reads blacklisted and
/// is never claimed, as is any number past an op array's safepoints, which
/// reads the sealed word.
#[test]
fn the_sealed_word_reads_blacklisted_and_is_never_claimed() {
    assert_eq!(read(&Word::sealed()), State::Blacklisted);
    assert_eq!(read(&Word::default()), State::Cold);
    let (policy, _) = read_ini(&[]);
    let (epoch, func) = fixture();
    let past = root(&epoch, func, 2);
    assert_eq!(read(past.word()), State::Blacklisted);
    assert!(claim(past, &policy, Start::Loop).is_none());
}

/// php's `TRACE` lines, each under its `jit_debug` bit, the blank line
/// after a root's lines under any of the four that end them.
#[test]
fn the_dump_writes_phps_trace_lines_under_their_bits() {
    let (all, _) = read_ini(&[("opcache.jit_debug", "258048")]);
    let (none, _) = read_ini(&[]);
    let at = Place {
        scope: Some(b"Foo"),
        function: Some(b"bar"),
        file: b"/t.php",
        line: 4,
    };
    let main = Place {
        scope: None,
        function: None,
        file: b"/t.php",
        line: 2,
    };
    let events = [
        TraceEvent::Start {
            trace: trace(1),
            start: Start::Loop,
            at,
        },
        TraceEvent::Start {
            trace: trace(2),
            start: Start::Enter,
            at: main,
        },
        TraceEvent::Start {
            trace: trace(3),
            start: side(),
            at: main,
        },
        TraceEvent::Stop {
            trace: trace(1),
            stop: Stop::Loop,
        },
        TraceEvent::Stop {
            trace: trace(2),
            stop: Stop::Link(trace(1)),
        },
        TraceEvent::Compiled { trace: trace(1) },
        TraceEvent::Abort {
            trace: trace(3),
            stop: Stop::CompilerError,
        },
        TraceEvent::Blacklisted { trace: trace(3) },
        TraceEvent::Done,
    ];
    let mut written = Vec::new();
    let mut quiet = Vec::new();
    for event in &events {
        all.dump(&mut written, event).ok();
        none.dump(&mut quiet, event).ok();
    }
    assert_eq!(
        String::from_utf8_lossy(&written),
        "---- TRACE 1 start (loop) Foo::bar() /t.php:4\n\
         ---- TRACE 2 start (enter) $main() /t.php:2\n\
         ---- TRACE 3 start (side trace 1/3) $main() /t.php:2\n\
         ---- TRACE 1 stop (loop)\n\
         ---- TRACE 2 stop (link to 1)\n\
         ---- TRACE 1 compiled\n\
         ---- TRACE 3 abort (JIT compilation error)\n\
         ---- TRACE 3 blacklisted\n\
         \n"
    );
    assert!(quiet.is_empty());
}

/// The tier declares its two directives, system ini with their defaults.
#[test]
fn the_tier_declares_jit_mode_and_jit_compile_as_system_ini() {
    let declared = super::ini();
    let names: Vec<_> = declared
        .iter()
        .map(|decl| decl.name.0.as_ref().to_vec())
        .collect();
    assert_eq!(names, [b"jit.mode".to_vec(), b"jit.compile".to_vec()]);
    assert!(
        declared
            .iter()
            .all(|decl| decl.system && !decl.user && !decl.per_dir)
    );
}

/// Under `force` and `exit-fuzz` every eligible root is claimed at its
/// first visit, and one whose `opcache.jit_hot_*` is zero still at none.
#[test]
fn the_lanes_claim_at_the_first_visit() {
    let (force, _) = read_ini(&[("jit.mode", "force")]);
    let (fuzz, _) = read_ini(&[("jit.mode", "exit-fuzz"), ("opcache.jit_hot_return", "0")]);
    for start in [Start::Loop, Start::Enter, Start::Return, side()] {
        assert_eq!(force.threshold(start), Some(1));
    }
    assert_eq!(fuzz.threshold(Start::Loop), Some(1));
    assert_eq!(fuzz.threshold(Start::Return), None);
}

/// An abort keeps its stop in the word, php's `bad_root_cache` slot: read
/// back once the word failed at least as often as asked, and gone once the
/// word compiles; a release leaves the word cold with its attempts and its
/// stop as they were and no heat.
#[test]
fn a_failed_attempt_keeps_its_stop_and_a_release_counts_none() {
    let (policy, _) = read_ini(&[]);
    let (epoch, func) = fixture();
    let holder = root(&epoch, func, 1);
    let word = holder.word();
    for stop in [Stop::InnerLoop, Stop::LoopExit] {
        if let Some(claimed) = claim(holder.clone(), &policy, Start::Loop) {
            assert_eq!(settle(claimed, &Outcome::Abort(stop)), State::Cold);
        }
    }
    assert_eq!(bad_stop(word, 2), Some(Stop::LoopExit));
    assert_eq!(bad_stop(word, 3), None);
    bump(word);
    if let Some(claimed) = claim(holder.clone(), &policy, Start::Loop) {
        assert_eq!(
            bad_stop(word, 2),
            Some(Stop::LoopExit),
            "kept while recording"
        );
        assert_eq!(settle(claimed, &Outcome::Released), State::Cold);
    }
    assert_eq!((attempts(word), heat(word)), (2, 0));
    assert_eq!(bad_stop(word, 2), Some(Stop::LoopExit));
    if let Some(claimed) = claim(holder.clone(), &policy, Start::Loop) {
        assert_eq!(
            settle(claimed, &Outcome::Published(trace(4))),
            State::Compiled(trace(4))
        );
    }
    assert_eq!(bad_stop(word, 0), None);
    assert_eq!(bad_stop(&Word::sealed(), 0), None);
}

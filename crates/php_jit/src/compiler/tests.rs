//! The pipeline at its seam (`jit-design.md` §4.8 *Tested*): over a
//! recording the recording tier took and a claim on the word it grew
//! from, each stage's failure settled as php settles it, a panicking
//! `Backend` among them, the stage's fixture standing for it; a held
//! unit's strings read off the request after its engine dropped; and the
//! worker, submitted to from four threads and drained, every word
//! settled once and no trace published twice, and its queue dropped with
//! the engine.

use std::fmt::Write as _;
use std::sync::Arc;

use php_engine::tier::FuncRef;
use php_unit::MapPtrOffset;

use super::{Job, pipeline, pipeline_with};
use crate::JitEngine;
use crate::lower::Compiled;
use crate::policy::{self, Holder, Outcome, Place, State};
use crate::record::{Recorded, Start, Stop};
use crate::table::{self, TraceId, TraceName};
use crate::testing::{self, records};
use crate::types::{Abort, TraceIr};

/// The ini every fixture records and compiles under: the tracing tier
/// with php's `jit_debug` lines, one chunk of code.
const INI: [(&str, &str); 3] = [
    ("opcache.jit", "tracing"),
    ("opcache.jit_debug", "258048"),
    ("opcache.jit_buffer_size", "64K"),
];

/// The function every stage's fixture records at its entry.
const RETURNS: &str = "function f($a) { return $a * 3 + 1; } echo f(2);";

/// What the recording tier recorded of `program`, `plan` added to the
/// fixture's ini and `more`, each record with the holder of the word it
/// grew from; the engine that served it dropped.
fn recorded(
    plan: &[(&str, &str)],
    more: &[(&str, &str)],
    program: &str,
) -> Vec<(Recorded, Holder)> {
    let ini: Vec<_> = more.iter().chain(plan).chain(&INI).copied().collect();
    let (kept, served) = records::kept(&ini, program);
    assert!(served.fatal.is_none(), "{:?}", served.fatal);
    assert_eq!(kept.taken.len(), kept.roots.len());
    kept.taken.into_iter().zip(kept.roots).collect()
}

/// The one job the recording tier took at `f`'s entry under `more`, its
/// word claimed.
fn job(more: &[(&str, &str)]) -> Result<(Job, Holder), String> {
    let mut taken = recorded(&records::at("f", "enter"), more, RETURNS);
    assert_eq!(taken.len(), 1);
    let (recorded, holder) = taken.remove(0);
    Ok((claimed(recorded, &holder)?, holder))
}

/// `recorded` with a claim on `holder`'s word and the name `f`'s place
/// gives it.
fn claimed(recorded: Recorded, holder: &Holder) -> Result<Job, String> {
    let policy = testing::policy(&INI);
    let claim =
        policy::claim(holder.clone(), &policy, Start::Enter).ok_or("the fixture's word is cold")?;
    let name = TraceName::of(&Place {
        scope: None,
        function: Some(b"f"),
        file: b"body.php",
        line: 2,
    });
    Ok(Job {
        recorded,
        claim,
        name,
    })
}

/// The engine state the pipeline runs under, built over `more`.
fn engine(more: &[(&str, &str)]) -> JitEngine {
    let ini: Vec<_> = more.iter().chain(&INI).copied().collect();
    JitEngine::of(&testing::ini(&ini))
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

/// A body of `len` bytes with no exit, named `f`.
fn body(len: usize) -> Compiled {
    Compiled {
        bytes: vec![0xc3; len],
        entry_offset: 0,
        align: 16,
        exits: Box::new([]),
        map_ptrs: Box::new([]),
        marks: Box::new([]),
        name: TraceName {
            at: b"f".to_vec().into_boxed_slice(),
            line: 2,
        },
    }
}

/// How a stage's fixture compiles.
#[derive(Clone, Copy)]
enum Backend {
    /// The engine's own.
    Host,
    /// Refuses with the stop.
    Refuses(Stop),
    /// A body whose entry is past its bytes, which the table refuses.
    Malformed,
    /// A panic.
    Panics,
}

/// `job` through the pipeline under `engine`, compiled as `backend`
/// says: what it settled as, the state it left its word in, and its
/// lines.
fn piped(
    engine: &JitEngine,
    backend: Backend,
    job: Job,
    holder: &Holder,
) -> (Outcome, State, String) {
    let shared = &engine.shared;
    let mut lines = Vec::new();
    let compile = |_: &TraceIr, _: TraceName| -> Result<Compiled, Abort> {
        match backend {
            Backend::Host => Err(Abort {
                stop: Stop::CompilerError,
                why: "the host's backend is not this closure",
            }),
            Backend::Refuses(stop) => Err(Abort {
                stop,
                why: "a fixture's",
            }),
            Backend::Malformed => Ok(Compiled {
                entry_offset: 4,
                ..body(4)
            }),
            Backend::Panics => std::panic::resume_unwind(Box::new("a backend that panics")),
        }
    };
    let outcome = match backend {
        Backend::Host => pipeline(shared, job, &mut lines),
        _ => pipeline_with(shared, compile, job, &mut lines),
    };
    let state = policy::read(holder.word());
    (outcome, state, String::from_utf8_lossy(&lines).into_owned())
}

/// The lines of trace `n` stopped at its return and ended as `end`.
fn stopped(n: u32, end: &str) -> String {
    format!("---- TRACE {n} stop (return)\n---- TRACE {n} {end}\n")
}

/// A recording compiled by the host's `Backend` is published, its word
/// compiled with the trace's number and the trace in the epoch's table.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_trace_is_built_lowered_and_published() -> Result<(), String> {
    let (job, holder) = job(&[])?;
    let (outcome, state, lines) = piped(&engine(&[]), Backend::Host, job, &holder);
    assert_eq!(outcome, Outcome::Published(trace(1)));
    assert_eq!(state, State::Compiled(trace(1)));
    assert_eq!(lines, stopped(1, "compiled"));
    let table = table::of_epoch(holder.epoch());
    let kept = table.and_then(|table| table.get(trace(1))).is_some();
    assert_eq!(
        kept,
        testing::MAPS_CODE,
        "its code kept where an arena is mapped"
    );
    Ok(())
}

/// Each stage's failure, settled once as php settles it: a failure php
/// recovers from counts an attempt and keeps its stop as the word's bad
/// stop, a bound blacklists, and a panic is `COMPILER_ERROR`; nothing is
/// published.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn each_stages_failure_settles_the_word_as_php_does() -> Result<(), String> {
    let error = Stop::CompilerError;
    let exits = Stop::TooManyExits;
    let cases = [
        (
            "the backend refuses",
            Backend::Refuses(error),
            Outcome::Abort(error),
        ),
        (
            "the backend meets a bound",
            Backend::Refuses(exits),
            Outcome::Blacklist(exits),
        ),
        (
            "the table refuses the body",
            Backend::Malformed,
            Outcome::Abort(error),
        ),
        ("the backend panics", Backend::Panics, Outcome::Abort(error)),
    ];
    for (case, backend, settled) in cases {
        let (job, holder) = job(&[])?;
        let (outcome, state, lines) = piped(&engine(&[]), backend, job, &holder);
        assert_eq!(outcome, settled, "{case}");
        let (word, end) = match settled {
            Outcome::Blacklist(stop) => (
                State::Blacklisted,
                format!("abort ({})\n---- TRACE 1 blacklisted", stop.describe()),
            ),
            _ => (State::Cold, "abort (JIT compilation error)".to_owned()),
        };
        assert_eq!(state, word, "{case}");
        assert_eq!(lines, stopped(1, &end), "{case}");
        if word == State::Cold {
            assert_eq!(policy::bad_stop(holder.word(), 1), Some(error), "{case}");
        }
        let table = table::of_epoch(holder.epoch());
        let none = table.is_some_and(|table| table.get(trace(1)).is_none());
        assert!(none, "{case}");
    }
    Ok(())
}

/// A record that does not fit the op arrays its view holds is
/// `COMPILER_ERROR` at the build, before any `Backend` is asked; a
/// recording that stopped on a failed attempt is settled with its stop,
/// and no `stop` line.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_record_that_builds_nothing_settles_before_the_backend() -> Result<(), String> {
    let (mut misfit, holder) = job(&[])?;
    misfit.recorded.record.root = FuncRef::at(MapPtrOffset::Engine(u32::MAX));
    let (outcome, state, lines) = piped(&engine(&[]), Backend::Panics, misfit, &holder);
    assert_eq!(outcome, Outcome::Abort(Stop::CompilerError));
    assert_eq!(state, State::Cold);
    assert_eq!(lines, stopped(1, "abort (JIT compilation error)"));
    let (mut long, holder) = job(&[])?;
    long.recorded.record.stop = Stop::TooLong;
    let (outcome, state, lines) = piped(&engine(&[]), Backend::Panics, long, &holder);
    assert_eq!(outcome, Outcome::Abort(Stop::TooLong));
    assert_eq!(state, State::Cold);
    assert_eq!(lines, "---- TRACE 1 abort (trace too long)\n");
    Ok(())
}

/// A spent arena's root: `COMPILER_ERROR`, one attempt (#3015's ruling).
fn spent_arena() -> Result<(), String> {
    let (spent, holder) = job(&[])?;
    let table = table::of_epoch(holder.epoch());
    let whole = table.map(|table| table.publish(body(1 << 16), Start::Loop));
    assert_eq!(whole, Some(Ok(trace(1))), "the arena spent");
    let (outcome, state, lines) = piped(&engine(&[]), Backend::Host, spent, &holder);
    assert_eq!(outcome, Outcome::Abort(Stop::CompilerError));
    assert_eq!(state, State::Cold);
    assert_eq!(lines, stopped(2, "abort (JIT compilation error)"));
    Ok(())
}

/// The table's bounds: a spent arena is `COMPILER_ERROR`, one attempt
/// (#3015's ruling); a table at `opcache.jit_max_root_traces` blacklists.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn the_tables_bounds_settle_as_php_settles_them() -> Result<(), String> {
    if testing::MAPS_CODE {
        spent_arena()?;
    }
    let bound = [("opcache.jit_max_root_traces", "1")];
    let (full, holder) = job(&bound)?;
    let (outcome, state, _) = piped(&engine(&bound), Backend::Host, full, &holder);
    assert_eq!(outcome, Outcome::Blacklist(Stop::TooManyTraces));
    assert_eq!(state, State::Blacklisted);
    Ok(())
}

/// A held unit's literals are the engine's interned strings, which the
/// epoch keeps: on a thread of its own, after the engine that served the
/// recording dropped, the view reads the name of the function the trace
/// calls and the pipeline publishes the trace.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_held_units_strings_read_off_the_request_after_its_engine_dropped() -> Result<(), String> {
    let program = "function repeated($s) { return $s . $s; } function f($s) { return repeated($s); } echo f('ab');";
    let mut taken = recorded(&records::at("f", "enter"), &[], program);
    let (recorded, holder) = taken.remove(0);
    let job = claimed(recorded, &holder)?;
    let engine = engine(&[]);
    let off = std::thread::spawn(move || {
        let root = job.recorded.record.root;
        let named = job.recorded.view.op_array(root).is_some_and(|code| {
            (code.literals.iter())
                .any(|literal| literal.as_str().is_some_and(|s| s.bytes() == b"repeated"))
        });
        let outcome = pipeline(&engine.shared, job, &mut Vec::new());
        (named, outcome)
    })
    .join();
    let (named, outcome) = off.map_err(|_| "the thread panicked")?;
    assert!(named, "the literal reads as the engine interned it");
    assert_eq!(outcome, Outcome::Published(trace(1)));
    assert_eq!(policy::read(holder.word()), State::Compiled(trace(1)));
    Ok(())
}

/// The worker: jobs for eight words of one epoch submitted from four
/// threads, then drained; every word settled once, compiled or
/// blacklisted, and each trace published once, under a number of its
/// own.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn four_threads_submit_and_every_word_settles_once() -> Result<(), String> {
    let quiet = [("opcache.jit_debug", "0")];
    let mut program = String::new();
    for k in 0..8 {
        writeln!(
            program,
            "function f{k}($a) {{ return $a * {k} + 1; }} echo f{k}(2);"
        )
        .map_err(|e| e.to_string())?;
    }
    let every = [("record.every", "1"), ("record.function", "-")];
    let taken = recorded(&every, &quiet, &program);
    assert_eq!(taken.len(), 8);
    let engine = engine(&quiet);
    assert!(engine.compiler.worker.is_some(), "a thread compiles");
    let holders: Vec<Holder> = taken.iter().map(|(_, holder)| holder.clone()).collect();
    let mut shares: Vec<Vec<Job>> = (0..4).map(|_| Vec::new()).collect();
    for (n, (recorded, holder)) in taken.into_iter().enumerate() {
        shares[n % 4].push(claimed(recorded, &holder)?);
    }
    std::thread::scope(|scope| {
        for share in shares {
            let engine = &engine;
            scope.spawn(move || {
                for job in share {
                    let queued = engine.compiler.submit(&engine.shared, job, &mut Vec::new());
                    assert!(queued.is_none(), "queued for the worker");
                }
            });
        }
    });
    engine.compiler.drain();
    let table = table::of_epoch(holders[0].epoch());
    let mut ids = Vec::new();
    for holder in &holders {
        match policy::read(holder.word()) {
            State::Compiled(id) => ids.push(id.get()),
            State::Blacklisted => {}
            other => return Err(format!("a word left {other:?}")),
        }
    }
    assert!(!ids.is_empty(), "the worker compiled");
    let kept = ids.iter().filter_map(|id| table?.get(trace(*id))).count();
    assert_eq!(kept, if testing::MAPS_CODE { ids.len() } else { 0 });
    let compiled = ids.len();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), compiled, "no trace published twice");
    Ok(())
}

/// The jobs still queued as the engine drops are dropped with it, each
/// claim settling itself as a failed attempt, `COMPILER_ERROR`.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn the_queue_drops_with_the_engine_and_each_claim_settles() -> Result<(), String> {
    let quiet = [("opcache.jit_debug", "0")];
    let (job, holder) = job(&quiet)?;
    let engine = engine(&quiet);
    let worker = (engine.compiler.worker.as_ref()).ok_or("a thread compiles")?;
    worker.queue.held().jobs.push_back(job);
    assert_eq!(policy::read(holder.word()), State::Recording);
    drop(engine);
    assert_eq!(policy::read(holder.word()), State::Cold);
    assert_eq!(
        policy::bad_stop(holder.word(), 1),
        Some(Stop::CompilerError)
    );
    Ok(())
}

/// No worker where the tier is off, or where a lane or `jit_debug`'s
/// lines ask for the compile on the request; there `submit` settles the
/// job at once and adds its lines.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn inline_settles_on_the_request_and_off_runs_no_worker() -> Result<(), String> {
    assert!(JitEngine::of(&testing::ini(&[])).compiler.worker.is_none());
    let forced = JitEngine::of(&testing::ini(&[("jit.mode", "force")]));
    assert!(forced.compiler.worker.is_none());
    let engine = engine(&[]);
    assert!(
        engine.compiler.worker.is_none(),
        "jit_debug asks for the lines"
    );
    let (job, holder) = job(&[])?;
    let mut lines = Vec::new();
    let outcome = engine.compiler.submit(&engine.shared, job, &mut lines);
    assert_eq!(outcome, Some(Outcome::Published(trace(1))));
    assert_eq!(String::from_utf8_lossy(&lines), stopped(1, "compiled"));
    assert_eq!(policy::read(holder.word()), State::Compiled(trace(1)));
    let shared = Arc::clone(&engine.shared);
    drop(engine);
    assert_eq!(Arc::strong_count(&shared), 1);
    Ok(())
}

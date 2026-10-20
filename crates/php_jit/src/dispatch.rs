//! The one machine over a word (`jit-design.md` §4.9): [`visit`], what a
//! safepoint does with the word it carries, [`hot`], the cold path that
//! records from a word it claimed, and [`safepoint`], what
//! `Jit::safepoint` is.
//!
//! Under `Mode::Off` no word is read or written. Otherwise a visit reads
//! the word: a compiled one is entered, a cold one is bumped and, at its
//! threshold, claimed, unless the request records already
//! ([`record::recording`], revision 30), when it is bumped and left,
//! as php's `zend_jit_trace_hot_root` returns at once while
//! `JIT_G(tracing)`. A claimed word is recorded from at once, the recording handed to the
//! pipeline with its claim, and the interpreter resumed where the
//! recording stopped; a stop at an inner loop records that loop at once,
//! as php's `ZEND_JIT_TRACE_STOP_REPEAT` does.
//!
//! **Where a claim is taken.** At the activation of a frame alone, for
//! now: the loop takes a `Flow::Resume` there and goes on in the frame
//! the tier names, where a back-edge's handler and a return's go on
//! where they stood (`tier::continued`), so a recording that ran the
//! program forward from one of those has nowhere to hand it back. A visit
//! at a loop header or a return bumps and never claims until a handler
//! can take a `Resume` (`jit-design.md` §2.2); php's inner-loop repeat
//! records the loops an entry's recording reaches.

use std::sync::Arc;

use php_engine::tier::Flow;
use php_engine::tier::{self, FrameRef, Request, SafepointIdx, Site, Word};

use crate::compiler::{self, Job};
use crate::policy::{self, Claim, Holder, Mode, Outcome, Place, Policy, State, TraceEvent};
use crate::record::{self, Bailed, Ended, Start, Stop};
use crate::table::{self, TraceId, TraceName};
use crate::{JitEngine, Shared};

#[cfg(test)]
mod tests;

/// What a visit to a word decided.
#[derive(Debug)]
pub(crate) enum Visited {
    /// Nothing happened: the interpreter runs the opline.
    Continue,
    /// The word names a compiled trace to enter.
    Enter(#[cfg_attr(not(test), expect(dead_code, reason = "run enters it (#2959)"))] TraceId),
    /// The word was claimed for a recording, which starts here.
    Hot(Claim),
}

/// The one state machine over a word, for either home: `holder` is the
/// home's, built only on the claim path; `claims` is false where the
/// request records already or the site cannot take a recording,
/// when a cold word is bumped and left. A sealed word reads blacklisted
/// and takes the last arm.
#[inline(always)] // jit-design.md §4.9: one load and a match on every visit
#[allow(clippy::inline_always)]
pub(crate) fn visit(
    word: &Word,
    policy: &Policy,
    start: Start,
    claims: bool,
    holder: impl FnOnce() -> Option<Holder>,
) -> Visited {
    match policy::read(word) {
        State::Compiled(id) => Visited::Enter(id),
        State::Cold => {
            let Some(threshold) = policy.threshold(start) else {
                return Visited::Continue;
            };
            if policy::bump(word) < threshold || !claims {
                return Visited::Continue;
            }
            holder()
                .and_then(|holder| policy::claim(holder, policy, start))
                .map_or(Visited::Continue, Visited::Hot)
        }
        State::Recording | State::Blacklisted => Visited::Continue,
    }
}

/// `Jit::safepoint`: under `Off`, `Continue` before any word is read;
/// otherwise the site's word visited. Here, and only here, `Continue`
/// means nothing happened.
#[inline(always)] // jit-design.md §4: the mode test inlined into the loop's three sites
#[allow(clippy::inline_always)]
pub(crate) fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
    let Some(engine) = req.engine::<JitEngine>() else {
        return Flow::Continue;
    };
    if engine.shared.policy.mode() == Mode::Off {
        return Flow::Continue;
    }
    visited(req, frame, site)
}

/// The site's word visited, and what came of it.
#[inline(never)]
fn visited(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
    let at = site.idx();
    let start = match site {
        Site::Loop(_) => Start::Loop,
        Site::Entry => Start::Enter,
        Site::Return(_) => Start::Return,
    };
    let claims = site == Site::Entry && !record::recording(req);
    let (decided, shared) = {
        let req: &Request = req;
        let Some(engine) = req.engine::<JitEngine>() else {
            return Flow::Continue;
        };
        // A table a hot root found full stopped its counters, as php's
        // `zend_jit_stop_counter_handlers` does: no word is claimed more.
        let holder = || {
            let table = table::of(req, &engine.shared.policy, &engine.shared.perf)?;
            if table.stopped() {
                return None;
            }
            Some(Holder::Root {
                epoch: tier::epoch(req),
                func: tier::func(req, frame)?,
                at,
            })
        };
        let word = tier::word(req, frame, at);
        let decided = visit(word, &engine.shared.policy, start, claims, holder);
        (decided, Arc::clone(&engine.shared))
    };
    match decided {
        // What enters a published trace is `run` (#2959); until then the
        // interpreter runs the opline, as for a word not compiled (§3.2).
        Visited::Enter(_) | Visited::Continue => Flow::Continue,
        Visited::Hot(claim) => hot(req, frame, claim, at, &shared),
    }
}

/// The cold path (`jit-design.md` §4.9): the `TRACE start` line, the
/// recording, and the pipeline with its claim, then, for a stop at an
/// inner loop, the same at that loop's header, as php's
/// `zend_jit_trace_hot_root` repeats; the interpreter resumes wherever
/// the last recording stopped. Recording is exclusive per request:
/// `record::run` holds the request's exclusion, php's `JIT_G(tracing)`.
///
/// A destructor that runs a hot loop inside a recording: the program's
/// own code is the root, and the destructor's loop, served by a nested
/// `execute` while it records, is bumped and not recorded, so one
/// `TRACE start` is written; php's `zend_jit_trace_hot_root` returns at
/// once while `JIT_G(tracing)`.
///
/// ```php
/// class D { function __destruct() { $s = 0; for ($i = 0; $i < 3; $i++) { $s += $i; } echo "d $s\n"; } }
/// function f() { $d = new D; $d = null; return 1; } echo f(), "\n";
/// ```
/// ```expect regime=jit-inline process
/// d 3
/// 1
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:3
/// ---- TRACE 1 stop (return)
/// ---- TRACE 1 compiled
///
/// ```
#[inline(never)]
pub(crate) fn hot(
    req: &mut Request,
    mut frame: FrameRef,
    mut claim: Claim,
    mut at: SafepointIdx,
    shared: &Shared,
) -> Flow {
    let mut stepped = false;
    let flow = loop {
        let start = claim.start();
        let epoch = table::holding(req, &shared.policy, &shared.perf);
        let table = table::of_epoch(&epoch);
        let name = started(
            req,
            shared,
            frame,
            table.and_then(table::TraceTable::next),
            start,
        );
        if table.is_none_or(table::TraceTable::full) {
            if let Some(table) = table {
                table.stop();
            }
            let recorded = record::refused(req, frame, start, Stop::TooManyTraces);
            settled(
                req,
                shared,
                Job {
                    recorded,
                    claim,
                    name,
                },
            );
            break resumed(frame, stepped);
        }
        let (recorded, ended) = match record::run(req, frame, start, at, &shared.policy) {
            Ok(done) => done,
            // php's bailout leaves the recorder with nothing written after
            // the start line and nothing counted against the root.
            Err(bailed) => {
                policy::settle(claim, &Outcome::Released);
                return match bailed {
                    Bailed::Fatal(fatal) => Flow::Bail(fatal),
                    Bailed::Unwinding(at) => Flow::Resume(at),
                };
            }
        };
        stepped |= ended.stepped;
        let Some((rooted, name)) = rerooted(req, shared, claim, name, &ended) else {
            break resumed(ended.at, stepped);
        };
        let job = Job {
            recorded,
            claim: rooted,
            name,
        };
        let outcome = settled(req, shared, job);
        let repeat = match (outcome, ended.inner) {
            (Some(Outcome::Abort(Stop::InnerLoop)), Some((func, header))) => policy::claim(
                Holder::Root {
                    epoch: tier::epoch(req),
                    func,
                    at: header,
                },
                &shared.policy,
                Start::Loop,
            ),
            _ => None,
        };
        let Some(inner) = repeat else {
            break resumed(ended.at, stepped);
        };
        frame = ended.at;
        claim = inner;
        at = match ended.inner {
            Some((_, header)) => header,
            None => at,
        };
    };
    let mut lines = Vec::new();
    shared.dump(&mut lines, &TraceEvent::Done);
    req.write_stderr(&lines);
    flow
}

/// `job` settled: a recording that stopped where a trace compiles
/// submitted to the engine's compiler, which settles it here or on its
/// worker; any other settled here at once as the attempt it failed, as
/// php writes its abort before it records again. Its lines written;
/// what it settled as, where it was settled here.
fn settled(req: &mut Request, shared: &Shared, job: Job) -> Option<Outcome> {
    let mut lines = Vec::new();
    let outcome = if job.recorded.record.stop.compiles() {
        let engine = req.engine::<JitEngine>();
        engine.and_then(|engine| engine.compiler.submit(shared, job, &mut lines))
    } else {
        Some(compiler::pipeline(shared, job, &mut lines))
    };
    req.write_stderr(&lines);
    outcome
}

/// The claim the recorded trace settles and the name it is published
/// under: the one it was recorded under, or, where the recorder made an
/// unrolled loop the trace, a claim of that loop's header, the first
/// released, after a second `TRACE start` line names it, as php writes
/// one when its trace's first opline moved. None where the header's word
/// could not be claimed.
fn rerooted(
    req: &mut Request,
    shared: &Shared,
    claim: Claim,
    name: TraceName,
    ended: &Ended,
) -> Option<(Claim, TraceName)> {
    let Some((func, header)) = ended.rerooted else {
        return Some((claim, name));
    };
    policy::settle(claim, &Outcome::Released);
    let root = Holder::Root {
        epoch: tier::epoch(req),
        func,
        at: header,
    };
    let claimed = policy::claim(root, &shared.policy, Start::Loop)?;
    let next = table::of(req, &shared.policy, &shared.perf).and_then(table::TraceTable::next);
    let name = started(req, shared, ended.at, next, Start::Loop);
    Some((claimed, name))
}

/// The flow back to the loop once recording stopped: a resume where it
/// stopped, or nothing where nothing was stepped.
fn resumed(at: FrameRef, stepped: bool) -> Flow {
    if stepped {
        Flow::Resume(at)
    } else {
        Flow::Continue
    }
}

/// php's `TRACE n start` line for a root at `frame`, where the table
/// numbers a next trace, which stands where the recording starts: the
/// class and the function the frame serves, `$main` for a file's code,
/// the file, and the line of the opline php's root's heat stands on, the
/// first `RECV` of a function with a typed parameter, whose prologue php
/// counts. The name a trace that starts there is published under, the
/// same place.
fn started(
    req: &mut Request,
    shared: &Shared,
    frame: FrameRef,
    trace: Option<TraceId>,
    start: Start,
) -> TraceName {
    let mut lines = Vec::new();
    let name = placed(req, frame, start, |at| {
        if let Some(trace) = trace {
            shared.dump(&mut lines, &TraceEvent::Start { trace, start, at });
        }
        TraceName::of(&at)
    });
    req.write_stderr(&lines);
    name.unwrap_or_else(|| {
        TraceName::of(&Place {
            scope: None,
            function: None,
            file: b"",
            line: 0,
        })
    })
}

/// What `with` makes of the [`Place`] a root at `frame` starts at; none
/// where the frame shows no op array.
fn placed<T>(
    req: &Request,
    frame: FrameRef,
    start: Start,
    with: impl FnOnce(Place<'_>) -> T,
) -> Option<T> {
    let view = tier::frame(req, frame)?;
    let function = view.function();
    let decl = function.and_then(|function| req.function_decl(function));
    let name = decl.map(|decl| decl.name.0.to_vec());
    let scope = function
        .and(view.function_scope())
        .map(|class| req.class_name(class).into_bytes());
    let typed = start == Start::Enter
        && decl.is_some_and(|decl| decl.parameters.iter().any(|p| p.ty.is_some()));
    let line = if typed {
        view.op_array()
            .and_then(|code| code.opline(php_unit::OplineIdx(0)))
            .map_or(view.line(), |op| op.line())
    } else {
        view.line()
    };
    Some(with(Place {
        scope: scope.as_deref(),
        function: name.as_deref(),
        file: view.filename(),
        line,
    }))
}

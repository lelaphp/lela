//! The one machine over a word (`jit-design.md` §4.9): [`visit`], what a
//! safepoint does with the word it carries, [`hot`], the cold path that
//! records from a word it claimed, [`run`], which enters a compiled
//! word's trace and follows it from trace to trace, and [`safepoint`],
//! what `Jit::safepoint` is.
//!
//! Under `Mode::Off` no word is read or written. Otherwise a visit reads
//! the word: a compiled one is entered ([`run`]), a cold one is bumped and, at its
//! threshold, claimed, unless the request records already
//! ([`record::recording`], revision 30), when it is bumped and left,
//! as php's `zend_jit_trace_hot_root` returns at once while
//! `JIT_G(tracing)`. A claimed word is recorded from at once, the recording handed to the
//! pipeline with its claim, and the interpreter resumed where the
//! recording stopped; a stop at an inner loop records that loop at once,
//! as php's `ZEND_JIT_TRACE_STOP_REPEAT` does.
//!
//! **Where a claim is taken.** At a frame's activation and at a loop
//! header: each site takes a `Flow::Resume` and goes on in the frame the
//! tier names, a back-edge's handler after the recording ran past the
//! jump (`tier::resumed`), so a loop in `$main` that no call activates
//! is a root of its own, as php's `opcache.jit_hot_loop` makes it. A
//! return's handler takes a `Resume` too, but a visit there bumps and
//! does not claim: php roots a return at the caller's landing past a
//! call, armed only once a trace returned into it
//! (`zend_jit_trace_setup_ret_counter`), where this tier's return word
//! is the callee's `RETURN` and every one is armed (#3036).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use php_engine::tier::Flow;
use php_engine::tier::{self, Fatal, FrameRef, Request, SafepointIdx, Site, Word};

use crate::compiler::{self, Job};
use crate::enter::{self, Crossed as _, Exited, Left};
use crate::policy::{
    self, Claim, Holder, JitDebug, Mode, Outcome, Place, Policy, State, TraceEvent,
};
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
    Enter(TraceId),
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

/// `Jit::safepoint`, the site's word visited and what came of it,
/// outlined, so the loop's site is one call. An engine whose tier runs
/// `Off` binds `Nop`'s loop and never asks (`Jit::serves`); asked under
/// `Off` all the same, it answers `Continue` before any word is read.
/// Here, and only here, `Continue` means nothing happened. The engine
/// state is looked up once, and its `Arc` is shared on only past a visit
/// that entered or claimed, so the visit that bumps and goes on writes
/// no count of the `Arc` the engine's threads share.
#[inline(never)]
pub(crate) fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
    let at = site.idx();
    let start = match site {
        Site::Loop(_) => Start::Loop,
        Site::Entry => Start::Enter,
        Site::Return(_) => Start::Return,
    };
    let claims = start != Start::Return && !record::recording(req);
    let (decided, shared) = {
        let req: &Request = req;
        let Some(engine) = req.engine::<JitEngine>() else {
            return Flow::Continue;
        };
        let shared = &engine.shared;
        if shared.policy.mode() == Mode::Off {
            return Flow::Continue;
        }
        // A table a hot root found full stopped its counters, as php's
        // `zend_jit_stop_counter_handlers` does: no word is claimed more.
        let holder = || {
            let table = table::of(req, &shared.policy, &shared.perf)?;
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
        match visit(word, &shared.policy, start, claims, holder) {
            Visited::Continue => return Flow::Continue,
            decided => (decided, Arc::clone(shared)),
        }
    };
    match decided {
        Visited::Continue => Flow::Continue,
        Visited::Enter(id) => run(req, frame, id, &shared),
        Visited::Hot(claim) => hot(req, frame, claim, at, &shared),
    }
}

/// Trace to trace (`jit-design.md` §4.9): `id` entered on `frame`, then,
/// for as long as the exit it left through names a trace published in
/// the epoch's table, that trace entered on the frame the exit landed:
/// the exit's link, a build-time fact followed without reading a word, or
/// the side trace its word names. Each trace is borrowed from the table
/// of the epoch the request holds, whose slots are written once, so
/// entering one takes no hold of its own.
///
/// The three meanings of "not compiled" have three answers. At the root,
/// a word that reads compiled whose slot is empty, the window §3.2 names
/// or a table that keeps no code (#2943), entered nothing: `Continue`.
/// After an exit a trace has done its work, so a link or a side whose
/// slot is empty, and an exit word that reads cold, recording or
/// blacklisted, are a `Resume` of the landed frame; so is a throwable a
/// kernel left pending, which the loop unwinds from where it was raised.
/// A cold exit word is bumped and claimed by nothing: what records a side
/// trace from a hot exit is #2964's, so until then no exit word reads
/// compiled.
///
/// A compiled loop leaves through its interrupt test once the engine's
/// timer raises the interrupt, and the `Resume` hands the loop its
/// interrupt check (§2.2), which raises php's fatal at the loop's line:
///
/// ```php
/// set_time_limit(1); for (;;) {}
/// ```
/// ```expect exit=255 regime=jit-force process wall=1s
/// Fatal error: Maximum execution time of 1 second exceeded in %s on line 2
/// ```
///
/// A signal `pcntl_async_signals` delivers while a compiled loop spins is
/// handled at the loop's check after the interrupt exit, and the loop
/// goes on to see what the handler wrote:
///
/// ```php
/// pcntl_async_signals(true); $done = false;
/// pcntl_signal(SIGALRM, function () use (&$done) { $done = true; echo "alarm\n"; });
/// pcntl_alarm(1); $n = 0; while (!$done) { $n++; } echo $n > 1000 ? "spun\n" : "short\n";
/// ```
/// ```expect exit=0 regime=jit-force process wall=1s
/// alarm
/// spun
/// ```
#[inline(never)]
pub(crate) fn run(req: &mut Request, frame: FrameRef, id: TraceId, shared: &Shared) -> Flow {
    let epoch = table::holding(req, &shared.policy, &shared.perf);
    let Some(table) = table::of_epoch(&epoch) else {
        return Flow::Continue;
    };
    let Some(mut trace) = table.get(id) else {
        return Flow::Continue;
    };
    let mut frame = frame;
    loop {
        let exited = match enter::enter(req, frame, trace) {
            Ok(Left::Exit(exited)) => exited,
            Ok(Left::Unwound(frame)) => return Flow::Resume(frame),
            Err(fatal) => return Flow::Bail(fatal),
        };
        frame = exited.frame;
        let Some(exit) = trace.exits.get(usize::from(exited.exit.0)) else {
            return Flow::Bail(Fatal::error(
                "InternalError: a trace landed an exit it does not have",
            ));
        };
        if req.unwinding() {
            return Flow::Resume(frame);
        }
        let next = match exit.link {
            Some(link) => Some(link),
            // A trace's end is no side exit: php returns to the interpreter.
            None if trace.leaves == Some(exited.exit) => None,
            None => sided(req, shared, trace.id, exited, &exit.word),
        };
        match next.and_then(|next| table.get(next)) {
            Some(next) => trace = next,
            None => return Flow::Resume(frame),
        }
    }
}

/// What an exit with no link goes on to (`jit-design.md` §4.9): the side
/// trace its word names, entered with no line written, as php's exit is
/// patched to jump to it; otherwise none, a cold word bumped toward
/// `opcache.jit_hot_side_exit` and claimed by nothing, recording a side
/// trace being #2964's, and php's
/// `TRACE n exit m` line written for the place it landed, as
/// `zend_jit_trace_exit` writes it unless the interrupt is raised or the
/// request records (`Z/ext/opcache/jit/zend_jit_trace.c:8806-8824`).
fn sided(
    req: &mut Request,
    shared: &Shared,
    trace: TraceId,
    exited: Exited,
    word: &Word,
) -> Option<TraceId> {
    match policy::read(word) {
        State::Compiled(side) => return Some(side),
        State::Cold => {
            policy::bump(word);
        }
        State::Recording | State::Blacklisted => {}
    }
    let quiet = req.limits().interrupt().raised().load(Ordering::Relaxed) || record::recording(req);
    if !quiet && shared.policy.debug().any(JitDebug::TRACE_EXIT) {
        let mut lines = Vec::new();
        placed(req, exited.frame, false, |at| {
            let event = TraceEvent::Exit {
                trace,
                exit: exited.exit,
                at,
            };
            shared.dump(&mut lines, &event);
        });
        req.response_mut().write_stderr(&lines);
    }
    None
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
        let next = table.and_then(table::TraceTable::next);
        // A frame that shows no op array has no place to record from.
        let Some(name) = started(req, shared, frame, next, start) else {
            policy::settle(claim, &Outcome::Released);
            break resumed(frame, stepped);
        };
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
    req.response_mut().write_stderr(&lines);
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
    req.response_mut().write_stderr(&lines);
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
    let Some(name) = started(req, shared, ended.at, next, Start::Loop) else {
        policy::settle(claimed, &Outcome::Released);
        return None;
    };
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
/// same place; none where the frame shows no op array.
fn started(
    req: &mut Request,
    shared: &Shared,
    frame: FrameRef,
    trace: Option<TraceId>,
    start: Start,
) -> Option<TraceName> {
    let mut lines = Vec::new();
    let name = placed(req, frame, start == Start::Enter, |at| {
        if let Some(trace) = trace {
            shared.dump(&mut lines, &TraceEvent::Start { trace, start, at });
        }
        TraceName::of(&at)
    });
    req.response_mut().write_stderr(&lines);
    name
}

/// What `with` makes of the [`Place`] `frame` stands at, at its opline's
/// line or, for an `entry` php counts at a typed prologue, its first
/// `RECV`'s; none where the frame shows no op array.
fn placed<T>(
    req: &Request,
    frame: FrameRef,
    entry: bool,
    with: impl FnOnce(Place<'_>) -> T,
) -> Option<T> {
    let view = tier::frame(req, frame)?;
    let function = view.function();
    let decl = function.and_then(|function| req.symbols().function_decl(function));
    let name = decl.map(|decl| decl.name.0.to_vec());
    let scope = function
        .and(view.function_scope())
        .map(|class| req.symbols().class_name(class).into_bytes());
    let typed = entry && decl.is_some_and(|decl| decl.parameters.iter().any(|p| p.ty.is_some()));
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

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
//! **Where a claim is taken.** At a frame's activation, at a loop
//! header, and at a return's landing: each site takes a `Flow::Resume`
//! and goes on in the frame the tier names, a back-edge's handler after
//! the recording ran past the jump (`tier::resumed`), so a loop in
//! `$main` that no call activates is a root of its own, as php's
//! `opcache.jit_hot_loop` makes it. A return root is php's, the caller's
//! landing past the call (`jit-design.md` §2.2, #3036): a return's site
//! visits the word of the opline its caller lands on, which a compiled
//! trace arms where it hands a caller with no trace of its own on to the
//! interpreter ([`record::landings`], php's
//! `zend_jit_trace_setup_ret_counter`), and the callee's own `RETURN`
//! word is neither read nor claimed. A trace that leaves for the
//! interpreter at an armed landing visits it too, as php's interpreter
//! runs the landing's handler ([`visit_landing`]).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use php_engine::tier::Flow;
use php_engine::tier::{self, Epoch, Fatal, FrameRef, Request, SafepointIdx, Site, Stepped, Word};
use php_unit::OplineIdx;

use crate::compiler::{self, Job};
use crate::enter::{self, At, Crossed as _, Exited, Left};
use crate::policy::{
    self, Claim, Holder, JitDebug, Mode, Outcome, Place, Policy, State, TraceEvent,
};
use crate::record::{self, Bailed, Ended, Headed, Start, Stop};
use crate::table::{self, TraceId, TraceName};
use crate::types::Grows;
use crate::{JitEngine, JitRequest, Shared};

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
/// `Off` binds `Nop`'s loop and never asks (`Jit::asked`); asked under
/// `Off` all the same, it answers `Continue` before any word is read.
/// Here, and only here, `Continue` means nothing happened. The engine
/// state is looked up once, and its `Arc` is shared on only past a visit
/// that entered or claimed, so the visit that bumps and goes on writes
/// no count of the `Arc` the engine's threads share. A return's word is
/// its caller's landing ([`Site`]'s third arm visits no word of the
/// callee's), which reads sealed, a word no visit acts on, until a
/// compiled trace armed it.
///
/// A loop in `$main` that no call activates is a root at its header:
/// the entry's recording aborts at the throwable `t()` throws, and the
/// loop's back edge claims its word:
///
/// ```php
/// function t() { throw new Exception("x"); }
/// try { t(); } catch (Exception $e) {}
/// $s = 0;
/// for ($i = 0; $i < 3; $i++) { $s += $i; }
/// echo $s, "\n";
/// ```
/// ```expect regime=jit-inline process
/// 3
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:3
/// ---- TRACE 1 abort (exception)
///
/// ---- TRACE 1 start (loop) $main() %s:5
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 1 $main() %s:6
/// ---- TRACE 2 start (side trace 1/1) $main() %s:6
/// ---- TRACE 2 stop (return)
/// ---- TRACE 2 compiled
///
/// ```
#[inline(never)]
pub(crate) fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
    let start = match site {
        Site::Loop(_) => Start::Loop,
        Site::Entry => Start::Enter,
        Site::Return(_) => Start::Return,
    };
    let claims = !quiet(req, Some(site));
    let (decided, shared) = {
        let req: &Request = req;
        let Some(engine) = req.engine::<JitEngine>() else {
            return Flow::Continue;
        };
        let shared = &engine.shared;
        if shared.policy.mode() == Mode::Off {
            return Flow::Continue;
        }
        let rooted = match site {
            Site::Loop(at) => Rooted::Safepoint(at),
            Site::Entry => Rooted::Safepoint(SafepointIdx::ENTRY),
            Site::Return(_) => match landing_past(req, frame) {
                Some((caller, at)) => Rooted::Landing(caller, at),
                None => return Flow::Continue,
            },
        };
        // A table a hot root found full stopped its counters, as php's
        // `zend_jit_stop_counter_handlers` does: no word is claimed more.
        let holder = || {
            let table = table::of(req, &shared.policy, &shared.perf)?;
            if table.stopped() {
                return None;
            }
            let epoch = tier::epoch(req);
            Some(match rooted {
                Rooted::Safepoint(at) => Holder::Root {
                    epoch,
                    func: tier::func(req, frame)?,
                    at,
                },
                Rooted::Landing(caller, at) => Holder::Landing {
                    epoch,
                    func: tier::func(req, caller)?,
                    at,
                },
            })
        };
        let word = match rooted {
            Rooted::Safepoint(at) => tier::word(req, frame, at),
            Rooted::Landing(caller, at) => tier::landing(req, caller, at),
        };
        match visit(word, &shared.policy, start, claims, holder) {
            Visited::Continue => return Flow::Continue,
            decided => (decided, Arc::clone(shared)),
        }
    };
    match (site, decided) {
        (_, Visited::Continue) => Flow::Continue,
        (Site::Return(at), decided) => returned(req, frame, at, decided, &shared),
        (Site::Loop(header), Visited::Enter(id)) => looped(req, frame, header, id, &shared),
        (Site::Entry, Visited::Enter(id)) => run(req, frame, id, &shared),
        (Site::Loop(_) | Site::Entry, Visited::Hot(claim)) => hot(req, frame, claim, &shared),
    }
}

/// Where a site's word stands: a safepoint of the frame's op array, or,
/// for a return, the opline past the call its caller stands at, in the
/// caller's.
#[derive(Debug, Clone, Copy)]
enum Rooted {
    Safepoint(SafepointIdx),
    Landing(FrameRef, OplineIdx),
}

/// The caller `frame` returns to and the opline it lands on there, the
/// one past the call it stands at; none where no frame of this serving
/// called it.
fn landing_past(req: &Request, frame: FrameRef) -> Option<(FrameRef, OplineIdx)> {
    let caller = tier::below(req, frame)?;
    let call = tier::frame(req, caller)?.opline();
    Some((caller, OplineIdx(call.0.checked_add(1)?)))
}

/// A return whose landing's word was compiled or claimed: php's
/// interpreter has returned before the landing's handler runs, so the
/// `RETURN` the safepoint `at` numbers is stepped first, its frame stood
/// there, which a `RETURN`'s inline path has not stood it at, then the
/// return trace is entered, or recorded from, at the landing in the
/// caller it popped to; a throwable the return left pending, a
/// destructor's, resumes where it was raised, the claim released.
///
/// The entry's trace stops at the `eval`, which it hands on to the
/// interpreter, arming `$main`'s landing past it as php's
/// `zend_jit_trace_setup_ret_counter` does; the `eval`'d code's return
/// lands there and roots php's return trace, whose recording reaches the
/// loop and records it next:
///
/// ```php
/// eval("\$x = 1;");
/// $s = 0;
/// for ($i = 0; $i < 3; $i++) { $s += $i; }
/// echo $s, "\n";
/// ```
/// ```expect regime=jit-inline process
/// 3
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:2
/// ---- TRACE 1 stop (exit to VM interpreter)
/// ---- TRACE 1 compiled
///
/// ---- TRACE 2 start (return) $main() %s:3
/// ---- TRACE 2 abort (inner loop)
/// ---- TRACE 2 start (loop) $main() %s:4
/// ---- TRACE 2 stop (loop)
/// ---- TRACE 2 compiled
///
///      TRACE 2 exit 1 $main() %s:5
/// ---- TRACE 3 start (side trace 2/1) $main() %s:5
/// ---- TRACE 3 stop (return)
/// ---- TRACE 3 compiled
///
/// ```
fn returned(
    req: &mut Request,
    frame: FrameRef,
    at: SafepointIdx,
    decided: Visited,
    shared: &Shared,
) -> Flow {
    let stepped = match tier::safepoint_at(req, frame, at) {
        Some(stands) => {
            tier::stand(req, frame, stands);
            tier::step(req, frame)
        }
        None => Err(Fatal::error(
            "InternalError: a return site whose number names no RETURN",
        )),
    };
    let caller = match stepped {
        Ok(Stepped::Popped(caller)) => caller,
        // Nothing was recorded: a claim is given back with nothing counted
        // against the landing, as a recording's bailout gives one back.
        gone => {
            if let Visited::Hot(claim) = decided {
                policy::settle(claim, &Outcome::Released);
            }
            return match gone {
                Ok(Stepped::Threw(at)) => Flow::Resume(at),
                Err(fatal) => Flow::Bail(fatal),
                Ok(_) => Flow::Bail(Fatal::error(
                    "InternalError: a return the tier stepped popped to no caller",
                )),
            };
        }
    };
    let flow = match decided {
        Visited::Continue => Flow::Continue,
        Visited::Enter(id) => run(req, caller, id, shared),
        Visited::Hot(claim) => hot(req, caller, claim, shared),
    };
    match flow {
        Flow::Continue => Flow::Resume(caller),
        flow => flow,
    }
}

/// A loop's trace `id` entered from its back edge, which left `frame`
/// where it lands: at that opline, the body of a `for` or a `while`,
/// where the trace keeps a way in there (`types::BodyEntry`) and the
/// frame holds what the trace knows there; otherwise on at the opline
/// php's trace roots at, the condition past the body
/// ([`record::headed`]), where php's handler is the trace. A body that
/// went another way on the way there resumes where it went, and one that
/// got there and entered nothing resumes there.
fn looped(
    req: &mut Request,
    frame: FrameRef,
    header: SafepointIdx,
    id: TraceId,
    shared: &Shared,
) -> Flow {
    if let Some(flow) = run_at(req, frame, id, shared, At::Body) {
        return flow;
    }
    match record::headed(req, frame, header) {
        Ok(Headed::Stands(at)) => run(req, at, id, shared),
        Ok(Headed::Stepped(at)) => match run(req, at, id, shared) {
            Flow::Continue => Flow::Resume(at),
            flow => flow,
        },
        Ok(Headed::Away(at)) => Flow::Resume(at),
        Ok(Headed::Unstepped) => Flow::Continue,
        Err(fatal) => Flow::Bail(fatal),
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
/// A cold exit word is bumped, and claimed at `opcache.jit_hot_side_exit`
/// for a side trace recorded from the landing ([`sided`]), which `run`
/// goes on in no further: the recording ran the program on.
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
/// Where the test takes php's interrupt stub, a loop that serves an
/// opline through the interpreter's handler and keeps nothing in
/// registers (`types::Interrupt::Stub`), `run` makes that check itself
/// and enters the trace again at its header, as php's stub goes on at the
/// header's handler, which is the trace; the timeout's fatal is raised
/// there:
///
/// ```php
/// set_time_limit(1); $i = 0; for (;;) { $s = (string)$i; }
/// ```
/// ```expect exit=255 regime=jit-force process wall=1s
/// Fatal error: Maximum execution time of 1 second exceeded in %s on line 2
/// ```
///
/// A signal `pcntl_async_signals` delivers while a compiled loop spins is
/// handled at the stub, and the loop, entered again, sees what the
/// handler wrote:
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
///
/// php's lines for it: the guard on `$done` the handler's write fails
/// leaves the trace entered again through its first exit, which grows a
/// side trace:
///
/// ```php
/// pcntl_async_signals(true); $done = false;
/// pcntl_signal(SIGALRM, function () use (&$done) { $done = true; echo "alarm\n"; });
/// pcntl_alarm(1); $n = 0; while (!$done) { $n++; } echo $n > 1000 ? "spun\n" : "short\n";
/// ```
/// ```expect exit=0 regime=jit-inline process wall=1s
/// alarm
/// spun
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:2
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) $main() %s:4
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 compiled
///
/// ---- TRACE 2 start (enter) {closure:%s:3}() %s:3
/// ---- TRACE 2 stop (return)
/// ---- TRACE 2 compiled
///
///      TRACE 1 exit 0 $main() %s:4
/// ---- TRACE 3 start (side trace 1/0) $main() %s:4
/// ---- TRACE 3 stop (return)
/// ---- TRACE 3 compiled
/// ```
#[inline(never)]
pub(crate) fn run(req: &mut Request, frame: FrameRef, id: TraceId, shared: &Shared) -> Flow {
    run_at(req, frame, id, shared, At::First).unwrap_or(Flow::Continue)
}

/// [`run`], its first trace entered `at` its first op or its loop's
/// body, and each after it at its first op; none where the first, asked
/// at its body, keeps no way in where the frame stands or declined the
/// entry, with nothing done.
fn run_at(
    req: &mut Request,
    frame: FrameRef,
    id: TraceId,
    shared: &Shared,
    at: At,
) -> Option<Flow> {
    let epoch = table::holding(req, &shared.policy, &shared.perf);
    let Some(table) = table::of_epoch(&epoch) else {
        return Some(Flow::Continue);
    };
    let Some(trace) = table.get(id) else {
        return Some(Flow::Continue);
    };
    let stands = tier::frame(req, frame).map(|view| view.opline());
    if at == At::Body && (trace.body.is_none() || trace.body != stands) {
        return None;
    }
    let entered = enter::enter(req, frame, (&epoch, trace), at);
    if matches!(entered, Ok(Left::Declined)) {
        return None;
    }
    Some(followed(req, shared, (&epoch, table), trace, entered))
}

/// What `trace` left through, `entered`, followed from trace to trace
/// as [`run`] says.
fn followed<'t>(
    req: &mut Request,
    shared: &Shared,
    (epoch, table): (&'t Epoch, &'t table::Table),
    mut trace: &'t table::Trace,
    mut entered: Result<Left, Fatal>,
) -> Flow {
    loop {
        let exited = match entered {
            Ok(Left::Exit(exited)) => exited,
            Ok(Left::Unwound(frame)) => return Flow::Resume(frame),
            // php's interrupt stub: the interrupt handled, and the trace
            // entered again at its header, its handler.
            Ok(Left::Interrupted(header)) => match tier::interrupted(req) {
                Ok(false) => {
                    entered = enter::enter(req, header, (epoch, trace), At::First);
                    continue;
                }
                Ok(true) => return Flow::Resume(header),
                Err(fatal) => return Flow::Bail(fatal),
            },
            Ok(Left::Declined) => {
                return Flow::Bail(Fatal::error(
                    "InternalError: a trace declined an entry at its first op",
                ));
            }
            Err(fatal) => return Flow::Bail(fatal),
        };
        let frame = exited.frame;
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
            None => match sided(req, shared, trace.id, exited, exit) {
                Visited::Enter(side) => Some(side),
                Visited::Continue => None,
                // A trace ran, so a recording that stepped nothing
                // resumes the landed frame.
                Visited::Hot(claim) => {
                    return match hot(req, frame, claim, shared) {
                        Flow::Continue => Flow::Resume(frame),
                        flow => flow,
                    };
                }
            },
        };
        let next = match next.and_then(|next| table.get(next)) {
            Some(next) => next,
            // Left for the interpreter, which runs the opline's handler:
            // where a return lands, the landing's.
            None => match visit_landing(req, shared, frame, Some(trace.id)) {
                Visited::Enter(id) => match table.get(id) {
                    Some(next) => next,
                    None => return Flow::Resume(frame),
                },
                Visited::Continue => return Flow::Resume(frame),
                Visited::Hot(claim) => {
                    return match hot(req, frame, claim, shared) {
                        Flow::Continue => Flow::Resume(frame),
                        flow => flow,
                    };
                }
            },
        };
        trace = next;
        entered = enter::enter(req, frame, (epoch, trace), At::First);
    }
}

/// The landing a trace or a recording left `frame` for the interpreter
/// at, visited as php's interpreter runs the handler of the opline it
/// goes on at (`zend_jit_trace_exit_stub`, `zend_jit_trace_return`): a
/// return trace compiled there is entered, and an armed landing's word
/// bumped and claimed at `opcache.jit_hot_return` (`jit-design.md` §2.2,
/// #3036). Nothing while the interrupt is raised or the request records,
/// nor where the landing is the root of `left`, the trace that left the
/// frame there, as php's `zend_jit_trace_exit` answers 1 at its own
/// trace's first opline and the opline's own handler runs, not the trace
/// again; a frame at no armed landing is left as it is.
fn visit_landing(
    req: &mut Request,
    shared: &Shared,
    frame: FrameRef,
    left: Option<TraceId>,
) -> Visited {
    if req.limits().interrupt().raised().load(Ordering::Relaxed)
        || req.state::<JitRequest>().recording
    {
        return Visited::Continue;
    }
    let claims = !quiet(req, None);
    let req: &Request = req;
    let Some(at) = tier::frame(req, frame).map(|view| view.opline()) else {
        return Visited::Continue;
    };
    let word = tier::landing(req, frame, at);
    if let Some(left) = left
        && policy::read(word) == State::Compiled(left)
    {
        return Visited::Continue;
    }
    let func = tier::func(req, frame);
    let holder = || {
        let table = table::of(req, &shared.policy, &shared.perf)?;
        if table.stopped() {
            return None;
        }
        Some(Holder::Landing {
            epoch: tier::epoch(req),
            func: func?,
            at,
        })
    };
    visit(word, &shared.policy, Start::Return, claims, holder)
}

/// What an exit with no link goes on to (`jit-design.md` §4.9), php's
/// `zend_jit_trace_exit` (`Z/ext/opcache/jit/zend_jit_trace.c:8796-8894`):
/// the side trace its word names, entered with no line written, as php's
/// exit is patched to jump to it; nothing at all for a word that grows
/// no side from now on, blacklisted, or while the interrupt is raised or
/// the request records, php returning there before it counts or writes;
/// otherwise php's `TRACE n exit m` line for the place it landed, and its
/// word bumped toward `opcache.jit_hot_side_exit` and claimed at it under
/// `Holder::Exit`, so a side trace is recorded from the landing, php's
/// `zend_jit_trace_hot_side`. An exit that grows none, php's
/// `ZEND_JIT_EXIT_TO_VM`, is claimed by nothing and spent
/// ([`policy::spend`]): blacklisted at the firing that brings its heat to
/// `opcache.jit_hot_side_exit` plus `opcache.jit_blacklist_side_trace`,
/// with php's `---- EXIT m/k blacklisted` after its exit line, and left
/// with no line from then on. One that names no opline is bumped and
/// claimed by nothing, its side being one php holds to the opline it
/// stands at with a guard this tier does not hold yet.
///
/// A `for` over lines leaves its trace through its test, `exit 3`, at the
/// line past the loop, where a side trace of it starts, named by its
/// parent's number and exit and that line, as php's
/// `zend_jit_trace_hot_side` names it; the second call's root links to
/// the loop, which leaves through the side:
///
/// ```php
/// function a($n) {
///     $s = 0;
///     for ($i = 0;
///          $i < $n;
///          $i++) {
///         $s += $i;
///     }
///     return $s;
/// }
/// echo a(100), " ", a(200), "\n";
/// ```
/// ```expect exit=0 regime=jit-inline process
/// 4950 19900
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:11
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) a() %s:5
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 3 a() %s:9
/// ---- TRACE 2 start (side trace 1/3) a() %s:9
/// ---- TRACE 2 stop (return)
/// ---- TRACE 2 compiled
///
/// ---- TRACE 3 start (enter) a() %s:3
/// ---- TRACE 3 stop (link to 1)
/// ---- TRACE 3 compiled
/// ```
fn sided(
    req: &mut Request,
    shared: &Shared,
    trace: TraceId,
    exited: Exited,
    exit: &table::Exit,
) -> Visited {
    match policy::read(&exit.word) {
        State::Compiled(side) => return Visited::Enter(side),
        State::Blacklisted => return Visited::Continue,
        State::Cold | State::Recording => {}
    }
    if req.limits().interrupt().raised().load(Ordering::Relaxed) || quiet(req, None) {
        return Visited::Continue;
    }
    if shared.policy.debug().any(JitDebug::TRACE_EXIT) {
        let mut lines = Vec::new();
        placed(req, exited.frame, Line::Stands, |at| {
            let event = TraceEvent::Exit {
                trace,
                exit: exited.exit,
                at,
            };
            shared.dump(&mut lines, &event);
        });
        req.response_mut().write_stderr(&lines);
    }
    let start = Start::Side {
        parent: trace,
        exit: exited.exit,
    };
    if exit.grows == Grows::Never {
        if policy::spend(&exit.word, &shared.policy, start) {
            let event = TraceEvent::ExitBlacklisted {
                trace,
                exit: exited.exit,
            };
            written(req, shared, &event);
        }
        return Visited::Continue;
    }
    let claims = exit.opline.is_some();
    let req: &Request = req;
    let holder = || {
        Some(Holder::Exit {
            epoch: tier::epoch(req),
            trace,
            exit: exited.exit,
        })
    };
    visit(&exit.word, &shared.policy, start, claims, holder)
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
///
/// #2999's program: the fiber `body()` runs inside `$main`'s recording,
/// so its loop records nothing, and `work()`'s loop is a root at its
/// header, recorded again at once where `$main`'s recording stops at it,
/// whose exit grows a side trace at the line past the loop; php's
/// optimizer numbers that exit otherwise (#1220):
///
/// ```php
/// function body() { $s = 0; for ($i = 0; $i < 3; $i++) { if ($i == 1) { Fiber::suspend(1); } $s += $i; } echo "never\n"; }
/// function work() { $s = 0; for ($i = 0; $i < 3; $i++) { $s += $i; } return $s; }
/// $f = new Fiber("body"); $f->start(); echo work(), "\n";
/// ```
/// ```expect regime=jit-inline process
/// 3
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:4
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) work() %s:3
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit %d work() %s:3
/// ---- TRACE 2 start (side trace 1/%d) work() %s:3
/// ---- TRACE 2 stop (return)
/// ---- TRACE 2 compiled
///
/// ```
#[inline(never)]
pub(crate) fn hot(
    req: &mut Request,
    mut frame: FrameRef,
    mut claim: Claim,
    shared: &Shared,
) -> Flow {
    let mut stepped = false;
    let (flow, stood, activated) = loop {
        let start = claim.start();
        let behind = match begins(req, frame, start, claim.holder()) {
            Ok(Begins::Behind(condition)) => Some(condition),
            Ok(Begins::Stands) => None,
            Ok(Begins::Stepped(header)) => {
                (frame, stepped) = (header, true);
                None
            }
            Err(flow) => {
                policy::settle(claim, &Outcome::Released);
                return flow;
            }
        };
        let epoch = table::holding(req, &shared.policy, &shared.perf);
        let table = table::of_epoch(&epoch);
        let next = table.and_then(table::TraceTable::next);
        // A frame that shows no op array has no place to record from.
        let Some(name) = started(req, shared, frame, next, start, behind) else {
            policy::settle(claim, &Outcome::Released);
            break (resumed(frame, stepped), None, false);
        };
        if let Some(refusal) = refused(table, start) {
            let recorded = record::refused(req, frame, start, refusal);
            settled(
                req,
                shared,
                Job {
                    recorded,
                    claim,
                    name,
                },
                GoesOn::Past,
            );
            break (resumed(frame, stepped), None, false);
        }
        let entered = side_entered(table, start);
        let opening = record::Opening { entered, behind };
        let claimed = claim.holder();
        let (recorded, ended) =
            match record::run(req, frame, start, claimed, &shared.policy, opening) {
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
        let stop = recorded.record.stop;
        let Some((rooted, name)) = rerooted(req, shared, claim, name, &ended) else {
            break (resumed(ended.at, stepped), None, ended.activated);
        };
        let job = Job {
            recorded,
            claim: rooted,
            name,
        };
        // A loop's trace closed at its header leaves the interpreter at
        // that header, short of the trace.
        let goes_on = if stop == Stop::Loop {
            GoesOn::Short
        } else {
            GoesOn::Past
        };
        let outcome = settled(req, shared, job, goes_on);
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
            // php's interpreter goes on at the opline the recording
            // stopped at, whose handler is the trace compiled there: the
            // one just published where it repeats, at its loop's header,
            // its recursive call's entry or its recursive return's
            // landing, or the one the recording stopped at.
            let stood = match (outcome, stop) {
                (
                    Some(Outcome::Published(id)),
                    Stop::Loop | Stop::RecursiveCall | Stop::RecursiveRet,
                ) => Some(id),
                _ => ended.stood,
            };
            let stood = stood.map(|id| (ended.at, id));
            break (resumed(ended.at, stepped), stood, ended.activated);
        };
        frame = ended.at;
        claim = inner;
    };
    written(req, shared, &TraceEvent::Done);
    went_on(req, shared, flow, stood, activated)
}

/// Where a recording that answered `flow` stopped, php's interpreter
/// goes on through the handler of the opline it stopped at: `stood`, the
/// compiled trace whose root it stopped at, entered; otherwise, where it
/// stopped at a call's activation, the entry's word visited as
/// `Site::Entry` visits it, and where a return landed, the landing's.
fn went_on(
    req: &mut Request,
    shared: &Shared,
    flow: Flow,
    stood: Option<(FrameRef, TraceId)>,
    activated: bool,
) -> Flow {
    let ran = match (stood.filter(|(at, id)| opens(req, *at, *id, shared)), &flow) {
        (Some((at, id)), _) => run(req, at, id, shared),
        (None, Flow::Resume(at)) if activated => safepoint(req, *at, Site::Entry),
        (None, Flow::Resume(at)) => match visit_landing(req, shared, *at, None) {
            Visited::Continue => Flow::Continue,
            Visited::Enter(id) => run(req, *at, id, shared),
            Visited::Hot(claim) => hot(req, *at, claim, shared),
        },
        (None, _) => Flow::Continue,
    };
    match ran {
        Flow::Continue => flow,
        ran => ran,
    }
}

/// `event`'s line, where `opcache.jit_debug` asks for it, written to the
/// request's error stream.
fn written(req: &mut Request, shared: &Shared, event: &TraceEvent<'_>) {
    let mut lines = Vec::new();
    shared.dump(&mut lines, event);
    req.response_mut().write_stderr(&lines);
}

/// Where a recording of `start` claimed at `at` on `frame` begins.
#[derive(Debug, Clone, Copy)]
enum Begins {
    /// Where the frame stands.
    Stands,
    /// At a loop's condition, which the back edge just served, one
    /// compare-and-branch the recording writes down as served
    /// (`record::behind`).
    Behind(OplineIdx),
    /// At a loop's condition the body was stepped on to
    /// (`record::headed`), in this frame.
    Stepped(FrameRef),
}

/// Where a loop's recording begins, where php's trace roots: its
/// condition, behind the frame or past the body the back edge landed
/// on, the header `claimed` holds; any other start's where the frame
/// stands. A body that went another way on the way there is the flow to
/// resume.
fn begins(
    req: &mut Request,
    frame: FrameRef,
    start: Start,
    claimed: &Holder,
) -> Result<Begins, Flow> {
    let (Start::Loop, Holder::Root { at, .. }) = (start, claimed) else {
        return Ok(Begins::Stands);
    };
    let at = *at;
    if let Some(condition) = record::behind(req, frame, at) {
        return Ok(Begins::Behind(condition));
    }
    match record::headed(req, frame, at) {
        Ok(Headed::Stands(_)) => Ok(Begins::Stands),
        Ok(Headed::Stepped(header)) => Ok(Begins::Stepped(header)),
        Ok(Headed::Away(away)) => Err(Flow::Resume(away)),
        Ok(Headed::Unstepped) => Err(Flow::Continue),
        Err(fatal) => Err(Flow::Bail(fatal)),
    }
}

/// Whether trace `id` opens where `frame` stands: its first recorded
/// opline is the frame's, in the frame's op array.
fn opens(req: &Request, frame: FrameRef, id: TraceId, shared: &Shared) -> bool {
    let at = tier::func(req, frame).zip(tier::frame(req, frame).map(|view| view.opline()));
    let table = table::of(req, &shared.policy, &shared.perf);
    at.is_some()
        && table
            .and_then(|table| table.get(id))
            .is_some_and(|trace| trace.opens == at)
}

/// Why a recording of `start` is refused before it records, php's
/// checks in `zend_jit_trace_hot_root` and `zend_jit_trace_hot_side`
/// (`Z/ext/opcache/jit/zend_jit_trace.c:8164-8167`, `:8513-8525`): a
/// table whose next id reached `opcache.jit_max_root_traces`, which a hot
/// root stops the counters of, and a side of a root that has
/// `opcache.jit_max_side_traces`.
fn refused(table: Option<&table::Table>, start: Start) -> Option<Stop> {
    let Some(table) = table else {
        return Some(Stop::TooManyTraces);
    };
    let side = start.grew().map(|(parent, _)| parent);
    if table.full() {
        if side.is_none() {
            table.stop();
        }
        return Some(Stop::TooManyTraces);
    }
    side.filter(|parent| table.children_full(*parent))
        .map(|_| Stop::TooManyChildren)
}

/// What a side trace of `start` enters with: what its parent's exit knew
/// of the slots of the frame it landed in; nothing for a root.
fn side_entered(table: Option<&table::Table>, start: Start) -> Vec<record::Entered> {
    let Some((parent, exit)) = start.grew() else {
        return Vec::new();
    };
    (table.and_then(|table| table.get(parent)))
        .and_then(|trace| trace.exits.get(usize::from(exit.0)))
        .map_or_else(Vec::new, |exit| exit.entered.to_vec())
}

/// `job` settled: a recording that stopped where a trace compiles
/// submitted to the engine's compiler, which settles it here or on its
/// worker; any other settled here at once as the attempt it failed, as
/// php writes its abort before it records again. Its lines written;
/// what it settled as, where it was settled here.
fn settled(req: &mut Request, shared: &Shared, job: Job, goes_on: GoesOn) -> Option<Outcome> {
    let mut lines = Vec::new();
    let outcome = if job.recorded.record.stop.compiles() {
        let word = job.claim.holder().clone();
        let engine = req.engine::<JitEngine>();
        match engine.map(|engine| engine.compiler.submit(shared, job, &mut lines)) {
            Some(None) => {
                req.state::<JitRequest>().submitted = Some(Submitted { word, goes_on });
                None
            }
            Some(settled) => settled,
            None => None,
        }
    } else {
        Some(compiler::pipeline(shared, job, &mut lines))
    };
    req.response_mut().write_stderr(&lines);
    outcome
}

/// A trace the request submitted to the worker (`jit-design.md` §4.8,
/// revision 55, as built until a person rules on #3191) and has
/// not reached yet, while which the request claims no word, as while it
/// records (revision 30). php compiles where it records and its
/// interpreter goes on at the opline the recording stopped at, whose
/// handler is then the trace, so nothing it runs between the two heats a
/// root; under `jit.compile=thread` the interpreter runs on meanwhile, and
/// what it ran heated what php never reaches. On spectralnorm, the outer
/// loop's recordings stopped at the inner loop's claimed header as at an
/// inner loop, spending the outer's attempts, and `eval_A`, called from
/// the interpreted inner loop, became a root, so the next inner loop's
/// trace linked to it at the call where php's inlines it, and closed no
/// loop. A word visited meanwhile is bumped and left. Under
/// `jit.compile=inline` nothing is submitted.
#[derive(Debug)]
pub(crate) struct Submitted {
    /// The trace's word, which reads recording while its job is queued
    /// or under way.
    word: Holder,
    /// Where the interpreter goes on.
    goes_on: GoesOn,
}

/// Where the interpreter goes on once a recording is submitted, beside
/// the trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GoesOn {
    /// Past it: the request reaches the trace once it is published.
    Past,
    /// Short of it, at the header a loop's trace closed at, where php's
    /// would have entered it: the request reaches it at the next loop or
    /// return site it visits, the loop's back edge or the return of what
    /// its body called, its loop having ended or not.
    Short,
}

/// Whether the request claims no word at `site`, a safepoint's or, for
/// none, an exit's landing: a recording under way, or a trace it
/// submitted it has not reached ([`Submitted`]); one it reached is
/// forgotten here.
fn quiet(req: &mut Request, site: Option<Site>) -> bool {
    quieted(req.state::<JitRequest>(), site)
}

/// [`quiet`] over the request's state.
fn quieted(state: &mut JitRequest, site: Option<Site>) -> bool {
    if state.recording {
        return true;
    }
    let Some(submitted) = &state.submitted else {
        return false;
    };
    let under_way = policy::read(submitted.word.word()) == State::Recording;
    let short = submitted.goes_on == GoesOn::Short
        && !matches!(site, Some(Site::Loop(_) | Site::Return(_)));
    if !under_way && !short {
        state.submitted = None;
    }
    under_way || short
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
    let Some(name) = started(req, shared, ended.at, next, Start::Loop, None) else {
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
    behind: Option<OplineIdx>,
) -> Option<TraceName> {
    let mut lines = Vec::new();
    let line = match (start, behind) {
        (_, Some(at)) => Line::At(at),
        (Start::Enter, None) => Line::Prologue,
        (Start::Loop | Start::Return | Start::Side { .. }, None) => Line::Stands,
    };
    let name = placed(req, frame, line, |at| {
        if let Some(trace) = trace {
            shared.dump(&mut lines, &TraceEvent::Start { trace, start, at });
        }
        TraceName::of(&at)
    });
    req.response_mut().write_stderr(&lines);
    name
}

/// Which opline's line a [`Place`] names: the one the frame stands at;
/// for an entry php counts at a typed prologue, its first `RECV`'s; or
/// the condition a loop's recording begins at behind the frame
/// (`record::behind`).
#[derive(Debug, Clone, Copy)]
enum Line {
    Stands,
    Prologue,
    At(OplineIdx),
}

/// What `with` makes of the [`Place`] `frame` stands at, at the line
/// `line` names; none where the frame shows no op array.
fn placed<T>(
    req: &Request,
    frame: FrameRef,
    line: Line,
    with: impl FnOnce(Place<'_>) -> T,
) -> Option<T> {
    let view = tier::frame(req, frame)?;
    let function = view.function();
    let decl = function.and_then(|function| req.symbols().function_decl(function));
    let name = decl.map(|decl| decl.name.0.to_vec());
    let scope = function
        .and(view.function_scope())
        .map(|class| req.symbols().class_name(class).into_bytes());
    let typed = decl.is_some_and(|decl| decl.parameters.iter().any(|p| p.ty.is_some()));
    let at = match line {
        Line::Prologue if typed => Some(OplineIdx(0)),
        Line::At(at) => Some(at),
        Line::Prologue | Line::Stands => None,
    };
    let line = at
        .and_then(|at| view.op_array()?.opline(at))
        .map_or(view.line(), |op| op.line());
    Some(with(Place {
        scope: scope.as_deref(),
        function: name.as_deref(),
        file: view.filename(),
        line,
    }))
}

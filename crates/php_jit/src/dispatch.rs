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
//! `zend_jit_trace_setup_ret_counter`), or, at a loop's root no trace
//! armed, the loop's word ([`goes_on`]), and the callee's own `RETURN`
//! word is neither read nor claimed. A trace that leaves for the
//! interpreter at such an opline visits its word too, as php's
//! interpreter runs the opline's handler ([`visit_landing`]).

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use php_engine::tier::Flow;
use php_engine::tier::{
    self, EngineFuncRef, Epoch, Fatal, FrameRef, FuncRef, Held, Request, SafepointIdx, Site, Word,
};
use php_unit::OplineIdx;

use crate::compiler::{self, Job};
use crate::enter::{self, Answered, At, Crossed as _, Exited, Left, Lent};
use crate::policy::{
    self, Claim, Holder, JitDebug, Landing, Mode, Outcome, Place, Policy, State, TraceEvent,
};
use crate::record::{self, Arrived, Bailed, Ended, Headed, Landed, Start, Stop};
use crate::table::{self, Trace, TraceId, TraceName};
use crate::types::Grows;
use crate::{Lines, Shared};

#[cfg(test)]
mod tests;

/// The tier's state on a request (`jit-design.md` §2.2, §4; #3144), as
/// php's tracing tier keeps `JIT_G`: the engine's `Shared` and where its
/// recordings compile, resolved once as the request is laid, so a visit
/// reads the `Policy`, and a recording that compiles submits, with no
/// lookup of the engine state; whether a recording is under way on the
/// request, php's `JIT_G(tracing)`, which no fiber switch saves (§4.9,
/// revision 30), set and cleared by `record`'s guard alone; and the trace
/// it last submitted to the worker and has not reached yet, while which it
/// claims nothing (`dispatch::Submitted`). `dispatch::quiet` reads the
/// two. Each in a `Cell`, so a visit reads the whole state through the
/// request it holds shared, once (`tier::state`).
pub struct JitRequest {
    pub(crate) shared: Arc<Shared>,
    pub(crate) submitter: compiler::Submitter,
    pub(crate) recording: Cell<bool>,
    /// Boxed, so a visit that finds none, nearly every one, reads one
    /// word ([`quieted`]).
    pub(crate) submitted: Cell<Option<Box<Submitted>>>,
    /// The headers of the calls its traces push in place, the request's.
    pub(crate) headers: enter::Headers,
    /// The request's hold on the epoch it started in, taken once, which
    /// every crossing borrows ([`enter::lent`]).
    pub(crate) epoch: std::cell::OnceCell<Epoch>,
    /// The trace table of that epoch, kept once a request of it made it.
    pub(crate) table: std::cell::OnceCell<Arc<table::Table>>,
    /// The loop header whose next arrival a recording aborted in a callee
    /// owes a count, as php's handler there counts it ([`Owed`]).
    pub(crate) owed: Cell<Option<Owed>>,
    /// The words of the oplines the request last went on at past a
    /// return or a leave ([`enter::Landings`]).
    pub(crate) landings: enter::Landings,
    /// How many times the crossing entered the trace compiled at a landing
    /// a trace escaped to, which the escaping trace's code enters itself
    /// once the request's row of it is laid: a test's count of the escapes
    /// the code did not take.
    #[cfg(test)]
    pub(crate) escapes_crossed: Cell<u32>,
    /// How many times a trace's exit or escape visited the opline it
    /// left a frame at ([`visit_landing`]): a test's count of the
    /// landings that were not left alone at once ([`unarmed`]).
    #[cfg(test)]
    pub(crate) landings_visited: Cell<u32>,
    /// How many times a return visited its site ([`return_visit`]): a test's
    /// count of the returns [`return_site`] let through.
    #[cfg(test)]
    pub(crate) returns_visited: Cell<u32>,
}

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
/// Here, and only here, `Continue` means nothing happened. The tier's
/// state is looked up once, and the request lends it, the engine's state
/// and its hold on the epoch to what the visit goes on to ([`enter::lent`]),
/// so a visit that bumps or enters writes no count of an `Arc` the
/// engine's threads share; one that claims clones the hold for its
/// holder, as [`hot`] takes its own. A return's word is
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
#[inline(always)] // jit-design.md §2.2: the return's test is in the `RETURN*` arms
#[allow(clippy::inline_always)]
pub(crate) fn safepoint(req: &mut Request, frame: FrameRef, site: Site, held: Held<'_>) -> Flow {
    match site {
        Site::Return(_) => return_site(req, frame, site, held),
        Site::Loop(_) | Site::Entry => visited(req, frame, site, held),
    }
}

/// A loop's or an entry's site, visited ([`safepoint_lent`]).
#[inline(never)]
fn visited(req: &mut Request, frame: FrameRef, site: Site, held: Held<'_>) -> Flow {
    enter::lent(req, |req, lent| {
        safepoint_lent(req, lent, (frame, site), held)
    })
    .unwrap_or(Flow::Continue)
}

/// A return that popped to `caller`, the tier's inlined test of it
/// (`jit-design.md` §2.2, #4105): the caller's landing word read off its
/// op array, and the frame the request owes a count to ([`owes`]), which
/// no thread else may read in a word. A sealed word and no count owed ask
/// nothing, no visit acting on either, as php's interpreter runs the
/// plain handler of an opline no trace armed. Anything else visits, with
/// what the word read ([`return_visit`]): a word a trace armed, and one
/// [`policy::head`]ed for the loop compiled there, a state of the
/// landing's word, so a return reads no loop of the op array here. A
/// landing is left sealed where its loop is cold, whose back edge counts
/// its heat ([`Goes::visit`]).
///
/// A return is tested on the frame it popped to, as that frame stands now,
/// and never of the number it stands at: a frame's number is a place on
/// the frame table another call takes up again. A trace entered at a
/// callee's number leaves it, runs through the caller and leaves in
/// another function at that number, the entry frame of a nested serving,
/// a generator's frame or a fiber's, and each goes on through its own op
/// array's word:
///
/// ```php
/// function add($a, $b) { return $a + $b; }
/// function quiet($n) { return $n <= 0 ? 0 : add(quiet($n - 1), 1); }
/// function other($x) { return strlen("abc") + add($x, 0); }
/// for ($i = 0; $i < 3000; $i++) { $s = quiet(5); }
/// function hot() { $s = 0; for ($i = 0; $i < 3000; $i++) { $s += quiet(5); $s += array_sum(array_map('other', [$i, $i + 1])); } return $s; }
/// echo hot(), "\n";
/// function g() { for ($i = 0; $i < 2000; $i++) { yield quiet(5) + other($i); } }
/// function hg() { $s = 0; foreach (g() as $v) { $s += $v; } return $s; }
/// echo hg(), "\n";
/// $fib = new Fiber(function () { $s = 0; for ($i = 0; $i < 2000; $i++) { $s += quiet(5) + other($i); if ($i % 500 == 0) { Fiber::suspend($s); } } return $s; });
/// $v = $fib->start(); while (!$fib->isTerminated()) { $v = $fib->resume(); } echo $fib->getReturn(), "\n";
/// ```
/// ```expect regime=jit-force process
/// 9033000
/// 2015000
/// 2015000
/// ```
#[inline(always)] // jit-design.md §2.2: in the `RETURN*` arms
#[allow(clippy::inline_always)]
fn return_site(req: &mut Request, caller: FrameRef, site: Site, held: Held<'_>) -> Flow {
    let landing = Landing::of(tier::landing_stood(req, caller));
    if landing.sealed() && !owes(req, caller) {
        return Flow::Continue;
    }
    return_visit(req, (caller, site), landing, held)
}

/// What a return's site does where a landing asked ([`return_site`]),
/// read with nothing else looked up, as php's interpreter runs the
/// handler a landing holds: a trace compiled there is entered in the
/// caller the return popped to, which the `RETURN` made before it asked
/// (#4056), and an armed word given up on, or one a recording holds, is
/// left alone.
#[derive(Debug, Clone, Copy)]
enum Returning {
    /// Nothing to do: the interpreter makes the return.
    Continue,
    /// The trace to enter in the caller, the return made.
    Enter(TraceId),
    /// The whole visit ([`safepoint_lent`]): a cold landing, which counts
    /// and may claim, a landing a loop's trace headed, whose word is the
    /// loop's, or a count the request owes ([`Owed`]).
    Ask,
}

/// The visit of a return [`return_site`] let through, `landing` what its
/// test read: the word read once.
#[inline(never)]
fn return_visit(
    req: &mut Request,
    (caller, site): (FrameRef, Site),
    landing: Landing,
    held: Held<'_>,
) -> Flow {
    enter::lent(req, |req, lent| {
        #[cfg(test)]
        (lent.state.returns_visited).set(lent.state.returns_visited.get().saturating_add(1));
        match returning(req, lent, landing) {
            Returning::Continue => Flow::Continue,
            Returning::Enter(id) => match lent.table.and_then(|table| table.get(id)) {
                Some(trace) => run_trace(req, lent, caller, trace),
                None => run(req, lent, caller, id),
            },
            Returning::Ask => safepoint_lent(req, lent, (caller, site), held),
        }
    })
    .unwrap_or(Flow::Continue)
}

/// [`Returning`] for a return whose landing's word read `landing`.
#[inline(always)] // once a visit, in `return_visit`
#[allow(clippy::inline_always)]
fn returning(req: &Request, lent: Lent<'_>, landing: Landing) -> Returning {
    if lent.shared().policy.mode() == Mode::Off {
        return Returning::Continue;
    }
    if tier::owed_frame(req).is_some() {
        return Returning::Ask;
    }
    if landing.headed() {
        return Returning::Ask;
    }
    match landing.state() {
        State::Compiled(id) => Returning::Enter(id),
        State::Cold => Returning::Ask,
        State::Recording | State::Blacklisted => Returning::Continue,
    }
}

/// [`safepoint`] under what the request lends: the whole visit, out of
/// line, so a return that [`returning`] answers is served with nothing
/// of it.
#[inline(never)]
fn safepoint_lent(
    req: &mut Request,
    lent: Lent<'_>,
    (frame, site): (FrameRef, Site),
    held: Held<'_>,
) -> Flow {
    let shared = lent.shared();
    if shared.policy.mode() == Mode::Off {
        return Flow::Continue;
    }
    let decided = {
        let req: &Request = req;
        let claims = !quieted(lent.state, Some(site));
        let (rooted, word) = match site {
            Site::Loop(at) => (Rooted::Safepoint(at, Start::Loop), held.word(req, at)),
            Site::Entry => {
                let at = SafepointIdx::ENTRY;
                (Rooted::Safepoint(at, Start::Enter), held.word(req, at))
            }
            Site::Return(_) => {
                let Some(at) = landing(req, frame) else {
                    return Flow::Continue;
                };
                let (goes, word) = goes_on(req, lent, (frame, at), shared.policy.folds());
                (Rooted::Goes(frame, goes), word)
            }
        };
        // A table a hot root found full stopped its counters, as php's
        // `zend_jit_stop_counter_handlers` does: no word is claimed more.
        let holder = || {
            let table = table::of(req, &shared.policy, &shared.perf)?;
            if table.stopped() {
                return None;
            }
            match rooted {
                Rooted::Safepoint(at, _) => Some(Holder::Root {
                    epoch: lent.epoch.clone(),
                    func: EngineFuncRef::of(held.func())?,
                    at,
                }),
                Rooted::Goes(caller, goes) => goes.holder(req, caller),
            }
        };
        match rooted {
            Rooted::Goes(_, goes) => goes.visit(word, &shared.policy, claims, holder),
            Rooted::Safepoint(_, start) => visit(word, &shared.policy, start, claims, holder),
        }
    };
    match (site, decided) {
        (Site::Return(_), decided) => returned(req, lent, frame, decided),
        (Site::Loop(_), Visited::Continue) => {
            paid(req, lent.state, frame);
            Flow::Continue
        }
        (_, Visited::Continue) => Flow::Continue,
        (Site::Loop(header), Visited::Enter(id)) => looped(req, lent, frame, header, id),
        (Site::Entry, Visited::Enter(id)) => run(req, lent, frame, id),
        (Site::Loop(_), Visited::Hot(claim)) => {
            paid(req, lent.state, frame);
            hot(req, frame, claim, shared)
        }
        (Site::Entry, Visited::Hot(claim)) => hot(req, frame, claim, shared),
    }
}

/// Where a site's word stands: a safepoint of the frame's op array,
/// with what a recording claimed there starts as, or, for a return,
/// where its caller goes on past the call it stands at.
#[derive(Debug, Clone, Copy)]
enum Rooted {
    Safepoint(SafepointIdx, Start),
    Goes(FrameRef, Goes),
}

/// The word a frame reads where it goes on at an opline the loop did
/// not reach by a site, past a return or a trace that left it there, as
/// php's interpreter runs that opline's handler ([`goes_on`]).
#[derive(Debug, Clone, Copy)]
enum Goes {
    /// The landing word of the opline, which a compiled trace arms past
    /// a call (`record::landings`): a return root's.
    Landing(OplineIdx),
    /// The word of the loop rooted at the opline: php's handler there is
    /// the loop's, its heat or the trace that replaced it (#3318).
    Header,
}

impl Goes {
    /// A visit of the word, `holder` the claim's where one is taken: a
    /// landing's as a return root's, bumped toward
    /// `opcache.jit_hot_return` and claimed there; a loop root's entered
    /// where it is compiled and never bumped or claimed, since the loop's
    /// back edge counts the loop's heat, one count a turn as php's
    /// header handler counts one, and an arrival past a return or an exit
    /// counted too would count a body that ends in a call twice a turn.
    fn visit(
        self,
        word: &Word,
        policy: &Policy,
        claims: bool,
        holder: impl FnOnce() -> Option<Holder>,
    ) -> Visited {
        match self {
            Goes::Landing(_) => visit(word, policy, Start::Return, claims, holder),
            Goes::Header => match policy::read(word) {
                State::Compiled(id) => Visited::Enter(id),
                State::Cold | State::Recording | State::Blacklisted => Visited::Continue,
            },
        }
    }

    /// The holder a claim of a landing's word in `frame` takes, under
    /// the epoch `req` started in; none where the frame runs no op array
    /// a row names, and for a loop root's word, which is not claimed here.
    fn holder(self, req: &Request, frame: FrameRef) -> Option<Holder> {
        match self {
            Goes::Landing(at) => Some(Holder::Landing {
                epoch: tier::epoch(req),
                func: EngineFuncRef::of(tier::running_func(req, frame)?)?,
                at,
            }),
            Goes::Header => None,
        }
    }
}

/// The one rule for the word `frame` reads where it goes on at opline
/// `at`, as php's interpreter runs the handler of the opline it goes on
/// at: a landing word a compiled trace armed, which it arms at no loop
/// php counts (`record::landings`); otherwise, at a loop's root, that
/// loop's word, as php's handler there is the loop's, whose trace is
/// entered from there ([`Goes::visit`]); the sealed landing word anywhere
/// else. The two words are read off the op array's row and its loops
/// searched once for an opline the request goes on at, and kept for the
/// next arrivals there, as php's interpreter reads the handler an opline
/// holds with no search ([`enter::Landings`]): a frame that runs its op
/// array's own cache names the op array by it with no row read.
///
/// A loop's trace entered from its root's word tests the interrupt at its
/// header, as every loop's does but one by recursive return, which takes
/// none (`types::Interrupt::None`): that is a return trace's, which
/// `record` stops at a recursive return alone for a `Return` start, rooted
/// at a landing a trace armed, which this reads first, so no loop's root
/// word names one. A trace that leaves for the interpreter where it
/// was entered enters nothing there ([`visit_landing`]), as php's
/// `zend_jit_trace_exit` runs the opline's own handler.
///
/// A `while` whose condition php keeps is headed there, and `f`'s
/// return lands on it: the loop's trace, which links to `f`'s, is
/// entered there, so the interrupt the timer raises is met inside `f`
/// and its fatal is raised at `f`'s line each time, as php's (#3318):
///
/// ```php
/// set_time_limit(1);
/// function f($n) { return $n ? f($n - 1) + 1 : 0; }
/// const T = true;
/// while (T) { f(50); }
/// ```
/// ```expect exit=255 regime=jit-inline process wall=1s
/// Fatal error: Maximum execution time of 1 second exceeded in %s on line 3
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:2
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) $main() %s:5
/// ---- TRACE 1 abort (deep recursion)
///
/// ---- TRACE 1 start (enter) f() %s:3
/// ---- TRACE 1 stop (recursive call)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 1 f() %s:3
/// ---- TRACE 2 start (side trace 1/1) f() %s:3
/// ---- TRACE 2 stop (return)
/// ---- TRACE 2 compiled
///
/// ---- TRACE 3 start (return) f() %s:3
/// ---- TRACE 3 stop (recursive return)
/// ---- TRACE 3 compiled
///
/// ---- TRACE 4 start (loop) $main() %s:5
/// ---- TRACE 4 stop (link to 1)
/// ---- TRACE 4 compiled
/// ```
///
/// The same over a variable's condition:
///
/// ```php
/// set_time_limit(1);
/// function f($n) { return $n ? f($n - 1) + 1 : 0; }
/// $t = true;
/// while ($t) { f(50); }
/// ```
/// ```expect exit=255 regime=jit-inline process wall=1s
/// Fatal error: Maximum execution time of 1 second exceeded in %s on line 3
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:2
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) $main() %s:5
/// ---- TRACE 1 abort (deep recursion)
///
/// ---- TRACE 1 start (enter) f() %s:3
/// ---- TRACE 1 stop (recursive call)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 1 f() %s:3
/// ---- TRACE 2 start (side trace 1/1) f() %s:3
/// ---- TRACE 2 stop (return)
/// ---- TRACE 2 compiled
///
/// ---- TRACE 3 start (return) f() %s:3
/// ---- TRACE 3 stop (recursive return)
/// ---- TRACE 3 compiled
///
/// ---- TRACE 4 start (loop) $main() %s:5
/// ---- TRACE 4 stop (link to 1)
/// ---- TRACE 4 compiled
/// ```
#[inline(always)] // a kept opline's words read where the visit reads them; the miss is out of line
#[allow(clippy::inline_always)]
fn goes_on<'r>(
    req: &'r Request,
    lent: Lent<'r>,
    (frame, at): (FrameRef, OplineIdx),
    folds: record::Folds,
) -> (Goes, &'r Word) {
    let kept = (tier::frame_cache(req, frame).and_then(EngineFuncRef::of))
        .and_then(|func| lent.landed(func, at));
    let Some(landed) = kept else {
        return goes_on_found(req, lent, (frame, at), folds);
    };
    landed.goes(at)
}

/// [`goes_on`] at an opline the request keeps no words of under the
/// frame's cache: the op array's row looked up, as a frame that reads a
/// cache no op array owns, a closure's, names its op array, and its
/// loops searched where the request keeps none under the op array either.
#[cold]
#[inline(never)]
fn goes_on_found<'r>(
    req: &'r Request,
    lent: Lent<'r>,
    (frame, at): (FrameRef, OplineIdx),
    folds: record::Folds,
) -> (Goes, &'r Word) {
    let (landing, held) = tier::landing(req, frame, at);
    match held.and_then(|held| Some((held, EngineFuncRef::of(held.func())?))) {
        None => (Goes::Landing(at), landing),
        Some((held, func)) => (lent.landed(func, at))
            .unwrap_or_else(|| lent.keep(func, at, record::loop_at(held.code(), at, folds)))
            .goes(at),
    }
}

impl<'l> enter::Landed<'l> {
    /// The word a frame reads where it goes on at `at`, whose words these
    /// are ([`goes_on`]): an armed landing's, the loop's rooted there, or
    /// the landing's, sealed.
    #[inline]
    fn goes(self, at: OplineIdx) -> (Goes, &'l Word) {
        match self.header {
            Some(header) if !policy::armed(self.landing) => (Goes::Header, header),
            _ => (Goes::Landing(at), self.landing),
        }
    }
}

/// The opline the caller a `RETURN` popped to lands on, the one past
/// its call, where the pop stood it; none where the frame shows none.
#[inline]
fn landing(req: &Request, caller: FrameRef) -> Option<OplineIdx> {
    Some(tier::frame(req, caller)?.opline())
}

/// A return, asked in the caller it popped its frame to and standing at
/// the landing past the call (`Site::Return`, #4056): php's interpreter
/// has returned before the landing's handler runs, so the return trace
/// is entered, or recorded from, there, with nothing of the `RETURN`
/// left to serve. A landing whose word asked nothing pays what a
/// recording that aborted in a callee owes the caller's loop
/// ([`owed`]). A throwable the return left pending, a destructor's, is
/// the caller's to raise at its call opline, and asks no site.
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
#[inline(never)]
fn returned(req: &mut Request, lent: Lent<'_>, caller: FrameRef, decided: Visited) -> Flow {
    match decided {
        Visited::Continue if owes(req, caller) => {
            owed(req, lent.shared(), caller).unwrap_or(Flow::Continue)
        }
        Visited::Continue => Flow::Continue,
        Visited::Enter(id) => run(req, lent, caller, id),
        Visited::Hot(claim) => hot(req, caller, claim, lent.shared()),
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
    lent: Lent<'_>,
    frame: FrameRef,
    header: SafepointIdx,
    id: TraceId,
) -> Flow {
    if let Some(flow) = run_at(req, lent, frame, id, At::Body) {
        return flow;
    }
    match record::headed(req, frame, header, lent.shared().policy.folds()) {
        Ok(Headed::Stands(at)) => run(req, lent, at, id),
        Ok(Headed::Stepped(at)) => match run(req, lent, at, id) {
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
#[inline]
pub(crate) fn run(req: &mut Request, lent: Lent<'_>, frame: FrameRef, id: TraceId) -> Flow {
    run_at(req, lent, frame, id, At::First).unwrap_or(Flow::Continue)
}

/// [`run`] of `trace`, of the table the request lends, entered at its
/// first op with nothing of the table read.
#[inline]
fn run_trace(req: &mut Request, lent: Lent<'_>, frame: FrameRef, trace: &Trace) -> Flow {
    let Some(table) = lent.table else {
        return run(req, lent, frame, trace.id);
    };
    let entered = enter::enter(req, Some(lent.state), frame, (lent.epoch, trace), At::First);
    followed(req, lent, table, entered)
}

/// [`run`], its first trace entered `at` its first op or its loop's
/// body, and each after it at its first op, out of the epoch the request
/// lends, whose table holds it; none where the first, asked at its body,
/// keeps no way in where the frame stands or declined the entry, with
/// nothing done.
fn run_at(req: &mut Request, lent: Lent<'_>, frame: FrameRef, id: TraceId, at: At) -> Option<Flow> {
    let epoch = lent.epoch;
    // The table the request kept, or, where it kept none yet, the one a
    // request of the epoch made since the lend began, whose trace the
    // word names.
    let Some(table) = lent.table.or_else(|| table::of_epoch(epoch)) else {
        return Some(Flow::Continue);
    };
    let Some(trace) = table.get(id) else {
        return Some(Flow::Continue);
    };
    if at == At::Body {
        let stands = tier::frame(req, frame).map(|view| view.opline());
        if trace.body.is_none() || trace.body != stands {
            return None;
        }
    }
    let entered = enter::enter(req, Some(lent.state), frame, (epoch, trace), at);
    if matches!(
        entered,
        Ok(Answered {
            left: Left::Declined,
            ..
        })
    ) {
        return None;
    }
    Some(followed(req, lent, table, entered))
}

/// What `trace` left through, `entered`, followed from trace to trace
/// as [`run`] says. A return past a trace's lowest frame that lands
/// elsewhere than the trace goes on escapes, as php's
/// `zend_jit_leave_func` escapes (`Z/ext/opcache/jit/zend_jit_ir.c:11188-11224`):
/// no exit, no line and no word, the interpreter going on through the
/// handler of the opline the return left the frame at, the landing's
/// trace where one is compiled there ([`visit_landing`]). So fib's
/// recursive return, landing past its second call, writes no exit line,
/// as php writes none (the exit numbers are #1220's):
///
/// ```php
/// function fib($n) { if ($n < 2) { return $n; } return fib($n - 1) + fib($n - 2); }
/// echo fib(12), "\n";
/// ```
/// ```expect regime=jit-inline process
/// 144
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:3
/// ---- TRACE 1 abort (deep recursion)
///
/// ---- TRACE 1 start (enter) fib() %s:2
/// ---- TRACE 1 stop (recursive call)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit %d fib() %s:2
/// ---- TRACE 2 start (side trace 1/%d) fib() %s:2
/// ---- TRACE 2 stop (return)
/// ---- TRACE 2 compiled
///
/// ---- TRACE 3 start (return) fib() %s:2
/// ---- TRACE 3 stop (recursive return)
/// ---- TRACE 3 compiled
///
///      TRACE 3 exit %d fib() %s:2
/// ---- TRACE 4 start (side trace 3/%d) fib() %s:2
/// ---- TRACE 4 stop (link to 1)
/// ---- TRACE 4 compiled
///
///      TRACE 1 exit 1 fib() %s:2
/// ---- TRACE 5 start (side trace 1/1) fib() %s:2
/// ---- TRACE 5 stop (return)
/// ---- TRACE 5 compiled
///
/// ```
fn followed<'t>(
    req: &mut Request,
    lending: Lent<'t>,
    table: &'t table::Table,
    mut entered: Result<Answered<'t>, Fatal>,
) -> Flow {
    let (state, shared, epoch) = (lending.state, lending.shared(), lending.epoch);
    loop {
        // The trace that answered: the one entered, or one it jumped to.
        let (left, mut trace, from) = match entered {
            Ok(Answered { left, by, from }) => (left, by, from),
            Err(fatal) => return Flow::Bail(fatal),
        };
        let exited = match left {
            Left::Exit(exited) => exited,
            Left::Unwound(frame) => return Flow::Resume(frame),
            // php's `ZEND_VM_RETURN` past the serving's entry frame.
            Left::Returned => return Flow::Returned,
            // php's escape stub: no exit, no line and no word; the
            // interpreter runs the handler of the opline the return left
            // the frame at, a landing's trace where one is compiled there.
            // Its code read the landing's word and found no trace there, so
            // one that counts nothing either goes on with no visit.
            Left::Escaped(frame) if unarmed(req, lending, frame) => return Flow::Resume(frame),
            Left::Escaped(frame) => match visit_landing(req, lending, frame, Came::Escaped) {
                Visited::Enter(id) => match table.get(id) {
                    Some(next) => {
                        #[cfg(test)]
                        state
                            .escapes_crossed
                            .set(state.escapes_crossed.get().saturating_add(1));
                        state.headers.escaped_on(trace);
                        trace = next;
                        entered = enter::enter(req, Some(state), frame, (epoch, trace), At::First);
                        continue;
                    }
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
            // php's interrupt stub: the interrupt handled, and the trace
            // entered again at its header, its handler.
            Left::Interrupted(header) => match tier::interrupted(req) {
                Ok(false) => {
                    entered = enter::enter(req, Some(state), header, (epoch, trace), At::First);
                    continue;
                }
                Ok(true) => return Flow::Resume(header),
                Err(fatal) => return Flow::Bail(fatal),
            },
            Left::Declined => {
                return Flow::Bail(Fatal::error(
                    "InternalError: a trace declined an entry at its first op",
                ));
            }
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
            None if let Some(flow) = owed(req, shared, frame) => return flow,
            None if unarmed(req, lending, frame) => return Flow::Resume(frame),
            None => match visit_landing(req, lending, frame, Came::Left(Some(from))) {
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
        entered = enter::enter(req, Some(state), frame, (epoch, trace), At::First);
    }
}

/// How the interpreter came to a landing ([`visit_landing`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Came {
    /// A trace's escape, which php's stub hands to the handler of the
    /// opline the frame stands at with nothing tested
    /// (`zend_jit_trace_escape_stub`), as the escaping trace's own code
    /// does where the trace compiled there is ready ([`crate::lower::Escapes`]).
    Escaped,
    /// A trace's exit, with the frame and the opline the [`enter::enter`]
    /// it answered found, or a recording's stop, none, which php's
    /// `zend_jit_trace_exit` hands on only while the interrupt is not
    /// raised.
    Left(Option<(FrameRef, OplineIdx)>),
}

/// Whether the opline a trace left `frame` at is one no visit acts on,
/// read where `frame` stands through the words the request keeps for it
/// ([`goes_on`]): its landing unarmed, sealed or given up, and no loop's
/// trace compiled there. php's interpreter goes on at such an opline
/// through the opline's own handler, which counts nothing
/// (`zend_jit_trace_setup_ret_counter` arms a landing by setting its
/// handler, `Z/ext/opcache/jit/zend_jit_trace.c:3418-3430`), so a trace
/// that leaves a frame there hands it straight back to the loop, with no
/// call into [`visit_landing`] (#4100). False where the request keeps no
/// words for the opline: the visit looks them up.
#[inline(always)] // once an exit, where the trace left for the interpreter
#[allow(clippy::inline_always)]
fn unarmed(req: &Request, lending: Lent<'_>, frame: FrameRef) -> bool {
    let Some((Some(func), at)) = tier::standing_at(req, frame) else {
        return false;
    };
    let Some(landed) = EngineFuncRef::of(func).and_then(|func| lending.landed(func, at)) else {
        return false;
    };
    let (goes, word) = landed.goes(at);
    !matches!(
        (goes, policy::read(word)),
        (_, State::Compiled(_)) | (Goes::Landing(_), State::Cold)
    )
}

/// The landing a trace or a recording left `frame` for the interpreter
/// at, visited as php's interpreter runs the handler of the opline it
/// goes on at (`zend_jit_trace_exit_stub`, `zend_jit_trace_return`), the
/// word [`goes_on`] names: a trace compiled there is entered, and an
/// armed landing's word bumped and claimed at `opcache.jit_hot_return`
/// (`jit-design.md` §2.2, #3036), and a loop root's compiled trace
/// entered (#3318). Nothing while the request records, nor, but at an
/// escape, while the interrupt is raised, nor where the trace left the
/// frame where it was entered; a frame at an opline with a sealed word is
/// left as it is.
///
/// php's `zend_jit_trace_exit` ends by answering
/// `orig_opline == t->opline && EX(opline) == orig_opline`
/// (`Z/ext/opcache/jit/zend_jit_trace.c:8920`). A root's `t->opline` is
/// its first recorded opline (`:7547`), so a root that leaves there
/// answers 1, in whichever frame, and the exit stub
/// calls the opline's original handler, past the trace php patched in
/// (`zend_jit_trace_exit_stub`, `Z/ext/opcache/jit/zend_jit_ir.c:2517`). A
/// side's `t->opline` is null (`:8381`), so a side answers 0 and the stub
/// goes through `EX(opline)->handler`, which is the root's where one
/// stands there. That is safe in php because its `RETURN` always leaves:
/// a trace that stands at the opline it opened at made its way there.
///
/// Here it may have done nothing. A `RETURN` a trace stops at can refuse
/// its leave ([`crate::lower`]'s `leave_refused`: arguments
/// past the parameters, an annex, a first-class callable's closure, a
/// frame served from outside), and a `RETURN_BY_REF` is the interpreter's
/// alone: each takes the trace's end with the frame untouched, so a return
/// root and its sides all come back to the opline they opened at having
/// done nothing, and asking the word again enters the root again, and the
/// interpreter never makes the return (#4148).
/// So the rule kept is about progress rather than about roots: any trace,
/// root or side, whose call of [`enter::enter`] found the frame where it
/// left it goes to the opline's own handler. It reads the place and the
/// opline `enter` found, not where the trace opens: a side that recorded
/// no opline is held by it with no branch of its own, and a root that
/// left at its own opline in another frame, by a recursive return, made
/// way and enters, where php's root runs the handler. A [`FrameRef`] is a
/// place, so a frame popped and pushed again at the same place and opline
/// reads as not having moved, which costs one opline in the interpreter.
///
/// Under the exit-fuzz lane each guard failing once grows those sides. A
/// first-class callable's return, a closure's passed one argument more
/// and a return by reference each loop here without the rule:
///
/// ```php
/// function f($x) { if ($x % 3 == 0) { return 1.5; } if ($x % 3 == 1) { return 2; } return true; }
/// function g($i) { return f($i); }
/// function &r($i) { static $v = 1; f($i); return $v; }
/// $h = g(...); $e = function ($i) { return f($i); };
/// $s = 0;
/// for ($i = 0; $i < 120; $i++) { $s += (int) $h($i); $s += (int) $e($i, 1); $s += (int) r($i); }
/// echo $s, "\n";
/// ```
/// ```expect regime=jit-exit-fuzz process
/// 440
/// ```
///
/// A closure's return, which leaves in place, whatever hosts the loop:
///
/// ```php
/// function f($x) { if ($x % 3 == 0) { return 1.5; } if ($x % 3 == 1) { return 2; } return true; }
/// $s = 0; $h = function ($i) { return f($i); }; for ($i = 0; $i < 120; $i++) { $s += (int) $h($i); }
/// echo $s, "\n";
/// ```
/// ```expect regime=jit-exit-fuzz process
/// 160
/// ```
///
/// ```php
/// function f($x) { if ($x % 3 == 0) { return 1.5; } if ($x % 3 == 1) { return 2; } return true; }
/// function a() { $s = 0; $h = function ($i) { return f($i); }; for ($i = 0; $i < 120; $i++) { $s += (int) $h($i); } return $s; }
/// class C {
///     function b() { $s = 0; $h = function ($i) { return f($i); }; for ($i = 0; $i < 120; $i++) { $s += (int) $h($i); } return $s; }
///     static function c() { $s = 0; $h = function ($i) { return f($i); }; for ($i = 0; $i < 120; $i++) { $s += (int) $h($i); } return $s; }
///     function __call($n, $a) { $s = 0; $h = function ($i) { return f($i); }; for ($i = 0; $i < 120; $i++) { $s += (int) $h($i); } return $s; }
///     function __invoke() { $s = 0; $h = function ($i) { return f($i); }; for ($i = 0; $i < 120; $i++) { $s += (int) $h($i); } return $s; }
/// }
/// trait T { function e() { $s = 0; $h = function ($i) { return f($i); }; for ($i = 0; $i < 120; $i++) { $s += (int) $h($i); } return $s; } }
/// class D { use T; }
/// $g = function () { $s = 0; $h = fn ($i) => f($i); for ($i = 0; $i < 120; $i++) { $s += (int) $h($i); } return $s; };
/// $o = new C;
/// echo a(), " ", $o->b(), " ", C::c(), " ", $o->d(), " ", $o(), " ", (new D)->e(), " ", $g(), "\n";
/// ```
/// ```expect regime=jit-exit-fuzz process
/// 160 160 160 160 160 160 160
/// ```
fn visit_landing(req: &Request, lending: Lent<'_>, frame: FrameRef, came: Came) -> Visited {
    #[cfg(test)]
    (lending.state.landings_visited).set(lending.state.landings_visited.get().saturating_add(1));
    if matches!(came, Came::Left(_)) && req.limits().interrupt().raised().load(Ordering::Relaxed) {
        return Visited::Continue;
    }
    let (state, shared) = (lending.state, lending.shared());
    if state.recording.get() {
        return Visited::Continue;
    }
    let Some(at) = tier::frame(req, frame).map(|view| view.opline()) else {
        return Visited::Continue;
    };
    let (goes, word) = goes_on(req, lending, (frame, at), shared.policy.folds());
    // A word that neither enters nor counts is read once and left, as
    // php's escape runs the plain handler of an opline no trace armed.
    match (goes, policy::read(word)) {
        (_, State::Compiled(_)) if came == Came::Left(Some((frame, at))) => Visited::Continue,
        (_, State::Compiled(id)) => Visited::Enter(id),
        (Goes::Landing(_), State::Cold) => {
            let claims = !quieted(state, None);
            let holder = || {
                let table = table::of(req, &shared.policy, &shared.perf)?;
                if table.stopped() {
                    return None;
                }
                goes.holder(req, frame)
            };
            goes.visit(word, &shared.policy, claims, holder)
        }
        _ => Visited::Continue,
    }
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
/// with no line from then on. One that names no opline, a table's jump
/// or a call that may be another's, leaves the frame where it stood: its
/// side is recorded from there and holds that opline at its first op,
/// php's IP guard (`zend_jit_trace_opline_guard`), whose own exit names
/// none and grows a side of its own, so php's lines stop once the side
/// is compiled ([`record::Landed`]).
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
///
/// A `match` over strings jumps through a table, so its loop's trace
/// takes an exit per opline the jump may go to but the one it went to,
/// the default's and then each arm's, as php's `zend_jit_switch` takes
/// them. The second arm's key leaves through that arm's own exit, 5, and
/// the side trace grows from it, so the exit's line is written once, as
/// php's, and not at every key (#3203):
///
/// ```php
/// function f($keys) { $s = 0; foreach ($keys as $k) { $s += match ($k) { 'a' => 1, 'b' => 2 }; } return $s; }
/// echo f(['a', 'a', 'a', 'b', 'b', 'b']), "\n";
/// ```
/// ```expect regime=jit-inline process
/// 9
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:3
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) f() %s:2
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 5 f() %s:2
/// ---- TRACE 2 start (side trace 1/5) f() %s:2
/// ---- TRACE 2 stop (link to 1)
/// ---- TRACE 2 compiled
///
///      TRACE 1 exit 3 f() %s:2
/// ---- TRACE 3 start (side trace 1/3) f() %s:2
/// ---- TRACE 3 stop (return)
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
    let mut lines = Lines::of(req, shared);
    if lines.debug().any(JitDebug::TRACE_EXIT) {
        placed(req, exited.frame, Line::Stands, |at| {
            let event = TraceEvent::Exit {
                trace,
                exit: exited.exit,
                at,
            };
            lines.dump(&event);
        });
        lines.write(req);
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
    let req: &Request = req;
    let holder = || {
        Some(Holder::Exit {
            epoch: tier::epoch(req),
            trace,
            exit: exited.exit,
        })
    };
    visit(&exit.word, &shared.policy, start, true, holder)
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
    loop {
        let (flow, stood, activated) = match recordings(req, frame, claim, shared, &mut stepped) {
            Ok(last) => last,
            Err(left) => return left,
        };
        written(req, shared, &TraceEvent::Done);
        if let Some(Stood::Header(at, func, header)) = stood {
            match counted(req, shared, at, func, header) {
                Counted::Again(again) => {
                    (frame, claim) = (at, again);
                    continue;
                }
                Counted::Stands => {}
                Counted::On(on) => return on,
            }
        }
        return went_on(req, shared, flow, stood, activated);
    }
}

/// The recordings a claim of `frame`'s word makes, [`hot`]'s, the inner
/// loop each repeats at once included: where the last stopped, the flow
/// back to the loop, what its handler there is, and whether it stopped at
/// a call's activation; `Err` with the flow where a recording was never
/// made or bailed out. `stepped` says whether anything was stepped,
/// across every call for one [`hot`].
fn recordings(
    req: &mut Request,
    mut frame: FrameRef,
    mut claim: Claim,
    shared: &Shared,
    stepped: &mut bool,
) -> Result<(Flow, Option<Stood>, bool), Flow> {
    loop {
        let start = claim.start();
        let behind = match begins(req, frame, start, claim.holder(), shared.policy.folds()) {
            Ok(Begins::Behind(condition)) => Some(condition),
            Ok(Begins::Stands) => None,
            Ok(Begins::Stepped(header)) => {
                (frame, *stepped) = (header, true);
                None
            }
            // php's interpreter runs the header's own handler, which php
            // never swapped, and writes no line: an inner loop's header,
            // which a recording met, is always counted
            // (`record::header_at`).
            Ok(Begins::Uncounted) => {
                policy::settle(claim, &Outcome::Uncounted);
                return Err(resumed(frame, *stepped));
            }
            Err(flow) => {
                policy::settle(claim, &Outcome::Released);
                return Err(flow);
            }
        };
        let epoch = table::holding(req, &shared.policy, &shared.perf);
        let table = table::of_epoch(&epoch);
        let next = table.and_then(table::TraceTable::next);
        // A frame that shows no op array has no place to record from.
        let Some(name) = started(req, shared, frame, next, start, behind) else {
            policy::settle(claim, &Outcome::Released);
            return Ok((resumed(frame, *stepped), None, false));
        };
        if let Some(refusal) = refused(table, start) {
            refused_settled(req, shared, claim, refusal);
            return Ok((resumed(frame, *stepped), None, false));
        }
        let opening = side_opened(req, frame, (table, shared.policy.folds()), start, behind);
        let Some(root) = record::held_root(req, frame) else {
            refused_settled(req, shared, claim, Stop::RequestUnit);
            return Ok((resumed(frame, *stepped), None, false));
        };
        let claimed = claim.holder();
        let (recorded, ended) =
            match record::run(req, root, frame, start, claimed, &shared.policy, opening) {
                Ok(done) => done,
                // php's bailout leaves the recorder with nothing written after
                // the start line and nothing counted against the root.
                Err(bailed) => {
                    policy::settle(claim, &Outcome::Released);
                    return Err(match bailed {
                        Bailed::Fatal(fatal) => Flow::Bail(fatal),
                        Bailed::Unwinding(at) => Flow::Resume(at),
                    });
                }
            };
        *stepped |= ended.stepped;
        let stop = recorded.record.stop;
        let Some((rooted, name)) = rerooted(req, shared, claim, name, &ended) else {
            return Ok((resumed(ended.at, *stepped), None, ended.activated));
        };
        let header = header_of(&rooted);
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
        owe(req, outcome, (frame, header), ended.at);
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
            // landing, or the one the recording stopped at; at a loop's
            // header whose trace was not published, the header's handler
            // still counts the loop.
            let stood = match (outcome, stop, header) {
                (
                    Some(Outcome::Published(id)),
                    Stop::Loop | Stop::RecursiveCall | Stop::RecursiveRet,
                    _,
                ) => Some(Stood::Trace(ended.at, id)),
                (Some(Outcome::Abort(_)), Stop::Loop, Some((func, header))) => {
                    Some(Stood::Header(ended.at, func, header))
                }
                _ => ended.stood.map(|id| Stood::Trace(ended.at, id)),
            };
            return Ok((resumed(ended.at, *stepped), stood, ended.activated));
        };
        frame = ended.at;
        claim = inner;
    }
}

/// A loop's header whose next arrival php's handler at the header counts and
/// the back edge does not see: the loop's recording in `frame` aborted
/// in a callee, a may-recover abort that leaves the header's handler in
/// place (`Z/ext/opcache/jit/zend_jit_trace.c:8262-8268`), and the
/// interpreter reaches the header next from the call's landing, past
/// the body's rest, where the condition may leave the loop ([`owed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Owed {
    func: EngineFuncRef,
    header: SafepointIdx,
}

/// A count owed to loop `header`'s frame where its recording, `outcome`,
/// aborted in a callee, `at` ([`Owed`]).
fn owe(
    req: &Request,
    outcome: Option<Outcome>,
    (frame, header): (FrameRef, Option<(EngineFuncRef, SafepointIdx)>),
    at: FrameRef,
) {
    if let (Some(Outcome::Abort(_)), Some((func, header))) = (outcome, header)
        && at != frame
        && let Some(state) = tier::state::<JitRequest>(req)
    {
        state.owed.set(Some(Owed { func, header }));
        tier::set_owed_frame(req, Some(frame));
    }
}

/// Whether a return landed in `caller`, the frame a count is owed to
/// ([`Owed`]), which [`returned`] pays: the frame beside the tier's state
/// ([`tier::owed_frame`]), where [`return_site`] reads it with no check
/// of that state's type.
#[inline(always)] // jit-design.md §2.2: in the `RETURN*` arms
#[allow(clippy::inline_always)]
fn owes(req: &Request, caller: FrameRef) -> bool {
    tier::owed_frame(req) == Some(caller)
}

/// A back edge in `frame` counted its loop, so what a return into
/// `frame` owed is counted already.
fn paid(req: &Request, state: &JitRequest, frame: FrameRef) {
    if owes(req, frame) {
        state.owed.set(None);
        tier::set_owed_frame(req, None);
    }
}

/// The count a return into `frame` owes its loop's header ([`Owed`]):
/// the frame stepped from its landing to the header, as php's
/// interpreter runs the body's rest, and the header's word visited
/// there, as php's `zend_jit_trace_counter_helper` counts the arrival
/// (`Z/ext/opcache/jit/zend_jit_vm_helpers.c:464-497`) ([`counted`]).
/// None where nothing is owed to `frame`, or a recording is under way.
///
/// `$main`'s recording enters `f`, whose outer loop aborts it, and that
/// loop's own recording aborts at the inner loop compiled already; `f`
/// returns, and the arrival at `$main`'s header counts, the last one
/// where the condition leaves the loop and the recording stops at
/// `$main`'s return (#3586):
///
/// ```php
/// function a(int $i, int $j): float { $ij = $i + $j; return 1.0 / ($ij * ($ij + 1) / 2 + $i + 1); }
/// function f(int $n, array $u): array {
///     $out = [];
///     for ($i = 0; $i < $n; $i++) { $sum = 0.0; for ($j = 0; $j < $n; $j++) { $sum += a($i, $j) * $u[$j]; } $out[$i] = $sum; }
///     return $out;
/// }
/// $u = array_fill(0, 60, 1.0);
/// for ($k = 0; $k < 4; $k++) { $u = f(60, $u); }
/// printf("%.6f\n", $u[0]);
/// ```
/// ```expect regime=jit-inline process
/// 5.188575
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %SCRATCH%/witness.php:8
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) $main() %SCRATCH%/witness.php:9
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) f() %SCRATCH%/witness.php:5
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) f() %SCRATCH%/witness.php:5
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit 4 f() %SCRATCH%/witness.php:5
/// ---- TRACE 2 start (side trace 1/4) f() %SCRATCH%/witness.php:5
/// ---- TRACE 2 stop (link to 1)
/// ---- TRACE 2 compiled
///
///      TRACE 2 exit 0 f() %SCRATCH%/witness.php:5
/// ---- TRACE 3 start (side trace 2/0) f() %SCRATCH%/witness.php:5
/// ---- TRACE 3 stop (link to 1)
/// ---- TRACE 3 compiled
///
///      TRACE 3 exit 1 f() %SCRATCH%/witness.php:6
/// ---- TRACE 4 start (side trace 3/1) f() %SCRATCH%/witness.php:6
/// ---- TRACE 4 stop (return)
/// ---- TRACE 4 compiled
///
/// ---- TRACE 5 start (loop) $main() %SCRATCH%/witness.php:9
/// ---- TRACE 5 abort (inner loop)
/// ---- TRACE 5 start (loop) f() %SCRATCH%/witness.php:5
/// ---- TRACE 5 abort (compiled loop)
///
/// ---- TRACE 5 start (loop) $main() %SCRATCH%/witness.php:9
/// ---- TRACE 5 abort (inner loop)
/// ---- TRACE 5 start (loop) f() %SCRATCH%/witness.php:5
/// ---- TRACE 5 abort (compiled loop)
///
/// ---- TRACE 5 start (loop) $main() %SCRATCH%/witness.php:9
/// ---- TRACE 5 abort (inner loop)
/// ---- TRACE 5 start (loop) f() %SCRATCH%/witness.php:5
/// ---- TRACE 5 abort (compiled loop)
///
/// ---- TRACE 5 start (loop) $main() %SCRATCH%/witness.php:9
/// ---- TRACE 5 stop (return)
/// ---- TRACE 5 compiled
/// ```
fn owed(req: &mut Request, shared: &Shared, frame: FrameRef) -> Option<Flow> {
    let state = tier::state::<JitRequest>(req)?;
    let owed = state.owed.get().filter(|_| owes(req, frame))?;
    state.owed.set(None);
    tier::set_owed_frame(req, None);
    if state.recording.get() || tier::func(req, frame) != Some(owed.func.into()) {
        return None;
    }
    let at = match record::arrived(req, frame, owed.header, shared.policy.folds()) {
        Ok(Arrived::Header(at)) => at,
        // The call's return lands here again, still owed the count.
        Ok(Arrived::Call(at)) => {
            tier::state::<JitRequest>(req)?.owed.set(Some(owed));
            tier::set_owed_frame(req, Some(frame));
            return Some(Flow::Resume(at));
        }
        Ok(Arrived::Away(at)) => return Some(Flow::Resume(at)),
        Ok(Arrived::Unstepped) => return None,
        Err(fatal) => return Some(Flow::Bail(fatal)),
    };
    let flow = match counted(req, shared, at, owed.func, owed.header) {
        Counted::Again(claim) => hot(req, at, claim, shared),
        Counted::Stands => went_on(req, shared, Flow::Resume(at), None, false),
        Counted::On(on) => on,
    };
    Some(match flow {
        Flow::Continue => Flow::Resume(at),
        flow => flow,
    })
}

/// The loop header a loop's claim holds the word of, by its op array and
/// safepoint; none for any other claim.
fn header_of(claim: &Claim) -> Option<(EngineFuncRef, SafepointIdx)> {
    match (claim.start(), claim.holder()) {
        (Start::Loop, Holder::Root { func, at, .. }) => Some((*func, *at)),
        _ => None,
    }
}

/// What php's interpreter meets at the opline a recording stopped at,
/// where that opline's handler is the tier's.
#[derive(Debug, Clone, Copy)]
enum Stood {
    /// The compiled trace rooted there, which it enters at once.
    Trace(FrameRef, TraceId),
    /// The header of the loop, in this op array, whose recording closed
    /// there and published nothing: the handler counts the loop.
    Header(FrameRef, EngineFuncRef, SafepointIdx),
}

/// What came of the count at a loop's header ([`counted`]).
enum Counted {
    /// The count claimed the header's word, recorded from at once.
    Again(Claim),
    /// Nothing was counted or stepped: the interpreter goes on where the
    /// recording stopped, through [`went_on`], which enters a trace a
    /// thread published at the header meanwhile.
    Stands,
    /// The interpreter goes on where the loop's condition went.
    On(Flow),
}

/// php's interpreter going on at the header of loop `header` of `func`,
/// where `frame`'s recording of the loop closed and the loop's trace
/// failed to compile: php's `zend_jit_trace_counter_helper`, which the
/// header's handler still is, counts the visit there and, at
/// `opcache.jit_hot_loop`, calls `zend_jit_trace_hot_root` again
/// (`Z/ext/opcache/jit/zend_jit_vm_helpers.c:464-497`), since an abort
/// that may recover leaves the header's handler as it stood
/// (`Z/ext/opcache/jit/zend_jit_trace.c:8262-8268`). Here the header's
/// word is visited as a site visits it, claimed for a recording from
/// the header where its heat reaches the threshold. A word that is not
/// cold, blacklisted at this abort or claimed or published elsewhere, is
/// not visited, and the interpreter goes on where it stood. Where the visit claimed nothing, the loop's
/// condition, which its back edge counts php's visit of, is stepped
/// through to where it goes, so the back edge does not count it twice
/// ([`record::served`]).
///
/// A loop whose trace calls 31 functions, more op arrays than php's
/// `ZEND_JIT_TRACE_MAX_FUNCS` lets a trace's build analyse
/// (`analysis::MAX_FUNCS`), aborts at each compile; the interpreter goes
/// on at the header after each, and the sixth visit, where the condition
/// leaves the loop, records the loop's exit to `$main`'s return (#3576):
///
/// ```php
/// function f0($x) { return $x + 0; } function f1($x) { return $x + 1; } function f2($x) { return $x + 2; } function f3($x) { return $x + 3; } function f4($x) { return $x + 4; } function f5($x) { return $x + 5; } function f6($x) { return $x + 6; } function f7($x) { return $x + 7; }
/// function f8($x) { return $x + 8; } function f9($x) { return $x + 9; } function f10($x) { return $x + 10; } function f11($x) { return $x + 11; } function f12($x) { return $x + 12; } function f13($x) { return $x + 13; } function f14($x) { return $x + 14; } function f15($x) { return $x + 15; }
/// function f16($x) { return $x + 16; } function f17($x) { return $x + 17; } function f18($x) { return $x + 18; } function f19($x) { return $x + 19; } function f20($x) { return $x + 20; } function f21($x) { return $x + 21; } function f22($x) { return $x + 22; } function f23($x) { return $x + 23; }
/// function f24($x) { return $x + 24; } function f25($x) { return $x + 25; } function f26($x) { return $x + 26; } function f27($x) { return $x + 27; } function f28($x) { return $x + 28; } function f29($x) { return $x + 29; } function f30($x) { return $x + 30; }
/// $s = 0;
/// for ($i = 0; $i < 5; $i++) {
///     $s += f0($i); $s += f1($i); $s += f2($i); $s += f3($i); $s += f4($i); $s += f5($i); $s += f6($i); $s += f7($i);
///     $s += f8($i); $s += f9($i); $s += f10($i); $s += f11($i); $s += f12($i); $s += f13($i); $s += f14($i); $s += f15($i);
///     $s += f16($i); $s += f17($i); $s += f18($i); $s += f19($i); $s += f20($i); $s += f21($i); $s += f22($i); $s += f23($i);
///     $s += f24($i); $s += f25($i); $s += f26($i); $s += f27($i); $s += f28($i); $s += f29($i); $s += f30($i);
/// }
/// echo $s, "\n";
/// ```
/// ```expect regime=jit-inline process
/// 2635
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:6
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) $main() %s:7
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 abort (JIT compilation error)
///
/// ---- TRACE 1 start (loop) $main() %s:7
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 abort (JIT compilation error)
///
/// ---- TRACE 1 start (loop) $main() %s:7
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 abort (JIT compilation error)
///
/// ---- TRACE 1 start (loop) $main() %s:7
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 abort (JIT compilation error)
///
/// ---- TRACE 1 start (loop) $main() %s:7
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 abort (JIT compilation error)
///
/// ---- TRACE 1 start (loop) $main() %s:7
/// ---- TRACE 1 stop (return)
/// ---- TRACE 1 compiled
///
/// ```
///
/// The same under the force lane and the exit-fuzz lane:
///
/// ```php
/// function f0($x) { return $x + 0; } function f1($x) { return $x + 1; } function f2($x) { return $x + 2; } function f3($x) { return $x + 3; } function f4($x) { return $x + 4; } function f5($x) { return $x + 5; } function f6($x) { return $x + 6; } function f7($x) { return $x + 7; }
/// function f8($x) { return $x + 8; } function f9($x) { return $x + 9; } function f10($x) { return $x + 10; } function f11($x) { return $x + 11; } function f12($x) { return $x + 12; } function f13($x) { return $x + 13; } function f14($x) { return $x + 14; } function f15($x) { return $x + 15; }
/// function f16($x) { return $x + 16; } function f17($x) { return $x + 17; } function f18($x) { return $x + 18; } function f19($x) { return $x + 19; } function f20($x) { return $x + 20; } function f21($x) { return $x + 21; } function f22($x) { return $x + 22; } function f23($x) { return $x + 23; }
/// function f24($x) { return $x + 24; } function f25($x) { return $x + 25; } function f26($x) { return $x + 26; } function f27($x) { return $x + 27; } function f28($x) { return $x + 28; } function f29($x) { return $x + 29; } function f30($x) { return $x + 30; }
/// $s = 0;
/// for ($i = 0; $i < 5; $i++) {
///     $s += f0($i); $s += f1($i); $s += f2($i); $s += f3($i); $s += f4($i); $s += f5($i); $s += f6($i); $s += f7($i);
///     $s += f8($i); $s += f9($i); $s += f10($i); $s += f11($i); $s += f12($i); $s += f13($i); $s += f14($i); $s += f15($i);
///     $s += f16($i); $s += f17($i); $s += f18($i); $s += f19($i); $s += f20($i); $s += f21($i); $s += f22($i); $s += f23($i);
///     $s += f24($i); $s += f25($i); $s += f26($i); $s += f27($i); $s += f28($i); $s += f29($i); $s += f30($i);
/// }
/// echo $s, "\n";
/// ```
/// ```expect regime=jit-force process
/// 2635
/// ```
///
/// ```php
/// function f0($x) { return $x + 0; } function f1($x) { return $x + 1; } function f2($x) { return $x + 2; } function f3($x) { return $x + 3; } function f4($x) { return $x + 4; } function f5($x) { return $x + 5; } function f6($x) { return $x + 6; } function f7($x) { return $x + 7; }
/// function f8($x) { return $x + 8; } function f9($x) { return $x + 9; } function f10($x) { return $x + 10; } function f11($x) { return $x + 11; } function f12($x) { return $x + 12; } function f13($x) { return $x + 13; } function f14($x) { return $x + 14; } function f15($x) { return $x + 15; }
/// function f16($x) { return $x + 16; } function f17($x) { return $x + 17; } function f18($x) { return $x + 18; } function f19($x) { return $x + 19; } function f20($x) { return $x + 20; } function f21($x) { return $x + 21; } function f22($x) { return $x + 22; } function f23($x) { return $x + 23; }
/// function f24($x) { return $x + 24; } function f25($x) { return $x + 25; } function f26($x) { return $x + 26; } function f27($x) { return $x + 27; } function f28($x) { return $x + 28; } function f29($x) { return $x + 29; } function f30($x) { return $x + 30; }
/// $s = 0;
/// for ($i = 0; $i < 5; $i++) {
///     $s += f0($i); $s += f1($i); $s += f2($i); $s += f3($i); $s += f4($i); $s += f5($i); $s += f6($i); $s += f7($i);
///     $s += f8($i); $s += f9($i); $s += f10($i); $s += f11($i); $s += f12($i); $s += f13($i); $s += f14($i); $s += f15($i);
///     $s += f16($i); $s += f17($i); $s += f18($i); $s += f19($i); $s += f20($i); $s += f21($i); $s += f22($i); $s += f23($i);
///     $s += f24($i); $s += f25($i); $s += f26($i); $s += f27($i); $s += f28($i); $s += f29($i); $s += f30($i);
/// }
/// echo $s, "\n";
/// ```
/// ```expect regime=jit-exit-fuzz process
/// 2635
/// ```
fn counted(
    req: &mut Request,
    shared: &Shared,
    frame: FrameRef,
    func: EngineFuncRef,
    header: SafepointIdx,
) -> Counted {
    let epoch = tier::epoch(req);
    let word = epoch.word(func, header);
    if policy::read(word) != State::Cold {
        return Counted::Stands;
    }
    let claims = !quiet(req, Some(Site::Loop(header)));
    let holder = || {
        let table = table::of(req, &shared.policy, &shared.perf)?;
        (!table.stopped()).then(|| Holder::Root {
            epoch: epoch.clone(),
            func,
            at: header,
        })
    };
    match visit(word, &shared.policy, Start::Loop, claims, holder) {
        Visited::Hot(claim) => return Counted::Again(claim),
        Visited::Enter(_) => return Counted::Stands,
        Visited::Continue => {}
    }
    match record::served(req, frame, header, shared.policy.folds()) {
        Ok(Some(at)) => Counted::On(Flow::Resume(at)),
        Ok(None) => Counted::Stands,
        Err(fatal) => Counted::On(Flow::Bail(fatal)),
    }
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
    stood: Option<Stood>,
    activated: bool,
) -> Flow {
    let trace = match stood {
        Some(Stood::Trace(at, id)) if opens(req, at, id, shared) => Some((at, id)),
        Some(Stood::Trace(..) | Stood::Header(..)) | None => None,
    };
    let ran = match (trace, &flow) {
        (Some((at, id)), _) => run_lent(req, at, id),
        // The callee's op array, as the epoch holds it; one of the
        // request's reads the sealed word, which nothing visits.
        (None, Flow::Resume(at)) if activated => {
            let epoch = tier::epoch(req);
            match tier::func(req, *at).and_then(|f| epoch.held(f)) {
                Some(held) => safepoint(req, *at, Site::Entry, held),
                None => Flow::Continue,
            }
        }
        (None, Flow::Resume(at)) => match enter::lent(req, |req, lent| {
            visit_landing(req, lent, *at, Came::Left(None))
        })
        .unwrap_or(Visited::Continue)
        {
            Visited::Continue => Flow::Continue,
            Visited::Enter(id) => run_lent(req, *at, id),
            Visited::Hot(claim) => hot(req, *at, claim, shared),
        },
        (None, _) => Flow::Continue,
    };
    match ran {
        Flow::Continue => flow,
        ran => ran,
    }
}

/// [`run`] under a lend of its own, which finds the table a claim just
/// made where the lend its caller is under found none; nothing for a
/// request the tier laid no state on.
fn run_lent(req: &mut Request, frame: FrameRef, id: TraceId) -> Flow {
    enter::lent(req, |req, lent| run(req, lent, frame, id)).unwrap_or(Flow::Continue)
}

/// `event`'s line, where `opcache.jit_debug` asks for it, written to the
/// request's error stream.
fn written(req: &mut Request, shared: &Shared, event: &TraceEvent<'_>) {
    let mut lines = Lines::of(req, shared);
    lines.dump(event);
    lines.write(req);
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
    /// Nowhere: php counts no heat of the loop (`record::counts`), and
    /// records it never.
    Uncounted,
}

/// Where a loop's recording begins, where php's trace roots: its
/// condition, behind the frame or past the body the back edge landed
/// on, the header `claimed` holds, or nowhere for a loop php counts no
/// heat of; any other start's where the frame stands. A body that went
/// another way on the way there is the flow to resume.
fn begins(
    req: &mut Request,
    frame: FrameRef,
    start: Start,
    claimed: &Holder,
    folds: record::Folds,
) -> Result<Begins, Flow> {
    let (Start::Loop, Holder::Root { at, .. }) = (start, claimed) else {
        return Ok(Begins::Stands);
    };
    let at = *at;
    if !record::counts(req, frame, at, folds) {
        return Ok(Begins::Uncounted);
    }
    if let Some(condition) = record::behind(req, frame, at, folds) {
        return Ok(Begins::Behind(condition));
    }
    match record::headed(req, frame, at, folds) {
        Ok(Headed::Stands(_)) => Ok(Begins::Stands),
        Ok(Headed::Stepped(header)) => Ok(Begins::Stepped(header)),
        Ok(Headed::Away(away)) => Err(Flow::Resume(away)),
        Ok(Headed::Unstepped) => Err(Flow::Continue),
        Err(fatal) => Err(Flow::Bail(fatal)),
    }
}

/// Whether trace `id` opens where `frame` stands: its first recorded
/// opline is that one, in the frame's op array.
fn opens(req: &Request, frame: FrameRef, id: TraceId, shared: &Shared) -> bool {
    let at = tier::standing_at(req, frame);
    let table = table::of(req, &shared.policy, &shared.perf);
    at.zip(table.and_then(|table| table.get(id)))
        .is_some_and(|((_, at), trace)| {
            tier::func(req, frame).is_some_and(|func| {
                trace
                    .opens
                    .is_some_and(|(opens, opened)| FuncRef::from(opens) == func && opened == at)
            })
        })
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

/// What a recording of `start` opens with, `behind` beside it: for a
/// side trace, what its parent's exit knew of the slots of the frame it
/// landed in, and whether the exit named the opline it landed at or left
/// the frame where it stood, and how many returns past `frame` it
/// follows back to its root's loop ([`record::ret_depth`]); nothing of
/// any for a root.
fn side_opened(
    req: &Request,
    frame: FrameRef,
    (table, folds): (Option<&table::Table>, record::Folds),
    start: Start,
    behind: Option<OplineIdx>,
) -> record::Opening {
    let table = table.filter(|_| start.grew().is_some());
    let parent = (start.grew()).and_then(|(parent, exit)| Some((table?.get(parent)?, exit)));
    let exit = parent.and_then(|(trace, exit)| trace.exits.get(usize::from(exit.0)));
    let Some(exit) = exit else {
        return record::Opening {
            behind,
            ..record::Opening::default()
        };
    };
    record::Opening {
        entered: exit.entered.to_vec(),
        landed: if exit.opline.is_some() {
            Landed::Named
        } else {
            Landed::Stood
        },
        behind,
        ret_depth: parent
            .and_then(|(trace, _)| table?.get(trace.root))
            .filter(|root| root.leaves.is_none())
            .and_then(|root| root.opens)
            .map_or(0, |root| record::ret_depth(req, frame, root, folds)),
    }
}

/// A recording refused with `stop` before its first opline, settled on
/// the request as the pipeline settles an attempt that failed
/// ([`compiler::refused`]), with no record.
fn refused_settled(req: &mut Request, shared: &Shared, claim: Claim, stop: Stop) {
    let mut lines = Lines::of(req, shared);
    compiler::refused(claim, stop, &mut lines);
    lines.write(req);
}

/// `job` settled: a recording that stopped where a trace compiles
/// submitted through the request's [`compiler::Submitter`], which
/// settles it here or on the engine's worker; any other settled here at once as the attempt it failed, as
/// php writes its abort before it records again. Its lines written;
/// what it settled as, where it was settled here.
fn settled(req: &mut Request, shared: &Shared, job: Job, goes_on: GoesOn) -> Option<Outcome> {
    let mut lines = Lines::of(req, shared);
    let outcome = if job.recorded.record.stop.compiles() {
        let word = job.claim.holder().clone();
        let state = tier::state::<JitRequest>(req);
        match state.map(|state| (state, state.submitter.submit(shared, job, &mut lines))) {
            Some((state, None)) => {
                state
                    .submitted
                    .set(Some(Box::new(Submitted { word, goes_on })));
                None
            }
            Some((_, settled)) => settled,
            None => None,
        }
    } else {
        Some(compiler::pipeline(shared, job, &mut lines))
    };
    lines.write(req);
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
fn quiet(req: &Request, site: Option<Site>) -> bool {
    tier::state::<JitRequest>(req).is_some_and(|state| quieted(state, site))
}

/// [`quiet`] over the request's state: in line where nothing is
/// recorded or submitted, every visit under `jit.compile=inline`.
#[inline(always)] // jit-design.md §4.9: two loads on every visit
#[allow(clippy::inline_always)]
fn quieted(state: &JitRequest, site: Option<Site>) -> bool {
    if state.recording.get() {
        return true;
    }
    match state.submitted.take() {
        None => false,
        Some(submitted) => submitted_quiets(state, submitted, site),
    }
}

/// [`quieted`] where the request submitted a trace it has not reached.
#[cold]
#[inline(never)]
fn submitted_quiets(state: &JitRequest, submitted: Box<Submitted>, site: Option<Site>) -> bool {
    let under_way = policy::read(submitted.word.word()) == State::Recording;
    let short = submitted.goes_on == GoesOn::Short
        && !matches!(site, Some(Site::Loop(_) | Site::Return(_)));
    if under_way || short {
        state.submitted.set(Some(submitted));
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
    let mut lines = Lines::of(req, shared);
    let line = match (start, behind) {
        (_, Some(at)) => Line::At(at),
        (Start::Enter, None) => Line::Prologue,
        (Start::Loop | Start::Return | Start::Side { .. }, None) => Line::Stands,
    };
    let name = placed(req, frame, line, |at| {
        if let Some(trace) = trace {
            lines.dump(&TraceEvent::Start { trace, start, at });
        }
        TraceName::of(&at)
    });
    lines.write(req);
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
        .map(|class| req.symbols().class_name(class).to_vec());
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

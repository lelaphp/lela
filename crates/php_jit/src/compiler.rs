//! What becomes of a recording (`jit-design.md` §4.8): [`pipeline`],
//! `types::build`, then the engine's `Backend`, then the epoch's
//! `TraceTable::publish`, then `policy::settle`, with each failure mapped
//! to the one [`Outcome`] php settles it as; and [`Compiler`], where a
//! job runs, on the request under `jit.compile=inline` and on the
//! engine's one worker under `thread`.
//!
//! A published trace is in the table and its word reads `compiled`; what
//! enters it is `dispatch::run` (#2959), so until then every reader takes
//! the interpreter's path as before.
//!
//! **The worker** (`jit-design.md` §4.8, ADR-0010's Threads) is the
//! engine's [`tier::Worker`]: no thread until the first job, one at most,
//! parked between jobs, joined before every fork and started again after
//! it. It touches no request, frame or map pointer: a [`Job`] is a
//! [`Recorded`], whose view holds the epoch and through it the units and
//! the engine's interned strings it reads, and a [`Claim`], which holds
//! its word's epoch, so a job outlives the request and the engine that
//! made it. Dropping the [`Compiler`] drops the jobs still queued, each
//! claim settling itself, and the last hold on the worker, the engine's
//! or a request's [`Submitter`], joins the one under way.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};

use php_engine::api::abi::EngineState;
use php_engine::tier::{self, Epoch, Parked};

use crate::lower::{Compiled, Escapes, Linked};
use crate::policy::{self, Claim, Compile, Mode, Outcome, Policy, State, TraceEvent};
use crate::record::{self, Recorded, Start, Stop};
use crate::table::{self, Table, TraceId, TraceName, Unpublished};
use crate::types::{self, Abort, End, TraceIr};
use crate::{Lines, Shared};

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]
mod tests;

/// A recording, the claim on the word it grew from, and the name its
/// trace is published under, the place its `TRACE start` line named:
/// what the pipeline takes, on the request or off it.
#[derive(Debug, EngineState)]
pub(crate) struct Job {
    pub(crate) recorded: Recorded,
    pub(crate) claim: Claim,
    pub(crate) name: TraceName,
}

/// Where a job compiles: on the request that submits it, or queued for
/// the engine's worker.
#[derive(Debug, EngineState)]
pub(crate) struct Compiler {
    worker: Option<Threaded>,
}

/// The engine's worker and the queue its body serves.
#[derive(Debug, Clone, EngineState)]
struct Threaded {
    worker: Arc<tier::Worker>,
    queue: Arc<Queue>,
}

/// The jobs waiting for the worker, and what its body needs to serve
/// them.
#[derive(Debug, EngineState)]
struct Queue {
    shared: Arc<Shared>,
    held: Mutex<Held>,
    /// Signalled when a job is queued or done, and when the body is asked
    /// to end.
    changed: Condvar,
}

/// What the queue's mutex guards.
#[derive(Debug, Default, EngineState)]
struct Held {
    jobs: VecDeque<Job>,
    /// How many jobs a thread is serving.
    serving: usize,
    /// Whether the body is asked to return (`tier::Parked::end`).
    ending: bool,
}

impl Compiler {
    /// A worker under `jit.compile=thread`, which `force` and `exit-fuzz`
    /// make `inline` (`Policy`); none under `Mode::Off`. Its thread
    /// starts with the first job, which no recording whose request asks
    /// for a trace's end lines submits ([`Submitter::submit`]).
    pub(crate) fn new(shared: &Arc<Shared>) -> Compiler {
        let threaded =
            shared.policy.mode() != Mode::Off && shared.policy.compile() == Compile::Thread;
        let worker = threaded.then(|| {
            let queue = Arc::new(Queue {
                shared: Arc::clone(shared),
                held: Mutex::default(),
                changed: Condvar::new(),
            });
            let worker = tier::Worker::new("php-jit", Arc::clone(&queue) as Arc<dyn Parked>);
            Threaded { worker, queue }
        });
        Compiler { worker }
    }

    /// Where a request of the engine submits its jobs, laid with the
    /// request's `JitRequest`: the engine's worker and its queue, shared.
    pub(crate) fn submitter(&self) -> Submitter {
        Submitter(self.worker.clone())
    }

    /// Every queued job served to completion, those this thread takes
    /// from the queue here, and the worker's under way waited for: what
    /// a test calls before it reads a word.
    #[cfg(test)]
    pub(crate) fn drain(&self) {
        if let Some(worker) = &self.worker {
            worker.queue.drain();
        }
    }
}

/// Where a request's recordings compile (`jit-design.md` §4.8): on the
/// engine's worker, through its queue, under `jit.compile=thread`;
/// otherwise on the request. A request holds its own, laid with it, so
/// a recording that compiles reads no engine state (#3227).
#[derive(Debug, Clone, EngineState)]
pub(crate) struct Submitter(Option<Threaded>);

impl Submitter {
    /// Jobs compiled on the request that submits them.
    #[cfg(test)]
    pub(crate) const INLINE: Submitter = Submitter(None);

    /// `job` through the [`pipeline`]: here, its lines added to `lines`,
    /// and what it settled the claim with, where the request has no
    /// worker or its bits ask for a line the compile writes
    /// ([`policy::JitDebug::on_request`]); or queued for the worker, which
    /// writes no line, and none. Where the system refuses the worker a
    /// thread, the queue is served here.
    pub(crate) fn submit(&self, shared: &Shared, job: Job, lines: &mut Lines) -> Option<Outcome> {
        let threaded = match &self.0 {
            Some(threaded) if !lines.debug().on_request() => threaded,
            _ => return Some(pipeline(shared, job, lines)),
        };
        threaded.queue.held().jobs.push_back(job);
        threaded.queue.changed.notify_all();
        if !threaded.worker.wake() {
            threaded.queue.drain();
        }
        None
    }
}

impl Drop for Compiler {
    /// The queued jobs dropped, each claim settling itself as a failed
    /// attempt; the field's drop then joins the worker, once the job it
    /// serves is done.
    fn drop(&mut self) {
        if let Some(worker) = &self.worker {
            let jobs = std::mem::take(&mut worker.queue.held().jobs);
            drop(jobs);
        }
    }
}

impl Queue {
    /// The queue's mutex, taken as a thread that panicked holding it
    /// left it: nothing it guards is written in two steps.
    fn held(&self) -> MutexGuard<'_, Held> {
        self.held.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// `job` through the pipeline off any request, counted as served
    /// until it is done however it ends, and the pages the compile freed
    /// given back. No line is written: a request whose
    /// `opcache.jit_debug` asks for one compiles inline.
    fn serve(&self, job: Job) {
        let _done = Served(self);
        pipeline(&self.shared, job, &mut Lines::default());
        table::code::give_back_freed_pages();
    }

    /// Every job served on this thread, and those under way elsewhere
    /// waited for.
    fn drain(&self) {
        loop {
            let job = {
                let mut held = self.held();
                loop {
                    if let Some(job) = held.jobs.pop_front() {
                        held.serving += 1;
                        break Some(job);
                    }
                    if held.serving == 0 {
                        break None;
                    }
                    held = self
                        .changed
                        .wait(held)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            };
            match job {
                Some(job) => self.serve(job),
                None => return,
            }
        }
    }
}

/// A job a thread took from the queue, served until this drops.
struct Served<'q>(&'q Queue);

impl Drop for Served<'_> {
    fn drop(&mut self) {
        self.0.held().serving -= 1;
        self.0.changed.notify_all();
    }
}

impl Parked for Queue {
    /// The worker's body: each job as it is queued, and between two of
    /// them, where it is asked to end, a return. A job that panics past
    /// the pipeline's own catch, whose claim its drop settled, ends no
    /// body: the queue behind it is served still.
    fn run(&self) {
        loop {
            let job = {
                let mut held = self.held();
                loop {
                    if held.ending {
                        held.ending = false;
                        return;
                    }
                    if let Some(job) = held.jobs.pop_front() {
                        held.serving += 1;
                        break job;
                    }
                    held = self
                        .changed
                        .wait(held)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            };
            catch_unwind(AssertUnwindSafe(|| self.serve(job))).ok();
        }
    }

    fn end(&self) {
        self.held().ending = true;
        self.changed.notify_all();
    }
}

/// php's `zend_jit_trace_hot_root` after the recording
/// (`Z/ext/opcache/jit/zend_jit_trace.c:8200-8245`) over the engine's
/// `Backend`: a stop that compiles is written as `TRACE n stop`, built,
/// lowered and published in the table of the epoch its claim holds,
/// `TRACE n compiled`; any other stop, and any stage that fails, is an
/// attempt failed ([`failed`]), written as `TRACE n abort` and, where it
/// blacklisted the word, `TRACE n blacklisted`, each added to `lines`. A
/// stage that panics is caught here and is php's `COMPILER_ERROR`, as
/// php reports a compile that failed. The claim is settled once, here.
pub(crate) fn pipeline(shared: &Shared, job: Job, lines: &mut Lines) -> Outcome {
    match &shared.backend {
        Some(backend) => pipeline_with(
            shared,
            |ir, name, linked, escapes| backend.compile(ir, name, linked, escapes),
            job,
            lines,
        ),
        None => pipeline_with(shared, |_, _, _, _| Err(no_backend()), job, lines),
    }
}

/// [`pipeline`] over the `Backend` that `compile` stands for: the seam its
/// tests drive each stage's failure through.
fn pipeline_with(
    shared: &Shared,
    compile: impl Fn(&TraceIr, TraceName, Option<Linked>, Option<Escapes>) -> Result<Compiled, Abort>,
    job: Job,
    lines: &mut Lines,
) -> Outcome {
    let Job {
        recorded,
        claim,
        name,
    } = job;
    let stop = recorded.record.stop;
    let table = table::of_epoch(claim.epoch());
    let number = table.and_then(Table::next);
    let outcome = match table {
        Some(table) if stop.compiles() => {
            if let Some(trace) = number {
                lines.dump(&TraceEvent::Stop { trace, stop });
            }
            let start = claim.start();
            catch_unwind(AssertUnwindSafe(|| {
                // `zend_jit_trace_build_tssa` analyses the op arrays first.
                let opt_level = shared.policy.start_setting().opt_level;
                let (record, view) = (&recorded.record, &recorded.view);
                let Some(analysed) =
                    crate::analysis::build(record, view, table.analyses(), opt_level, lines)
                else {
                    return Outcome::Abort(Stop::CompilerError);
                };
                published(
                    &recorded,
                    &analysed,
                    name,
                    &compile,
                    table,
                    claim.epoch(),
                    start,
                )
            }))
            .unwrap_or(Outcome::Abort(Stop::CompilerError))
        }
        Some(_) => failed(stop),
        None => failed(Stop::TooManyTraces),
    };
    if matches!(outcome, Outcome::Published(_)) && shared.policy.threshold(Start::Return).is_some()
    {
        arm_landings(&recorded, claim.epoch(), &shared.policy);
    }
    concluded(claim, outcome, number, lines)
}

/// [`pipeline`] for a recording refused with `stop` before its first
/// opline, which never compiles: an attempt failed, settled and written
/// as the pipeline settles and writes one. It has no record to build,
/// since the frame it was to start in may serve no op array the engine
/// numbered, which a record roots in.
pub(crate) fn refused(claim: Claim, stop: Stop, lines: &mut Lines) -> Outcome {
    let number = table::of_epoch(claim.epoch()).map(Table::next);
    let outcome = match number {
        Some(_) => failed(stop),
        None => failed(Stop::TooManyTraces),
    };
    concluded(claim, outcome, number.flatten(), lines)
}

/// The claim settled with `outcome`, and the attempt written as trace
/// `number`: `TRACE n compiled`, or `TRACE n abort` and, where it
/// blacklisted the word, `TRACE n blacklisted`.
fn concluded(
    claim: Claim,
    outcome: Outcome,
    number: Option<TraceId>,
    lines: &mut Lines,
) -> Outcome {
    let start = claim.start();
    let settled = policy::settle(claim, &outcome);
    match (outcome, number) {
        (Outcome::Published(trace), _) => lines.dump(&TraceEvent::Compiled { trace }),
        (Outcome::Abort(stop) | Outcome::Blacklist(stop), Some(trace)) => {
            lines.dump(&TraceEvent::Abort { trace, stop });
            // A side's exit is what php blacklists, by its parent's number.
            let blacklisted = match start.grew() {
                Some((parent, exit)) => TraceEvent::ExitBlacklisted {
                    trace: parent,
                    exit,
                },
                None => TraceEvent::Blacklisted { trace },
            };
            if settled == State::Blacklisted {
                lines.dump(&blacklisted);
            }
        }
        _ => {}
    }
    outcome
}

/// The landings a published trace hands on to the interpreter at
/// (`record::landings`) armed in `epoch`, as php's code generator arms
/// them once the trace's code is generated, where
/// `opcache.jit_hot_return` is not zero
/// (`zend_jit_trace_setup_ret_counter`, `Z/ext/opcache/jit/zend_jit_trace.c:3404-3418`;
/// `jit-design.md` §2.2, #3036).
fn arm_landings(recorded: &Recorded, epoch: &Epoch, policy: &Policy) {
    for (func, at) in record::landings(&recorded.record, &recorded.view, policy.folds()) {
        if let Some(word) = epoch.lay_landing(func, at) {
            policy::arm(word, policy);
        }
    }
}

/// The pipeline's middle: the record built through its view, the trace
/// compiled under `name`, its link a jump to the trace it links to where
/// `table` holds that trace's code (`jit-design.md` §4.3) and its escape
/// read through the landing index of `epoch`, the table's (§4.9), and the
/// code published into `table`, or the failure of the first stage that
/// failed.
fn published(
    recorded: &Recorded,
    analysed: &crate::analysis::Analysed,
    name: TraceName,
    compile: &impl Fn(&TraceIr, TraceName, Option<Linked>, Option<Escapes>) -> Result<Compiled, Abort>,
    table: &Table,
    epoch: &Epoch,
    start: Start,
) -> Outcome {
    let compiled = types::build(&recorded.record, &recorded.view, analysed).and_then(|ir| {
        let link = match ir.end {
            End::Leave(end) => ir.exits.get(usize::from(end.0)).and_then(|exit| exit.link),
            End::Loop(_) | End::Return(_) => None,
        };
        let linked = link.and_then(|link| table.linked(link, start));
        compile(&ir, name, linked, Some(table.escapes(epoch)))
    });
    match compiled.map(|compiled| table.publish(compiled, start)) {
        Ok(Ok(trace)) => Outcome::Published(trace),
        Err(abort) => failed(abort.stop),
        Ok(Err(Unpublished::Full(full))) => failed(full.stop()),
        Ok(Err(Unpublished::Malformed(_))) => failed(Stop::CompilerError),
    }
}

/// What a pipeline with no `Backend` compiles to: nothing, php's
/// `COMPILER_ERROR`. No engine runs one, since a host with no `Backend`
/// runs the tier `Off` (`jit-design.md` §4).
fn no_backend() -> Abort {
    Abort {
        stop: Stop::CompilerError,
        why: "no backend",
    }
}

/// An attempt failed with `stop`: one counted toward the word's
/// blacklist where php recovers from it, a full arena's `COMPILER_ERROR`
/// among them (#3015's ruling), and the word blacklisted where it does
/// not.
fn failed(stop: Stop) -> Outcome {
    if stop.recovers() {
        Outcome::Abort(stop)
    } else {
        Outcome::Blacklist(stop)
    }
}

//! What becomes of a recording (`jit-design.md` §4.8), so far the
//! pipeline's ends without its middle: no `Backend` lowers a trace yet
//! (#2955), so [`submit`] runs on the request, writes the stop php
//! writes, numbers a trace that would compile as published in the
//! epoch's table (`TraceTable::number`), and settles the claim, so the
//! tier records and dumps under `jit.mode=force` and
//! `jit.compile=inline` as php does under `opcache.jit_debug`.
//!
//! A numbered trace has no code: its word reads `compiled` and enters
//! nothing, since the table's `get` answers none for it, which every
//! reader takes as "not compiled" (`jit-design.md` §3.2, §4.9), and a
//! later recording that reaches it links to it by that number, as php's
//! does. #2958 replaces the middle of [`pipeline`], `types::build`,
//! `Backend::compile` and `TraceTable::publish` of the `Compiled`, on one
//! worker or inline, and keeps its two ends and this one representation
//! of a job.

use crate::Shared;
use crate::policy::{self, Claim, Outcome, State, TraceEvent};
use crate::record::{Recorded, Stop};
use crate::table::{self, Unpublished};

/// A recording and the claim on the word it grew from: what the pipeline
/// takes, on the request or off it.
#[derive(Debug)]
pub(crate) struct Job {
    pub(crate) recorded: Recorded,
    pub(crate) claim: Claim,
}

/// The job through the pipeline on the calling thread, which is every
/// job until a worker exists: what it settled the claim with, its lines
/// added to `lines`.
pub(crate) fn submit(shared: &Shared, job: Job, lines: &mut Vec<u8>) -> Outcome {
    pipeline(shared, job, lines)
}

/// php's `zend_jit_trace_hot_root` after the recording
/// (`Z/ext/opcache/jit/zend_jit_trace.c:8200-8245`): a stop that compiles
/// is written as `TRACE n stop` and the trace numbered in the table of
/// the epoch its claim holds, or blacklisted with the stop of the bound
/// the table met; any other is an attempt failed, written as
/// `TRACE n abort` and, where it blacklisted the word,
/// `TRACE n blacklisted`, each added to `lines`. The claim is settled
/// once, here.
pub(crate) fn pipeline(shared: &Shared, job: Job, lines: &mut Vec<u8>) -> Outcome {
    let Job { recorded, claim } = job;
    let stop = recorded.record.stop;
    let table = table::of_epoch(claim.epoch());
    let number = table.and_then(table::TraceTable::next);
    let outcome = if stop.compiles() {
        if let Some(trace) = number {
            shared.dump(lines, &TraceEvent::Stop { trace, stop });
        }
        match table.map(|table| table.number(claim.start())) {
            Some(Ok(trace)) => Outcome::Published(trace),
            Some(Err(Unpublished::Full(full))) => Outcome::Blacklist(full.stop()),
            Some(Err(Unpublished::Malformed(_))) => Outcome::Abort(Stop::CompilerError),
            None => Outcome::Blacklist(Stop::TooManyTraces),
        }
    } else if stop.recovers() {
        Outcome::Abort(stop)
    } else {
        Outcome::Blacklist(stop)
    };
    let settled = policy::settle(claim, &outcome);
    match (outcome, number) {
        (Outcome::Published(trace), _) => shared.dump(lines, &TraceEvent::Compiled { trace }),
        (Outcome::Abort(stop) | Outcome::Blacklist(stop), Some(trace)) => {
            shared.dump(lines, &TraceEvent::Abort { trace, stop });
            if settled == State::Blacklisted {
                shared.dump(lines, &TraceEvent::Blacklisted { trace });
            }
        }
        _ => {}
    }
    outcome
}

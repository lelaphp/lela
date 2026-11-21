//! `xtask soak`: the reading that stakes design §5.14 on a
//! deployment. One makala, at its default request threads, serves the
//! corpus to many clients at once for a fixed time, and each response is
//! held to the same request served alone. The design promises one engine
//! serving requests on any number of threads, each request dropped whole;
//! a defect that crosses from one request to another shows here as a
//! response that is not the one it is alone, and one that keeps what a
//! request should have let go shows as a leak finding or as growth.
//!
//! **What it counts.** The requests served, each one a response came for;
//! the mismatches, a response
//! that differs from the request's alone; the failures, a request that
//! got no response before the recipe's timeout; the request leak sensor's
//! findings, each an `InternalError: the request ended with ...` the
//! sensor ends a request in, which makala's error log takes under
//! `log_errors=1`, and the panics makala answered 500 and logged; and the
//! resident memory of the process, sampled each second through `ps`, where
//! the clients start, at its peak over the first half, at its peak over
//! the whole and where they end: the first half's peak is what many
//! requests at once take, the allocator keeping what it was handed, and
//! the whole's above it is how far that kept growing after. The
//! growth is stated and judged by nothing, since no bound is ruled.
//!
//! **What alone is.** Before the clients start, each entry is served twice
//! with nothing else in flight. An entry whose two answers differ moves
//! run to run by itself (a time, a temporary file's name) and no
//! comparison holds it, so it is named and sent by no client; an entry
//! makala did not answer alone is a bork. An entry with an `--INI--` of its
//! own is left out, since one process serves every request under one ini.
//!
//! **Its length and lane** are as ruled 2026-10-09: [`NIGHTLY`] and
//! [`CLIENTS`], nightly on CI's Linux runner (`nightly.yml`'s `soak` job, which
//! asks for it with `--nightly`, a flag `lanes.nightly-soak` denies outside
//! CI); a bare call is the smoke's [`SMOKE`], as makala's own test runs it
//! (ruled 2026-10-10). `--seconds` and `--clients` take another run over
//! either. The ruling's "debug build" is read as a build with debug
//! assertions, since those are what arm the leak sensor
//! (`cfg(debug_assertions)`): the lanes' `release-da`, as every binary the
//! harness serves through is built, not `dev`.

use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};

use super::{Entry, SCOPE};
use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::oracle::{Promise, diff};
use crate::reading::{Number, Reading, Status};
use crate::scratch::Tree;
use crate::witness::makala::{Server, from_http};

/// The instrument's name, the reading's `instrument` field.
pub(crate) const INSTRUMENT: &str = "soak";

/// How long the nightly lane's clients send, as ruled 2026-10-09: ten
/// minutes.
pub(crate) const NIGHTLY: u64 = 600;

/// How long a bare call's clients send: the smoke's 30 seconds, as ruled
/// 2026-10-09 for `sapi/makala/tests/soak.rs` and 2026-10-10 for the
/// default.
pub(crate) const SMOKE: u64 = 30;

/// How many clients send at once, as ruled 2026-10-09.
pub(crate) const CLIENTS: usize = 64;

/// How many mismatches the message names, the rest counted.
const NAMED: usize = 8;

/// What the request leak sensor ends a request in (`php_heap`'s `Leak`).
const LEAK: &str = "InternalError: the request ended with";

/// What makala logs for a request whose thread panicked.
const PANIC: &str = "\" panicked: ";

/// How long the clients send: `seconds` where named, else the nightly
/// lane's length under `--nightly` and the smoke's otherwise.
pub(crate) fn seconds(nightly: bool, seconds: Option<u64>) -> u64 {
    seconds.unwrap_or(if nightly { NIGHTLY } else { SMOKE })
}

/// What the clients found while they sent.
#[derive(Debug, Default)]
struct Found {
    served: AtomicU64,
    mismatches: AtomicU64,
    failed: AtomicU64,
    named: Mutex<Vec<String>>,
}

impl Found {
    /// One mismatch or failure, named while fewer than [`NAMED`] are.
    fn name(&self, what: String) {
        let mut named = crate::locked(&self.named);
        if named.len() < NAMED {
            named.push(what);
        }
    }
}

/// `xtask soak`: `clients` clients for `seconds` over the corpus; one
/// reading, a skip where this machine has no pinned php-src.
pub(crate) fn run(
    root: &Path,
    manifest: &Manifest,
    seconds: u64,
    clients: usize,
) -> Result<Reading> {
    let snapshot = Snapshot::disk(root)?;
    let krate = (manifest.crate_named(SCOPE)).ok_or_else(|| anyhow!("no [[crate]] {SCOPE}"))?;
    let mut reading = Reading::for_crate(INSTRUMENT, krate, &snapshot)?;
    let Some(tree) = Promise::Tree.or_skip(manifest.pin()?.source.tree())? else {
        reading.message =
            format!("{INSTRUMENT}: this machine has no pinned php-src, so nothing was sent");
        return Ok(reading);
    };
    let (subset, mut left) = super::subset(&tree)?;
    // A section the soak does not run decides whether a test applies.
    let (gated, subset): (Vec<Entry>, Vec<Entry>) =
        subset.into_iter().partition(|entry| entry.skipif.is_some());
    left.extend(gated.into_iter().map(|entry| entry.id));
    let with_ini = subset.iter().filter(|entry| !entry.ini.is_empty()).count();
    let mut entries: Vec<Entry> = subset.into_iter().filter(|e| e.ini.is_empty()).collect();
    entries.extend(super::workloads(root)?);
    let host = super::makala(root)?;
    reading.judged(&host);
    let recipe = &manifest.pin()?.recipe;
    let own = Tree::new("soak")?;
    let names: Vec<String> = (0..entries.len()).map(|n| format!("{n}.php")).collect();
    for (entry, name) in entries.iter().zip(&names) {
        own.write(name, &entry.program)?;
    }
    let log = own.join("errors");
    let timeout = Duration::from_secs(recipe.limits.timeout_s);
    let ini = ["log_errors=1".to_owned(), past_patience(timeout)];
    let server = Server::start(recipe, &host.path, own.path(), &ini, None, &log, &[])?;
    let ask = |n: usize| -> Result<Vec<u8>> {
        let response = server.ask(&entries[n].http, &names[n], timeout)?.0;
        Ok(entries[n].randoms_named_alike(&from_http(&response)))
    };
    let Alone {
        held: alone,
        unstable,
        borked,
    } = served_alone(&entries, &ask);
    let found = Found::default();
    let memory = sent(&found, &alone, clients, seconds, server.pid(), |n| {
        (ask(n), entries[n].id.as_str())
    });
    drop(server);
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    let leaks = logged.lines().filter(|line| line.contains(LEAK)).count();
    let panics = logged.lines().filter(|line| line.contains(PANIC)).count();
    let numbers = [
        ("seconds", seconds),
        ("clients", clients as u64),
        ("entries", entries.len() as u64),
        ("left_out", left.len() as u64),
        ("with_ini", with_ini as u64),
        ("unstable", unstable.len() as u64),
        ("borked", borked.len() as u64),
        ("served", found.served.load(Ordering::Relaxed)),
        ("mismatches", found.mismatches.load(Ordering::Relaxed)),
        ("failed", found.failed.load(Ordering::Relaxed)),
        ("leaks", leaks as u64),
        ("panics", panics as u64),
        ("rss_start_kb", memory.start),
        ("rss_half_kb", memory.half),
        ("rss_peak_kb", memory.peak),
        ("rss_end_kb", memory.end),
    ];
    for (key, n) in numbers {
        reading.count(key, n)?;
    }
    let mismatched = (found.named.into_inner()).unwrap_or_else(PoisonError::into_inner);
    judged(&mut reading, &mismatched, &unstable, &borked);
    Ok(reading)
}

/// The `max_execution_time` every request is served under: twice the
/// client's patience, `timeout`. php's deadline is wall-clock time where
/// the oracle runs (`ITIMER_REAL` on Apple silicon,
/// `Z/Zend/zend_execute_API.c:1611-1614`), as the engine's is, so under a
/// shared machine's load a kernel of the bench workloads ran past the
/// default 30 seconds among 64 clients and its body was php's fatal: a
/// mismatch made by the load and by no request beside it. Past
/// the client's patience the timer is still armed and disarmed on every
/// request, and a request slower than that is a failure the client
/// counts, never a body that differs.
fn past_patience(timeout: Duration) -> String {
    format!("max_execution_time={}", timeout.as_secs() * 2)
}

/// What each entry served alone came to.
struct Alone {
    /// The answer each stable entry is held to, by its number.
    held: Vec<(usize, Vec<u8>)>,
    /// The entries whose two answers differ.
    unstable: Vec<String>,
    /// The entries not answered, each with why.
    borked: Vec<String>,
}

/// Each entry served twice with nothing else in flight.
fn served_alone(entries: &[Entry], ask: &impl Fn(usize) -> Result<Vec<u8>>) -> Alone {
    let (mut alone, mut unstable, mut borked) = (Vec::new(), Vec::new(), Vec::new());
    for (n, entry) in entries.iter().enumerate() {
        match (ask(n), ask(n)) {
            (Ok(first), Ok(second)) if first == second => alone.push((n, first)),
            (Ok(_), Ok(_)) => unstable.push(entry.id.clone()),
            (Err(why), _) | (_, Err(why)) => borked.push(format!("{} ({why:#})", entry.id)),
        }
    }
    Alone {
        held: alone,
        unstable,
        borked,
    }
}

/// The resident memory of the server, in KiB, over the clients' time.
struct Memory {
    start: u64,
    /// The peak over the first half of the clients' time.
    half: u64,
    peak: u64,
    end: u64,
}

/// `clients` clients sending the `alone` entries in turn for `seconds`,
/// each answer `ask` gives held to the entry's alone, into `found`; and
/// the memory of the process `pid`, sampled each second.
fn sent<'a>(
    found: &Found,
    alone: &[(usize, Vec<u8>)],
    clients: usize,
    seconds: u64,
    pid: u32,
    ask: impl Fn(usize) -> (Result<Vec<u8>>, &'a str) + Sync,
) -> Memory {
    let start = rss(pid);
    let (mut peak, mut half) = (start, start);
    let started = Instant::now();
    let deadline = started + Duration::from_secs(seconds);
    std::thread::scope(|scope| {
        for client in 0..clients {
            let ask = &ask;
            scope.spawn(move || {
                let mut turn = client;
                while !alone.is_empty() && Instant::now() < deadline {
                    let (n, wanted) = &alone[turn % alone.len()];
                    turn += clients;
                    match ask(*n) {
                        (Ok(got), _) if got == *wanted => {
                            found.served.fetch_add(1, Ordering::Relaxed);
                        }
                        (Ok(got), id) => {
                            found.served.fetch_add(1, Ordering::Relaxed);
                            found.mismatches.fetch_add(1, Ordering::Relaxed);
                            let line = super::compare::first_line(&diff(wanted, &got));
                            found.name(format!("{id} ({line})"));
                        }
                        (Err(why), id) => {
                            found.failed.fetch_add(1, Ordering::Relaxed);
                            found.name(format!("{id} ({why:#})"));
                        }
                    }
                }
            });
        }
        while Instant::now() < deadline {
            std::thread::sleep(Duration::from_secs(1));
            let now = rss(pid);
            peak = peak.max(now);
            if started.elapsed().as_secs() <= seconds / 2 {
                half = half.max(now);
            }
        }
    });
    Memory {
        start,
        half,
        peak,
        end: rss(pid),
    }
}

/// The status and message from the numbers already counted: a pass where
/// something was served and nothing mismatched, failed, leaked, panicked
/// or borked.
fn judged(reading: &mut Reading, named: &[String], unstable: &[String], borked: &[String]) {
    let n = |key: &str| match reading.numbers.get(key) {
        Some(Number::Int(n)) => *n,
        _ => 0,
    };
    let defects = ["mismatches", "failed", "leaks", "panics", "borked"];
    reading.status = if n("served") > 0 && defects.iter().all(|key| n(key) == 0) {
        Status::Pass
    } else {
        Status::Fail
    };
    let mut message = format!(
        "{} requests served by {} clients in {} s over {} entries: {} mismatches, {} failed, {} leak findings, {} panics; resident memory {} KiB where the clients started, {} KiB at its peak over the first half, {} KiB over the whole, {} KiB where they ended ({:+} KiB; the peak {:+} KiB over the second half)",
        n("served"),
        n("clients"),
        n("seconds"),
        n("entries"),
        n("mismatches"),
        n("failed"),
        n("leaks"),
        n("panics"),
        n("rss_start_kb"),
        n("rss_half_kb"),
        n("rss_peak_kb"),
        n("rss_end_kb"),
        n("rss_end_kb") - n("rss_start_kb"),
        n("rss_peak_kb") - n("rss_half_kb"),
    );
    for (what, list) in [
        ("mismatched", named),
        ("borked alone", borked),
        ("unstable alone", unstable),
    ] {
        if !list.is_empty() {
            write!(message, "; {what}: {}", list.join("; ")).ok();
        }
    }
    reading.message = message;
}

/// The resident memory of the process `pid` in KiB, as `ps -o rss=`
/// reports it; 0 where it reports nothing.
fn rss(pid: u32) -> u64 {
    let ran = Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output();
    ran.ok()
        .and_then(|out| String::from_utf8_lossy(&out.stdout).trim().parse().ok())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bare call is the smoke's 30 seconds and `--nightly` the nightly
    /// lane's ten minutes, both with the nightly client count, and
    /// `--seconds` stands over either.
    #[test]
    fn a_bare_call_is_the_smoke_and_nightly_the_nightly_length() {
        use clap::Parser;
        let asked = |args: &[&str]| match crate::cli::Cli::try_parse_from(args).unwrap().command {
            crate::cli::Command::Soak {
                nightly,
                seconds,
                clients,
            } => (super::seconds(nightly, seconds), clients),
            other => panic!("{other:?} is no soak"),
        };
        assert_eq!(asked(&["xtask", "soak"]), (30, 64));
        assert_eq!(asked(&["xtask", "soak", "--nightly"]), (600, 64));
        assert_eq!(
            asked(&["xtask", "soak", "--nightly", "--seconds", "45"]).0,
            45
        );
        assert_eq!(asked(&["xtask", "soak", "--clients", "8"]), (30, 8));
    }

    /// A run that served and found nothing passes and states its memory;
    /// one mismatch, leak or bork fails it, and is named.
    #[test]
    fn any_defect_fails_the_soak_and_the_memory_is_stated() {
        let reading = |pairs: &[(&str, u64)]| {
            let mut reading = Reading::new(INSTRUMENT, SCOPE);
            for (key, n) in pairs {
                reading.count(key, *n).unwrap();
            }
            reading
        };
        let mut clean = reading(&[
            ("served", 10),
            ("rss_start_kb", 100),
            ("rss_half_kb", 80),
            ("rss_peak_kb", 95),
            ("rss_end_kb", 90),
        ]);
        judged(&mut clean, &[], &["x.phpt".to_owned()], &[]);
        assert_eq!(clean.status, Status::Pass);
        let stated = "(-10 KiB; the peak +15 KiB over the second half)";
        assert!(clean.message.contains(stated), "{}", clean.message);
        assert!(clean.message.contains("unstable alone: x.phpt"));
        for defect in ["mismatches", "failed", "leaks", "panics", "borked"] {
            let mut bad = reading(&[("served", 10), (defect, 1)]);
            judged(&mut bad, &["a.phpt (-x / +y)".to_owned()], &[], &[]);
            assert_eq!(bad.status, Status::Fail, "{defect}");
            assert!(bad.message.contains("mismatched: a.phpt (-x / +y)"));
        }
        let mut idle = reading(&[]);
        judged(&mut idle, &[], &[], &[]);
        assert_eq!(idle.status, Status::Fail, "nothing served is no pass");
    }

    /// Every request is served under a deadline past the client's
    /// patience, so the load reaches the client's timeout first.
    #[test]
    fn the_deadline_stands_past_the_client_s_patience() {
        assert_eq!(
            past_patience(Duration::from_mins(1)),
            "max_execution_time=120"
        );
    }

    /// The process's own resident memory reads as a number of KiB.
    #[test]
    fn the_resident_memory_of_a_process_is_read() {
        assert!(rss(std::process::id()) > 0);
    }
}

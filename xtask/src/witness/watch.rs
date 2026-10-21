//! A witness run served in a process of its own and watched from this one
//! (#976, #1037, #1043, #1060). The lane serves every snippet through the
//! engine linked into the harness, on every core at once, so what the
//! engine or a C library it calls does wrong can end the process on a
//! signal: an abort, a trap, a fault. A process that dies that way writes
//! nothing, neither a line on either stream nor a reading, and `done` and
//! a person are left with a shell's `Abort trap: 6` and a store that says
//! nothing happened.
//!
//! So `xtask witness` does not serve the snippets itself. It resolves the
//! crates the run is for, starts this same binary over them with
//! `--journal`, and waits. The served process judges as the lane always
//! has, prints its verdicts on the streams it was given, and appends to
//! the journal, one JSON line at a time: each snippet as it starts and as
//! it ends, and each crate's reading once judged. It records nothing; the
//! watching process is the one writer of the store. When the served
//! process exits, its readings are recorded as any instrument's are. When
//! it dies on a signal, every crate it was for and did not send a reading
//! for gets a failing one naming the signal and the snippets being served
//! when it came, and the command exits non-zero with a line saying so.
//!
//! What a journal line carries is the snippet's place and not its text: a
//! signal says what was running, which with memory already damaged is not
//! always what damaged it, so the reading names what was in flight and
//! claims no more.
//!
//! The journal is the run's claim on System V IPC ([`sysv::Claim`],
//! #2115): a fence that makes a segment, a semaphore set or a queue and
//! is cut off before it removes it, and php's own `msg_get_queue(0)`,
//! which leaves one nobody can use, leave objects the kernel's small
//! tables keep after every process is gone. When the served process ends,
//! however it ends, what was made while it ran and nothing holds is
//! removed, the run's or another of the runner's that left it so; a run
//! whose watching process was killed too leaves its claim, and the next
//! run removes what it left before it starts. Each reading of the run
//! counts what was removed as `sysv_removed` and its message names it.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::{Ask, INSTRUMENT, coverage_rule, fence::Witness, under_rule};
use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::oracle::Ended;
use crate::reading::{Reading, Status, Store};
use crate::sysv;

/// One line of the journal.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "entry", rename_all = "snake_case")]
enum Entry {
    /// A snippet of `krate` handed to the engine, by where its fence
    /// stands.
    Serving { krate: String, at: String },
    /// The same snippet judged.
    Served { krate: String, at: String },
    /// A crate's reading, as the log carries one.
    Judged { reading: serde_json::Value },
}

/// The served process's side: the file it appends to. Every entry is one
/// `write` of one line to a file opened for appending, so the pool's
/// threads share it without a lock and a process that dies between two
/// entries leaves every earlier one whole.
#[derive(Debug)]
pub(crate) struct Journal {
    file: File,
}

impl Journal {
    pub(crate) fn open(path: &Path) -> Result<Journal> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening the journal {}", path.display()))?;
        Ok(Journal { file })
    }

    fn append(&self, entry: &Entry) -> Result<()> {
        let mut line = serde_json::to_string(entry).context("rendering a journal entry")?;
        line.push('\n');
        (&self.file)
            .write_all(line.as_bytes())
            .context("appending to the journal")
    }

    /// The watch over one crate's snippets.
    pub(crate) fn over<'j>(&'j self, krate: &'j str) -> Watch<'j> {
        Watch {
            journal: self,
            krate,
        }
    }

    pub(crate) fn judged(&self, reading: &Reading) -> Result<()> {
        self.append(&Entry::Judged {
            reading: reading.to_json()?,
        })
    }
}

/// The journal as one crate's snippets are served.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Watch<'j> {
    journal: &'j Journal,
    krate: &'j str,
}

impl Watch<'_> {
    pub(crate) fn serving(&self, witness: &Witness) -> Result<()> {
        self.journal.append(&Entry::Serving {
            krate: self.krate.to_owned(),
            at: witness.at(),
        })
    }

    pub(crate) fn served(&self, witness: &Witness) -> Result<()> {
        self.journal.append(&Entry::Served {
            krate: self.krate.to_owned(),
            at: witness.at(),
        })
    }
}

/// What the journal held when the served process ended: the readings it
/// sent, and by crate the snippets it started and the ones of them it had
/// not finished.
#[derive(Debug, Default)]
struct Kept {
    readings: Vec<Reading>,
    started: BTreeMap<String, usize>,
    in_flight: BTreeMap<String, Vec<String>>,
}

impl Kept {
    /// Every whole line of the journal; a line the death cut short is the
    /// last one and is passed over.
    fn read(path: &Path) -> Result<Kept> {
        let text = crate::read_if(path)
            .context("reading the journal")?
            .unwrap_or_default();
        let mut kept = Kept::default();
        for line in text.lines() {
            let Ok(entry) = serde_json::from_str::<Entry>(line) else {
                continue;
            };
            match entry {
                Entry::Serving { krate, at } => {
                    *kept.started.entry(krate.clone()).or_default() += 1;
                    kept.in_flight.entry(krate).or_default().push(at);
                }
                Entry::Served { krate, at } => {
                    if let Some(running) = kept.in_flight.get_mut(&krate) {
                        running.retain(|a| *a != at);
                    }
                }
                Entry::Judged { reading } => {
                    kept.readings
                        .push(Reading::from_json(&reading.to_string())?);
                }
            }
        }
        Ok(kept)
    }
}

/// A signal by its number, and by its name where the platform's is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Signal(pub i32);

impl std::fmt::Display for Signal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.name() {
            Some(name) => write!(f, "{name} (signal {})", self.0),
            None => write!(f, "signal {}", self.0),
        }
    }
}

impl Signal {
    /// The names both platforms the oracle runs on number alike, and
    /// `SIGBUS`, which they number apart.
    fn name(self) -> Option<&'static str> {
        Some(match self.0 {
            1 => "SIGHUP",
            2 => "SIGINT",
            3 => "SIGQUIT",
            4 => "SIGILL",
            5 => "SIGTRAP",
            6 => "SIGABRT",
            8 => "SIGFPE",
            9 => "SIGKILL",
            11 => "SIGSEGV",
            13 => "SIGPIPE",
            14 => "SIGALRM",
            15 => "SIGTERM",
            #[cfg(target_os = "macos")]
            10 => "SIGBUS",
            #[cfg(target_os = "linux")]
            7 => "SIGBUS",
            _ => return None,
        })
    }
}

/// The crates a run is for, by manifest name: the ones asked for, or with
/// `--staged` the ones with a staged change less those whose reading
/// stands for what is staged (#1143), or every crate the manifest names.
/// `None` is a staged run with nothing left to measure.
fn planned(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    ask: &Ask,
) -> Result<Option<Vec<String>>> {
    manifest.crates_named(&ask.crates)?;
    if ask.staged {
        if !ask.crates.is_empty() {
            bail!("witness: --staged names its own crates; drop --crate");
        }
        let measured = crate::sources::for_commit(root, manifest, store, INSTRUMENT)?;
        return Ok((!measured.is_empty()).then_some(measured));
    }
    Ok(Some(if ask.crates.is_empty() {
        manifest.crates.iter().map(|c| c.name.clone()).collect()
    } else {
        ask.crates.clone()
    }))
}

/// The command line's entry without `--journal`: the run served by this
/// binary in a process of its own over the crates it is for, watched, and
/// its readings recorded here, a failing one for every crate a death on a
/// signal left without its own.
pub(crate) fn watch(root: &Path, manifest: &Manifest, store: &Store, ask: &Ask) -> Result<()> {
    let Some(crates) = planned(root, manifest, store, ask)? else {
        return Ok(());
    };
    // The tree before the served run starts: what a crate it died before
    // sending a reading for is stamped with.
    let snapshot = Snapshot::disk(root)?;
    let grace = Duration::from_secs(manifest.pin.recipe.limits.timeout_s);
    let mut removed = sysv::Removed::new(INSTRUMENT);
    let claim = sysv::Claim::taken(INSTRUMENT, grace, &mut removed)?;
    let journal: PathBuf = claim.path().to_path_buf();
    let exe = std::env::current_exe().context("locating this binary")?;
    let mut served = Command::new(&exe);
    served.arg("--root").arg(root).arg(INSTRUMENT);
    for name in &crates {
        served.arg("--crate").arg(name);
    }
    served.args(ask.accept.args());
    served.args(ask.scope.args());
    served.arg("--journal").arg(&journal);
    // The served run is this one's child, seated with it (#2982).
    crate::seat::lend(&mut served);
    let status = served
        .status()
        .with_context(|| format!("starting {} witness", exe.display()))?;
    let kept = Kept::read(&journal)?;
    let mut readings = kept.readings.clone();
    let ended = Ended::of(status);
    if let Ended::Signal(number) = ended {
        let signal = Signal(number);
        readings.extend(died(&snapshot, manifest, &crates, &kept, signal)?);
        eprintln!(
            "witness: the served run died on {signal}; {}",
            in_flight(&kept)
        );
    }
    removed.add("of what was left while it ran", claim.settle());
    if removed.count > 0 {
        for reading in &mut readings {
            reading.count(sysv::REMOVED, removed.count)?;
            reading.message = format!("{}; {}", reading.message, removed.words.join("; "));
        }
    }
    ask.scope.record(store, &readings)?;
    match ended {
        Ended::Status(code) if code != 0 => {
            bail!("witness: the served run ended with exit {code}; the line above it says why")
        }
        _ => Ok(()),
    }
}

/// The snippets in flight when the served process died, as a clause.
fn in_flight(kept: &Kept) -> String {
    let running: Vec<String> = kept
        .in_flight
        .iter()
        .flat_map(|(krate, ats)| ats.iter().map(move |at| format!("{krate} at {at}")))
        .collect();
    if running.is_empty() {
        "no snippet was being served".to_owned()
    } else {
        format!("being served: {}", running.join(", "))
    }
}

/// A failing reading for every crate of the run that sent none: what died,
/// on which signal, and what of the crate was being served.
fn died(
    snapshot: &Snapshot,
    manifest: &Manifest,
    crates: &[String],
    kept: &Kept,
    signal: Signal,
) -> Result<Vec<Reading>> {
    let rule = coverage_rule(manifest);
    let mut readings = Vec::new();
    for krate in &manifest.crates {
        if !crates.contains(&krate.name) || kept.readings.iter().any(|r| r.scope == krate.name) {
            continue;
        }
        let name = &krate.name;
        let running = kept
            .in_flight
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let started = kept.started.get(name).copied().unwrap_or_default();
        let when = if !running.is_empty() {
            format!("while serving {}", running.join(", "))
        } else if started > 0 {
            format!("after serving {started} of its snippets, with none in flight")
        } else {
            "before it reached this crate".to_owned()
        };
        let mut reading = under_rule(krate, snapshot, rule)?.under(&manifest.pin);
        reading.status = Status::Fail;
        reading.message = format!(
            "{name}: the witness run died on {signal} {when}; nothing it judged of this crate was written, and this reading stands for the run"
        );
        reading.count("signal", u64::try_from(signal.0).unwrap_or_default())?;
        reading.count("in_flight", running.len() as u64)?;
        readings.push(reading);
    }
    Ok(readings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::Tree;

    #[test]
    fn a_signal_is_named_where_both_platforms_agree_and_numbered_where_not() {
        assert_eq!(Signal(6).to_string(), "SIGABRT (signal 6)");
        assert_eq!(Signal(5).to_string(), "SIGTRAP (signal 5)");
        assert_eq!(Signal(64).to_string(), "signal 64");
    }

    #[test]
    fn the_journal_keeps_what_was_in_flight_and_passes_over_a_line_cut_short() {
        let tree = Tree::new("witness-journal-test").unwrap();
        let path = tree.join("journal.jsonl");
        let journal = Journal::open(&path).unwrap();
        let entry = |krate: &str, at: &str, done: bool| {
            let (krate, at) = (krate.to_owned(), at.to_owned());
            if done {
                Entry::Served { krate, at }
            } else {
                Entry::Serving { krate, at }
            }
        };
        journal
            .append(&entry("ext/a", "src/lib.rs:1", false))
            .unwrap();
        journal
            .append(&entry("ext/a", "src/lib.rs:9", false))
            .unwrap();
        journal
            .append(&entry("ext/a", "src/lib.rs:1", true))
            .unwrap();
        (&journal.file).write_all(b"{\"entry\":\"serv").unwrap();
        let kept = Kept::read(&path).unwrap();
        assert_eq!(kept.started["ext/a"], 2);
        assert_eq!(kept.in_flight["ext/a"], ["src/lib.rs:9"]);
        assert_eq!(in_flight(&kept), "being served: ext/a at src/lib.rs:9");
    }
}

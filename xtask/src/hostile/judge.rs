//! What one program's two sides are judged to be, and what a run of the
//! corpus says against its base. A program is held by three things of
//! each side's run, its first stream, its second and how it ended
//! ([`seen`]); the oracle's are kept per pin, so only the engine runs
//! each time. Under the interpreter the engine's are held to the
//! oracle's through [`oracle::compare`] under `hostile:<id>`; under the
//! compiled tier only a fault is judged (Round 3, Q24 (a)).

use std::fmt;

use crate::divergence::{HOSTILE_PREFIX, Registry};
use crate::oracle::{self, Ended, Observation};

use super::kept::Verdict;

/// How one side's run of a program came out: what it observed, or the
/// recipe's limit, past which it was stopped.
#[derive(Debug, Clone)]
pub(crate) enum Came {
    Observed(Observation),
    PastLimit(u64),
}

/// `code` run by `binary` under the oracle's recipe and `regimes`, as
/// both sides of a lane that holds a program to php run it.
pub(crate) fn came(
    oracle: &oracle::Oracle<'_>,
    binary: &std::path::Path,
    code: &str,
    regimes: &[&str],
) -> anyhow::Result<Came> {
    let ran = oracle.run_under(&oracle::Invocation {
        regimes,
        ..oracle::Invocation::new(binary, oracle::Program::Snippet(code))
    });
    match ran {
        Ok(observed) => Ok(Came::Observed(observed)),
        Err(why) => match why.downcast_ref::<oracle::PastLimit>() {
            Some(past) => Ok(Came::PastLimit(past.limit_s)),
            None => Err(why),
        },
    }
}

/// The engine's fault on a program: what no php program ends in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Fault {
    /// A signal ended it.
    Signal(i32),
    /// It ran past the recipe's limit, in seconds.
    PastLimit(u64),
    /// It said an invariant of its own failed: an `InternalError`, its
    /// heap's leak sensor's among them, or a panic, by the line that says so.
    Internal(String),
}

/// What a line the engine prints of a failed invariant carries.
const INTERNAL: [&str; 2] = ["InternalError: ", "panicked at "];

impl Fault {
    /// The engine's fault in `came`, where the oracle's bytes `theirs`
    /// do not carry the line that names it.
    pub(crate) fn of(came: &Came, theirs: &[u8]) -> Option<Fault> {
        match came {
            Came::PastLimit(limit) => Some(Fault::PastLimit(*limit)),
            Came::Observed(observed) => Fault::observed(observed, theirs),
        }
    }

    /// The same of a run that ended within the limit.
    fn observed(observed: &Observation, theirs: &[u8]) -> Option<Fault> {
        if let Ended::Signal(signal) = observed.exit {
            return Some(Fault::Signal(signal));
        }
        let theirs = String::from_utf8_lossy(theirs);
        let streams = [&observed.stdout, &observed.stderr];
        let lines = streams.into_iter().flat_map(|s| s.split(|b| *b == b'\n'));
        let said = lines
            .map(String::from_utf8_lossy)
            .find(|line| INTERNAL.iter().any(|m| line.contains(m)) && !theirs.contains(&**line));
        said.map(|line| Fault::Internal(line.trim().chars().take(240).collect()))
    }
}

impl fmt::Display for Fault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Fault::Signal(signal) => write!(f, "signal {signal} ended it"),
            Fault::PastLimit(limit) => write!(f, "it ran past the recipe's {limit}s"),
            Fault::Internal(line) => write!(f, "{line}"),
        }
    }
}

/// What a side's run is held by: its first stream, its second where it
/// wrote one, and how it ended, as one text a diff can name a line of.
pub(crate) fn seen(observed: &Observation) -> Vec<u8> {
    let mut seen = observed.stdout.clone();
    if !observed.stderr.is_empty() {
        seen.extend_from_slice(b"\n--- the second stream\n");
        seen.extend_from_slice(&observed.stderr);
    }
    seen.extend_from_slice(format!("\n--- ended on {}\n", observed.exit).as_bytes());
    seen
}

/// The verdict on one program under the interpreter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Judged {
    Met,
    /// The row that accepts the difference.
    Accepted(String),
    /// The first line that differs.
    Differs(String),
    /// A row pins the difference it accepts by digests and the engine's
    /// or the oracle's side has moved past them: the row's words.
    Moved(String),
    Fault(Fault),
}

impl Judged {
    /// The engine's run `came` held to the oracle's bytes `theirs` under
    /// the program `id`'s divergence key.
    pub(crate) fn of(registry: &Registry, id: &str, theirs: &[u8], came: &Came) -> Judged {
        Judged::keyed(registry, &key(id), theirs, came)
    }

    /// The same under the divergence key `key` whole, which the `fuzz`
    /// lane holds its programs by.
    pub(crate) fn keyed(registry: &Registry, key: &str, theirs: &[u8], came: &Came) -> Judged {
        let observed = match came {
            Came::PastLimit(limit) => return Judged::Fault(Fault::PastLimit(*limit)),
            Came::Observed(observed) => observed,
        };
        if let Some(fault) = Fault::observed(observed, theirs) {
            return Judged::Fault(fault);
        }
        let ours = seen(observed);
        match oracle::compare(registry, key, theirs, &ours) {
            oracle::Verdict::Same => Judged::Met,
            oracle::Verdict::Diverged(row) => Judged::Accepted(row.id.to_string()),
            oracle::Verdict::Moved(moved) => Judged::Moved(moved.to_string()),
            oracle::Verdict::Differs(diff) => {
                Judged::Differs(crate::crosscheck::first_difference(&diff, "program"))
            }
        }
    }

    /// The verdict a run keeps.
    pub(crate) fn verdict(&self) -> Verdict {
        match self {
            Judged::Met => Verdict::Met,
            Judged::Accepted(_) => Verdict::Accepted,
            Judged::Differs(_) | Judged::Moved(_) => Verdict::Differs,
            Judged::Fault(_) => Verdict::Fault,
        }
    }
}

/// The divergence key of the program `id`.
pub(crate) fn key(id: &str) -> String {
    format!("{HOSTILE_PREFIX}{id}")
}

/// One program as the run found it: its verdict under the interpreter,
/// and its fault under the compiled tier where it faulted there; or why
/// the oracle's side holds nothing, where it did not complete it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Found {
    Held {
        interpreter: Judged,
        tier: Option<Fault>,
    },
    /// How the oracle's run ended, which no program of the corpus ends in.
    Unheld(String),
}

/// What a run says against its base: each program that refuses the run,
/// and the counts that refuse nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Said {
    pub refusals: Vec<String>,
    pub met: u64,
    pub accepted: u64,
    pub differs: u64,
    pub regressed: u64,
    pub faults: u64,
    pub tier_faults: u64,
    pub unheld: u64,
    /// The programs whose verdict at the base the run was held to.
    pub held_at_base: u64,
}

impl Said {
    /// What `found`, one program per id, says against `base`, the kept
    /// run's verdicts at the merge base where one is kept (Round 1, Q5
    /// (b)): a fault under either pass refuses the run, as does a
    /// difference where the program met php at the base, a row's moved
    /// digests and a program the oracle did not complete; a difference
    /// otherwise is counted and refuses nothing.
    pub(crate) fn of<'a>(
        found: impl IntoIterator<Item = (&'a str, &'a Found)>,
        base: Option<(&str, &std::collections::BTreeMap<String, Verdict>)>,
    ) -> Said {
        let mut said = Said::default();
        for (id, found) in found {
            let (interpreter, tier) = match found {
                Found::Unheld(why) => {
                    said.unheld += 1;
                    said.refusals.push(format!(
                        "{id}: the oracle did not complete it, {why}, and the corpus holds only programs it completes"
                    ));
                    continue;
                }
                Found::Held { interpreter, tier } => (interpreter, tier),
            };
            let at_base = base.and_then(|(commit, verdicts)| Some((commit, verdicts.get(id)?)));
            if at_base.is_some() {
                said.held_at_base += 1;
            }
            match interpreter {
                Judged::Met => said.met += 1,
                Judged::Accepted(_) => said.accepted += 1,
                Judged::Fault(fault) => {
                    said.faults += 1;
                    said.refusals.push(format!("{id} faults: {fault}"));
                }
                Judged::Moved(moved) => {
                    said.differs += 1;
                    said.refusals.push(format!("{id}: {moved}"));
                }
                Judged::Differs(first) => {
                    said.differs += 1;
                    if let Some((commit, Verdict::Met)) = at_base {
                        said.regressed += 1;
                        said.refusals
                            .push(format!("{id} met php at {commit} and differs now: {first}"));
                    }
                }
            }
            if let Some(fault) = tier {
                said.tier_faults += 1;
                said.refusals
                    .push(format!("{id} faults under the compiled tier: {fault}"));
            }
        }
        said
    }
}

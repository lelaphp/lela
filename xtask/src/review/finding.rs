//! One finding and what became of it, as Mitchell ruled in Round 5 (Q8
//! (a)): a finding is confirmed by an oracle-backed reproducer or by two
//! skeptics holding it with none refuting; only someone other than the
//! branch's builder may refute it; and a split vote stays a split, kept as
//! the votes that made it. What is done about a finding, fixed in a commit
//! or carried to a ticket, is its disposition, and stands beside whether it
//! was confirmed rather than in place of it, since the experiment counts
//! confirmed defects whatever became of them.
//!
//! A finding's reading carries its whole state, so the last reading of its
//! scope is the finding: who named it and in which role, its file, the
//! builder, each vote by voter, who reproduced it and with what, and its
//! disposition, as group keys `<kind>:<value>`; a ticket's number is a
//! number. The claim and its scenario are the message, carried forward
//! whole and never taken apart. The outcome is derived from the rest and
//! never stored.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;

use anyhow::{Context, Result, bail, ensure};
use clap::Subcommand;

use super::{Claimed, Findings, HASH, INSTRUMENT, Scope, digest};
use crate::reading::{Number, Reading, Status};
use crate::track::{self, short};

/// The group kinds a finding's reading carries, each `<kind>:<value>`.
const ROLE: &str = "role";
const REVIEWER: &str = "reviewer";
const BUILDER: &str = "builder";
const FILE: &str = "file";
const REPRODUCER: &str = "reproducer";
const REPRODUCED: &str = "reproduced";
const HOLDS: &str = "holds";
const REFUTES: &str = "refutes";
const FIXED: &str = "fixed";

/// The number a ticketed finding carries its ticket under.
const TICKETED: &str = "ticketed";
/// The number that says whether the finding is confirmed, derived on write
/// for the reader that counts, and never read back.
const CONFIRMED: &str = "confirmed";

/// What one resolve does to a finding.
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum Act {
    /// A skeptic holds the finding: two holding it, with none refuting,
    /// confirm it.
    Holds {
        /// Who holds it.
        #[arg(long)]
        by: String,
    },
    /// A skeptic refutes the finding; refused to the branch's builder.
    Refutes {
        /// Who refutes it.
        #[arg(long)]
        by: String,
    },
    /// The finding's reproducer was run against the oracle and showed the
    /// defect, which confirms it.
    Reproduced {
        /// Who ran it.
        #[arg(long)]
        by: String,
        /// The reproducer run, where the findings file named none.
        #[arg(long, value_name = "PATH")]
        reproducer: Option<String>,
    },
    /// The finding was fixed in a commit.
    Fixed {
        /// The commit that fixed it.
        commit: String,
    },
    /// The finding was carried to a ticket.
    Ticketed {
        /// The ticket's number.
        number: u64,
    },
}

/// A skeptic's vote on a finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Vote {
    Holds,
    Refutes,
}

/// What was done about a finding.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Disposition {
    Fixed(String),
    Ticketed(u64),
}

/// Where a finding stands, derived from its votes, its reproduction and
/// its disposition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Claimed, and neither confirmed nor refuted.
    Open,
    /// Held by some skeptics and refuted by others.
    Split,
    /// Reproduced, or held by two skeptics with none refuting.
    Confirmed,
    /// Refuted, and held by no one.
    Refuted,
    /// Fixed in the commit named.
    Fixed(String),
    /// Carried to the ticket named.
    Ticketed(u64),
}

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Open => f.write_str("open"),
            Outcome::Split => f.write_str("split"),
            Outcome::Confirmed => f.write_str("confirmed"),
            Outcome::Refuted => f.write_str("refuted"),
            Outcome::Fixed(commit) => write!(f, "fixed in {}", short(commit)),
            Outcome::Ticketed(n) => write!(f, "ticketed as #{n}"),
        }
    }
}

/// One finding: its scope and key, the head it was found at, what it
/// claims and everything said of it since.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
    pub(crate) scope: Scope,
    key: String,
    head: String,
    file: String,
    said: String,
    roles: BTreeSet<String>,
    reviewers: BTreeSet<String>,
    builders: BTreeSet<String>,
    reproducer: Option<String>,
    reproduced: Option<String>,
    votes: BTreeMap<String, Vote>,
    disposition: Option<Disposition>,
}

impl Finding {
    /// The finding `claimed` makes at `head`, as `file`'s reviewer wrote it.
    pub(crate) fn claimed(file: &Findings, claimed: &Claimed, head: &str) -> Finding {
        let key = digest(&[claimed.file.trim(), claimed.claim.trim()]);
        Finding {
            scope: Scope::Finding {
                ticket: file.ticket,
                head: short(head).to_owned(),
                hash: key[..HASH].to_owned(),
            },
            key,
            head: head.to_owned(),
            file: claimed.file.trim().to_owned(),
            said: format!(
                "{}; scenario: {}",
                claimed.claim.trim(),
                claimed.scenario.trim()
            ),
            roles: BTreeSet::from([file.role.trim().to_owned()]),
            reviewers: BTreeSet::from([file.reviewer.trim().to_owned()]),
            builders: BTreeSet::from([file.builder.trim().to_owned()]),
            reproducer: claimed.reproducer.clone().filter(|r| !r.trim().is_empty()),
            reproduced: None,
            votes: BTreeMap::new(),
            disposition: None,
        }
    }

    /// `other`'s roles, reviewers, builders and reproducer added to this
    /// finding's: a second review naming the same claim at the same head.
    pub(crate) fn join(&mut self, other: &Finding) {
        self.roles.extend(other.roles.iter().cloned());
        self.reviewers.extend(other.reviewers.iter().cloned());
        self.builders.extend(other.builders.iter().cloned());
        if self.reproducer.is_none() {
            self.reproducer.clone_from(&other.reproducer);
        }
    }

    /// The finding a reading of its scope holds.
    pub(crate) fn of(reading: &Reading) -> Result<Finding> {
        let scope = Scope::of(&reading.scope)
            .filter(|s| matches!(s, Scope::Finding { .. }))
            .with_context(|| format!("{} is no finding's scope", reading.scope))?;
        let mut finding = Finding {
            scope,
            key: reading.key().to_owned(),
            head: reading.commit.clone(),
            file: String::new(),
            said: reading.message.clone(),
            roles: BTreeSet::new(),
            reviewers: BTreeSet::new(),
            builders: BTreeSet::new(),
            reproducer: None,
            reproduced: None,
            votes: BTreeMap::new(),
            disposition: reading
                .int(TICKETED)
                .map(|n| Disposition::Ticketed(n.unsigned_abs())),
        };
        for key in reading.groups.keys() {
            let (kind, value) = key.split_once(':').with_context(|| {
                format!(
                    "{}: the group {key:?} is no `<kind>:<value>`",
                    reading.scope
                )
            })?;
            let value = value.to_owned();
            match kind {
                ROLE => {
                    finding.roles.insert(value);
                }
                REVIEWER => {
                    finding.reviewers.insert(value);
                }
                BUILDER => {
                    finding.builders.insert(value);
                }
                FILE => finding.file = value,
                REPRODUCER => finding.reproducer = Some(value),
                REPRODUCED => finding.reproduced = Some(value),
                HOLDS => {
                    finding.votes.insert(value, Vote::Holds);
                }
                REFUTES => {
                    finding.votes.insert(value, Vote::Refutes);
                }
                FIXED => finding.disposition = Some(Disposition::Fixed(value)),
                _ => bail!(
                    "{}: a finding carries no group of kind {kind:?}",
                    reading.scope
                ),
            }
        }
        ensure!(
            !finding.file.is_empty(),
            "{}: a finding names its file",
            reading.scope
        );
        Ok(finding)
    }

    /// The finding with `act` applied, or the refusal that leaves it as it
    /// was.
    pub(crate) fn apply(&mut self, root: &Path, act: &Act) -> Result<()> {
        let named = |by: &str| -> Result<String> {
            let by = by.trim();
            ensure!(!by.is_empty(), "a vote or a reproduction names who made it");
            Ok(by.to_owned())
        };
        match act {
            Act::Holds { by } => {
                self.votes.insert(named(by)?, Vote::Holds);
            }
            Act::Refutes { by } => {
                let by = named(by)?;
                if self.builders.contains(&by) {
                    bail!(
                        "{by} built the branch {} was found in, and only someone other than the branch's builder may refute it (Round 5, Q8 (a))",
                        self.scope
                    );
                }
                self.votes.insert(by, Vote::Refutes);
            }
            Act::Reproduced { by, reproducer } => {
                let by = named(by)?;
                let reproducer = reproducer
                    .clone()
                    .filter(|r| !r.trim().is_empty())
                    .or_else(|| self.reproducer.clone())
                    .with_context(|| {
                        format!(
                            "{} names no reproducer; `--reproducer` names the one run",
                            self.scope
                        )
                    })?;
                self.reproducer = Some(reproducer);
                self.reproduced = Some(by);
            }
            Act::Fixed { commit } => {
                self.disposition = Some(Disposition::Fixed(track::rev(root, commit)?));
            }
            Act::Ticketed { number } => {
                ensure!(*number > 0, "a ticket's number is positive");
                self.disposition = Some(Disposition::Ticketed(*number));
            }
        }
        Ok(())
    }

    /// How many skeptics hold the finding and how many refute it.
    fn tally(&self) -> (usize, usize) {
        let holds = self.votes.values().filter(|v| **v == Vote::Holds).count();
        (holds, self.votes.len() - holds)
    }

    /// Whether the finding is confirmed: reproduced, or held by two
    /// skeptics with none refuting.
    pub(crate) fn confirmed(&self) -> bool {
        let (holds, refutes) = self.tally();
        self.reproduced.is_some() || (holds >= 2 && refutes == 0)
    }

    /// Where the finding stands: its disposition where it has one, else
    /// what its reproduction and votes make it.
    pub(crate) fn outcome(&self) -> Outcome {
        match &self.disposition {
            Some(Disposition::Fixed(commit)) => return Outcome::Fixed(commit.clone()),
            Some(Disposition::Ticketed(n)) => return Outcome::Ticketed(*n),
            None => {}
        }
        if self.confirmed() {
            return Outcome::Confirmed;
        }
        match self.tally() {
            (_, 0) => Outcome::Open,
            (0, _) => Outcome::Refuted,
            _ => Outcome::Split,
        }
    }

    /// The finding as a reading of its scope: a confirmed one fails, since
    /// a defect stands; a fixed or a refuted one passes; the rest are
    /// skipped, no one having settled them.
    pub(crate) fn reading(&self) -> Result<Reading> {
        let mut reading = Reading::keyed(INSTRUMENT, &self.scope.to_string(), self.key.clone())?;
        reading.commit.clone_from(&self.head);
        let outcome = self.outcome();
        reading.status = match outcome {
            Outcome::Confirmed => Status::Fail,
            Outcome::Fixed(_) | Outcome::Refuted => Status::Pass,
            Outcome::Open | Outcome::Split | Outcome::Ticketed(_) => Status::Skip,
        };
        let mut groups: Vec<(&str, &str)> = vec![(FILE, &self.file)];
        groups.extend(self.roles.iter().map(|r| (ROLE, r.as_str())));
        groups.extend(self.reviewers.iter().map(|r| (REVIEWER, r.as_str())));
        groups.extend(self.builders.iter().map(|b| (BUILDER, b.as_str())));
        groups.extend(self.reproducer.iter().map(|r| (REPRODUCER, r.as_str())));
        groups.extend(self.reproduced.iter().map(|r| (REPRODUCED, r.as_str())));
        for (who, vote) in &self.votes {
            groups.push((if *vote == Vote::Holds { HOLDS } else { REFUTES }, who));
        }
        if let Some(Disposition::Fixed(commit)) = &self.disposition {
            groups.push((FIXED, commit));
        }
        for (kind, value) in groups {
            reading.group(&format!("{kind}:{value}"), 1)?;
        }
        if let Some(Disposition::Ticketed(n)) = &self.disposition {
            reading.count(TICKETED, *n)?;
        }
        reading.set(CONFIRMED, Number::count(u8::from(self.confirmed())));
        reading.message.clone_from(&self.said);
        Ok(reading)
    }
}

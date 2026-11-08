//! `xtask review`, the review module (harness row H24; c6.b to c6.h): it
//! classes a diff, records what a review found, resolves each finding and
//! writes the coverage of each merge into `main`, and it refuses nothing.
//! There is no hold here, as Mitchell ruled in Round 2 (Q33 (a)): no
//! `land`, `done` or hook reads a review reading as a reason to stop, and
//! every subcommand records its readings as measurements, whose exit is not
//! their status ([`Store::record_measurement`]). Whether review comes to
//! bind waits on the experiment it exists to make possible (c6.f).
//!
//! What the module writes, it writes into the main checkout's store, the one
//! store (harness-design.md §5), from whichever checkout it runs in, so a
//! review recorded in a worktree is found at the land without a retire.
//! Three scopes, each a [`Scope`]:
//!
//! - `<ticket>/<head7>`, one reading per review: the range a reviewer read,
//!   from a commit to an explicit head, and how many findings it made.
//! - `<ticket>/<head7>-<hash>`, one reading per finding (Round 4, Q34), the
//!   hash over the finding's file and claim, so a second reviewer making the
//!   same claim at the same head adds itself to the one finding rather than
//!   counting it twice. A later reading of the same scope is the finding's
//!   state after a resolve ([`finding`]).
//! - `merge/<commit7>`, one reading per merge into `main`: the merge's class
//!   and whether its branch was covered by the reviews that predate it
//!   ([`coverage`]).
//!
//! The reviewer writes the findings file and the reviewer, or the workflow
//! that ran it, runs `record` (Round 3, Q25 (a)). Role and file are group
//! keys of a reading, claim and scenario its message, and nothing reads a
//! message back.

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;

use crate::manifest::Manifest;
use crate::reading::{Reading, Store};
use crate::sha256::Sha256;
use crate::track::{self, MAIN, short};

pub(crate) mod coverage;
mod finding;

pub use coverage::{Class, Coverage};
pub use finding::{Act, Outcome};

/// The instrument's name, the `instrument` of every reading it writes.
pub const INSTRUMENT: &str = "review";

/// How far back `cover` looks for a merge it has not judged, in days: the
/// window the local facts' line counts over.
pub const DAYS: i64 = 7;

/// The scope a review reading is written under, its commits by their
/// first seven hex digits and a finding's hash by its first twelve, as
/// [`Scope::of`] reads them back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    /// One review of a ticket's branch, up to the head it read.
    Range { ticket: u64, head: String },
    /// One finding of a review at a head, by the hash of what it claims.
    Finding {
        ticket: u64,
        head: String,
        hash: String,
    },
    /// One merge into `main`, by its commit.
    Merge { commit: String },
}

/// How many hex digits of a finding's hash its scope carries.
const HASH: usize = 12;

/// The first directory of a merge's scope.
const MERGE: &str = "merge";

impl Scope {
    /// The scope `text` spells, where it is one of the three; none for
    /// any other.
    pub fn of(text: &str) -> Option<Scope> {
        let (first, rest) = text.split_once('/')?;
        let hex = |s: &str, n: usize| s.len() == n && s.bytes().all(|b| b.is_ascii_hexdigit());
        if first == MERGE {
            return hex(rest, 7).then(|| Scope::Merge {
                commit: rest.to_owned(),
            });
        }
        let ticket = first.parse::<u64>().ok().filter(|n| *n > 0)?;
        match rest.split_once('-') {
            None => hex(rest, 7).then(|| Scope::Range {
                ticket,
                head: rest.to_owned(),
            }),
            Some((head, hash)) => (hex(head, 7) && hex(hash, HASH)).then(|| Scope::Finding {
                ticket,
                head: head.to_owned(),
                hash: hash.to_owned(),
            }),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Scope::Range { ticket, head } => write!(f, "{ticket}/{}", short(head)),
            Scope::Finding { ticket, head, hash } => {
                write!(
                    f,
                    "{ticket}/{}-{}",
                    short(head),
                    &hash[..hash.len().min(HASH)]
                )
            }
            Scope::Merge { commit } => write!(f, "{MERGE}/{}", short(commit)),
        }
    }
}

/// The digest of `parts`, one per line, as lowercase hex: the key of a
/// review reading, and a finding's hash.
fn digest(parts: &[&str]) -> String {
    let text = Sha256::of(parts.join("\n").as_bytes()).to_string();
    text.split_once(':')
        .map_or(text.clone(), |(_, hex)| hex.to_owned())
}

/// The store every review reading is written in and read from: the main
/// checkout's, whichever checkout of the repository `root` is.
fn store(root: &Path) -> Result<(PathBuf, Store)> {
    let main = track::main_worktree(root)?;
    let store = Store::of_workspace(&main);
    Ok((main, store))
}

/// Each reading printed as one line, then recorded as a measurement.
fn written(store: &Store, readings: &[Reading]) -> Result<()> {
    for reading in readings {
        println!("{reading}");
    }
    store.record_measurement(readings)
}

/// Whether `ancestor` is `commit` or reachable from it.
fn is_ancestor(root: &Path, ancestor: &str, commit: &str) -> bool {
    crate::git::git(root, &["merge-base", "--is-ancestor", ancestor, commit]).is_ok()
}

/// The findings file a reviewer writes, as JSON: the ticket whose branch it
/// read, the range, who read it in which role, who built the branch, and
/// what it found. `from` is the merge base of `head` with `main` where it
/// is left out.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Findings {
    pub ticket: u64,
    #[serde(default)]
    pub from: Option<String>,
    pub head: String,
    pub role: String,
    pub reviewer: String,
    /// Who built the branch at `head`, the one who may not refute what was
    /// found in it (Round 5, Q8 (a)): a name the brief gives, since a
    /// session cannot tell a reviewer from the builder that spawned it.
    pub builder: String,
    #[serde(default)]
    pub findings: Vec<Claimed>,
}

/// One finding as the reviewer wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Claimed {
    pub file: String,
    pub claim: String,
    pub scenario: String,
    /// A program that shows the defect, where the reviewer wrote one.
    #[serde(default)]
    pub reproducer: Option<String>,
}

impl Findings {
    /// The file at `path`, parsed and held to carrying every name.
    pub fn read(path: &Path) -> Result<Findings> {
        let text = crate::read(path)?;
        let file: Findings = serde_json::from_str(&text)
            .with_context(|| format!("{} is no findings file", path.display()))?;
        ensure!(file.ticket > 0, "a findings file names its ticket");
        for (what, value) in [
            ("head", &file.head),
            ("role", &file.role),
            ("reviewer", &file.reviewer),
            ("builder", &file.builder),
        ] {
            ensure!(!value.trim().is_empty(), "a findings file names its {what}");
        }
        for claimed in &file.findings {
            ensure!(
                !claimed.file.trim().is_empty() && !claimed.claim.trim().is_empty(),
                "each finding names its file and its claim"
            );
        }
        Ok(file)
    }
}

/// `xtask review record <file>`: one reading of the range the review read,
/// and one per finding, each merged into the finding of the same hash at
/// the same head where one is recorded. Refused, writing nothing, where the
/// file does not parse or its range names no commits of the repository.
pub fn record(root: &Path, path: &Path) -> Result<()> {
    let file = Findings::read(path)?;
    let (_, store) = store(root)?;
    let head = track::rev(root, &file.head)?;
    let from = match &file.from {
        Some(from) => track::rev(root, from)?,
        None => track::merge_base(root, &head, MAIN)?,
    };
    if !is_ancestor(root, &from, &head) {
        bail!(
            "{} is not reachable from {}, so {}..{} is no range of the branch",
            short(&from),
            short(&head),
            short(&from),
            short(&head)
        );
    }
    let mut readings = vec![coverage::Range::reading(&file, &from, &head)?];
    let mut found: Vec<finding::Finding> = Vec::new();
    for claimed in &file.findings {
        let made = finding::Finding::claimed(&file, claimed, &head);
        if let Some(joined) = found.iter_mut().find(|f| f.scope == made.scope) {
            joined.join(&made);
            continue;
        }
        let mut finding = match store.last_of(INSTRUMENT, &made.scope.to_string())? {
            Some(stored) => finding::Finding::of(&stored)?,
            None => made.clone(),
        };
        finding.join(&made);
        found.push(finding);
    }
    for finding in &found {
        readings.push(finding.reading()?);
    }
    written(&store, &readings)
}

/// `xtask review resolve <scope> <act>`: the finding recorded under
/// `scope`, with `act` applied, written again under it. Refused, writing
/// nothing, where no finding stands there or the act is not its
/// author's to make: a refute by the branch's builder (Round 5, Q8 (a)).
pub fn resolve(root: &Path, scope: &str, act: &Act) -> Result<()> {
    let Some(Scope::Finding { .. }) = Scope::of(scope) else {
        bail!("{scope:?} is no finding's scope, `<ticket>/<head7>-<hash>`");
    };
    let (_, store) = store(root)?;
    let held = store
        .last_of(INSTRUMENT, scope)?
        .with_context(|| format!("no finding is recorded under {scope}"))?;
    let mut finding = finding::Finding::of(&held)?;
    finding.apply(root, act)?;
    written(&store, &[finding.reading()?])?;
    println!("review resolve: {scope} is {}", finding.outcome());
    Ok(())
}

/// `xtask review cover`: the coverage reading of `merge`, or of each merge
/// of `main`'s first parents in the last [`DAYS`] days that has none. Each
/// merge is classed by the `[review]` table of the main checkout's manifest
/// and judged against every review reading the store holds.
pub fn cover(root: &Path, merge: Option<&str>) -> Result<()> {
    let (main, store) = store(root)?;
    let manifest = Manifest::load(&main)?;
    let merges = match merge {
        Some(merge) => vec![track::rev(&main, merge)?],
        None => coverage::unjudged(&main, &store)?,
    };
    if merges.is_empty() {
        println!("review cover: every merge into {MAIN} of the last {DAYS} days is judged");
        return Ok(());
    }
    let ranges = coverage::Range::all(&store)?;
    let mut readings = Vec::new();
    for commit in merges {
        readings.push(
            coverage::Judged::judge(&main, manifest.review.as_ref(), &commit, &ranges)?
                .reading()?,
        );
    }
    written(&store, &readings)
}

/// What `worktree land` calls once `main` has moved: [`cover`] over the
/// merges not yet judged, best effort, so a failure is said and the land
/// stands (Round 4, Q35).
pub fn after_land(root: &Path) {
    if let Err(err) = cover(root, None) {
        eprintln!(
            "worktree land: the review coverage was not written, and the land stands: {err:#}"
        );
    }
}

/// `xtask review class [--head <rev>]`: the class of the diff from `head`'s
/// merge base with `main` to `head`, printed; it writes no reading.
pub fn class(root: &Path, head: &str) -> Result<()> {
    let main = track::main_worktree(root)?;
    let manifest = Manifest::load(&main)?;
    let head = track::rev(root, head)?;
    let base = track::merge_base(root, &head, MAIN)?;
    let class = Class::of(root, manifest.review.as_ref(), &base, &head)?;
    println!(
        "review class: {}..{} is {class}",
        short(&base),
        short(&head)
    );
    Ok(())
}

#[cfg(test)]
mod tests;

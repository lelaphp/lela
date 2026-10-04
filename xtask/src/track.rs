//! What every reader of a worktree asks (CONTEXT.md, Track;
//! harness-design.md §5; ADR-0009), answered here for one worktree by
//! [`Track::of`]: the branch it is on, the commit the branch was taken off
//! `main` at, the main checkout whose `target/harness` is the one store,
//! and the ticket the branch's work is for. `xtask track` prints them, as
//! text or as one JSON object, and writes no reading, since it measures
//! nothing; and the add command asks for a branch not yet made. A track
//! is a label on the tracker and has no branch, so no track is read
//! here: `done`'s base is the merge base with `main`, and the frontier
//! is named by `--track`.
//!
//! The ticket is a record on the branch, `branch.<name>.ticket` in the
//! repository's configuration (#30): git's own per-branch record, shared
//! by every worktree of the clone, written once by `xtask worktree add
//! --ticket`, read by name here, kept when the worktree is retired, since
//! the branch is, and gone with the branch when a person deletes it. A
//! branch with no record names no ticket, which is stated, and no reader
//! guesses one from a number in the branch's name.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::git::git;

/// The branch every ticket's branch is cut from, and the track of a
/// ticket that carries no `track:` label (ADR-0009).
pub(crate) const MAIN: &str = "main";

/// The ticket a branch's work is for: the number of the GitHub issue
/// recorded on the branch when its worktree was laid down, or the one a
/// `[parity]` ruling names, read from the manifest as the bare number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Deserialize)]
#[serde(transparent)]
pub struct Ticket(u64);

impl Ticket {
    pub fn new(number: u64) -> Ticket {
        Ticket(number)
    }

    pub fn number(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for Ticket {
    /// As an issue is cited: `#30`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// One worktree's branch and what follows from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    /// The branch these are the facts of; `None` for a detached `HEAD`.
    pub branch: Option<String>,
    /// The merge base of the branch with `main`, and `main`'s tip for a
    /// branch not yet made, which is where the add command takes it off.
    pub base: String,
    /// The root whose `target/harness` is the one store: the main
    /// worktree, which is the checkout every linked worktree was added
    /// from (harness-design.md §4.2, §5).
    pub store: PathBuf,
    /// The ticket recorded on the branch; `None` for a branch that names
    /// none and for a detached `HEAD`.
    pub ticket: Option<Ticket>,
}

impl Track {
    /// The facts of `branch`, or of `HEAD`'s branch when none is named,
    /// or of `HEAD` itself when it is detached, which is a shape and not
    /// an error; a repository with no `main`, its own or `origin`'s, is
    /// one.
    pub fn of(root: &Path, branch: Option<&str>) -> Result<Track> {
        let branch = branch.map(str::to_owned).or_else(|| head_branch(root));
        let main = ref_of(root, MAIN).with_context(|| {
            format!(
                "the repository has no {MAIN}, its own or origin's; nothing is measured since it"
            )
        })?;
        let from = match &branch {
            Some(b) => ref_of(root, b).unwrap_or_else(|| main.clone()),
            None => "HEAD".to_owned(),
        };
        let ticket = match &branch {
            Some(b) => ticket_of(root, b)?,
            None => None,
        };
        Ok(Track {
            base: merge_base(root, &from, &main)?,
            branch,
            store: main_worktree(root)?,
            ticket,
        })
    }

    /// One line per fact, each stated plainly.
    pub fn render(&self) -> String {
        let branch = match &self.branch {
            Some(b) => b.clone(),
            None => "none; HEAD is detached".to_owned(),
        };
        let ticket = match self.ticket {
            Some(ticket) => format!("{ticket}, what the branch's work is for"),
            None => {
                "none; the branch records no ticket, and none is guessed from its name".to_owned()
            }
        };
        format!(
            "branch: {branch}\nbase: {}, where the branch was taken off {MAIN}\nstore: {}, whose target/harness is the one store\nticket: {ticket}\n",
            short(&self.base),
            self.store.display()
        )
    }

    /// The same facts as one object.
    pub fn to_json(&self) -> Value {
        json!({
            "branch": self.branch,
            "base": self.base,
            "store": self.store,
            "ticket": self.ticket.map(Ticket::number),
        })
    }
}

/// The branch `HEAD` is on, or `None` when `HEAD` is detached.
pub(crate) fn head_branch(root: &Path) -> Option<String> {
    let name = git(root, &["symbolic-ref", "--quiet", "--short", "HEAD"], None).ok()?;
    let name = name.trim().to_owned();
    (!name.is_empty()).then_some(name)
}

/// The configuration key that is the branch's ticket record.
fn ticket_key(branch: &str) -> String {
    format!("branch.{branch}.ticket")
}

/// The ticket recorded on `branch`, `None` where the branch records none;
/// a record that is not a number is an error, not a branch with none.
fn ticket_of(root: &Path, branch: &str) -> Result<Option<Ticket>> {
    let key = ticket_key(branch);
    let Ok(text) = git(root, &["config", "--get", &key], None) else {
        return Ok(None);
    };
    let text = text.trim();
    let number = text
        .parse()
        .with_context(|| format!("{key} is {text:?}, not an issue number"))?;
    Ok(Some(Ticket(number)))
}

/// The ticket recorded on the branch `HEAD` is on, `None` for a branch
/// that records none and for a detached `HEAD`: what `done` asks for
/// condition 7 (#31, harness-design.md §4.5), the record alone, with no
/// merge base taken, so a base a caller passed is not second-guessed and
/// a `main` that is missing is not a reason to skip the ticket.
pub(crate) fn head_ticket(root: &Path) -> Result<Option<Ticket>> {
    match head_branch(root) {
        Some(branch) => ticket_of(root, &branch),
        None => Ok(None),
    }
}

/// The ticket recorded on `branch`: the one write of the record.
pub(crate) fn record_ticket(root: &Path, branch: &str, ticket: Ticket) -> Result<()> {
    let key = ticket_key(branch);
    git(root, &["config", &key, &ticket.number().to_string()], None)
        .with_context(|| format!("cannot record {ticket} as {key}"))?;
    Ok(())
}

/// The branch whose record names `ticket`, if one does: what refuses a
/// second branch on the same ticket, so no ticket is closed twice.
pub(crate) fn branch_of_ticket(root: &Path, ticket: Ticket) -> Option<String> {
    Records::of(branch_records(root), &BTreeSet::new())
        .branch(ticket)
        .map(str::to_owned)
}

/// Every branch record of the repository, each branch with the ticket it
/// records, in git's order: the one read of `branch.<name>.ticket` across
/// branches. A record that is not a number is no record here, and git
/// not answering, as it does by exiting 1 when no branch records one, is
/// no record at all.
fn branch_records(root: &Path) -> Vec<(String, Ticket)> {
    let found = git(
        root,
        &["config", "--get-regexp", r"^branch\..*\.ticket$"],
        None,
    )
    .unwrap_or_default();
    found
        .lines()
        .filter_map(|line| {
            let (key, number) = line.split_once(' ')?;
            let branch = key.strip_prefix("branch.")?.strip_suffix(".ticket")?;
            Some((branch.to_owned(), Ticket::new(number.trim().parse().ok()?)))
        })
        .collect()
}

/// The repository's branch records as the frontier reads them, once per
/// `track next` (#1850): the branch recording each ticket, and the
/// tickets recorded by a branch `main` does not contain yet, each with
/// that branch, which a held ticket waits on (#1857). Containment
/// is git's own reachability, the test `merge-base --is-ancestor` makes,
/// asked of every branch in one `for-each-ref --no-merged`; a record
/// whose branch is gone names no branch `main` could lack.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Records {
    branches: BTreeMap<Ticket, String>,
    unmerged: BTreeMap<Ticket, String>,
}

impl Records {
    /// The records of the repository at `root`, held to its `main`, its
    /// own or `origin`'s; a repository with neither is an error, since
    /// nothing can be said contained in it.
    pub(crate) fn read(root: &Path) -> Result<Records> {
        let main = ref_of(root, MAIN).with_context(|| {
            format!("the repository has no {MAIN}, its own or origin's, to hold a branch to")
        })?;
        let unmerged = git(
            root,
            &[
                "for-each-ref",
                "--format=%(refname:lstrip=2)",
                &format!("--no-merged={main}"),
                "refs/heads/",
            ],
            None,
        )
        .with_context(|| format!("the branches {main} does not contain could not be listed"))?;
        let unmerged: BTreeSet<&str> = unmerged.lines().collect();
        Ok(Records::of(branch_records(root), &unmerged))
    }

    /// Each branch of `pairs` with the ticket it records, the branches in
    /// `unmerged` being the ones `main` does not contain; the first branch
    /// recording a ticket is the one named for it, and the first not in
    /// `main` holds it and is the one named for that.
    pub(crate) fn of(pairs: Vec<(String, Ticket)>, unmerged: &BTreeSet<&str>) -> Records {
        let mut records = Records::default();
        for (branch, ticket) in pairs {
            if unmerged.contains(branch.as_str()) {
                records
                    .unmerged
                    .entry(ticket)
                    .or_insert_with(|| branch.clone());
            }
            records.branches.entry(ticket).or_insert(branch);
        }
        records
    }

    /// The branch recording `ticket`, if one does.
    pub(crate) fn branch(&self, ticket: Ticket) -> Option<&str> {
        self.branches.get(&ticket).map(String::as_str)
    }

    /// The branch recording `ticket` that `main` does not contain, if one
    /// does.
    pub(crate) fn unmerged(&self, ticket: Ticket) -> Option<&str> {
        self.unmerged.get(&ticket).map(String::as_str)
    }
}

/// The ref `name` is here: the branch itself, then `origin`'s.
fn ref_of(root: &Path, name: &str) -> Option<String> {
    let remote = format!("origin/{name}");
    let candidates = [("refs/heads/", name), ("refs/remotes/", remote.as_str())];
    candidates.into_iter().find_map(|(namespace, candidate)| {
        let full = format!("{namespace}{candidate}");
        git(root, &["rev-parse", "--verify", "--quiet", &full], None)
            .ok()
            .map(|_| candidate.to_owned())
    })
}

/// The merge base of `from` and `other`, `other` being the ref it is here
/// when it is a branch, and the name given otherwise, so git's own text
/// says what is missing.
pub(crate) fn merge_base(root: &Path, from: &str, other: &str) -> Result<String> {
    let at = ref_of(root, other);
    let name = at.as_deref().unwrap_or(other);
    git(root, &["merge-base", from, name], None)
        .map(|id| id.trim().to_owned())
        .with_context(|| format!("no merge base of {from} with {other}"))
}

/// The first seven characters, what `rev-parse --short` prints on a small
/// repository; `track work` cites commits the same way.
pub(crate) fn short(commit: &str) -> &str {
    &commit[..commit.len().min(7)]
}

/// The main worktree of the repository `root` is in: the first entry of
/// `git worktree list`, the path git prints, which is canonical.
pub(crate) fn main_worktree(root: &Path) -> Result<PathBuf> {
    worktrees(root)?
        .into_iter()
        .next()
        .map(|(path, _)| path)
        .context("git worktree list names no main worktree")
}

/// Every worktree of the repository `root` is in, the main one first,
/// each with the branch it is on, `None` for a detached `HEAD`: `git
/// worktree list --porcelain`, one stanza per worktree.
pub(crate) fn worktrees(root: &Path) -> Result<Vec<(PathBuf, Option<String>)>> {
    let list = git(root, &["worktree", "list", "--porcelain"], None)?;
    let mut found = Vec::new();
    for line in list.lines() {
        if let Some(path) = line.strip_prefix("worktree ") {
            found.push((PathBuf::from(path), None));
        } else if let Some(branch) = line.strip_prefix("branch refs/heads/")
            && let Some(last) = found.last_mut()
        {
            last.1 = Some(branch.to_owned());
        }
    }
    Ok(found)
}

/// The full id of `name`.
pub(crate) fn rev(root: &Path, name: &str) -> Result<String> {
    git(
        root,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("{name}^{{commit}}"),
        ],
        None,
    )
    .map(|id| id.trim().to_owned())
    .with_context(|| format!("{name} names no commit"))
}

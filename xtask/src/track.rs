//! The track (CONTEXT.md, harness-design.md §4.5): the branch a set of
//! branches is worked above, named by being a proper path prefix of theirs.
//! Four things every reader of a track asks are answered here, for one
//! worktree, by [`Track::of`]: which track its branch is on, the commit
//! `done` measures since, which root holds the track's store, and which
//! ticket the branch's work is for. `xtask track` prints them, as text or
//! as one JSON object, and writes no reading, since it measures nothing;
//! `done` takes its base through [`Base::of`]; and the add command asks
//! for a branch not yet made.
//!
//! The rule once lived in `done`'s reads and again in a prefix loop of the
//! add command, and the console was about to write a third. This module
//! is the one statement the three read.
//!
//! The ticket is a record on the branch, `branch.<name>.ticket` in the
//! repository's configuration (#30): git's own per-branch record, shared
//! by every worktree of the clone, written once by `xtask worktree add
//! --ticket`, read by name here, kept when the worktree is retired, since
//! the branch is, and gone with the branch when a person deletes it. A
//! branch with no record names no ticket, which is stated, and no reader
//! guesses one from a number in the branch's name.
//!
//! A track's own clone is detached at `origin/<name>`, so its `HEAD` is
//! on no branch, and which track it is is a record on the clone too
//! (#76): `track.name`, written once by `xtask track new` and read here
//! for a detached `HEAD` alone, since a branch names its own track. The
//! commit a track's clone is detached at is not the record: two tracks
//! laid down from the same `main` share it. Beside the name the clone
//! records the model its sessions run on, role-keyed (#75):
//! `track.model.session` and `track.model.subagent`, the second
//! defaulting to the first. A clone with no record names no model,
//! which is stated, and none is guessed from the track's name.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::git::{git, has_ref, is_ancestor};
use crate::worktree::say;

/// The track a branch with no track of its own is on.
pub(crate) const MAIN: &str = "main";

/// How a track was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Found {
    /// A proper path prefix of the branch's name is a branch: the track.
    Prefix,
    /// No prefix names a branch, so the track is [`MAIN`].
    NoPrefix,
    /// `HEAD` is detached and the clone records no track, so the track
    /// is [`MAIN`].
    Detached,
    /// `HEAD` is detached and the clone records its track: a track's own
    /// clone, detached at `origin/<name>`.
    Record,
}

impl Found {
    fn as_str(self) -> &'static str {
        match self {
            Found::Prefix => "prefix",
            Found::NoPrefix => "no-prefix",
            Found::Detached => "detached",
            Found::Record => "record",
        }
    }
}

/// The ticket a branch's work is for: the number of the GitHub issue
/// recorded on the branch when its worktree was laid down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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

/// The model a track's sessions run on, as the clone records it (#75):
/// the session's, and its subagents', which is the session's where the
/// record names none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub session: String,
    /// The subagents' model where the record names one; `None` defaults
    /// to the session's.
    pub subagent: Option<String>,
}

impl Model {
    pub fn new(session: String, subagent: Option<String>) -> Model {
        Model { session, subagent }
    }

    /// The subagents' model: the recorded one, or the session's.
    pub fn subagent(&self) -> &str {
        self.subagent.as_deref().unwrap_or(&self.session)
    }
}

/// One worktree's track and what follows from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Track {
    /// The branch whose track this is; `None` for a detached `HEAD`.
    pub branch: Option<String>,
    pub found: Found,
    /// The track's name: the prefix branch, or [`MAIN`].
    pub name: String,
    /// The ref the track is here: its own branch where the repository has
    /// it, `origin`'s otherwise, which is where a track lives in a
    /// repository with worktrees on its branches (harness ledger 14).
    pub at: String,
    /// The commit `done` measures since: the merge base of the branch with
    /// the track, and the track's tip for a branch not yet made, which is
    /// where the add command takes it off.
    pub base: String,
    /// The root whose `target/harness` is the track's store: the main
    /// worktree, which is the checkout every linked worktree was added
    /// from (harness-design.md §4.2, §5).
    pub store: PathBuf,
    /// The ticket recorded on the branch; `None` for a branch that names
    /// none and for a detached `HEAD`.
    pub ticket: Option<Ticket>,
    /// The model the clone records; `None` for a clone that records none.
    pub model: Option<Model>,
}

impl Track {
    /// The track of `branch`, or of `HEAD`'s branch when none is named,
    /// or the clone's record for a detached `HEAD`. A detached `HEAD` with
    /// no record and a branch whose prefixes name no branch are shapes,
    /// not errors; a track that is neither a branch nor `origin`'s is one.
    pub fn of(root: &Path, branch: Option<&str>) -> Result<Track> {
        let (branch, found, name) = match branch.map(str::to_owned).or_else(|| head_branch(root)) {
            None => match name_of(root) {
                Some(name) => (None, Found::Record, name),
                None => (None, Found::Detached, MAIN.to_owned()),
            },
            Some(branch) => match prefix_branch(root, &branch) {
                Some(track) => (Some(branch), Found::Prefix, track),
                None => (Some(branch), Found::NoPrefix, MAIN.to_owned()),
            },
        };
        let at = ref_of(root, &name).with_context(|| {
            format!(
                "the track {name} is neither a branch nor origin's; nothing is measured since it"
            )
        })?;
        let from = match &branch {
            Some(b) if is_branch(root, b) => b.as_str(),
            Some(_) => at.as_str(),
            None => "HEAD",
        };
        let base = merge_base(root, from, &at)?;
        let ticket = match &branch {
            Some(b) => ticket_of(root, b)?,
            None => None,
        };
        Ok(Track {
            branch,
            found,
            name,
            at,
            base,
            store: main_worktree(root)?,
            ticket,
            model: model_of(root)?,
        })
    }

    /// One line per fact, each stated plainly.
    pub fn render(&self) -> String {
        let branch = match (&self.branch, self.found) {
            (Some(b), Found::Prefix) => b.clone(),
            (Some(b), _) if *b == self.name => format!("{b}, the track itself"),
            (Some(b), _) => {
                format!("{b}, whose prefixes name no branch, so on no track of its own")
            }
            (None, Found::Record) => {
                format!(
                    "none; HEAD is detached at the track {}, which the clone records",
                    self.name
                )
            }
            _ => "none; HEAD is detached and the clone records no track, so on none".to_owned(),
        };
        let ticket = match self.ticket {
            Some(ticket) => format!("{ticket}, what the branch's work is for"),
            None => {
                "none; the branch records no ticket, and none is guessed from its name".to_owned()
            }
        };
        let (session, subagent) = match &self.model {
            Some(model) => (
                format!("{}, what a session on the track runs on", model.session),
                match &model.subagent {
                    Some(subagent) => format!("{subagent}, what its subagents run on"),
                    None => format!(
                        "{}, the session's, since the clone records none for the role",
                        model.session
                    ),
                },
            ),
            None => (
                "none; the clone records no model, and none is guessed from the track's name"
                    .to_owned(),
                "none; the same record".to_owned(),
            ),
        };
        format!(
            "branch: {branch}\ntrack: {} ({})\nbase: {}, what done measures since\nstore: {}, whose target/harness is the track's\nticket: {ticket}\nmodel.session: {session}\nmodel.subagent: {subagent}\n",
            self.name,
            self.at,
            short(&self.base),
            self.store.display()
        )
    }

    /// The same facts as one object.
    pub fn to_json(&self) -> Value {
        json!({
            "branch": self.branch,
            "found": self.found.as_str(),
            "track": self.name,
            "at": self.at,
            "base": self.base,
            "store": self.store,
            "ticket": self.ticket.map(Ticket::number),
            "model": self.model.as_ref().map(|model| json!({
                "session": model.session,
                "subagent": model.subagent(),
            })),
        })
    }
}

/// The commit the changes are measured since, and the ref it was taken
/// from: `--base`'s, or the branch's track ([`Track::of`]), which is
/// [`MAIN`] for a branch that is on none (harness-design.md §4.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    pub since: String,
    pub commit: String,
}

impl Base {
    /// The base of a bare `done`: the branch's track, or [`MAIN`]; with a
    /// ref, the merge base of `HEAD` with it, the ref first and then
    /// `origin`'s, for a checkout that has only the remote's.
    pub fn of(root: &Path, since: Option<&str>) -> Result<Base> {
        if let Some(since) = since {
            return Ok(Base {
                since: since.to_owned(),
                commit: merge_base(root, "HEAD", since)?,
            });
        }
        let track = Track::of(root, None)?;
        Ok(Base {
            since: track.name,
            commit: track.base,
        })
    }
}

impl std::fmt::Display for Base {
    /// The ref the base was taken from and the commit it resolved to, and
    /// the commit alone when the ref was that commit.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let short = short(&self.commit);
        if self.since == self.commit || self.since == short {
            write!(f, "{short}")
        } else {
            write!(f, "{} ({short})", self.since)
        }
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
/// condition 7 (#31, proposed for harness-design.md §4.5), the record alone, with no
/// merge base taken, so a base a caller passed is not second-guessed and
/// a track that is missing is not a reason to skip the ticket.
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

/// The configuration key that is the clone's track record.
const NAME_KEY: &str = "track.name";
/// The configuration keys that are the clone's model record, one per
/// role (#75).
const SESSION_KEY: &str = "track.model.session";
const SUBAGENT_KEY: &str = "track.model.subagent";

/// One configuration value, `None` where the key is unset or empty:
/// the clone's records, read here and by `track work` for its own.
pub(crate) fn config(root: &Path, key: &str) -> Option<String> {
    let text = git(root, &["config", "--get", key], None).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// The track the clone records, `None` where it records none.
fn name_of(root: &Path) -> Option<String> {
    config(root, NAME_KEY)
}

/// The model the clone records, `None` where it records none; a
/// subagents' model recorded with no session's is an error, since the
/// default runs the other way.
fn model_of(root: &Path) -> Result<Option<Model>> {
    let subagent = config(root, SUBAGENT_KEY);
    match config(root, SESSION_KEY) {
        Some(session) => Ok(Some(Model { session, subagent })),
        None if subagent.is_some() => {
            bail!(
                "{SUBAGENT_KEY} is recorded and {SESSION_KEY} is not; the subagents' model defaults to the session's, not the other way"
            )
        }
        None => Ok(None),
    }
}

/// The track and the model recorded on the clone at `root`: the one write
/// of both records, what `track new` does to the clone it laid down.
pub(crate) fn record_track(root: &Path, name: &str, model: &Model) -> Result<()> {
    let mut records = vec![(NAME_KEY, name), (SESSION_KEY, model.session.as_str())];
    if let Some(subagent) = &model.subagent {
        records.push((SUBAGENT_KEY, subagent));
    }
    for (key, value) in records {
        git(root, &["config", key, value], None)
            .with_context(|| format!("cannot record {value:?} as {key}"))?;
    }
    Ok(())
}

/// The branch whose record names `ticket`, if one does: what refuses a
/// second branch on the same ticket, so no ticket is closed twice.
pub(crate) fn branch_of_ticket(root: &Path, ticket: Ticket) -> Option<String> {
    let number = ticket.number().to_string();
    let found = git(
        root,
        &[
            "config",
            "--get-regexp",
            r"^branch\..*\.ticket$",
            &format!("^{number}$"),
        ],
        None,
    )
    .ok()?;
    found.lines().find_map(|line| {
        let (key, _) = line.split_once(' ')?;
        key.strip_prefix("branch.")?
            .strip_suffix(".ticket")
            .map(str::to_owned)
    })
}

/// Whether `name` is a branch here: a local one, or `origin`'s, which is
/// what a clone that has never checked the branch out has of it.
fn is_branch(root: &Path, name: &str) -> bool {
    ref_of(root, name).is_some()
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

/// The longest existing branch that is a proper path prefix of `branch`,
/// so `t-opus-5/witness` is on the track `t-opus-5`; `None` when no
/// prefix names a branch.
fn prefix_branch(root: &Path, branch: &str) -> Option<String> {
    let mut name = branch;
    while let Some((prefix, _)) = name.rsplit_once('/') {
        if is_branch(root, prefix) {
            return Some(prefix.to_owned());
        }
        name = prefix;
    }
    None
}

/// The merge base of `from` and `other`, `other` being the ref it is here
/// when it is a branch, and the name given otherwise, so git's own text
/// says what is missing.
fn merge_base(root: &Path, from: &str, other: &str) -> Result<String> {
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
fn main_worktree(root: &Path) -> Result<PathBuf> {
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

/// Why `tip` is no fast-forward of the track, `None` when it is one: the
/// track's own commit, or a commit that does not descend from it.
pub(crate) fn not_ahead(root: &Path, track: &Track, tip: &str) -> Result<Option<String>> {
    let old = rev(root, &track.at)?;
    if old == *tip {
        return Ok(Some(format!(
            "the branch is the track's own commit, {}",
            short(&old)
        )));
    }
    if !is_ancestor(root, &old, tip) {
        return Ok(Some(format!(
            "the track is at {}, which the branch does not descend from",
            short(&old)
        )));
    }
    Ok(None)
}

/// The track's branch moved to `tip` and pushed, by fast-forward alone:
/// `origin/<name>` through the push first, since a push refused leaves
/// nothing moved, then the clone's own branch where it has one, through
/// `merge --ff-only` where it is checked out and `update-ref` where it is
/// not; a detached `HEAD` at the old tip follows, since a track's clone
/// is detached at its track. `whose` prefixes the lines: `track work`
/// moves a track to a ticket's tip and `track sync` to main's.
pub(crate) fn fast_forward(whose: &str, root: &Path, track: &Track, tip: &str) -> Result<()> {
    let old = rev(root, &track.at)?;
    if let Some(why) = not_ahead(root, track, tip)? {
        bail!("{why}");
    }
    let name = &track.name;
    git(
        root,
        &["push", "-q", "origin", &format!("{tip}:refs/heads/{name}")],
        None,
    )
    .with_context(|| format!("git push refused origin/{name}"))?;
    if has_ref(root, &format!("refs/heads/{name}")) {
        if head_branch(root).as_deref() == Some(name.as_str()) {
            git(root, &["merge", "-q", "--ff-only", tip], None)
                .with_context(|| format!("{name} is checked out here and did not fast-forward"))?;
        } else {
            git(
                root,
                &["update-ref", &format!("refs/heads/{name}"), tip, &old],
                None,
            )
            .with_context(|| format!("{name} did not move"))?;
        }
    }
    say(
        whose,
        &format!(
            "{name} fast-forwarded {} to {} and pushed to origin",
            short(&old),
            short(tip)
        ),
    );
    if head_branch(root).is_none() && rev(root, "HEAD").is_ok_and(|head| head == old) {
        match git(root, &["checkout", "-q", "--detach", tip], None) {
            Ok(_) => say(
                whose,
                &format!("HEAD, detached at the track, follows it to {}", short(tip)),
            ),
            Err(err) => say(
                whose,
                &format!("HEAD stays detached at {}: {err:#}", short(&old)),
            ),
        }
    }
    Ok(())
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

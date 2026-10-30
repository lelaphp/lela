//! The merge queue (#3618 M2, #3622): merges into `main` are made in a
//! worktree of their own, `_merge` on the branch `staging`, and `main`
//! moves only by a fast-forward to a commit `done` was met at there.
//! main's checkout then has one writer that never merges, and a red merge
//! check is a `git reset --keep` in `_merge` with `main` where it stood.
//!
//! `xtask worktree queue` ([`claim`]) lays `_merge` down beside the clone
//! as `worktree add` lays a ticket's, cutting `staging` from `main` where
//! no such branch is, seeding its `target/` copy-on-write from main's for
//! each build the merge lanes run, and records the calling session as
//! its owner ([`crate::owner`]); where it stands already, the call claims
//! it, refused where another session owns it. `xtask worktree land`
//! ([`land`]) is the one step that moves `main`: it asks `done` in
//! `_merge` since main's tip, and on a pass lands `_merge`'s readings in
//! main's store, as a retire does, then fast-forwards `main` to the
//! commit judged. The worktree stays, since the queue is persistent, and
//! its per-key files stay with it, so `done` there still finds them.
//!
//! Each step is the one GitHub's merge queue takes, so moving there later
//! replaces the runner and not the protocol: `staging` is the queue's
//! `gh-readonly-queue/main/…` branch, the merge lanes in `_merge` are the
//! required `merge_group` status, and [`land`] is the queue's
//! fast-forward of `main` to a group that passed.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::divergence::Registry;
use crate::done;
use crate::git::{git, has_ref};
use crate::manifest::Manifest;
use crate::owner::{self, SessionId};
use crate::reading::Store;
use crate::track::{self, MAIN, short};
use crate::worktree::{self, say};

/// The branch the queue's merges are made on, which `main` is
/// fast-forwarded to: GitHub's `gh-readonly-queue/main/…`.
pub(crate) const STAGING: &str = "staging";

/// The merge worktree's directory, among the worktrees beside the clone.
const DIR: &str = "_merge";

/// The builds the merge lanes run, seeded from main's: the hook's, the
/// command hook's harness and the lanes' (specs §6).
const BUILDS: [&str; 3] = ["debug", "release", "release-da"];

const QUEUE: &str = "worktree queue";
const LAND: &str = "worktree land";

/// The merge worktree of the repository whose main checkout is `main`,
/// the worktree on [`STAGING`], where one stands; an error for one on
/// [`STAGING`] somewhere other than `_merge`.
fn standing(main: &Path) -> Result<Option<PathBuf>> {
    let dir = worktree::homes(main)?.join(DIR);
    let on = track::worktrees(main)?
        .into_iter()
        .find_map(|(path, on)| (on.as_deref() == Some(STAGING)).then_some(path));
    match on {
        Some(path) if same(&path, &dir) => Ok(Some(path)),
        Some(path) => bail!(
            "{STAGING} is checked out at {}, and the merge worktree is {}; retire that worktree and call again",
            path.display(),
            dir.display()
        ),
        None => Ok(None),
    }
}

/// Whether two paths name one directory.
fn same(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Lay the merge worktree down, or claim the one that stands, for
/// `owner`: refused where another session owns it. The directory.
pub fn claim(root: &Path, owner: Option<&SessionId>) -> Result<PathBuf> {
    let main = track::main_worktree(root)?;
    if let Some(merge) = standing(&main)? {
        if let Some(other) = owner::another(&merge, owner)? {
            bail!(
                "{} is session {other}'s; {other} lets it go with `xtask worktree own --release` there, or a person hands it over with `xtask worktree own --session <id>`",
                merge.display()
            );
        }
        say!(QUEUE, "{} stands on {STAGING}", merge.display());
        worktree::record_owner(&merge, owner, QUEUE)?;
        return Ok(merge);
    }
    let homes = worktree::homes(&main)?;
    let dir = homes.join(DIR);
    if dir.exists() {
        bail!(
            "{} exists and is no worktree on {STAGING}; a person removes it",
            dir.display()
        );
    }
    if !has_ref(&main, &format!("refs/heads/{STAGING}")) {
        git(&main, &["branch", STAGING, MAIN])?;
        say!(QUEUE, "{STAGING} cut from {MAIN}");
    }
    git(
        &main,
        &["worktree", "add", "-q", &dir.to_string_lossy(), STAGING],
    )
    .context("git worktree add refused; nothing was laid down")?;
    let head = track::rev(&dir, "HEAD")?;
    say!(QUEUE, "{} on {STAGING} at {}", dir.display(), short(&head));
    worktree::record_owner(&dir, owner, QUEUE)?;
    worktree::make_ready(dir, &main, &homes, QUEUE, &BUILDS)
}

/// A commit of [`STAGING`] that `done` was met at in the merge worktree,
/// since main's tip: the one thing `main` is fast-forwarded to.
struct Passed {
    head: String,
}

impl Passed {
    /// `done` over the merge worktree since `tip`, its reading written
    /// to that worktree's store and its list printed; the head it judged,
    /// or the refusal that leaves `main` at `tip`. Work standing
    /// uncommitted there is refused first: it would be judged and not
    /// landed.
    fn judge(merge: &Path, tip: &str) -> Result<Passed> {
        let untouched = format!("{MAIN} is untouched at {}", short(tip));
        let standing = git(merge, &["status", "--porcelain", "--untracked-files=no"])?;
        if !standing.trim().is_empty() {
            bail!(
                "{} holds uncommitted work, which done would judge and the fast-forward would not take; commit it or `git reset --keep`; {untouched}",
                merge.display()
            );
        }
        let head = track::rev(merge, "HEAD")?;
        done::run(
            merge,
            &Manifest::load(merge)?,
            &Registry::load(merge)?,
            &Store::of_workspace(merge),
            Some(tip),
            &[],
        )
        .with_context(|| {
            format!(
                "done is unmet in {} since {MAIN}; {untouched}",
                merge.display()
            )
        })?;
        Ok(Passed { head })
    }
}

/// Fast-forward `main` to the merge worktree's head once `done` is met
/// there since main's tip ([`Passed`]), landing the worktree's readings
/// in main's store first, so the hooks the fast-forward runs and
/// everything after them find them by key (ADR-0011). A `main` that moved
/// past what `staging` holds, a person's commit there, is refused, and
/// so is a main checkout off `main`. With nothing to fast-forward, the
/// readings are landed alone.
pub fn land(root: &Path) -> Result<()> {
    let main = track::main_worktree(root)?;
    let merge = standing(&main)?.with_context(|| {
        format!("no merge worktree stands on {STAGING}; `xtask worktree queue` lays it down")
    })?;
    if track::head_branch(&main).as_deref() != Some(MAIN) {
        bail!("{} is not on {MAIN}; nothing was landed", main.display());
    }
    let tip = track::rev(&main, "HEAD")?;
    let head = track::rev(&merge, "HEAD")?;
    let behind = git(&merge, &["merge-base", "--is-ancestor", &tip, &head]).is_err();
    if behind {
        bail!(
            "{MAIN} moved to {} past what {STAGING} holds: in {}, merge {MAIN} with `--no-commit`, commit it and run the merge lanes again; {MAIN} is untouched",
            short(&tip),
            merge.display()
        );
    }
    let passed = (head != tip)
        .then(|| Passed::judge(&merge, &tip))
        .transpose()?;
    append(&merge, &main)?;
    let Some(Passed { head }) = passed else {
        say!(
            LAND,
            "{STAGING} is {MAIN}'s tip, {}; nothing to fast-forward",
            short(&tip)
        );
        return Ok(());
    };
    let status = Command::new("git")
        .args(["merge", "--ff-only", "-q", &head])
        .current_dir(&main)
        .status()
        .context("git did not run")?;
    if !status.success() {
        bail!(
            "git merge --ff-only {} refused in {}; the readings are landed, so call again once it would",
            short(&head),
            main.display()
        );
    }
    say!(
        LAND,
        "{MAIN} fast-forwarded from {} to {}; push it: git -C {} push origin {MAIN}",
        short(&tip),
        short(&head),
        main.display()
    );
    Ok(())
}

/// The merge worktree's log landed in main's store ([`Store::land`]) and
/// emptied there, so a second call lands nothing twice; its per-key files
/// stay with it, and so does the worktree.
fn append(merge: &Path, main: &Path) -> Result<()> {
    let (from, store) = (Store::of_workspace(merge), Store::of_workspace(main));
    let landed = store.land(&from).with_context(|| {
        format!(
            "cannot land {} in {}",
            from.root().display(),
            store.root().display()
        )
    })?;
    say!(
        LAND,
        "landed {landed} lines of {}'s log in {}; the worktree stays",
        merge.display(),
        store.root().display()
    );
    Ok(())
}

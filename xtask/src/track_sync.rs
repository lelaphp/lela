//! A track caught up with main (#80; harness-design.md §5): `xtask track
//! sync`. A harness fix that lands on main under `track:main` reaches a
//! track only if something moves the track, and until now nothing did:
//! both experiment clones sat behind main by every commit since they
//! were laid down. From inside a track's clone the command fetches
//! `origin`, merges `origin/main` into the track's branch, pushes the
//! branch to `origin`, re-detaches the clone at the new tip and builds
//! `xtask` there, so the track never runs a harness older than main's.
//! `track work` runs it before each ticket, the first time being the
//! start of the run, so the ticket's worktree is laid down off a track
//! that has main.
//!
//! The merge is made without the working tree: `git merge-tree
//! --write-tree` writes the merged tree or names the files that
//! conflict, and `commit-tree` makes the commit from it, so a merge that
//! conflicts refuses with the conflict named and the branch, `origin`
//! and the clone as they were, with no merge in progress to abort. A
//! track that is at main's tip, or ahead of it, moves nothing and builds
//! nothing; a track that main has moved past with no commit of its own
//! fast-forwards to main rather than making a merge commit. The push,
//! the clone's own branch where it has one and `HEAD` following are
//! [`track::fast_forward`]'s, the same act `track work` does to the
//! track at a ticket's tip; a detached `HEAD` that was not at the
//! track's tip is re-detached at the new one all the same, since the
//! build that follows is of the checkout, and a `HEAD` on a branch of
//! the track is a worktree's or a person's checkout, so the sync refuses
//! before anything moves. On `main`, which is no track's, there is nothing to catch up
//! with, and the line says so. A command, not an instrument: no reading
//! is written.

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::git::{git, is_ancestor};
use crate::track::{MAIN, Track, fast_forward, head_branch, rev, short};
use crate::worktree::build;

/// The prefix of every line printed, so a person reads whose line it is.
const SYNC: &str = "track sync";

/// How the track took main's tip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum How {
    /// The track had no commit of its own past main, so it is main's tip.
    FastForward,
    /// Main was merged into the track as one commit, `commit`, with the
    /// track's tip as its first parent and main's as its second.
    Merge { commit: String },
}

/// What the sync did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Synced {
    /// The track is main; nothing is above it to catch up with.
    Main,
    /// The track has main's tip, `main`, already; nothing moved.
    Already { track: String, main: String },
    /// The track moved from `from` to a tip that has main's tip, `main`,
    /// pushed, HEAD detached at it and `xtask` built there.
    Moved {
        track: String,
        from: String,
        main: String,
        how: How,
    },
}

impl Synced {
    /// The one line: what moved.
    pub fn render(&self) -> String {
        match self {
            Synced::Main => format!(
                "{SYNC}: the track is {MAIN}, which has nothing to catch up with; nothing moved\n"
            ),
            Synced::Already { track, main } => format!(
                "{SYNC}: {track} has {MAIN}'s tip ({}) already; nothing moved\n",
                short(main)
            ),
            Synced::Moved {
                track,
                from,
                main,
                how,
            } => {
                let how = match how {
                    How::FastForward => {
                        format!("fast-forwarded {} to {}", short(from), short(main))
                    }
                    How::Merge { commit } => {
                        format!("merged it into {} as {}", short(from), short(commit))
                    }
                };
                format!(
                    "{SYNC}: {track} caught up with {MAIN} ({}): {how}; pushed to origin, HEAD follows, xtask built\n",
                    short(main)
                )
            }
        }
    }
}

/// The track of the clone at `root` caught up with main: fetched,
/// merged, pushed, re-detached and built; or refused with the conflict
/// named and nothing changed.
pub fn sync(root: &Path) -> Result<Synced> {
    let track = Track::of(root, None)?;
    if track.name == MAIN {
        return Ok(Synced::Main);
    }
    if let Some(branch) = head_branch(root)
        && branch != track.name
    {
        bail!(
            "HEAD is on {branch}, not detached at the track {}; a track's clone is, and this checkout is a person's, so nothing moved",
            track.name
        );
    }
    git(root, &["fetch", "-q", "origin"], None)
        .context("git fetch refused, so main's tip is not known here; nothing moved")?;
    let main = rev(root, &format!("refs/remotes/origin/{MAIN}"))
        .context("origin has no main to catch up with")?;
    let from = rev(root, &track.at)?;
    if is_ancestor(root, &main, &from) {
        return Ok(Synced::Already {
            track: track.name,
            main,
        });
    }
    let (tip, how) = if is_ancestor(root, &from, &main) {
        (main.clone(), How::FastForward)
    } else {
        let commit = merge(root, &track.name, &from, &main)?;
        (commit.clone(), How::Merge { commit })
    };
    fast_forward(SYNC, root, &track, &tip).with_context(|| {
        format!(
            "{} did not move to {}, which has {MAIN}'s tip; nothing is pushed",
            track.name,
            short(&tip)
        )
    })?;
    if head_branch(root).is_none() && rev(root, "HEAD")? != tip {
        git(root, &["checkout", "-q", "--detach", &tip], None).with_context(|| {
            format!(
                "{} is caught up with {MAIN} and pushed, but the clone could not be detached at {}; `git checkout --detach {}` there, then `cargo build -p xtask`",
                track.name,
                short(&tip),
                short(&tip)
            )
        })?;
    }
    if !build(SYNC, root) {
        bail!(
            "{} is caught up with {MAIN} and pushed, but xtask did not build at {}, so its harness is older than {MAIN}'s until `cargo build -p xtask` there",
            track.name,
            root.display()
        );
    }
    Ok(Synced::Moved {
        track: track.name,
        from,
        main,
        how,
    })
}

/// One commit merging `main` into the track at `from`, made from the
/// merged tree without the working tree; a conflict is the error naming
/// the files, with nothing written that a ref points at.
fn merge(root: &Path, name: &str, from: &str, main: &str) -> Result<String> {
    let merged = match merge_tree(root, from, main)? {
        Merged::Tree(tree) => tree,
        Merged::Conflicts(conflicts) => bail!(
            "{MAIN} does not merge into {name}: conflicts in {}; the branch and the clone are as they were, and a person merges {MAIN} ({}) into {name} ({}) by hand",
            conflicts.join(", "),
            short(main),
            short(from)
        ),
    };
    let message = format!("{SYNC}: {name} caught up with {MAIN} at {}", short(main));
    let commit = git(
        root,
        &[
            "commit-tree",
            &merged,
            "-p",
            from,
            "-p",
            main,
            "-m",
            &message,
        ],
        None,
    )
    .context("the merge commit could not be made; nothing moved")?;
    Ok(commit.trim().to_owned())
}

/// What `merge-tree` wrote: the merged tree, or the files that conflict.
enum Merged {
    Tree(String),
    Conflicts(Vec<String>),
}

/// `git merge-tree --write-tree --name-only`: the merged tree's id on
/// exit 0, the conflicting files on exit 1, both on stdout, which is
/// why this is not [`git`]; any other exit is git's error.
fn merge_tree(root: &Path, from: &str, main: &str) -> Result<Merged> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["merge-tree", "--write-tree", "--name-only", from, main])
        .stdin(Stdio::null())
        .output()
        .context("running git merge-tree")?;
    let text = String::from_utf8_lossy(&out.stdout);
    match out.status.code() {
        Some(0) => Ok(Merged::Tree(text.trim().to_owned())),
        Some(1) => Ok(Merged::Conflicts(conflicts(&text))),
        _ => bail!(
            "git merge-tree {} {}: {}",
            short(from),
            short(main),
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// The files `merge-tree --name-only` names as conflicting: the lines
/// after the tree's id, up to the blank line before its messages.
fn conflicts(text: &str) -> Vec<String> {
    text.lines()
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .map(|line| line.trim().to_owned())
        .collect()
}

/// `xtask track sync`: the line printed.
pub fn command(root: &Path) -> Result<()> {
    print!("{}", sync(root)?.render());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{How, Synced, conflicts};

    #[test]
    fn the_conflicting_files_are_the_lines_between_the_tree_and_the_messages() {
        let text =
            "9d0cf984\nf\ndir/g\n\nAuto-merging f\nCONFLICT (content): Merge conflict in f\n";
        assert_eq!(conflicts(text), vec!["f".to_owned(), "dir/g".to_owned()]);
        assert!(conflicts("9d0cf984\n").is_empty());
    }

    #[test]
    fn the_line_says_what_moved() {
        let moved = Synced::Moved {
            track: "t-x".to_owned(),
            from: "1111111aaaa".to_owned(),
            main: "2222222bbbb".to_owned(),
            how: How::Merge {
                commit: "3333333cccc".to_owned(),
            },
        };
        assert_eq!(
            moved.render(),
            "track sync: t-x caught up with main (2222222): merged it into 1111111 as 3333333; pushed to origin, HEAD follows, xtask built\n"
        );
        assert_eq!(
            Synced::Main.render(),
            "track sync: the track is main, which has nothing to catch up with; nothing moved\n"
        );
    }
}

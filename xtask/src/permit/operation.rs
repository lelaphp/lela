//! The git operation in progress in a checkout, and the checkout a call
//! runs in (#2476, harness-design.md §4.4). A merge, cherry-pick, revert,
//! rebase or `am` in progress ends in a commit of the whole index, so a
//! path another session stages in that checkout lands in that commit: on
//! 2026-10-09 a settled divergence row staged by pathspec in main's
//! checkout rode into `1bd555d0`, an unrelated merge. Whose the operation
//! is, is whose the checkout is: one session writes in a checkout, the
//! one its owner record names ([`crate::owner`], #3618 M1, #3621), so a
//! rule carrying `where = "operating"` refuses the operation's steps to
//! every other session ([`super::owned`]) and its owner's `git merge
//! --abort` and commit pass. Who started an operation is recorded nowhere.
//!
//! What `permit --after` reads here is where a call ran ([`ran_in`]), the
//! commit `HEAD` stands at ([`head`]) and whether the operation in
//! progress has staged a path ([`staged`]).

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use super::argv::Line;
use super::shell::acts_in;
use crate::git;

/// An operation that ends in a commit of the whole index, by the marker
/// git keeps in the checkout's git dir while it stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Operation {
    Merge,
    CherryPick,
    Revert,
    Rebase,
    /// `git am`, which stands in `rebase-apply/` beside its `applying`
    /// marker.
    Am,
}

impl Operation {
    /// The operation standing in `git_dir`, if one does.
    pub(super) fn in_progress(git_dir: &Path) -> Option<Operation> {
        Operation::marker(git_dir).map(|(operation, _)| operation)
    }

    /// The operation standing in `git_dir` and the marker git keeps for it.
    fn marker(git_dir: &Path) -> Option<(Operation, PathBuf)> {
        [
            ("MERGE_HEAD", Operation::Merge),
            ("CHERRY_PICK_HEAD", Operation::CherryPick),
            ("REVERT_HEAD", Operation::Revert),
            ("rebase-apply/applying", Operation::Am),
            ("rebase-merge", Operation::Rebase),
            ("rebase-apply", Operation::Rebase),
        ]
        .into_iter()
        .map(|(marker, operation)| (operation, git_dir.join(marker)))
        .find(|(_, marker)| marker.exists())
    }
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Operation::Merge => "a merge",
            Operation::CherryPick => "a cherry-pick",
            Operation::Revert => "a revert",
            Operation::Rebase => "a rebase",
            Operation::Am => "an am",
        })
    }
}

/// The checkout a call runs in: where it acts ([`acts_in`]), its
/// `workspace` moved by the line's leading `cd`, `pushd` and `popd`, or
/// the one directory git's `-C` names where every command past them is
/// git, at the top of its worktree, a linked one's own; the workspace
/// where that names no checkout, as it is for a `cd` to a variable the
/// line did not assign. What `permit --after` holds, and the one
/// checkout it puts a file back in (#2597, #2607). A line whose commands
/// run in several checkouts is held where its shell stands, since a
/// `-C` moves one command and the call is held in one.
pub(super) fn ran_in(workspace: &Path, line: Option<&Line>) -> PathBuf {
    let top = git::rev_parse(&acts_in(workspace, line), &["--show-toplevel"]);
    let top = top.map_or_else(|_| workspace.to_path_buf(), PathBuf::from);
    fs::canonicalize(&top).unwrap_or(top)
}

/// The operation standing in `checkout`, if one does.
pub(super) fn standing_in(checkout: &Path) -> Option<Operation> {
    Operation::in_progress(&git::git_dir(checkout)?)
}

/// The operation standing in `checkout` that has staged `path`, its index
/// entry other than `HEAD`'s and not a conflict's: the operation's commit
/// carries it, so `permit --after` never puts it back (#2597), where a
/// conflicted path it leaves to the hand that resolves it (#2880). An
/// index git cannot read is read as staged, the side that puts nothing
/// back. What the index cannot say is whose hand staged the entry, so a
/// call's own `git add` while the operation stands is read as the
/// operation's and rides its commit; another session's staging there is
/// what a `where = "operating"` rule refuses at the call
/// ([`super::owned`]).
pub(super) fn staged(checkout: &Path, path: &str) -> Option<Operation> {
    let operation = Operation::in_progress(&git::git_dir(checkout)?)?;
    let args = ["diff", "--cached", "--quiet", "--diff-filter=u", "--", path];
    let differs = git::git(checkout, &args).is_err();
    differs.then_some(operation)
}

/// The commit `HEAD` stands at in `checkout`, its full id; `None` for an
/// unborn one.
pub(super) fn head(checkout: &Path) -> Option<String> {
    git::rev_parse(checkout, &["--verify", "--quiet", "HEAD"]).ok()
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::Line;
    use crate::scratch::Tree;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(crate::scratch::UNHOOKED)
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {}", args.join(" "));
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    }

    /// A repository of one commit on `main`, with the merge marker git
    /// writes when a merge stops before its commit.
    fn repository(name: &str) -> Tree {
        let dir = Tree::new(name).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["commit", "-q", "--allow-empty", "-m", "one"]);
        dir
    }

    /// A linked worktree of `dir` at `linked`, on a branch of its own.
    fn worktree(dir: &Path, linked: &Path) {
        git(
            dir,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "t1",
                linked.to_str().unwrap(),
            ],
        );
    }

    /// The checkout a call ran in is its workspace moved by a leading
    /// `cd`, past the assignments before it, at the top of the worktree
    /// the `cd` names; a `cd` into no checkout leaves the workspace.
    #[test]
    fn a_call_ran_in_the_top_of_the_worktree_its_leading_cd_names() {
        let dir = repository("operation-ran-in");
        let base = Tree::new("operation-ran-in-linked").unwrap();
        let linked = base.join("t1");
        worktree(&dir, &linked);
        std::fs::create_dir(linked.join("sub")).unwrap();
        let top = linked.canonicalize().unwrap();
        let ran_in = |line: &str| super::ran_in(&dir, Some(&Line::read(line)));
        let at = linked.display();
        assert_eq!(ran_in(&format!("cd {at}/sub && touch x")), top);
        assert_eq!(ran_in(&format!("S=/tmp/s && cd {at} && touch x")), top);
        let exported = format!("export A=1 B=2 && cd {at} && touch x");
        assert_eq!(ran_in(&exported), top);
        let workspace = dir.canonicalize().unwrap();
        assert_eq!(ran_in("touch x"), workspace);
        assert_eq!(ran_in("cd /var/empty/none && touch x"), workspace);
    }

    /// #2607: a `cd` to a variable the line assigned before it, plainly or
    /// by `export`, and a `pushd` move the call to the worktree they name,
    /// and a `popd` returns to where the `pushd` left; git's `-C` does
    /// where every command past them is git and names one directory. A
    /// change the line cannot read leaves it where it stood, the
    /// workspace for the first.
    #[test]
    fn a_call_ran_in_the_worktree_an_assigned_cd_a_pushd_or_git_s_c_names() {
        let dir = repository("operation-ran-in-shapes");
        let base = Tree::new("operation-ran-in-shapes-linked").unwrap();
        let linked = base.join("t1");
        worktree(&dir, &linked);
        std::fs::create_dir(linked.join("sub")).unwrap();
        let top = linked.canonicalize().unwrap();
        let workspace = dir.canonicalize().unwrap();
        let ran_in = |line: &str| super::ran_in(&dir, Some(&Line::read(line)));
        let at = linked.display();
        assert_eq!(ran_in(&format!("W={at} && cd \"$W\" && touch x")), top);
        assert_eq!(ran_in(&format!("W={at}; cd $W/sub && touch x")), top);
        // #2995: a `cd` into a file joined by `;` fails, and the line may
        // run where it started as well, so it is held there; one into a
        // directory it can enter does not fail (#3257).
        assert_eq!(ran_in(&format!("W={at}; cd $W/.git; touch x")), workspace);
        assert_eq!(ran_in(&format!("W={at}; cd $W/sub; touch x")), top);
        let exported = format!("export A=1 W={at} && cd \"${{W}}\" && touch x");
        assert_eq!(ran_in(&exported), top);
        assert_eq!(ran_in("cd \"$UNSET_BY_THE_LINE\" && touch x"), workspace);
        assert_eq!(ran_in(&format!("W={at} cd \"$W\" && touch x")), workspace);
        assert_eq!(ran_in(&format!("pushd {at}/sub && touch x")), top);
        assert_eq!(ran_in(&format!("pushd {at} && popd && touch x")), workspace);
        assert_eq!(ran_in("popd && touch x"), workspace);
        let flagged = format!("git -C {at} checkout -- x && git -C {at} status");
        assert_eq!(ran_in(&flagged), top);
        let assigned = format!("export W={at} && git -C \"$W\" checkout -- x");
        assert_eq!(ran_in(&assigned), top);
        assert_eq!(ran_in(&format!("cd {at} && git -C sub status")), top);
        // A `-C` moves its command, not the shell: a line whose other
        // commands run where the shell stands is held there.
        let later = format!("touch x && git -C {at} status");
        assert_eq!(ran_in(&later), workspace);
        let beside = format!("git -C {at} status && git checkout -- x");
        assert_eq!(ran_in(&beside), workspace);
        let after = format!("git -C {at} status && touch x");
        assert_eq!(ran_in(&after), workspace);
        // A change the line cannot read leaves it where it stood.
        assert_eq!(ran_in(&format!("cd {at} && cd \"$UNSET\" && touch x")), top);
        assert_eq!(ran_in(&format!("cd {at} && popd && touch x")), top);
        assert_eq!(ran_in(&format!("pushd -n {at} && touch x")), workspace);
    }
}

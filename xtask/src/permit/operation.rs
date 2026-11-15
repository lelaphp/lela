//! The git operation in progress in a checkout (harness-design.md
//! §4.4). A merge, cherry-pick, revert,
//! rebase or `am` in progress ends in a commit of the whole index, so a
//! path another session stages in that checkout lands in that commit: on
//! 2026-10-09 a settled divergence row staged by pathspec in main's
//! checkout rode into `1bd555d0`, an unrelated merge. Whose the operation
//! is, is whose the checkout is: one session writes in a checkout, the
//! one its owner record names ([`crate::owner`], harness revision 179), so
//! a rule carrying `where = "operating"` refuses the operation's steps to
//! every other session ([`super::owned`]) and its owner's `git merge
//! --abort` and commit pass. Who started an operation is recorded nowhere.
//!
//! What `permit --after` reads here is the commit `HEAD` stands at
//! ([`head`]) and whether the operation in progress has staged a path
//! ([`staged`]); where a call ran is the walk's ([`super::shell::Walk::ran_in`]).

use std::fmt;
use std::path::{Path, PathBuf};

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

/// The operation standing in `checkout`, if one does.
pub(super) fn standing_in(checkout: &Path) -> Option<Operation> {
    Operation::in_progress(&git::git_dir(checkout)?)
}

/// The operation standing in `checkout` that has staged `path`, its index
/// entry other than `HEAD`'s and not a conflict's: the operation's commit
/// carries it, so `permit --after` never puts it back, where a
/// conflicted path it leaves to the hand that resolves it. An
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

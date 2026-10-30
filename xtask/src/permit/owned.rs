//! The checkouts a writing call is placed in, and whose each is (#3618
//! M1, #3621): what a rule scoped `where = "owned"` refuses, a write in
//! a checkout another session owns, and what one scoped `where =
//! "operating"` refuses, a step of an operation in progress there. The
//! owner is the checkout's record ([`crate::owner`]); the session is the
//! call's, which a subagent's calls share with the session that started
//! it. A call that names no session, the diff judge's, is judged by
//! neither: pre-commit asks its own question of a commit (`xtask worktree
//! whose`), and a checkout no record names is no session's, and nothing
//! is refused there.
//!
//! An `Edit` or a `Write` is placed by its file. A shell line is placed
//! where each command the rule fires on runs or reaches ([`fired_in`]),
//! and, for `where = "owned"`, where each redirection it makes writes,
//! read where its shell stands; a target spelled through a variable or
//! `~` is no place the line says, and is not read. For `where =
//! "operating"` a command is placed where git reads its repository, in
//! the git dir `GIT_DIR` or `--git-dir` names in place of where it runs
//! ([`Places::Repositories`], #3271).

use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::argv::{Argv, Line};
use super::call::Call;
use super::operation::Operation;
use super::rules::{Places, fired_in, runs_in};
use crate::git;
use crate::owner::{self, SessionId};

/// A checkout another session owns where the call would write: what a
/// rule scoped `where = "owned"` or `where = "operating"` refuses, said
/// in its denial.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Owned {
    pub(crate) checkout: PathBuf,
    pub(crate) owner: SessionId,
    /// The operation in progress there, which `where = "operating"` asks
    /// for.
    pub(crate) operation: Option<Operation>,
}

impl fmt::Display for Owned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (checkout, owner) = (self.checkout.display(), &self.owner);
        match self.operation {
            Some(operation) => write!(
                f,
                "{operation} is in progress in {checkout}, which session {owner} owns."
            ),
            None => write!(f, "{checkout} is session {owner}'s."),
        }
    }
}

/// Which of the two places a rule is scoped to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Scope {
    /// `where = "owned"`: any checkout another session owns.
    Owned,
    /// `where = "operating"`: one where an operation is in progress, too.
    Operating,
}

/// The first checkout another session owns among those `call` would write
/// in, for the commands of `line` that `fires`; `None` where every one is
/// the caller's or no session's, or the call names no session. A record
/// that does not read is an error, so the call is judged by no build
/// rather than let through.
pub(super) fn owned(
    root: &Path,
    call: &Call,
    line: Option<&Line>,
    scope: Scope,
    fires: impl Fn(&Argv) -> bool,
) -> Result<Option<Owned>> {
    let Some(caller) = SessionId::new(&call.session_id) else {
        return Ok(None);
    };
    let places = match (line, call.tool_input.file_path.as_deref()) {
        (Some(line), _) if scope == Scope::Operating => {
            fired_in(root, line, Places::Repositories, fires)
        }
        (Some(line), _) => {
            let mut places = fired_in(root, line, Places::Every, fires);
            places.extend(redirected(root, line));
            places
        }
        (None, Some(file)) => vec![root.join(file)],
        (None, None) => vec![root.to_path_buf()],
    };
    for place in places {
        let Some(checkout) = owner::checkout_of(&place) else {
            continue;
        };
        let operation = match scope {
            Scope::Owned => None,
            Scope::Operating => {
                match git::own_dir(&checkout).and_then(|dir| Operation::in_progress(&dir)) {
                    Some(operation) => Some(operation),
                    None => continue,
                }
            }
        };
        if let Some(owner) = owner::another(&checkout, Some(&caller))? {
            return Ok(Some(Owned {
                checkout,
                owner,
                operation,
            }));
        }
    }
    Ok(None)
}

/// Where the redirections of `line` write, each target read where the
/// line's shell stands ([`runs_in`]): one spelled through a variable or
/// `~`, a file descriptor (`>&2`) and a device are no file of a checkout.
pub(super) fn redirected(root: &Path, line: &Line) -> Vec<PathBuf> {
    let at = runs_in(root, Some(line));
    line.written()
        .iter()
        .filter(|target| {
            !target.starts_with(['$', '~', '&', '-'])
                && !target.starts_with("/dev/")
                && !target.chars().all(|c| c.is_ascii_digit())
        })
        .map(|target| at.join(target))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::{Line, Operation, Owned, Scope, owned};
    use crate::owner::{self, SessionId};
    use crate::permit::call::Call;
    use crate::permit::writes::changes;
    use crate::scratch::Tree;

    fn git(dir: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(crate::scratch::UNHOOKED)
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {}", args.join(" "));
    }

    /// A repository of one commit, owned by `the-owner`, with a linked
    /// worktree `linked` of no session's.
    fn owned_checkout(name: &str) -> Tree {
        let dir = Tree::new(name).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        git(&dir, &["commit", "-q", "--allow-empty", "-m", "one"]);
        git(&dir, &["worktree", "add", "-q", "-b", "t1", "linked"]);
        owner::record(&dir, &SessionId::new("the-owner").unwrap()).unwrap();
        dir
    }

    fn call(session: &str, input: &serde_json::Value) -> Call {
        let tool = if input.get("command").is_some() {
            "Bash"
        } else {
            "Edit"
        };
        let call = serde_json::json!({"tool_name": tool, "session_id": session,
            "tool_input": input});
        Call::parse(&call.to_string()).unwrap()
    }

    fn written(dir: &Path, session: &str, line: &str) -> Option<Owned> {
        let call = call(session, &serde_json::json!({"command": line}));
        let line = Line::read(line);
        owned(dir, &call, Some(&line), Scope::Owned, changes).unwrap()
    }

    /// #3621: another session's write in an owned checkout is placed there
    /// and named with its owner, by an edit's file, a command that
    /// changes the checkout, or a redirection into it, from wherever the
    /// shell stands; its reads, its writes elsewhere and the owner's own
    /// are not, nor a call that names no session.
    #[test]
    fn a_write_in_a_checkout_another_session_owns_is_placed_there() {
        let dir = owned_checkout("owned-writes");
        let top = dir.canonicalize().unwrap();
        let edit = |session: &str, file: &Path| {
            let call = call(
                session,
                &serde_json::json!({"file_path": file,
                "old_string": "a", "new_string": "b"}),
            );
            owned(&dir, &call, None, Scope::Owned, changes).unwrap()
        };
        let named = edit("another", &dir.join("AGENTS.md")).unwrap();
        assert_eq!(named.checkout, top);
        assert_eq!(named.owner.as_str(), "the-owner");
        assert_eq!(
            named.to_string(),
            format!("{} is session the-owner's.", top.display())
        );
        assert_eq!(edit("the-owner", &dir.join("AGENTS.md")), None);
        assert_eq!(edit("", &dir.join("AGENTS.md")), None);
        assert_eq!(edit("another", &dir.join("linked/AGENTS.md")), None);
        assert_eq!(edit("another", Path::new("/var/empty/x")), None);

        let elsewhere = dir.join("linked");
        let at = elsewhere.display();
        for line in [
            "git add -- a && git commit -F m -- a".to_owned(),
            "sed -i '' -e s/a/b/ AGENTS.md".to_owned(),
            "echo x >> notes.md".to_owned(),
            format!("cd {at} && git -C {} checkout -- a", dir.display()),
            format!("cd {at}; echo x > {}/notes.md", dir.display()),
        ] {
            let named = written(&dir, "another", &line);
            assert_eq!(named.map(|o| o.checkout), Some(top.clone()), "{line}");
            assert_eq!(written(&dir, "the-owner", &line), None, "{line}");
            let from_elsewhere = written(&elsewhere, "another", &line).is_some();
            assert_eq!(
                from_elsewhere,
                line.contains(&*dir.display().to_string()),
                "{line}"
            );
        }
        for line in [
            "git status --short && git log -3 > /tmp/log".to_owned(),
            "cat AGENTS.md 2>&1 >&2 | head -3 > $S/x".to_owned(),
            format!("cd {at} && cargo xtask test --crate xtask"),
            "cd /var/empty && touch x".to_owned(),
        ] {
            assert_eq!(written(&dir, "another", &line), None, "{line}");
        }
    }

    /// `where = "operating"` asks for an operation in progress besides the
    /// owner: none in progress, nothing is refused; one in progress, it is
    /// named with the owner, and the owner's own steps pass.
    #[test]
    fn an_operation_in_a_checkout_another_session_owns_is_named_with_its_owner() {
        let dir = owned_checkout("owned-operating");
        let stepped = |session: &str| {
            let line = Line::read("git merge --abort");
            let call = call(
                session,
                &serde_json::json!({"command": "git merge --abort"}),
            );
            owned(&dir, &call, Some(&line), Scope::Operating, |_| true).unwrap()
        };
        assert_eq!(stepped("another"), None);
        std::fs::write(dir.join(".git/MERGE_HEAD"), "0".repeat(40) + "\n").unwrap();
        let named = stepped("another").unwrap();
        assert_eq!(named.operation, Some(Operation::Merge));
        let top = dir.canonicalize().unwrap();
        assert_eq!(
            named.to_string(),
            format!(
                "a merge is in progress in {}, which session the-owner owns.",
                top.display()
            )
        );
        assert_eq!(stepped("the-owner"), None);
    }
}

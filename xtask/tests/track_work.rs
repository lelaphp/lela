//! `xtask track work`, one ticket from start to retire without a person
//! (#77): held against a scratch repository shaped as a track's clone,
//! detached at `origin/t-x` with the track and its model recorded, a bare
//! `origin` beside it, a tracker that is a shell file answering `issue
//! list` and `issue view` from recorded transcripts, as the `track_next`
//! tests do, and a session that is a shell file recording what it was
//! started with, committing on the branch and closing the ticket or not,
//! as the act the test wrote down says.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn sh(root: &Path, args: &[&str]) -> String {
    let out = Command::new(args[0])
        .args(&args[1..])
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "x")
        .env("GIT_AUTHOR_EMAIL", "x@x")
        .env("GIT_COMMITTER_NAME", "x")
        .env("GIT_COMMITTER_EMAIL", "x@x")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn rev(root: &Path, name: &str) -> String {
    sh(root, &["git", "rev-parse", name]).trim().to_owned()
}

fn has_ref(root: &Path, name: &str) -> bool {
    Command::new("git")
        .args([
            "-C",
            root.to_str().unwrap(),
            "rev-parse",
            "--verify",
            "--quiet",
            name,
        ])
        .output()
        .unwrap()
        .status
        .success()
}

/// The scratch: the track's clone at `repo`, its bare `origin.git`, the
/// two stand-ins under `bin`, the tracker's transcripts and what the
/// session wrote down.
struct Scratch {
    dir: PathBuf,
    root: PathBuf,
    origin: PathBuf,
}

impl Scratch {
    /// A track's clone of this working tree's files: one commit on
    /// `main`, pushed to `origin` as `main` and as `t-x`, the clone
    /// detached at `origin/t-x` recording `track.name` and the model per
    /// role, and the session program recorded as the stand-in.
    fn lay(name: &str) -> Scratch {
        let dir =
            std::env::temp_dir().join(format!("xtask-track-work-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let root = dir.join("repo");
        let origin = dir.join("origin.git");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&origin).unwrap();
        fs::create_dir_all(dir.join("bin")).unwrap();
        let root = fs::canonicalize(root).unwrap();
        let origin = fs::canonicalize(origin).unwrap();
        let dir = fs::canonicalize(dir).unwrap();
        let listed = sh(
            workspace(),
            &[
                "git",
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        );
        for file in listed.split('\0').filter(|f| !f.is_empty()) {
            let to = root.join(file);
            fs::create_dir_all(to.parent().unwrap()).unwrap();
            fs::copy(workspace().join(file), &to).unwrap_or_else(|e| panic!("{file}: {e}"));
        }
        sh(&origin, &["git", "init", "-q", "--bare", "-b", "main"]);
        sh(&root, &["git", "init", "-q", "-b", "main"]);
        sh(&root, &["git", "config", "core.hooksPath", "scripts/hooks"]);
        sh(&root, &["git", "add", "-A"]);
        sh(
            &root,
            &[
                "git",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/var/empty",
                "commit",
                "-q",
                "-m",
                "scratch",
            ],
        );
        sh(
            &root,
            &["git", "remote", "add", "origin", origin.to_str().unwrap()],
        );
        sh(
            &root,
            &["git", "push", "-q", "origin", "main", "main:refs/heads/t-x"],
        );
        sh(&root, &["git", "checkout", "-q", "--detach", "origin/t-x"]);
        sh(&root, &["git", "config", "track.name", "t-x"]);
        sh(
            &root,
            &["git", "config", "track.model.session", "claude-opus-5"],
        );
        sh(
            &root,
            &["git", "config", "track.model.subagent", "claude-sonnet-5"],
        );
        let scratch = Scratch { dir, root, origin };
        scratch.stand_ins();
        scratch
    }

    /// The tracker, `bin/gh`: `issue list` answers with `list.json`,
    /// `issue view` with `view.json`, and every other call is written to
    /// `calls`. The session, `bin/claude`: writes its arguments, its
    /// directory and the subagents' model down, then does what `act`
    /// says: `commit` commits a file on the branch, `close` flips the
    /// ticket's recorded state to closed, and either or both.
    fn stand_ins(&self) {
        let dir = self.dir.display();
        let gh = format!(
            "#!/bin/sh\nif [ \"$1\" = issue ] && [ \"$2\" = list ]; then cat '{dir}/list.json'; exit $?; fi\nif [ \"$1\" = issue ] && [ \"$2\" = view ]; then cat '{dir}/view.json'; exit $?; fi\nprintf '%s\\n' \"$*\" >> '{dir}/calls'\nexit 0\n"
        );
        let claude = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > '{dir}/session.args'\npwd -P > '{dir}/session.cwd'\nprintf '%s\\n' \"${{CLAUDE_CODE_SUBAGENT_MODEL-unset}}\" > '{dir}/session.subagent'\nact=$(cat '{dir}/act')\ncase \"$act\" in *commit*) echo worked > worked.txt && git add -- worked.txt && git -c commit.gpgsign=false -c core.hooksPath=/var/empty -c user.name=x -c user.email=x@x commit -q -m 'the work' -- worked.txt || exit 3;; esac\ncase \"$act\" in *close*) sed 's/\"OPEN\"/\"CLOSED\"/' '{dir}/view.json' > '{dir}/view.tmp' && mv '{dir}/view.tmp' '{dir}/view.json';; esac\nexit 0\n"
        );
        for (name, text) in [("gh", gh), ("claude", claude)] {
            let file = self.dir.join("bin").join(name);
            fs::write(&file, text).unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
        }
        sh(
            &self.root,
            &[
                "git",
                "config",
                "track.session.program",
                self.dir.join("bin/claude").to_str().unwrap(),
            ],
        );
    }

    /// The open tickets, each `ready-for-agent` and unblocked, and the
    /// ticket `issue view` answers with, open.
    fn tracker(&self, open: &[u64], viewed: u64) {
        let issues = open
            .iter()
            .map(|number| {
                serde_json::json!({
                    "number": number,
                    "title": format!("ticket {number}"),
                    "url": format!("https://example.invalid/issues/{number}"),
                    "labels": [{"name": "ready-for-agent"}],
                    "blockedBy": {"nodes": [], "totalCount": 0},
                })
            })
            .collect::<Vec<_>>();
        fs::write(
            self.dir.join("list.json"),
            serde_json::Value::Array(issues).to_string(),
        )
        .unwrap();
        fs::write(
            self.dir.join("view.json"),
            serde_json::json!({
                "number": viewed,
                "title": format!("ticket {viewed}"),
                "body": "## Acceptance criteria\n\n- [x] done\n",
                "state": "OPEN",
                "url": format!("https://example.invalid/issues/{viewed}"),
            })
            .to_string(),
        )
        .unwrap();
    }

    /// What the session does when started.
    fn act(&self, act: &str) {
        fs::write(self.dir.join("act"), act).unwrap();
    }

    /// The command, this crate's own binary, run against the clone with
    /// the stand-in tracker first on `PATH`.
    fn work(&self) -> (i32, String) {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .arg("--root")
            .arg(&self.root)
            .args(["track", "work"])
            .current_dir(&self.root)
            .env_remove("CARGO")
            .env_remove("CARGO_MANIFEST_DIR")
            .env_remove("CLAUDE_CODE_SUBAGENT_MODEL")
            .env("PATH", path)
            .env("GIT_AUTHOR_NAME", "x")
            .env("GIT_AUTHOR_EMAIL", "x@x")
            .env("GIT_COMMITTER_NAME", "x")
            .env("GIT_COMMITTER_EMAIL", "x@x")
            .output()
            .unwrap();
        let text = format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        (out.status.code().unwrap_or(-1), text)
    }

    fn wrote(&self, name: &str) -> String {
        fs::read_to_string(self.dir.join(name))
            .unwrap_or_default()
            .trim_end()
            .to_owned()
    }

    fn worktree(&self, branch: &str) -> PathBuf {
        self.root.join(".claude/worktrees").join(branch)
    }

    fn worktrees(&self) -> Vec<String> {
        sh(&self.root, &["git", "worktree", "list", "--porcelain"])
            .lines()
            .filter_map(|l| l.strip_prefix("worktree ").map(str::to_owned))
            .collect()
    }
}

/// The whole path: the next ticket, its worktree on `<track>/t<n>`, the
/// session started as `claude -p --model <session>` with the subagents'
/// model in its environment and the constants for the mode and the
/// effort, in the worktree, with the prompt running `/implement <n>`;
/// the ticket closed, so the track's branch is fast-forwarded to the
/// tip, pushed to `origin`, the worktree retired, the branch and its
/// record kept, HEAD following the track; one line naming the ticket,
/// the commit and the outcome; no trust line.
#[test]
fn a_closed_ticket_is_fast_forwarded_pushed_and_retired() {
    let scratch = Scratch::lay("closed");
    scratch.tracker(&[20, 21], 20);
    scratch.act("commit close");
    let before = rev(&scratch.root, "origin/t-x");
    let (code, out) = scratch.work();
    assert_eq!(code, 0, "{out}");

    let tip = rev(&scratch.root, "t-x/t20");
    assert_ne!(tip, before, "the session committed: {out}");
    assert_eq!(rev(&scratch.origin, "t-x"), tip, "pushed to origin: {out}");
    assert_eq!(rev(&scratch.root, "origin/t-x"), tip, "{out}");
    assert_eq!(
        rev(&scratch.root, "HEAD"),
        tip,
        "HEAD follows the track: {out}"
    );
    assert!(
        !has_ref(&scratch.root, "refs/heads/t-x"),
        "the track stays origin's branch: {out}"
    );
    assert!(
        out.lines().any(|line| line
            == format!(
                "track work: #20 closed at {}; t-x fast-forwarded to it and pushed to origin, t-x/t20 retired",
                &tip[..7]
            )),
        "{out}"
    );

    // The session was started as decided (#75), in the worktree.
    assert_eq!(
        scratch.wrote("session.args"),
        "-p --model claude-opus-5 --permission-mode bypassPermissions --effort high /implement 20"
    );
    assert_eq!(scratch.wrote("session.subagent"), "claude-sonnet-5");
    assert_eq!(
        scratch.wrote("session.cwd"),
        scratch.worktree("t-x/t20").to_str().unwrap()
    );

    // Retired: the worktree gone, the branch and its record kept.
    assert!(!scratch.worktree("t-x/t20").exists(), "{out}");
    assert_eq!(scratch.worktrees().len(), 1, "{out}");
    assert_eq!(
        sh(&scratch.root, &["git", "config", "branch.t-x/t20.ticket"]).trim(),
        "20"
    );

    // No trust line, no grant waited on (#74); the tracker was asked
    // nothing but the list and the view.
    assert!(!out.contains("computed trust"), "{out}");
    assert!(!out.contains("a person runs"), "{out}");
    assert_eq!(scratch.wrote("calls"), "", "{out}");

    let _ = fs::remove_dir_all(&scratch.dir);
}

/// A session that ends with the ticket open: the worktree stands, the
/// track's branch and `origin` are where they were, nothing is retired,
/// the record is kept and the exit is 1 with the reason; and the clone's
/// `track.permission-mode` and `track.effort` override the constants.
#[test]
fn a_ticket_left_open_leaves_the_worktree_standing() {
    let scratch = Scratch::lay("open");
    scratch.tracker(&[20], 20);
    scratch.act("commit");
    sh(
        &scratch.root,
        &["git", "config", "track.permission-mode", "acceptEdits"],
    );
    sh(&scratch.root, &["git", "config", "track.effort", "max"]);
    let before = rev(&scratch.root, "origin/t-x");
    let (code, out) = scratch.work();
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("#20 is open"), "{out}");
    assert!(
        out.contains(&format!(
            "stands at {}",
            scratch.worktree("t-x/t20").display()
        )),
        "{out}"
    );
    assert!(scratch.worktree("t-x/t20").is_dir(), "{out}");
    assert_eq!(scratch.worktrees().len(), 2, "{out}");
    assert_eq!(rev(&scratch.origin, "t-x"), before, "{out}");
    assert_eq!(rev(&scratch.root, "HEAD"), before, "{out}");
    assert_ne!(
        rev(&scratch.root, "t-x/t20"),
        before,
        "the work stands: {out}"
    );
    assert_eq!(
        scratch.wrote("session.args"),
        "-p --model claude-opus-5 --permission-mode acceptEdits --effort max /implement 20"
    );
    assert!(!out.contains("computed trust"), "{out}");
    let _ = fs::remove_dir_all(&scratch.dir);
}

/// A ticket closed with the branch at the track's own commit is not a
/// fast-forward: the track does not move, the worktree stands.
#[test]
fn a_tip_not_ahead_of_the_track_refuses() {
    let scratch = Scratch::lay("behind");
    scratch.tracker(&[20], 20);
    scratch.act("close");
    let before = rev(&scratch.root, "origin/t-x");
    let (code, out) = scratch.work();
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("not ahead of the track"), "{out}");
    assert_eq!(rev(&scratch.origin, "t-x"), before, "{out}");
    assert!(scratch.worktree("t-x/t20").is_dir(), "{out}");
    let _ = fs::remove_dir_all(&scratch.dir);
}

/// A clone with no model record refuses before any worktree is laid
/// down, and an empty frontier lays none down and is not an error.
#[test]
fn no_model_refuses_before_a_worktree_and_an_empty_frontier_is_none() {
    let scratch = Scratch::lay("refuse");
    scratch.tracker(&[20], 20);
    scratch.act("commit close");
    sh(
        &scratch.root,
        &["git", "config", "--unset", "track.model.subagent"],
    );
    sh(
        &scratch.root,
        &["git", "config", "--unset", "track.model.session"],
    );
    let (code, out) = scratch.work();
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("records no model"), "{out}");
    assert_eq!(scratch.worktrees().len(), 1, "{out}");
    assert!(!has_ref(&scratch.root, "refs/heads/t-x/t20"), "{out}");
    assert_eq!(scratch.wrote("session.args"), "", "no session started");

    sh(
        &scratch.root,
        &["git", "config", "track.model.session", "claude-opus-5"],
    );
    scratch.tracker(&[], 20);
    let (code, out) = scratch.work();
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("none: the frontier of t-x is empty"), "{out}");
    assert_eq!(scratch.worktrees().len(), 1, "{out}");
    assert_eq!(scratch.wrote("session.args"), "", "no session started");
    let _ = fs::remove_dir_all(&scratch.dir);
}

//! `xtask track work`, one ticket from start to retire without a person
//! (#77) and the frontier until it empties (#78): held against a scratch
//! repository shaped as a track's clone,
//! detached at `origin/t-x` with the track and its model recorded, a bare
//! `origin` beside it, a tracker that is a shell file answering `issue
//! list` and `issue view` from recorded transcripts, as the `track_next`
//! tests do, and a session that is a shell file recording what it was
//! started with, committing on the branch and closing the ticket or not,
//! as the act the test wrote down says, for every ticket or for one.

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
    /// `issue view <n>` with `view.<n>.json` where the test wrote one and
    /// `view.json` otherwise, and every other call is written to `calls`.
    /// The session, `bin/claude`: appends its arguments, its directory
    /// and the subagents' model to what earlier sessions wrote, then does
    /// what `act.<n>` says, or `act` where the test wrote none for the
    /// ticket: `commit` commits a file named for the ticket on the branch, `close` flips the
    /// ticket's recorded state to closed, and either or both. Started on
    /// `/frontier` (#85) it runs the shell file `act.frontier` instead,
    /// where the test writes what the planning session files.
    fn stand_ins(&self) {
        let dir = self.dir.display();
        let gh = format!(
            "#!/bin/sh\nif [ \"$1\" = issue ] && [ \"$2\" = list ]; then cat '{dir}/list.json'; exit $?; fi\nif [ \"$1\" = issue ] && [ \"$2\" = view ]; then f='{dir}/view.'$3'.json'; [ -f \"$f\" ] || f='{dir}/view.json'; cat \"$f\"; exit $?; fi\nprintf '%s\\n' \"$*\" >> '{dir}/calls'\nexit 0\n"
        );
        let claude = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{dir}/session.args'\npwd -P >> '{dir}/session.cwd'\nprintf '%s\\n' \"${{CLAUDE_CODE_SUBAGENT_MODEL-unset}}\" >> '{dir}/session.subagent'\n# the ticket: the last argument is the prompt, `/<skill> <n>`\nfor a; do last=$a; done; n=${{last##* }}\nif [ \"$last\" = /frontier ]; then sh '{dir}/act.frontier'; exit $?; fi\nact=$(cat '{dir}/act.'$n 2>/dev/null || cat '{dir}/act')\nview='{dir}/view.'$n'.json'; [ -f \"$view\" ] || view='{dir}/view.json'\ncase \"$act\" in *commit*) echo worked > worked.$n.txt && git add -- worked.$n.txt && git -c commit.gpgsign=false -c core.hooksPath=/var/empty -c user.name=x -c user.email=x@x commit -q -m 'the work' -- worked.$n.txt || exit 3;; esac\ncase \"$act\" in *close*) sed 's/\"OPEN\"/\"CLOSED\"/' \"$view\" > '{dir}/view.tmp' && mv '{dir}/view.tmp' \"$view\";; esac\nexit 0\n"
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
            .map(|&number| (number, ["ready-for-agent"].as_slice()))
            .collect::<Vec<_>>();
        fs::write(self.dir.join("list.json"), listed(&issues)).unwrap();
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

    /// One more label on the listed ticket `number`, as the tracker
    /// spells it: how a test puts a ticket on the frontier's research or
    /// decision side (#81).
    fn label(&self, number: u64, label: &str) {
        let file = self.dir.join("list.json");
        let mut issues: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        for issue in issues.as_array_mut().unwrap() {
            if issue["number"] == number {
                issue["labels"]
                    .as_array_mut()
                    .unwrap()
                    .push(serde_json::json!({"name": label}));
            }
        }
        fs::write(&file, issues.to_string()).unwrap();
    }

    /// The ticket `issue view <n>` answers with for `number` alone, open.
    fn viewed(&self, number: u64) {
        fs::write(
            self.dir.join(format!("view.{number}.json")),
            serde_json::json!({
                "number": number,
                "title": format!("ticket {number}"),
                "body": "## Acceptance criteria\n\n- [x] done\n",
                "state": "OPEN",
                "url": format!("https://example.invalid/issues/{number}"),
            })
            .to_string(),
        )
        .unwrap();
    }

    /// What the session does when started, for every ticket.
    fn act(&self, act: &str) {
        fs::write(self.dir.join("act"), act).unwrap();
    }

    /// What the session does when started for `number`.
    fn act_on(&self, number: u64, act: &str) {
        fs::write(self.dir.join(format!("act.{number}")), act).unwrap();
    }

    /// What the session started on `/frontier` does, as shell: the first
    /// time it replaces the tracker's open list with `list.next.json`,
    /// written from `filed`, the tickets it filed, each with its labels;
    /// the second time it files nothing (#85).
    fn frontier_files(&self, filed: &[(u64, &[&str])]) {
        fs::write(self.dir.join("list.next.json"), listed(filed)).unwrap();
        let dir = self.dir.display();
        fs::write(
            self.dir.join("act.frontier"),
            format!(
                "if [ ! -f '{dir}/planned' ]; then touch '{dir}/planned'; cp '{dir}/list.next.json' '{dir}/list.json'; fi\nexit 0\n"
            ),
        )
        .unwrap();
    }

    /// One more shell line the session runs after its act, before it
    /// ends: how a test breaks the harness from inside a session.
    fn session_then(&self, line: &str) {
        let file = self.dir.join("bin/claude");
        let text = fs::read_to_string(&file).unwrap();
        let (body, end) = text.rsplit_once("exit 0\n").unwrap();
        fs::write(&file, format!("{body}{line}\nexit 0\n{end}")).unwrap();
    }

    /// The command, this crate's own binary, run against the clone with
    /// the stand-in tracker first on `PATH`, `args` after `track work`.
    fn work(&self, args: &[&str]) -> (i32, String) {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .arg("--root")
            .arg(&self.root)
            .args(["track", "work"])
            .args(args)
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

    /// One commit on origin's `branch` writing `content` to `file`, made
    /// from a branch of the clone that is deleted after; the clone is
    /// left where it was, or detached at the commit when the branch is
    /// the track's, since a track's clone sits at its track's tip.
    fn land(&self, branch: &str, file: &str, content: &str) -> String {
        let head = rev(&self.root, "HEAD");
        sh(
            &self.root,
            &[
                "git",
                "checkout",
                "-q",
                "-b",
                "landing",
                &format!("origin/{branch}"),
            ],
        );
        fs::write(self.root.join(file), content).unwrap();
        sh(&self.root, &["git", "add", "--", file]);
        sh(
            &self.root,
            &[
                "git",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/var/empty",
                "commit",
                "-q",
                "-m",
                "landed",
            ],
        );
        let landed = rev(&self.root, "HEAD");
        sh(
            &self.root,
            &[
                "git",
                "-c",
                "core.hooksPath=/var/empty",
                "push",
                "-q",
                "origin",
                &format!("landing:refs/heads/{branch}"),
            ],
        );
        let back = if branch == "t-x" { &landed } else { &head };
        sh(&self.root, &["git", "checkout", "-q", "--detach", back]);
        sh(&self.root, &["git", "branch", "-q", "-D", "landing"]);
        landed
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

/// The tracker's open list, each ticket with its labels as the tracker
/// spells them, unblocked.
fn listed(issues: &[(u64, &[&str])]) -> String {
    let issues = issues
        .iter()
        .map(|(number, labels)| {
            serde_json::json!({
                "number": number,
                "title": format!("ticket {number}"),
                "url": format!("https://example.invalid/issues/{number}"),
                "labels": labels.iter().map(|name| serde_json::json!({"name": name})).collect::<Vec<_>>(),
                "blockedBy": {"nodes": [], "totalCount": 0},
            })
        })
        .collect::<Vec<_>>();
    serde_json::Value::Array(issues).to_string()
}

/// The whole path, `--one`: the next ticket, its worktree on `<track>/t<n>`, the
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
    let (code, out) = scratch.work(&["--one"]);
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

/// A session that ends with the ticket open, `--one`: the worktree stands, the
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
    let (code, out) = scratch.work(&["--one"]);
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
/// fast-forward: the track does not move, the worktree stands, and that
/// is the session's failure, so the run goes on to "none" and names the
/// ticket as standing.
#[test]
fn a_tip_not_ahead_of_the_track_stands() {
    let scratch = Scratch::lay("behind");
    scratch.tracker(&[20], 20);
    scratch.act("close");
    let before = rev(&scratch.root, "origin/t-x");
    let (code, out) = scratch.work(&[]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains("#20 closed, but the branch is not ahead of the track: the branch is the track's own commit"),
        "{out}"
    );
    assert!(
        out.contains("track work: the frontier of t-x is empty after 1 ticket: #20 standing"),
        "{out}"
    );
    assert_eq!(rev(&scratch.origin, "t-x"), before, "{out}");
    assert!(scratch.worktree("t-x/t20").is_dir(), "{out}");
    let _ = fs::remove_dir_all(&scratch.dir);
}

/// A clone with no model record refuses before any worktree is laid
/// down, and an empty frontier lays none down and is not an error: with
/// no `track.frontier` record the run ends at "none" and starts no
/// `/frontier` session, though the stand-in would file a ticket (#85).
#[test]
fn no_model_refuses_before_a_worktree_and_an_empty_frontier_is_none() {
    let scratch = Scratch::lay("refuse");
    scratch.tracker(&[20], 20);
    scratch.act("commit close");
    scratch.frontier_files(&[(21, &["ready-for-agent"])]);
    sh(
        &scratch.root,
        &["git", "config", "--unset", "track.model.subagent"],
    );
    sh(
        &scratch.root,
        &["git", "config", "--unset", "track.model.session"],
    );
    let (code, out) = scratch.work(&["--one"]);
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
    let (code, out) = scratch.work(&["--one"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("none: the frontier of t-x is empty"), "{out}");
    assert_eq!(scratch.worktrees().len(), 1, "{out}");
    assert_eq!(scratch.wrote("session.args"), "", "no session started");
    assert!(!scratch.dir.join("planned").exists(), "no planner: {out}");
    let _ = fs::remove_dir_all(&scratch.dir);
}

/// Without `--one`, the frontier until it empties: two tickets, the
/// session closing the first and leaving the second open. The first is
/// fast-forwarded, pushed and retired; the second's worktree stands for a
/// person and does not stop the run, since its branch records the
/// ticket and `track next` names it no more; the run ends at "none",
/// exit 0, one line per ticket and a closing line naming each with its
/// outcome.
#[test]
fn the_frontier_is_worked_until_it_empties_past_a_ticket_left_open() {
    let scratch = Scratch::lay("frontier");
    scratch.tracker(&[20, 21], 20);
    scratch.viewed(20);
    scratch.viewed(21);
    scratch.act_on(20, "commit close");
    scratch.act_on(21, "commit");
    let before = rev(&scratch.root, "origin/t-x");
    let (code, out) = scratch.work(&[]);
    assert_eq!(code, 0, "{out}");

    let first = rev(&scratch.root, "t-x/t20");
    let second = rev(&scratch.root, "t-x/t21");
    assert_ne!(first, before, "{out}");
    assert_eq!(
        rev(&scratch.origin, "t-x"),
        first,
        "pushed to origin: {out}"
    );
    assert_eq!(rev(&scratch.root, "HEAD"), first, "HEAD follows: {out}");
    assert!(!scratch.worktree("t-x/t20").exists(), "retired: {out}");
    assert!(scratch.worktree("t-x/t21").is_dir(), "stands: {out}");
    assert_eq!(scratch.worktrees().len(), 2, "{out}");
    assert_eq!(
        sh(&scratch.root, &["git", "merge-base", "t-x/t21", "t-x/t20"]).trim(),
        first,
        "the second was laid down on the moved track: {out}"
    );

    // Both sessions started, each in its own worktree.
    assert_eq!(
        scratch.wrote("session.args"),
        "-p --model claude-opus-5 --permission-mode bypassPermissions --effort high /implement 20\n-p --model claude-opus-5 --permission-mode bypassPermissions --effort high /implement 21"
    );
    assert_eq!(
        scratch.wrote("session.cwd"),
        format!(
            "{}\n{}",
            scratch.worktree("t-x/t20").display(),
            scratch.worktree("t-x/t21").display()
        )
    );

    // One line per ticket, then the closing line naming each with its outcome.
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines.contains(&format!(
            "track work: #20 closed at {}; t-x fast-forwarded to it and pushed to origin, t-x/t20 retired",
            &first[..7]
        ).as_str()),
        "{out}"
    );
    assert!(
        lines.contains(&format!(
            "track work: #21 is open after the session (exit status: 0), the branch at {}; the worktree stands at {} for a person, nothing retired",
            &second[..7],
            scratch.worktree("t-x/t21").display()
        ).as_str()),
        "{out}"
    );
    assert!(
        lines.contains(
            &"track work: the frontier of t-x is empty after 2 tickets: #20 closed, #21 standing"
        ),
        "{out}"
    );
    let _ = fs::remove_dir_all(&scratch.dir);
}

/// A failure of the harness stops the run and is named: the tracker
/// that answered the first ticket stops answering, so the second's
/// `track next` fails, after the first's line and with a closing line naming what
/// was worked.
#[test]
fn a_tracker_that_stops_answering_stops_the_run_named() {
    let scratch = Scratch::lay("tracker");
    scratch.tracker(&[20, 21], 20);
    scratch.viewed(20);
    scratch.viewed(21);
    scratch.act_on(20, "commit close");
    scratch.session_then(&format!(
        "rm -f '{}'",
        scratch.dir.join("list.json").display()
    ));
    let (code, out) = scratch.work(&[]);
    assert_eq!(code, 1, "{out}");
    assert!(!scratch.worktree("t-x/t20").exists(), "retired: {out}");
    assert!(!has_ref(&scratch.root, "refs/heads/t-x/t21"), "{out}");
    assert!(
        out.contains("the open tickets could not be listed from the tracker"),
        "{out}"
    );
    assert!(
        out.contains("stopped by the harness after 1 ticket: #20 closed"),
        "{out}"
    );
    let _ = fs::remove_dir_all(&scratch.dir);
}

/// The track is caught up with main before the ticket (#80): main moved
/// under the track, so the run's first act fast-forwards the track to
/// main's tip, pushes it, follows it with HEAD and builds `xtask`, and
/// the ticket's worktree is laid down off that tip; the sync's line is
/// printed before the ticket's.
#[test]
fn the_track_is_caught_up_with_main_before_the_ticket() {
    let scratch = Scratch::lay("sync");
    scratch.tracker(&[20], 20);
    scratch.act("commit close");
    let before = rev(&scratch.root, "origin/t-x");
    let main = scratch.land("main", "landed.txt", "on main\n");
    let (code, out) = scratch.work(&["--one"]);
    assert_eq!(code, 0, "{out}");
    let tip = rev(&scratch.root, "t-x/t20");
    assert_eq!(
        sh(&scratch.root, &["git", "rev-parse", "t-x/t20^"]).trim(),
        main,
        "the worktree was laid down off main's tip: {out}"
    );
    assert_eq!(rev(&scratch.origin, "t-x"), tip, "{out}");
    assert_eq!(rev(&scratch.root, "HEAD"), tip, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    let synced = lines
        .iter()
        .position(|line| line.starts_with("track sync: t-x caught up with main"))
        .unwrap_or_else(|| panic!("{out}"));
    let worked = lines
        .iter()
        .position(|line| line.starts_with("track work: #20 closed at"))
        .unwrap_or_else(|| panic!("{out}"));
    assert!(synced < worked, "{out}");
    assert!(
        lines.contains(
            &format!(
                "track sync: t-x caught up with main ({}): fast-forwarded {} to {}; pushed to origin, HEAD follows, xtask built",
                &main[..7],
                &before[..7],
                &main[..7]
            )
            .as_str()
        ),
        "{out}"
    );
    let _ = fs::remove_dir_all(&scratch.dir);
}

/// A sync that refuses stops the run as a failure of the harness, named:
/// main and the track conflict, so no worktree is laid down, no session
/// starts, and the exit names the conflict.
#[test]
fn a_sync_that_refuses_stops_the_run_named() {
    let scratch = Scratch::lay("conflict");
    scratch.tracker(&[20], 20);
    scratch.act("commit close");
    let track = scratch.land("t-x", "README.md", "the track's\n");
    scratch.land("main", "README.md", "main's\n");
    let (code, out) = scratch.work(&[]);
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("main does not merge into t-x: conflicts in README.md"),
        "{out}"
    );
    assert!(
        out.contains("stopped by the harness after no ticket"),
        "{out}"
    );
    assert_eq!(rev(&scratch.origin, "t-x"), track, "{out}");
    assert_eq!(scratch.worktrees().len(), 1, "no worktree laid down: {out}");
    assert!(!has_ref(&scratch.root, "refs/heads/t-x/t20"), "{out}");
    assert_eq!(scratch.wrote("session.args"), "", "no session started");
    let _ = fs::remove_dir_all(&scratch.dir);
}

/// A ticket labelled `frontier:research` starts the session on
/// `/research <n>` (#81): the prompt is the one thing that changes, the
/// model, the mode and the effort as #75 set them, and the path from the
/// close to the retire is the same.
#[test]
fn a_research_ticket_starts_the_session_on_research() {
    let scratch = Scratch::lay("research");
    scratch.tracker(&[20], 20);
    scratch.label(20, "frontier:research");
    scratch.act("commit close");
    let (code, out) = scratch.work(&["--one"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        scratch.wrote("session.args"),
        "-p --model claude-opus-5 --permission-mode bypassPermissions --effort high /research 20"
    );
    assert_eq!(scratch.wrote("session.subagent"), "claude-sonnet-5");
    assert!(out.contains("#20 closed at"), "{out}");
    assert!(!scratch.worktree("t-x/t20").exists(), "retired: {out}");
    let _ = fs::remove_dir_all(&scratch.dir);
}

/// A ticket labelled `frontier:decision` starts the session on
/// `/grill-design <n>` (#81).
#[test]
fn a_decision_ticket_starts_the_session_on_grill_design() {
    let scratch = Scratch::lay("decision");
    scratch.tracker(&[20], 20);
    scratch.label(20, "frontier:decision");
    scratch.act("commit close");
    let (code, out) = scratch.work(&["--one"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        scratch.wrote("session.args"),
        "-p --model claude-opus-5 --permission-mode bypassPermissions --effort high /grill-design 20"
    );
    assert!(out.contains("#20 closed at"), "{out}");
    let _ = fs::remove_dir_all(&scratch.dir);
}

/// A ticket carrying both `frontier:` labels names no one skill: the
/// step refuses before any worktree is laid down, naming the ticket and
/// both labels, no session started (#81).
#[test]
fn a_ticket_with_both_frontier_labels_refuses_before_a_worktree() {
    let scratch = Scratch::lay("both");
    scratch.tracker(&[20], 20);
    scratch.label(20, "frontier:research");
    scratch.label(20, "frontier:decision");
    scratch.act("commit close");
    let (code, out) = scratch.work(&["--one"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("#20"), "{out}");
    assert!(out.contains("frontier:research"), "{out}");
    assert!(out.contains("frontier:decision"), "{out}");
    assert!(out.contains("nothing is laid down"), "{out}");
    assert_eq!(scratch.worktrees().len(), 1, "{out}");
    assert!(!has_ref(&scratch.root, "refs/heads/t-x/t20"), "{out}");
    assert_eq!(scratch.wrote("session.args"), "", "no session started");
    let _ = fs::remove_dir_all(&scratch.dir);
}

/// With `track.frontier=auto` the run fills its own frontier (#85): the
/// one ticket closed, `next` answers "none", so a `/frontier` session is
/// started in the clone itself, on the same model, mode and effort, and
/// the run goes on with the ticket it filed; that one closed, the
/// frontier is still empty after the second `/frontier` session, which
/// ends the run listing the track's open `for-a-person` tickets, those
/// of another track left out.
#[test]
fn an_empty_frontier_is_filled_by_a_frontier_session_until_it_stays_empty() {
    let scratch = Scratch::lay("auto");
    scratch.tracker(&[20], 20);
    scratch.viewed(20);
    scratch.viewed(21);
    scratch.act_on(20, "commit close");
    scratch.act_on(21, "commit close");
    scratch.frontier_files(&[
        (21, &["ready-for-agent"]),
        (30, &["for-a-person", "track:t-x"]),
        (31, &["for-a-person", "track:main"]),
        (32, &["for-a-person"]),
    ]);
    sh(&scratch.root, &["git", "config", "track.frontier", "auto"]);
    let (code, out) = scratch.work(&[]);
    assert_eq!(code, 0, "{out}");

    let first = rev(&scratch.root, "t-x/t20");
    let second = rev(&scratch.root, "t-x/t21");
    assert_eq!(rev(&scratch.origin, "t-x"), second, "{out}");
    assert_eq!(
        sh(&scratch.root, &["git", "rev-parse", "t-x/t21^"]).trim(),
        first,
        "the filed ticket was laid down off the moved track: {out}"
    );
    assert!(!scratch.worktree("t-x/t20").exists(), "retired: {out}");
    assert!(!scratch.worktree("t-x/t21").exists(), "retired: {out}");

    // Four sessions: the ticket, the planner in the clone, the ticket it
    // filed, the planner again.
    let session = "-p --model claude-opus-5 --permission-mode bypassPermissions --effort high";
    assert_eq!(
        scratch.wrote("session.args"),
        format!(
            "{session} /implement 20\n{session} /frontier\n{session} /implement 21\n{session} /frontier"
        )
    );
    assert_eq!(
        scratch.wrote("session.cwd"),
        format!(
            "{}\n{}\n{}\n{}",
            scratch.worktree("t-x/t20").display(),
            scratch.root.display(),
            scratch.worktree("t-x/t21").display(),
            scratch.root.display()
        )
    );
    assert_eq!(
        scratch.wrote("session.subagent"),
        "claude-sonnet-5\nclaude-sonnet-5\nclaude-sonnet-5\nclaude-sonnet-5"
    );

    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines.contains(
            &"track work: none: the frontier of t-x is still empty after the /frontier session; nothing laid down; the track waits on 2 for-a-person tickets:"
        ),
        "{out}"
    );
    assert!(lines.contains(&"  #30 ticket 30"), "{out}");
    assert!(lines.contains(&"  #32 ticket 32"), "{out}");
    assert!(!out.contains("#31"), "another track's: {out}");
    assert!(
        lines.contains(
            &"track work: the frontier of t-x is empty after 2 tickets: #20 closed, #21 closed"
        ),
        "{out}"
    );
    let _ = fs::remove_dir_all(&scratch.dir);
}

/// With the record, `--one` on an empty frontier starts the planner
/// once; a frontier still empty with no `for-a-person` ticket says so in
/// the line, exit 0; and a record that is not `auto` refuses, named,
/// before any session.
#[test]
fn a_frontier_that_stays_empty_ends_the_run_and_a_record_not_auto_refuses() {
    let scratch = Scratch::lay("still");
    scratch.tracker(&[], 20);
    scratch.frontier_files(&[]);
    sh(&scratch.root, &["git", "config", "track.frontier", "auto"]);
    let (code, out) = scratch.work(&["--one"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        scratch.wrote("session.args"),
        "-p --model claude-opus-5 --permission-mode bypassPermissions --effort high /frontier"
    );
    assert!(
        out.contains(
            "track work: none: the frontier of t-x is still empty after the /frontier session; nothing laid down; the track waits on no for-a-person ticket"
        ),
        "{out}"
    );
    assert_eq!(scratch.worktrees().len(), 1, "{out}");

    sh(&scratch.root, &["git", "config", "track.frontier", "yes"]);
    let (code, out) = scratch.work(&["--one"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("track.frontier is \"yes\""), "{out}");
    assert_eq!(
        scratch.wrote("session.args").lines().count(),
        1,
        "no second session: {out}"
    );
    let _ = fs::remove_dir_all(&scratch.dir);
}

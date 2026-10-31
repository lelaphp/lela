//! `xtask track work`, one ticket from start to retire without a person
//! (#77) and the frontier until it empties (#78), a closed ticket's branch
//! left for /specs and `main` never moved (#1749): held against a scratch
//! repository shaped as main's checkout, on `main` with no record of a
//! track or a model (ADR-0009), a bare `origin` beside it, a [`Tracker`]
//! answering `issue list` and `issue view` with what each test lays, as
//! the `track_next` tests do, and a session that is
//! `fixtures/stand-ins/claude` first on `PATH`, recording what it was
//! started with, committing on the branch and closing the ticket or not,
//! as the act the test wrote down says, for every ticket or for one.
#![cfg(tracked)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::scratch::{self, Tree};
use xtask::ticket::{Issue, Open, State};

mod common {
    pub mod tracker;
}
use common::tracker::{Tracker, blocked_by, issue, open};

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
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

/// What `track work` reads of the checkout, and the worktree it lays
/// down of it: the harness's own files, the map `status --session` renders,
/// the tracked settings and what git ignores there. Each test laid the
/// whole tree, some 4,400 files, and checked a worktree of it out, until
/// #2882.
const READ: [&str; 6] = [
    "seams.toml",
    "oracle.toml",
    "docs/divergence.toml",
    "AGENTS.md",
    ".claude",
    ".gitignore",
];

/// The scratch: main's checkout at `repo`, its bare `origin.git`, the
/// session and the build under `bin`, what the session wrote down, and
/// the tracker it asks.
struct Scratch {
    dir: Tree,
    root: PathBuf,
    origin: PathBuf,
    tracker: Tracker,
}

impl Scratch {
    /// Main's checkout of the files of this working tree [`READ`] names,
    /// laid by [`scratch::repository`]: one commit on `main`, pushed to
    /// `origin`, and the checkout on `main`. `origin` is a bare clone borrowing the
    /// checkout's objects, as the checkout borrows the workspace's, so the
    /// push writes no pack and each scratch holds no copy of the tree's
    /// objects (#644).
    fn lay(name: &str) -> Scratch {
        let dir = Tree::new(&format!("track-work-{name}")).unwrap();
        let root = dir.join("repo");
        let origin = dir.join("origin.git");
        scratch::repository(workspace(), &root, &READ).unwrap();
        fs::create_dir_all(dir.join("bin")).unwrap();
        sh(
            &dir,
            &[
                "git",
                "clone",
                "-q",
                "--bare",
                "--shared",
                root.to_str().unwrap(),
                origin.to_str().unwrap(),
            ],
        );
        sh(
            &root,
            &["git", "remote", "add", "origin", origin.to_str().unwrap()],
        );
        sh(
            &root,
            &[
                "git",
                "-c",
                "core.hooksPath=/var/empty",
                "push",
                "-q",
                "origin",
                "main",
            ],
        );
        let scratch = Scratch {
            dir,
            root,
            origin,
            tracker: Tracker::empty(&format!("track-work-{name}")),
        };
        scratch.stand_ins();
        scratch
    }

    /// The session, `bin/claude`: `fixtures/stand-ins/claude`, whose
    /// header says what it records and what each act does. The build,
    /// `bin/cargo`: [`scratch::cargo`], so the worktree is laid down with
    /// a harness it did not compile, and the session's `cargo xtask` is
    /// this crate's own binary.
    fn stand_ins(&self) {
        let claude = self.dir.join("bin/claude");
        fs::copy(
            xtask::package_dir().join("fixtures/stand-ins/claude"),
            &claude,
        )
        .unwrap();
        fs::set_permissions(&claude, fs::Permissions::from_mode(0o755)).unwrap();
        scratch::cargo(
            &self.dir.join("bin"),
            scratch::xtask(env!("CARGO_BIN_EXE_xtask")),
        )
        .unwrap();
    }

    /// The open tickets, each `ready-for-agent` and unblocked, and the
    /// ticket `issue view` answers for, open.
    fn tracker(&self, open: &[u64], viewed: u64) {
        let ready: Vec<Open> = open
            .iter()
            .map(|&number| self::open(number, &["ready-for-agent"]))
            .collect();
        self.tracker.listing(&ready);
        self.viewed(viewed);
    }

    /// The ticket `issue view <n>` answers with for `number`, open.
    fn viewed(&self, number: u64) {
        self.tracker.with_issue(&ticket(number));
    }

    /// The ticket `number` closed at the tracker by a person, as `issue
    /// view <n>` answers for it.
    fn closed_by_hand(&self, number: u64) {
        self.tracker.with_issue(&Issue {
            state: State::Closed,
            ..ticket(number)
        });
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
    /// time it replaces the tracker's open list with `filed`, the tickets
    /// it filed, each `ready-for-agent` and unblocked unless it names its
    /// own labels; the second time it files nothing (#85).
    fn frontier_files(&self, filed: &[(u64, &[&str])]) {
        let filed: Vec<Open> = filed
            .iter()
            .map(|&(number, labels)| open(number, labels))
            .collect();
        let next = self.dir.join("list.next.json");
        fs::write(&next, serde_json::to_string(&filed).unwrap()).unwrap();
        let (dir, list) = (self.dir.display(), self.tracker.file("list.json"));
        fs::write(
            self.dir.join("act.frontier"),
            format!(
                "if [ ! -f '{dir}/planned' ]; then touch '{dir}/planned'; cp '{}' '{}'; fi\nexit 0\n",
                next.display(),
                list.display()
            ),
        )
        .unwrap();
    }

    /// Shell the session runs after its act, before it ends: how a test
    /// breaks the harness from inside a session.
    fn session_then(&self, line: &str) {
        fs::write(self.dir.join("then"), line).unwrap();
    }

    /// `track work` on the model and the subagents' model every test
    /// but the one that names its own runs on, `args` after them.
    fn work(&self, args: &[&str]) -> (i32, String) {
        let models = [
            "--model",
            "claude-opus-5",
            "--subagent-model",
            "claude-sonnet-5",
        ];
        self.run(&[models.as_slice(), args].concat())
    }

    /// The command, this crate's own binary, run against the checkout
    /// with the stand-ins first on `PATH`, `args` after `track work`.
    fn run(&self, args: &[&str]) -> (i32, String) {
        let path = format!(
            "{}:{}:{}",
            self.dir.join("bin").display(),
            self.tracker.bin().display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let out = Command::new(scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
            .arg("--root")
            .arg(&self.root)
            .args(["track", "work"])
            .args(args)
            .current_dir(&self.root)
            .env_remove("CARGO")
            .env_remove("CARGO_MANIFEST_DIR")
            .env_remove("CLAUDE_CODE_SUBAGENT_MODEL")
            .env_remove(xtask::owner::SESSION_VARIABLE)
            .env("PATH", path)
            .env("STAND_IN_TRACKER", self.tracker.bin())
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

    /// What `xtask worktree whose` says of the checkout at `dir`, asked
    /// from a person's terminal.
    fn whose(dir: &Path) -> String {
        let out = Command::new(scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
            .arg("--root")
            .arg(dir)
            .args(["worktree", "whose"])
            .current_dir(dir)
            .env_remove(xtask::owner::SESSION_VARIABLE)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "worktree whose: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim_end().to_owned()
    }

    /// One section of `xtask status` over the checkout's store, as Markdown:
    /// what a person reads the run back from (#117).
    fn section(&self, name: &str) -> String {
        let out = Command::new(scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
            .arg("--root")
            .arg(&self.root)
            .args(["status", "--section", name])
            .current_dir(&self.root)
            .env_remove("CARGO")
            .env_remove("CARGO_MANIFEST_DIR")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "status --section {name}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    }

    /// What the sessions were started with, `session.args`, each ticket's
    /// own session id taken out: a fresh one per session, which its
    /// worktree's owner record names (#3621) and [`Scratch::ids`] reads.
    fn args(&self) -> String {
        let raw = self.wrote("session.args");
        let mut words: Vec<&str> = Vec::new();
        let mut lines: Vec<String> = Vec::new();
        for line in raw.lines() {
            let mut it = line.split(' ');
            while let Some(word) = it.next() {
                if word == "--session-id" {
                    it.next();
                } else {
                    words.push(word);
                }
            }
            lines.push(words.join(" "));
            words.clear();
        }
        lines.join("\n")
    }

    /// The session id each session was started with, in order.
    fn ids(&self) -> Vec<String> {
        let raw = self.wrote("session.args");
        raw.lines()
            .filter_map(|line| line.split_once("--session-id ").map(|(_, rest)| rest))
            .filter_map(|rest| rest.split(' ').next().map(str::to_owned))
            .collect()
    }

    fn wrote(&self, name: &str) -> String {
        fs::read_to_string(self.dir.join(name))
            .unwrap_or_default()
            .trim_end()
            .to_owned()
    }

    /// Where the worktree for `branch` is laid down: beside the checkout,
    /// in `repo-worktrees`, inside the scratch (revision 56).
    fn worktree(&self, branch: &str) -> PathBuf {
        self.dir.join("repo-worktrees").join(branch)
    }

    fn worktrees(&self) -> Vec<String> {
        sh(&self.root, &["git", "worktree", "list", "--porcelain"])
            .lines()
            .filter_map(|l| l.strip_prefix("worktree ").map(str::to_owned))
            .collect()
    }
}

/// Ticket `number` as `issue view` answers it: open, every box ticked.
fn ticket(number: u64) -> Issue {
    Issue {
        body: "## Acceptance criteria\n\n- [x] done\n".to_owned(),
        ..issue(number)
    }
}

/// The tracker's open list, each ticket with its labels as the tracker
/// spells them, unblocked.
fn listed(issues: &[(u64, &[&str])]) -> Vec<Open> {
    issues
        .iter()
        .map(|&(number, labels)| open(number, labels))
        .collect()
}

/// The whole path, `--one`: the next ticket, its worktree on `t<n>` off
/// `main`, the session started as `claude -p --model <model>` with the
/// subagents' model in its environment and the defaults for the mode
/// and the effort, in the worktree, with the prompt running `/build
/// <n>`; the branch recorded ready (#2311), so the worktree is retired
/// and the branch left for /specs to merge with its records kept, `main` and `origin`'s
/// `main` where they were (#1749, ADR-0009); one line naming the ticket,
/// the commit and the outcome; no trust line waited on.
#[test]
fn a_ready_tickets_branch_is_left_its_worktree_retired_and_main_unmoved() {
    let scratch = Scratch::lay("ready");
    scratch.tracker(&[20, 21], 20);
    scratch.act("commit ready");
    let before = rev(&scratch.root, "main");
    let (code, out) = scratch.work(&["--one"]);
    assert_eq!(code, 0, "{out}");

    let tip = rev(&scratch.root, "t20");
    assert_ne!(tip, before, "the session committed: {out}");
    assert_eq!(rev(&scratch.root, "t20^"), before, "cut from main: {out}");
    assert_eq!(rev(&scratch.root, "main"), before, "main unmoved: {out}");
    assert_eq!(rev(&scratch.root, "HEAD"), before, "{out}");
    assert_eq!(
        rev(&scratch.origin, "main"),
        before,
        "nothing pushed: {out}"
    );
    assert!(
        !has_ref(&scratch.origin, "refs/heads/t20"),
        "the branch is not pushed: {out}"
    );
    assert!(
        out.lines().any(|line| line
            == format!(
                "track work: #20 ready at {}; t20 is left for /specs to merge, its worktree retired, and the merge closes it",
                &tip[..7]
            )),
        "{out}"
    );

    // The session was started as the flags say, in the worktree.
    assert_eq!(
        scratch.args(),
        "-p --model claude-opus-5 --permission-mode auto --effort high /build 20"
    );
    // The prompt names the exit condition the run reads back (#115).
    let prompt = scratch.wrote("session.prompt");
    assert!(
        prompt.contains("cargo xtask ticket ready 20"),
        "the prompt names no ready record: {prompt}"
    );
    assert!(
        prompt.contains("gh issue create"),
        "the prompt says nothing of a blocker: {prompt}"
    );
    assert_eq!(scratch.wrote("session.subagent"), "claude-sonnet-5");
    assert_eq!(
        scratch.wrote("session.cwd"),
        scratch.worktree("t20").to_str().unwrap()
    );

    // Retired: the worktree gone, the branch and its record kept.
    assert!(!scratch.worktree("t20").exists(), "{out}");
    assert_eq!(scratch.worktrees().len(), 1, "{out}");
    assert_eq!(
        sh(&scratch.root, &["git", "config", "branch.t20.ticket"]).trim(),
        "20"
    );
    assert_eq!(
        sh(&scratch.root, &["git", "config", "branch.t20.ready-20"]).trim(),
        "nothing"
    );

    // The one trust line is the grant over the worktrees' directory, and
    // nothing waited on it (#74, revision 56); the tracker was asked
    // nothing but the list and the view.
    assert!(
        out.contains(&format!(
            "computed trust {}\n",
            scratch.dir.join("repo-worktrees").display()
        )),
        "{out}"
    );
    assert_eq!(out.matches("computed trust").count(), 1, "{out}");
    assert!(scratch.tracker.calls().is_empty(), "{out}");
}

/// A worktree left standing whose ticket a person closes later is not
/// the loop's (#1755): the next run works the next ticket, retires that
/// one's worktree alone, and leaves the standing worktree, its branch
/// and `main` as they were; retiring it is /specs' or a person's, with
/// `xtask worktree retire`, once its branch is merged or waits to be.
#[test]
fn a_standing_worktree_whose_ticket_a_person_closes_later_is_left_as_it_stands() {
    let scratch = Scratch::lay("later");
    scratch.tracker(&[20], 20);
    scratch.viewed(20);
    scratch.viewed(21);
    scratch.act_on(20, "commit");
    scratch.act_on(21, "commit close");
    let before = rev(&scratch.root, "main");
    let (code, out) = scratch.work(&["--one"]);
    assert_eq!(code, 1, "left open: {out}");
    let standing = rev(&scratch.root, "t20");

    // A person closes #20; #21 is the frontier now.
    scratch.closed_by_hand(20);
    scratch.tracker(&[21], 21);
    let (code, out) = scratch.work(&["--one"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("#21 closed at"), "{out}");
    assert!(
        !out.contains("#20"),
        "the run names only its own ticket: {out}"
    );
    assert!(!scratch.worktree("t21").exists(), "retired: {out}");
    assert!(scratch.worktree("t20").is_dir(), "still stands: {out}");
    assert_eq!(scratch.worktrees().len(), 2, "{out}");
    assert_eq!(rev(&scratch.root, "t20"), standing, "{out}");
    assert_eq!(rev(&scratch.root, "main"), before, "{out}");
    assert_eq!(rev(&scratch.origin, "main"), before, "{out}");
}

/// `--track` names the label track whose frontier is worked and
/// `--model` the model, the subagents running on it when nothing else is
/// named (ADR-0009): of three tickets, the one labelled
/// `track:parity-hosts` is the one laid down, on `t<n>` off `main`,
/// whatever number the others carry.
#[test]
fn a_run_on_a_label_track_names_its_ticket_and_lays_its_branch_off_main() {
    let scratch = Scratch::lay("label");
    scratch.tracker.listing(&listed(&[
        (19, &["ready-for-agent", "track:main"]),
        (21, &["ready-for-agent", "track:parity-hosts"]),
        (22, &["ready-for-agent", "track:parity-compiler"]),
    ]));
    scratch.viewed(21);
    scratch.act("commit");
    let main = rev(&scratch.root, "main");
    let (code, out) = scratch.run(&[
        "--one",
        "--track",
        "parity-hosts",
        "--model",
        "claude-fable-5",
    ]);
    assert_eq!(code, 1, "the ticket is left open: {out}");
    assert!(out.contains("track work: #21 ticket 21: on t21"), "{out}");
    assert_eq!(rev(&scratch.root, "t21^"), main, "cut from main: {out}");
    assert_eq!(
        sh(&scratch.root, &["git", "config", "branch.t21.ticket"]).trim(),
        "21"
    );
    assert!(!has_ref(&scratch.root, "refs/heads/t19"), "{out}");
    assert!(!has_ref(&scratch.root, "refs/heads/t22"), "{out}");
    assert_eq!(
        scratch.args(),
        "-p --model claude-fable-5 --permission-mode auto --effort high /build 21"
    );
    assert_eq!(scratch.wrote("session.subagent"), "claude-fable-5");
    assert_eq!(rev(&scratch.root, "main"), main, "{out}");
}

/// A session that ends with the ticket open, `--one`: the worktree stands,
/// `main` and `origin` are where they were, nothing is retired, the
/// record is kept and the exit is 1 with the reason; and
/// `--permission-mode` and `--effort` override the defaults. The
/// worktree is the session's while it runs and no session's once it has
/// ended, as `xtask worktree whose` says (#3782).
#[test]
fn a_ticket_left_open_leaves_the_worktree_standing() {
    let scratch = Scratch::lay("open");
    scratch.tracker(&[20], 20);
    scratch.act("commit");
    let during = scratch.dir.join("session.whose");
    scratch.session_then(&format!(
        "cargo xtask worktree whose > '{}'",
        during.display()
    ));
    let before = rev(&scratch.root, "main");
    let (code, out) = scratch.work(&[
        "--one",
        "--permission-mode",
        "acceptEdits",
        "--effort",
        "max",
    ]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("#20 is open"), "{out}");
    assert!(
        out.contains(&format!("stands at {}", scratch.worktree("t20").display())),
        "{out}"
    );
    assert!(scratch.worktree("t20").is_dir(), "{out}");
    assert_eq!(scratch.worktrees().len(), 2, "{out}");
    assert_eq!(rev(&scratch.origin, "main"), before, "{out}");
    assert_eq!(rev(&scratch.root, "HEAD"), before, "{out}");
    assert_ne!(rev(&scratch.root, "t20"), before, "the work stands: {out}");
    assert_eq!(
        scratch.args(),
        "-p --model claude-opus-5 --permission-mode acceptEdits --effort max /build 20"
    );
    // The session was started under the id its worktree's owner record
    // names, so its writes there are its own (#3621); once it has ended,
    // the record names a session no process runs, and is gone (#3782).
    let worktree = scratch.worktree("t20");
    let id = &scratch.ids()[0];
    assert_eq!(
        fs::read_to_string(&during).unwrap().trim_end(),
        format!(
            "worktree whose: {} is session {id}'s; a person is admitted",
            worktree.display()
        ),
        "{out}"
    );
    assert_eq!(xtask::owner::of(&worktree).unwrap(), None, "{out}");
    assert!(
        out.contains(&format!(
            "stands at {} for a person, no session's",
            worktree.display()
        )),
        "{out}"
    );
    assert_eq!(
        Scratch::whose(&worktree),
        format!("worktree whose: {} is no session's", worktree.display())
    );
    assert_eq!(out.matches("computed trust").count(), 1, "{out}");
}

/// A ticket closed with nothing committed, as a withdrawn one is, is
/// closed all the same: the close is the one signal (#115), so the
/// worktree is retired and the branch, at `main`'s own commit, left with
/// its record; what /specs merges of it is nothing.
#[test]
fn a_ticket_closed_with_nothing_committed_is_retired_all_the_same() {
    let scratch = Scratch::lay("bare");
    scratch.tracker(&[20], 20);
    scratch.act("close");
    let before = rev(&scratch.root, "main");
    let (code, out) = scratch.work(&[]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains(&format!(
            "track work: #20 closed at {}; t20 is left for /specs to merge, its worktree retired",
            &before[..7]
        )),
        "{out}"
    );
    assert!(
        out.contains("track work: the frontier of main is empty after 1 ticket: #20 closed"),
        "{out}"
    );
    assert_eq!(rev(&scratch.root, "t20"), before, "{out}");
    assert_eq!(rev(&scratch.origin, "main"), before, "{out}");
    assert!(!scratch.worktree("t20").exists(), "{out}");
}

/// A run with no `--model` refuses before any worktree is laid down, and
/// an empty frontier lays none down and is not an error: without
/// `--fill` the run ends at "none" and starts no `/frontier` session,
/// though the stand-in would file a ticket (#85).
#[test]
fn no_model_refuses_before_a_worktree_and_an_empty_frontier_is_none() {
    let scratch = Scratch::lay("refuse");
    scratch.tracker(&[20], 20);
    scratch.act("commit close");
    scratch.frontier_files(&[(21, &["ready-for-agent"])]);
    let (code, out) = scratch.run(&["--one"]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("--model <NAME>"), "{out}");
    assert_eq!(scratch.worktrees().len(), 1, "{out}");
    assert!(!has_ref(&scratch.root, "refs/heads/t20"), "{out}");
    assert_eq!(scratch.args(), "", "no session started");

    scratch.tracker(&[], 20);
    let (code, out) = scratch.work(&["--one"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("none: the frontier of main is empty"), "{out}");
    assert_eq!(scratch.worktrees().len(), 1, "{out}");
    assert_eq!(scratch.args(), "", "no session started");
    assert!(!scratch.dir.join("planned").exists(), "no planner: {out}");
}

/// Without `--one`, the frontier until it empties: two tickets, the
/// session closing the first and leaving the second open. The first's
/// worktree is retired and its branch left, `main` unmoved, so the second
/// is laid down off the same `main`; the second's worktree stands for a
/// person and does not stop the run, since its branch records the
/// ticket and `track next` names it no more; the run ends at "none",
/// exit 0, one line per ticket and a closing line naming each with its
/// outcome. A run that lands one ticket and strands another is also what
/// the readings say (#117): one per step scoped to the ticket, a pass and
/// a fail, each with its commit and its reason, and the run's own closing
/// one, which is what `status --section track` renders the run from.
/// What each session spent rides on its step (#210): the first's usage
/// reading reached the checkout's store by the retire, the second's is
/// still the standing worktree's, and each step carries its own, the run
/// the total.
#[test]
fn the_frontier_is_worked_until_it_empties_past_a_ticket_left_open() {
    let scratch = Scratch::lay("frontier");
    scratch.tracker(&[20, 21], 20);
    scratch.viewed(20);
    scratch.viewed(21);
    scratch.act_on(20, "commit close spend 1.25");
    scratch.act_on(21, "commit spend 2.5");
    let before = rev(&scratch.root, "main");
    let (code, out) = scratch.work(&[]);
    assert_eq!(code, 0, "{out}");

    let first = rev(&scratch.root, "t20");
    let second = rev(&scratch.root, "t21");
    assert_ne!(first, before, "{out}");
    assert_eq!(
        rev(&scratch.origin, "main"),
        before,
        "nothing pushed: {out}"
    );
    assert_eq!(rev(&scratch.root, "HEAD"), before, "main unmoved: {out}");
    assert!(!scratch.worktree("t20").exists(), "retired: {out}");
    assert!(scratch.worktree("t21").is_dir(), "stands: {out}");
    assert_eq!(scratch.worktrees().len(), 2, "{out}");
    assert_eq!(
        rev(&scratch.root, "t21^"),
        before,
        "the second was laid down off main as it was: {out}"
    );

    // Both sessions started, each in its own worktree.
    assert_eq!(
        scratch.args(),
        "-p --model claude-opus-5 --permission-mode auto --effort high /build 20\n-p --model claude-opus-5 --permission-mode auto --effort high /build 21"
    );
    assert_eq!(
        scratch.wrote("session.cwd"),
        format!(
            "{}\n{}",
            scratch.worktree("t20").display(),
            scratch.worktree("t21").display()
        )
    );

    // One line per ticket, then the closing line naming each with its outcome.
    let lines: Vec<&str> = out.lines().collect();
    assert!(
        lines.contains(&format!(
            "track work: #20 closed at {}; t20 is left for /specs to merge, its worktree retired",
            &first[..7]
        ).as_str()),
        "{out}"
    );
    assert!(
        lines.contains(&format!(
            "track work: #21 is open with no ready record after the session (exit status: 0), the branch at {}; the worktree stands at {} for a person, no session's, nothing retired",
            &second[..7],
            scratch.worktree("t21").display()
        ).as_str()),
        "{out}"
    );
    assert!(
        lines.contains(
            &"track work: the frontier of main is empty after 2 tickets: #20 closed, #21 standing"
        ),
        "{out}"
    );

    // One reading per step and one for the run, which is what the section
    // renders, in the order the run took them (#117).
    let section = scratch.section("track");
    let rows: Vec<&str> = section.lines().skip(2).collect();
    assert_eq!(rows.len(), 3, "{section}");
    assert!(
        rows[0].starts_with(&format!(
            "| 20 | pass | 1.25 | 152000 | 3 |  | {} | ",
            &first[..7]
        )),
        "the retired worktree's usage, read from the checkout's log: {section}"
    );
    assert!(
        rows[0].contains("#20 closed at") && rows[0].contains("its worktree retired"),
        "{section}"
    );
    assert!(
        rows[1].starts_with(&format!(
            "| 21 | fail | 2.5 | 152000 | 3 |  | {} | ",
            &second[..7]
        )),
        "the standing worktree's usage, read from its own store: {section}"
    );
    assert!(
        rows[1].contains("#21 is open with no ready record after the session")
            && rows[1].contains("stands at"),
        "{section}"
    );
    assert!(
        rows[2].starts_with("| run | fail | 3.75 |  |  | "),
        "{section}"
    );
    assert!(
        rows[2].contains("landed=1 run=2 standing=1"),
        "how many it worked and how many landed: {section}"
    );
    assert!(
        rows[2].contains("the frontier of main is empty after 2 tickets: #20 closed, #21 standing"),
        "how it ended: {section}"
    );
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
        "touch '{}'",
        scratch.tracker.file("down.list").display()
    ));
    let (code, out) = scratch.work(&[]);
    assert_eq!(code, 1, "{out}");
    assert!(!scratch.worktree("t20").exists(), "retired: {out}");
    assert!(!has_ref(&scratch.root, "refs/heads/t21"), "{out}");
    assert!(
        out.contains("the open tickets could not be listed from the tracker"),
        "{out}"
    );
    assert!(
        out.contains("stopped by the harness after 1 ticket: #20 closed"),
        "{out}"
    );
}

/// A run that readies a blocker does not lay its dependent down in the
/// same run (#2270, #2326): #21 is blocked by #20, the session writes
/// #20's ready record, and #20 stays open at the tracker until /specs
/// merges `t20`, so the run ends at "none" with #21 never laid down. Once
/// the merge holds `t20` in `main` and closes #20, the next run names #21
/// and cuts `t21` from that `main`, which holds what it waited on.
#[test]
fn a_run_that_readies_a_blocker_does_not_lay_down_its_dependent() {
    let scratch = Scratch::lay("blocker");
    let open = |blocker| blocked(&[(21, &[], &[(20, blocker)]), (20, &[], &[])]);
    scratch.tracker.listing(&open(State::Open));
    scratch.viewed(20);
    scratch.viewed(21);
    scratch.act_on(20, "commit ready");
    scratch.act_on(21, "commit");
    let (code, out) = scratch.work(&[]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("#20 ready at"), "{out}");
    assert!(
        !has_ref(&scratch.root, "refs/heads/t21"),
        "#21 is not laid down while #20 is open: {out}"
    );

    // /specs merges t20 and the merge closes #20; the next run names #21
    // and cuts it from that main.
    sh(&scratch.root, &["git", "merge", "-q", "--ff-only", "t20"]);
    scratch
        .tracker
        .listing(&blocked(&[(21, &[], &[(20, State::Closed)])]));
    let (code, out) = scratch.work(&["--one"]);
    assert_eq!(code, 1, "#21 is left open: {out}");
    assert!(out.contains("track work: #21 ticket 21: on t21"), "{out}");
    assert_eq!(
        rev(&scratch.root, "t21^"),
        rev(&scratch.root, "t20"),
        "cut from a main that holds t20: {out}"
    );
}

/// One ticket of [`blocked`]'s list: its number, its labels, and its
/// blockers, each `(number, state)`.
type Row<'a> = (u64, &'a [&'a str], &'a [(u64, State)]);

/// The tracker's open list as rows written whole: each ticket `ready-for-agent`
/// unless `labels` names its own, blocked by each `(number, state)` of
/// `blockers`.
fn blocked(rows: &[Row]) -> Vec<Open> {
    rows.iter()
        .map(|&(number, labels, blockers)| {
            let labels = if labels.is_empty() {
                &["ready-for-agent"][..]
            } else {
                labels
            };
            Open {
                blocked_by: blocked_by(blockers),
                ..open(number, labels)
            }
        })
        .collect()
}

/// With `--fill`, an empty frontier starts the `/frontier` session in
/// the checkout (#85): #22 waits on #23, open and a person's, so it is
/// blocked, and the planner is started in the checkout.
#[test]
fn an_empty_frontier_starts_the_fill_session_in_the_checkout() {
    let scratch = Scratch::lay("fill-checkout");
    scratch.tracker.listing(&blocked(&[
        (22, &[], &[(23, State::Open)]),
        (23, &["for-a-person"], &[]),
    ]));
    scratch.frontier_files(&[(23, &["for-a-person"])]);
    let (code, out) = scratch.work(&["--one", "--fill"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        scratch.args(),
        "-p --model claude-opus-5 --permission-mode auto --effort high /frontier",
        "{out}"
    );
    assert_eq!(scratch.wrote("session.cwd"), scratch.root.to_str().unwrap());
    assert!(
        scratch.dir.join("planned").exists(),
        "the planner ran: {out}"
    );
    assert!(
        out.contains(
            "track work: none: the frontier of main is still empty after the /frontier session; nothing laid down; the track waits on 1 for-a-person ticket:"
        ),
        "{out}"
    );
}

/// A ticket labelled `frontier:research` starts the session on
/// `/research <n>` (#81): the skill's line is the one thing that
/// changes, the model, the mode and the effort as the flags set them,
/// and the path from the close to the retire is the same.
#[test]
fn a_research_ticket_starts_the_session_on_research() {
    let scratch = Scratch::lay("research");
    scratch
        .tracker
        .listing(&listed(&[(20, &["ready-for-agent", "frontier:research"])]));
    scratch.viewed(20);
    scratch.act("commit close");
    let (code, out) = scratch.work(&["--one"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        scratch.args(),
        "-p --model claude-opus-5 --permission-mode auto --effort high /research 20"
    );
    assert_eq!(scratch.wrote("session.subagent"), "claude-sonnet-5");
    assert!(out.contains("#20 closed at"), "{out}");
    assert!(!scratch.worktree("t20").exists(), "retired: {out}");
}

/// A ticket labelled `frontier:decision` starts the session on
/// `/grill-design <n>` (#81).
#[test]
fn a_decision_ticket_starts_the_session_on_grill_design() {
    let scratch = Scratch::lay("decision");
    scratch
        .tracker
        .listing(&listed(&[(20, &["ready-for-agent", "frontier:decision"])]));
    scratch.viewed(20);
    scratch.act("commit close");
    let (code, out) = scratch.work(&["--one"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        scratch.args(),
        "-p --model claude-opus-5 --permission-mode auto --effort high /grill-design 20"
    );
    assert!(out.contains("#20 closed at"), "{out}");
}

/// A ticket carrying both `frontier:` labels names no one skill: the
/// step refuses before any worktree is laid down, naming the ticket and
/// both labels, no session started (#81).
#[test]
fn a_ticket_with_both_frontier_labels_refuses_before_a_worktree() {
    let scratch = Scratch::lay("both");
    scratch.tracker.listing(&listed(&[(
        20,
        &["ready-for-agent", "frontier:research", "frontier:decision"],
    )]));
    scratch.viewed(20);
    scratch.act("commit close");
    let (code, out) = scratch.work(&["--one"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("#20"), "{out}");
    assert!(out.contains("frontier:research"), "{out}");
    assert!(out.contains("frontier:decision"), "{out}");
    assert!(out.contains("nothing is laid down"), "{out}");
    assert_eq!(scratch.worktrees().len(), 1, "{out}");
    assert!(!has_ref(&scratch.root, "refs/heads/t20"), "{out}");
    assert_eq!(scratch.args(), "", "no session started");
}

/// With `--fill` the run fills its own frontier (#85): the one ticket
/// closed, `next` answers "none", so a `/frontier` session is started in
/// the checkout itself, on the same model, mode and effort, and the run
/// goes on with the ticket it filed; that one closed, the frontier is
/// still empty after the second `/frontier` session, which ends the run
/// listing the track's open `for-a-person` tickets, those of another
/// track left out.
#[test]
fn an_empty_frontier_is_filled_by_a_frontier_session_until_it_stays_empty() {
    let scratch = Scratch::lay("fill");
    scratch.tracker(&[20], 20);
    scratch.viewed(20);
    scratch.viewed(21);
    scratch.act_on(20, "commit close");
    scratch.act_on(21, "commit close");
    scratch.frontier_files(&[
        (21, &["ready-for-agent"]),
        (30, &["for-a-person", "track:main"]),
        (31, &["for-a-person", "track:parity-hosts"]),
        (32, &["for-a-person"]),
    ]);
    let (code, out) = scratch.work(&["--fill"]);
    assert_eq!(code, 0, "{out}");

    let before = rev(&scratch.root, "main");
    assert_eq!(rev(&scratch.origin, "main"), before, "{out}");
    assert_eq!(
        rev(&scratch.root, "t21^"),
        before,
        "the filed ticket was laid down off main: {out}"
    );
    assert!(!scratch.worktree("t20").exists(), "retired: {out}");
    assert!(!scratch.worktree("t21").exists(), "retired: {out}");

    // Four sessions: the ticket, the planner in the checkout, the ticket
    // it filed, the planner again.
    let session = "-p --model claude-opus-5 --permission-mode auto --effort high";
    assert_eq!(
        scratch.args(),
        format!(
            "{session} /build 20\n{session} /frontier\n{session} /build 21\n{session} /frontier"
        )
    );
    assert_eq!(
        scratch.wrote("session.cwd"),
        format!(
            "{}\n{}\n{}\n{}",
            scratch.worktree("t20").display(),
            scratch.root.display(),
            scratch.worktree("t21").display(),
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
            &"track work: none: the frontier of main is still empty after the /frontier session; nothing laid down; the track waits on 2 for-a-person tickets:"
        ),
        "{out}"
    );
    assert!(lines.contains(&"  #30 ticket 30"), "{out}");
    assert!(lines.contains(&"  #32 ticket 32"), "{out}");
    assert!(!out.contains("#31"), "another track's: {out}");
    assert!(
        lines.contains(
            &"track work: the frontier of main is empty after 2 tickets: #20 closed, #21 closed"
        ),
        "{out}"
    );
}

/// Under `--fill`, `--one` on an empty frontier starts the planner once,
/// and a frontier still empty with no `for-a-person` ticket says so in
/// the line, exit 0.
#[test]
fn a_frontier_that_stays_empty_after_the_planner_ends_the_run() {
    let scratch = Scratch::lay("still");
    scratch.tracker(&[], 20);
    scratch.frontier_files(&[]);
    let (code, out) = scratch.work(&["--one", "--fill"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        scratch.args(),
        "-p --model claude-opus-5 --permission-mode auto --effort high /frontier"
    );
    assert!(
        out.contains(
            "track work: none: the frontier of main is still empty after the /frontier session; nothing laid down; the track waits on no for-a-person ticket"
        ),
        "{out}"
    );
    assert_eq!(scratch.worktrees().len(), 1, "{out}");
}

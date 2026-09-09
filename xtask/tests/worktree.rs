//! The track worktree (harness-design.md §4.2, §5, §11): `scripts/worktree-add.sh`
//! lays one down ready to be worked in and `scripts/worktree-retire.sh`
//! retires it into the track's store. Both halves run against a fixture
//! repository whose `xtask` is the fixture's stand-in
//! (`xtask/fixtures/worktree`), so what is measured here is the command and
//! not the harness it builds.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::cli::{Format, Section};
use xtask::divergence::Registry;
use xtask::manifest::Manifest;
use xtask::reading::{Reading, Status, Store};
use xtask::status;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, text).unwrap();
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

/// One of the two commands, run from the fixture repository: its exit code
/// and its two streams joined, which is what a person reads.
fn run(root: &Path, name: &str, args: &[&str]) -> (i32, String) {
    let out = Command::new("bash")
        .arg(workspace().join("scripts").join(name))
        .args(args)
        .current_dir(root)
        .env_remove("CARGO")
        .env_remove("CARGO_MANIFEST_DIR")
        .env("GIT_AUTHOR_NAME", "x")
        .env("GIT_AUTHOR_EMAIL", "x@x")
        .env("GIT_COMMITTER_NAME", "x")
        .env("GIT_COMMITTER_EMAIL", "x@x")
        .output()
        .unwrap_or_else(|e| panic!("{name}: {e}"));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

/// Where the add command lays a worktree by default: a sibling tree named
/// after the repository, one directory per branch.
fn worktrees_of(root: &Path) -> PathBuf {
    let name = root.file_name().unwrap().to_string_lossy().to_string();
    root.parent().unwrap().join(format!("{name}-wt"))
}

/// A fixture repository: the manifest, the pin and the registry the
/// renderers read, the tracked settings the worktree inherits, the
/// fixture's `xtask` crate, one commit on `main`, and `origin/t-x` as the
/// track a worktree is taken off, which is where a track's branch lives in
/// a repository that has worktrees on it (harness-design.md ledger 14).
fn repository(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("xtask-worktree-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    let _ = fs::remove_dir_all(worktrees_of(&root));
    fs::create_dir_all(root.join("docs")).unwrap();
    for file in ["seams.toml", "oracle.toml", "docs/divergence.toml"] {
        fs::copy(workspace().join(file), root.join(file)).unwrap();
    }
    for file in ["Cargo.toml", "xtask/Cargo.toml", "xtask/src/main.rs"] {
        let text = fs::read_to_string(fixtures().join("worktree").join(file)).unwrap();
        write(&root, file, &text);
    }
    // `Cargo.lock` is tracked in the repository this fixture stands in for
    // and generated here, where a build would leave the worktree dirty and
    // `git worktree remove` would refuse it for a reason of the fixture's.
    write(
        &root,
        ".gitignore",
        "target/\n/Cargo.lock\n/.claude/harness.local.md\n",
    );
    write(
        &root,
        ".claude/settings.json",
        "{ \"enabledPlugins\": {} }\n",
    );
    write(&root, "scripts/hooks/pre-commit", "#!/bin/sh\nexit 0\n");
    sh(&root, &["git", "init", "-q", "-b", "main"]);
    sh(&root, &["git", "config", "core.hooksPath", "scripts/hooks"]);
    sh(&root, &["git", "add", "-A"]);
    commit(&root, "fixture");
    sh(
        &root,
        &["git", "update-ref", "refs/remotes/origin/t-x", "HEAD"],
    );
    root
}

/// One commit of what is staged, unsigned: a person's `commit.gpgsign` is
/// not the fixture's.
fn commit(root: &Path, message: &str) {
    sh(
        root,
        &[
            "git",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            message,
        ],
    );
}

fn worktrees(root: &Path) -> Vec<String> {
    sh(root, &["git", "worktree", "list", "--porcelain"])
        .lines()
        .filter_map(|l| l.strip_prefix("worktree ").map(str::to_owned))
        .collect()
}

/// The add command: one call creates the branch off its track, adds the
/// worktree, builds `xtask` so the first `Edit` is judged by a built
/// binary rather than a cold `cargo run` (§12 item 8), lays the local file
/// down and prints the `computed trust` line as a person's to run (§11).
#[test]
fn an_add_lays_a_worktree_down_ready_to_be_worked_in() {
    let root = repository("add");
    let (code, out) = run(&root, "worktree-add.sh", &["t-x/witness"]);
    assert_eq!(code, 0, "{out}");
    let dir = worktrees_of(&root).join("t-x/witness");
    assert!(dir.is_dir(), "{}: {out}", dir.display());
    // What git prints of the path, which the temporary root is a symlink to.
    let dir = fs::canonicalize(&dir).unwrap();

    // The branch is the track's tip, not `main`'s: `origin/t-x` is what a
    // track's branch is in a repository with worktrees on it.
    let head = sh(&root, &["git", "rev-parse", "t-x/witness"]);
    assert_eq!(head, sh(&root, &["git", "rev-parse", "origin/t-x"]));
    assert!(out.contains("t-x"), "the track is named: {out}");

    // Judged by a built binary from the first tool call.
    let built = dir.join("target/debug/xtask");
    assert!(built.is_file(), "no built xtask: {out}");

    // The local file is laid down and rendered, and the grant that would
    // render it is printed rather than run.
    assert!(dir.join(".claude/harness.local.md").is_file(), "{out}");
    assert!(
        out.contains(&format!("computed trust {}", dir.display())),
        "{out}"
    );
    assert!(
        out.contains("a person runs, once"),
        "the grant is a person's: {out}"
    );

    // What the clone already has is reported, not repeated.
    assert!(out.contains("core.hooksPath"), "{out}");
    assert!(out.contains(".claude/settings.json"), "{out}");
    assert!(
        out.contains("oracle.toml") && out.contains("php-src"),
        "the pin and the tree it names: {out}"
    );
    assert_eq!(
        sh(&root, &["git", "config", "core.hooksPath"]).trim(),
        "scripts/hooks",
        "the hooks path is asserted, not set again"
    );

    // A second add refuses and changes nothing.
    let before = fs::read_to_string(dir.join(".claude/harness.local.md")).unwrap();
    let listed = worktrees(&root);
    let (code, again) = run(&root, "worktree-add.sh", &["t-x/witness"]);
    assert_eq!(code, 1, "{again}");
    assert_eq!(worktrees(&root), listed, "{again}");
    assert_eq!(
        fs::read_to_string(dir.join(".claude/harness.local.md")).unwrap(),
        before
    );

    // A branch that exists with no worktree of its own refuses too.
    sh(&root, &["git", "branch", "t-x/other"]);
    let (code, other) = run(&root, "worktree-add.sh", &["t-x/other"]);
    assert_eq!(code, 1, "{other}");
    assert!(other.contains("t-x/other"), "{other}");
    assert_eq!(worktrees(&root), listed, "{other}");
    assert!(!worktrees_of(&root).join("t-x/other").exists(), "{other}");

    // A branch `origin` has is one an agent is already on, so it is refused
    // rather than taken off the track a second time.
    sh(
        &root,
        &["git", "update-ref", "refs/remotes/origin/pushed", "HEAD"],
    );
    let (code, pushed) = run(&root, "worktree-add.sh", &["pushed"]);
    assert_eq!(code, 1, "{pushed}");
    assert_eq!(worktrees(&root), listed, "{pushed}");
    assert!(!worktrees_of(&root).join("pushed").exists(), "{pushed}");

    // No branch is a usage error, not a refusal of something.
    let (code, usage) = run(&root, "worktree-add.sh", &[]);
    assert_eq!(code, 2, "{usage}");
    assert_eq!(worktrees(&root), listed, "{usage}");

    let _ = fs::remove_dir_all(worktrees_of(&root));
    let _ = fs::remove_dir_all(&root);
}

/// A branch whose prefixes name no branch is on no track, so it is taken
/// off `main`, which is what `done` judges it against (harness §4.5).
#[test]
fn a_branch_on_no_track_is_taken_off_main() {
    let root = repository("solo");
    commit(&root, "ahead");
    let (code, out) = run(&root, "worktree-add.sh", &["solo"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        sh(&root, &["git", "rev-parse", "solo"]),
        sh(&root, &["git", "rev-parse", "main"]),
        "{out}"
    );
    assert!(out.contains("off the track main"), "{out}");
    let _ = fs::remove_dir_all(worktrees_of(&root));
    let _ = fs::remove_dir_all(&root);
}

/// The retire half: a worktree's readings live in its own `target/harness/`
/// and removing the worktree removes them, so the log is appended into the
/// track's store first and the track's `status --section permits` counts
/// the retired agent's denials (harness-design.md §4.2).
#[test]
fn retiring_appends_the_readings_to_the_tracks_store_and_removes_the_worktree() {
    let root = repository("retire");
    let (code, out) = run(&root, "worktree-add.sh", &["t-x/witness"]);
    assert_eq!(code, 0, "{out}");
    let dir = worktrees_of(&root).join("t-x/witness");

    let mut denial = Reading::new("permit", "workspace");
    denial.rule = "harness.self".to_owned();
    denial.status = Status::Fail;
    denial.message = "Edit seams.toml: a person edits the manifest".to_owned();
    denial.session = "the-retired-agent".to_owned();
    let line = serde_json::to_string(&denial.to_json().unwrap()).unwrap();
    write(&dir, "target/harness/log.jsonl", &format!("{line}\n"));

    // A removal that refuses has appended the readings already, so the
    // call that follows it must not count them a second time.
    write(&dir, "uncommitted.rs", "fn left_behind() {}\n");
    let (code, dirty) = run(&root, "worktree-retire.sh", &["t-x/witness"]);
    assert_eq!(code, 1, "{dirty}");
    assert!(dir.is_dir(), "{dirty}");
    fs::remove_file(dir.join("uncommitted.rs")).unwrap();

    let (code, out) = run(&root, "worktree-retire.sh", &["t-x/witness"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        fs::read_to_string(root.join("target/harness/log.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1,
        "{dirty}{out}"
    );
    assert!(!dir.exists(), "the worktree is removed: {out}");
    assert_eq!(worktrees(&root).len(), 1, "{out}");

    let appended = fs::read_to_string(root.join("target/harness/log.jsonl")).unwrap();
    assert!(appended.contains("the-retired-agent"), "{appended}");

    let manifest = Manifest::load(&root).unwrap();
    let registry = Registry::load(&root).unwrap();
    let store = Store::of_workspace(&root);
    let permits = status::render(
        &manifest,
        &registry,
        &store,
        &root,
        Section::Permits,
        Format::Json,
        None,
        Some("the-retired-agent"),
    )
    .unwrap();
    let rendered: serde_json::Value = serde_json::from_str(&permits).unwrap();
    let row = rendered["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["rule"] == "harness.self")
        .unwrap_or_else(|| panic!("{permits}"));
    assert_eq!(row["denials"], 1, "{permits}");
    assert_eq!(row["this_session"], 1, "{permits}");

    // The branch is kept; deleting it is a person's line, printed.
    assert!(
        sh(&root, &["git", "branch", "--list", "t-x/witness"]).contains("t-x/witness"),
        "{out}"
    );
    assert!(out.contains("git branch -D t-x/witness"), "{out}");

    // The main worktree is not a thing this command retires.
    let (code, main) = run(&root, "worktree-retire.sh", &["main"]);
    assert_eq!(code, 1, "{main}");
    assert_eq!(worktrees(&root).len(), 1, "{main}");

    let _ = fs::remove_dir_all(worktrees_of(&root));
    let _ = fs::remove_dir_all(&root);
}

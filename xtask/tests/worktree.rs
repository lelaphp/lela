//! The track worktree (harness-design.md §4.2, §5, §11): `xtask worktree
//! add` lays one down ready to be worked in and `xtask worktree retire`
//! retires it into the track's store. Both run against a scratch repository
//! holding this working tree's tracked files, so the harness the add builds
//! in the worktree and asks for the local file is this one, and the store
//! the retire appends to is the real store.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::cli::{Format, Section};
use xtask::divergence::Registry;
use xtask::manifest::Manifest;
use xtask::permit::{Call, judge};
use xtask::reading::{Reading, Status, Store};
use xtask::status;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
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

/// A command's exit code and its two streams joined, which is what a
/// person reads.
fn joined(out: &std::process::Output) -> (i32, String) {
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

/// The instrument, this crate's own binary, run against the scratch root.
fn add(root: &Path, args: &[&str]) -> (i32, String) {
    worktree(root, "add", args)
}

/// The retire half, run against `root`, which is the checkout the
/// instrument is asked from: the track's own or a worktree of it.
fn retire(root: &Path, args: &[&str]) -> (i32, String) {
    worktree(root, "retire", args)
}

/// One half of `xtask worktree`, run from and against `root`.
fn worktree(root: &Path, half: &str, args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("--root")
        .arg(root)
        .args(["worktree", half])
        .args(args)
        .current_dir(root)
        .env_remove("CARGO")
        .env_remove("CARGO_MANIFEST_DIR")
        .env("GIT_AUTHOR_NAME", "x")
        .env("GIT_AUTHOR_EMAIL", "x@x")
        .env("GIT_COMMITTER_NAME", "x")
        .env("GIT_COMMITTER_EMAIL", "x@x")
        .output()
        .unwrap();
    joined(&out)
}

/// One of the two commands under `scripts/`, run from the scratch repository.
fn command(root: &Path, name: &str, args: &[&str]) -> (i32, String) {
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
    joined(&out)
}

/// Where a worktree of `root` lives: under the main checkout, one
/// directory per branch, where `permit` resolves a workspace.
fn worktree_of(root: &Path, branch: &str) -> PathBuf {
    root.join(".claude/worktrees").join(branch)
}

/// A scratch repository: this working tree's files, tracked or not yet, so the
/// `xtask` a worktree of it builds is this one and the manifest, the pin,
/// the registry, the map, the tracked settings and the hooks are the
/// real ones; one commit on `main`; and `origin/t-x` as the track a
/// worktree is taken off, which is where a track's branch lives in a
/// repository that has worktrees on it (harness-design.md ledger 14).
fn repository(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("xtask-worktree-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
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
    sh(&root, &["git", "init", "-q", "-b", "main"]);
    sh(&root, &["git", "config", "core.hooksPath", "scripts/hooks"]);
    sh(&root, &["git", "add", "-A"]);
    commit(&root, "scratch");
    sh(
        &root,
        &["git", "update-ref", "refs/remotes/origin/t-x", "HEAD"],
    );
    fs::canonicalize(root).unwrap()
}

/// One commit of what is staged, unsigned and unhooked: a person's
/// `commit.gpgsign` is not the scratch repository's, and the real
/// pre-commit measuring the scratch's own files is not the test.
fn commit(root: &Path, message: &str) {
    sh(
        root,
        &[
            "git",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/var/empty",
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

/// The add instrument: one call creates the branch off its track, adds the
/// worktree, builds `xtask` so the first `Edit` is judged by a built
/// binary rather than a cold `cargo run` (§12 item 8), lays the local file
/// down and prints the `computed trust` line as a person's to run (§11).
#[test]
fn an_add_lays_a_worktree_down_ready_to_be_worked_in() {
    let root = repository("add");
    let (code, out) = add(&root, &["t-x/witness"]);
    assert_eq!(code, 0, "{out}");
    let dir = worktree_of(&root, "t-x/witness");
    assert!(dir.is_dir(), "{}: {out}", dir.display());

    // The branch is the track's tip, not `main`'s: `origin/t-x` is what a
    // track's branch is in a repository with worktrees on it.
    let head = sh(&root, &["git", "rev-parse", "t-x/witness"]);
    assert_eq!(head, sh(&root, &["git", "rev-parse", "origin/t-x"]));
    assert!(
        out.contains("off the track t-x"),
        "the track is named: {out}"
    );

    // Judged by a built binary from the first tool call: this crate,
    // built at the worktree's own HEAD, which is what it says it is.
    let built = dir.join("target/debug/xtask");
    assert!(built.is_file(), "no built xtask: {out}");
    let track = Command::new(&built)
        .args(["track", "--json"])
        .current_dir(&dir)
        .output()
        .unwrap();
    let facts: serde_json::Value =
        serde_json::from_slice(&track.stdout).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(facts["branch"], "t-x/witness", "{facts}");
    assert_eq!(facts["store"], root.to_str().unwrap(), "{facts}");

    // The local file is laid down by that build and rendered, and the
    // grant that would render it is printed rather than run.
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
    assert!(
        out.contains(&format!("ready: cd {}", dir.display())),
        "{out}"
    );

    // A second add refuses and changes nothing.
    let before = fs::read_to_string(dir.join(".claude/harness.local.md")).unwrap();
    let listed = worktrees(&root);
    let (code, again) = add(&root, &["t-x/witness"]);
    assert_eq!(code, 1, "{again}");
    assert_eq!(worktrees(&root), listed, "{again}");
    assert_eq!(
        fs::read_to_string(dir.join(".claude/harness.local.md")).unwrap(),
        before
    );

    // A branch that exists with no worktree of its own refuses too.
    sh(&root, &["git", "branch", "t-x/other"]);
    let (code, other) = add(&root, &["t-x/other"]);
    assert_eq!(code, 1, "{other}");
    assert!(other.contains("t-x/other"), "{other}");
    assert_eq!(worktrees(&root), listed, "{other}");
    assert!(!worktree_of(&root, "t-x/other").exists(), "{other}");

    // A branch `origin` has is one an agent is already on, so it is refused
    // rather than taken off the track a second time.
    sh(
        &root,
        &["git", "update-ref", "refs/remotes/origin/pushed", "HEAD"],
    );
    let (code, pushed) = add(&root, &["pushed"]);
    assert_eq!(code, 1, "{pushed}");
    assert_eq!(worktrees(&root), listed, "{pushed}");
    assert!(!worktree_of(&root, "pushed").exists(), "{pushed}");

    // A directory that exists refuses before the branch is made.
    fs::create_dir_all(worktree_of(&root, "t-x/taken")).unwrap();
    let (code, taken) = add(&root, &["t-x/taken"]);
    assert_eq!(code, 1, "{taken}");
    assert_eq!(worktrees(&root), listed, "{taken}");
    assert!(
        !sh(&root, &["git", "branch", "--list", "t-x/taken"]).contains("t-x/taken"),
        "{taken}"
    );

    // No branch is a usage error, not a refusal of something.
    let (code, usage) = add(&root, &[]);
    assert_eq!(code, 2, "{usage}");
    assert_eq!(worktrees(&root), listed, "{usage}");

    let _ = fs::remove_dir_all(&root);
}

/// A worktree the instrument lays down is one `permit` resolves as a
/// workspace of its own: an `Edit` in it is judged by that worktree's
/// manifest, not as `.claude/**` of the main checkout and not by the main
/// checkout's rules (harness-design.md §4.4, #41).
#[test]
fn an_edit_in_a_laid_worktree_is_judged_by_that_worktrees_manifest() {
    let root = repository("permit");
    let (code, out) = add(&root, &["t-x/judged"]);
    assert_eq!(code, 0, "{out}");
    let dir = worktree_of(&root, "t-x/judged");

    // The worktree's manifest differs from the main checkout's in what
    // the rule says, so the denial says which manifest judged.
    let manifest = fs::read_to_string(dir.join("seams.toml")).unwrap();
    let marker = "edited by a person, says the worktree's manifest";
    let manifest = manifest.replacen("are edited by a person.", &format!("are {marker}."), 1);
    assert!(
        manifest.contains(marker),
        "the harness.self text is rewritten"
    );
    fs::write(dir.join("seams.toml"), manifest).unwrap();

    let call = Call::parse(
        &serde_json::json!({"tool_name": "Edit", "cwd": dir.to_str().unwrap(),
            "tool_input": {"file_path": dir.join("oracle.toml").to_str().unwrap(),
                "old_string": "a", "new_string": "b"}})
        .to_string(),
    )
    .unwrap();
    let workspace = call.workspace(&root);
    assert_eq!(workspace, dir, "the worktree is the workspace: {out}");
    let judged = judge(&Manifest::load(&workspace).unwrap(), &workspace, &call).unwrap();
    let denial = judged
        .denial()
        .expect("the pin is a person's in the worktree too");
    assert_eq!(denial.rule, "harness.self");
    assert!(denial.reason().contains(marker), "{}", denial.reason());

    let _ = fs::remove_dir_all(&root);
}

/// A branch whose prefixes name no branch is on no track, so it is taken
/// off `main`, which is what `done` judges it against (harness §4.5); and
/// `scripts/worktree-add.sh` is one `exec` of the instrument, so this one runs through it.
#[test]
fn a_branch_on_no_track_is_taken_off_main_through_the_add_command() {
    let root = repository("solo");
    commit(&root, "ahead");
    let (code, out) = command(&root, "worktree-add.sh", &["solo"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        sh(&root, &["git", "rev-parse", "solo"]),
        sh(&root, &["git", "rev-parse", "main"]),
        "{out}"
    );
    assert!(out.contains("off the track main"), "{out}");
    assert!(
        worktree_of(&root, "solo")
            .join("target/debug/xtask")
            .is_file(),
        "{out}"
    );
    let _ = fs::remove_dir_all(&root);
}

/// The retire half: a worktree's readings live in its own `target/harness/`
/// and removing the worktree removes them, so the log is appended into the
/// track's store first and the track's `status --section permits` counts
/// the retired agent's denials (harness-design.md §4.2). The store is the
/// one `Track::of` names, so a retire asked from a checkout that is not the
/// track's root, here the worktree itself, lands the readings there too.
/// The per-key files beside the log stay with the worktree: each is a
/// scope's latest run, and the track's own are not overwritten by a tree
/// the track does not have, which is what the design says (§5, "A
/// track's worktrees"; #35).
#[test]
fn retiring_appends_the_readings_to_the_tracks_store_and_removes_the_worktree() {
    let root = repository("retire");
    let (code, out) = add(&root, &["t-x/witness"]);
    assert_eq!(code, 0, "{out}");
    let dir = worktree_of(&root, "t-x/witness");

    let mut denial = Reading::new("permit", "workspace");
    denial.rule = "harness.self".to_owned();
    denial.status = Status::Fail;
    denial.message = "Edit seams.toml: a person edits the manifest".to_owned();
    denial.session = "the-retired-agent".to_owned();
    let line = serde_json::to_string(&denial.to_json().unwrap()).unwrap();
    write(&dir, "target/harness/log.jsonl", &format!("{line}\n"));
    let mut clippy = Reading::new("clippy", "xtask");
    clippy.message = "the worktree's clippy over xtask".to_owned();
    write(
        &dir,
        "target/harness/clippy/xtask.toml",
        &toml::to_string(&clippy).unwrap(),
    );
    clippy.message = "the track's clippy over xtask".to_owned();
    let tracks_own = toml::to_string(&clippy).unwrap();
    write(&root, "target/harness/clippy/xtask.toml", &tracks_own);

    // A removal that refuses has appended the readings already, so the
    // call that follows it must not count them a second time. Both calls
    // are asked from the worktree, which is not the track's root.
    write(&dir, "uncommitted.rs", "fn left_behind() {}\n");
    let (code, dirty) = retire(&dir, &["t-x/witness"]);
    assert_eq!(code, 1, "{dirty}");
    assert!(dir.is_dir(), "{dirty}");
    fs::remove_file(dir.join("uncommitted.rs")).unwrap();

    let (code, out) = retire(&dir, &["t-x/witness"]);
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
    assert_eq!(
        fs::read_to_string(root.join("target/harness/clippy/xtask.toml")).unwrap(),
        tracks_own,
        "the per-key files stay with the worktree: {out}"
    );

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

    // The main worktree is not a thing this command retires, nor is a
    // branch no worktree is on; and no branch is a usage error.
    let (code, main) = retire(&root, &["main"]);
    assert_eq!(code, 1, "{main}");
    assert_eq!(worktrees(&root).len(), 1, "{main}");
    let (code, gone) = retire(&root, &["t-x/witness"]);
    assert_eq!(code, 1, "{gone}");
    let (code, usage) = retire(&root, &[]);
    assert_eq!(code, 2, "{usage}");

    let _ = fs::remove_dir_all(&root);
}

/// `scripts/worktree-retire.sh` is one `exec` of the instrument, so a
/// worktree that wrote no readings is retired through it: nothing is
/// appended, the worktree is removed and the branch-delete line is a
/// person's.
#[test]
fn a_worktree_with_no_readings_is_retired_through_the_retire_command() {
    let root = repository("retire-none");
    let (code, out) = add(&root, &["t-x/quiet"]);
    assert_eq!(code, 0, "{out}");
    let dir = worktree_of(&root, "t-x/quiet");
    let (code, out) = command(&root, "worktree-retire.sh", &["t-x/quiet"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("worktree retire: "), "{out}");
    assert!(out.contains("wrote no readings"), "{out}");
    assert!(!dir.exists(), "{out}");
    assert!(!root.join("target/harness/log.jsonl").exists(), "{out}");
    assert!(out.contains("git branch -D t-x/quiet"), "{out}");
    let _ = fs::remove_dir_all(&root);
}

/// The track's record names its ticket (#30): the add writes it on the
/// branch, `xtask track` in the worktree reads it by name, a second
/// branch on the same ticket is refused before anything is laid down, and
/// the retire keeps the record with the branch it keeps.
#[test]
fn an_add_records_the_ticket_and_a_retire_keeps_it_with_the_branch() {
    let root = repository("ticket");
    let (code, out) = add(&root, &["t-x/thirty", "--ticket", "30"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("#30"), "the ticket is named: {out}");
    let dir = worktree_of(&root, "t-x/thirty");
    let track = Command::new(dir.join("target/debug/xtask"))
        .args(["track", "--json"])
        .current_dir(&dir)
        .output()
        .unwrap();
    let facts: serde_json::Value = serde_json::from_slice(&track.stdout).unwrap();
    assert_eq!(facts["ticket"], 30, "{facts}");

    // A second branch on the same ticket is refused and nothing is laid down.
    let listed = worktrees(&root);
    let (code, again) = add(&root, &["t-x/thirty-again", "--ticket", "30"]);
    assert_eq!(code, 1, "{again}");
    assert!(
        again.contains("t-x/thirty"),
        "names the branch that has it: {again}"
    );
    assert_eq!(worktrees(&root), listed, "{again}");
    assert!(
        !sh(&root, &["git", "branch", "--list", "t-x/thirty-again"]).contains("again"),
        "{again}"
    );

    let (code, out) = retire(&root, &["t-x/thirty"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("#30"), "the record is named as kept: {out}");
    assert_eq!(
        sh(&root, &["git", "config", "branch.t-x/thirty.ticket"]).trim(),
        "30",
        "the record stays with the branch"
    );
    let (code, out) = worktree(&root, "add", &["t-x/thirty-again", "--ticket", "30"]);
    assert_eq!(code, 1, "the retired branch still holds the ticket: {out}");
    let _ = fs::remove_dir_all(&root);
}

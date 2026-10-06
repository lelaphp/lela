//! A ticket's worktree (harness-design.md §4.2, §5, §11): `xtask worktree
//! add` lays one down ready to be worked in and `xtask worktree retire`
//! retires it into the main checkout's store. Both run against a scratch repository
//! holding this working tree's tracked files, so the harness the add builds
//! in the worktree and asks for the local file is this one, and the store
//! the retire appends to is the real store.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::cli::{Format, Section};
use xtask::divergence::Registry;
use xtask::manifest::Manifest;
use xtask::oracle::Promise;
use xtask::permit::{Call, judge};
use xtask::reading::{Reading, Status, Store};
use xtask::scratch::{self, Tree};
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

/// The add with `cargo` itself on `PATH` rather than the stand-in, for
/// the one test that holds what the build compiles.
fn add_building(root: &Path, args: &[&str]) -> (i32, String) {
    run(
        root,
        "add",
        args,
        &std::env::var_os("PATH").unwrap_or_default(),
    )
}

/// A `PATH` whose `cargo` is [`scratch::cargo`], from a tree of its own
/// beside the repository, so nothing of it is among the repository's
/// files; the tree goes with the value, once the call it served is over.
fn stand_in() -> (Tree, OsString) {
    let bin = Tree::new("worktree-cargo").unwrap();
    let path = scratch::cargo(&bin, Path::new(env!("CARGO_BIN_EXE_xtask"))).unwrap();
    (bin, path)
}

/// The retire half, run against `root`, which is the checkout the
/// instrument is asked from: the main checkout or a worktree of it.
fn retire(root: &Path, args: &[&str]) -> (i32, String) {
    worktree(root, "retire", args)
}

/// One half of `xtask worktree`, run from and against `root`, with the
/// build stood in.
fn worktree(root: &Path, half: &str, args: &[&str]) -> (i32, String) {
    let (_bin, path) = stand_in();
    run(root, half, args, &path)
}

/// One half of `xtask worktree` under `path`.
fn run(root: &Path, half: &str, args: &[&str], path: &OsString) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("--root")
        .arg(root)
        .args(["worktree", half])
        .args(args)
        .current_dir(root)
        .env("PATH", path)
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

/// One of the two commands under `scripts/`, run from the scratch
/// repository with the build stood in.
fn command(root: &Path, name: &str, args: &[&str]) -> (i32, String) {
    let (_bin, path) = stand_in();
    let out = Command::new("bash")
        .arg(workspace().join("scripts").join(name))
        .args(args)
        .current_dir(root)
        .env("PATH", &path)
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

/// Where a worktree of `root` lives: beside the main checkout, in the
/// directory named after it, one directory per branch (revision 56).
fn worktree_of(root: &Path, branch: &str) -> PathBuf {
    let name = root.file_name().unwrap().to_str().unwrap();
    root.with_file_name(format!("{name}-worktrees"))
        .join(branch)
}

/// A scratch repository at `repo` inside its tree, so the worktrees laid
/// down beside it are inside the tree too and go with it.
struct Repository {
    _tree: Tree,
    root: PathBuf,
}

impl AsRef<Path> for Repository {
    fn as_ref(&self) -> &Path {
        &self.root
    }
}

impl std::ops::Deref for Repository {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.root
    }
}

/// A scratch repository: this working tree's files, tracked or not yet, so the
/// `xtask` a worktree of it builds is this one and the manifest, the pin,
/// the registry, the map, the tracked settings and the hooks are the
/// real ones; and one commit on `main`, which every ticket's branch is
/// cut from (ADR-0009).
fn repository(name: &str) -> Repository {
    let tree = Tree::new(&format!("worktree-{name}")).unwrap();
    let root = tree.join("repo");
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
    Repository { _tree: tree, root }
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

/// The add instrument: one call cuts the branch from `main`, adds the
/// worktree beside the checkout (revision 56), builds `xtask` so the first
/// `Edit` is judged by a built binary rather than a cold `cargo run` (§12
/// item 8), lays the local file down and prints the one grant that covers
/// the worktrees' directory, waiting on none (#74, §11).
#[test]
fn an_add_lays_a_worktree_down_ready_to_be_worked_in() {
    let root = repository("add");
    let (code, out) = add(&root, &["t40"]);
    assert_eq!(code, 0, "{out}");
    let dir = worktree_of(&root, "t40");
    assert!(dir.is_dir(), "{}: {out}", dir.display());

    // The branch is cut from `main`'s tip, and the line says so.
    let head = sh(&root, &["git", "rev-parse", "t40"]);
    assert_eq!(head, sh(&root, &["git", "rev-parse", "main"]));
    let main = head.trim();
    assert!(
        out.contains(&format!("t40 off main at {}", &main[..7])),
        "{out}"
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
    assert_eq!(facts["branch"], "t40", "{facts}");
    assert_eq!(facts["store"], root.to_str().unwrap(), "{facts}");

    // The local file is laid down by that build. The worktree is beside
    // the clone, so no grant on it covers the worktree: the line is the
    // one grant over the directory the worktrees are laid down in, and
    // nothing waits on it (#74, revision 56).
    assert!(dir.join(".claude/harness.local.md").is_file(), "{out}");
    let homes = root.with_file_name("repo-worktrees");
    assert!(
        out.contains(&format!("computed trust {}\n", homes.display())),
        "one trust line, over the worktrees' directory: {out}"
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
    let (code, again) = add(&root, &["t40"]);
    assert_eq!(code, 1, "{again}");
    assert_eq!(worktrees(&root), listed, "{again}");
    assert_eq!(
        fs::read_to_string(dir.join(".claude/harness.local.md")).unwrap(),
        before
    );

    // A branch that exists with no worktree of its own refuses too.
    sh(&root, &["git", "branch", "t41"]);
    let (code, other) = add(&root, &["t41"]);
    assert_eq!(code, 1, "{other}");
    assert!(other.contains("t41"), "{other}");
    assert_eq!(worktrees(&root), listed, "{other}");
    assert!(!worktree_of(&root, "t41").exists(), "{other}");

    // A branch `origin` has is one an agent is already on, so it is refused
    // rather than cut from `main` a second time.
    sh(
        &root,
        &["git", "update-ref", "refs/remotes/origin/pushed", "HEAD"],
    );
    let (code, pushed) = add(&root, &["pushed"]);
    assert_eq!(code, 1, "{pushed}");
    assert_eq!(worktrees(&root), listed, "{pushed}");
    assert!(!worktree_of(&root, "pushed").exists(), "{pushed}");

    // A directory that exists refuses before the branch is made.
    fs::create_dir_all(worktree_of(&root, "t42")).unwrap();
    let (code, taken) = add(&root, &["t42"]);
    assert_eq!(code, 1, "{taken}");
    assert_eq!(worktrees(&root), listed, "{taken}");
    assert!(
        !sh(&root, &["git", "branch", "--list", "t42"]).contains("t42"),
        "{taken}"
    );

    // No branch is a usage error, not a refusal of something.
    let (code, usage) = add(&root, &[]);
    assert_eq!(code, 2, "{usage}");
    assert_eq!(worktrees(&root), listed, "{usage}");
}

/// A worktree the instrument lays down is one `permit` resolves as a
/// workspace of its own: an `Edit` in it is judged by that worktree's
/// manifest, not as `.claude/**` of the main checkout and not by the main
/// checkout's rules (harness-design.md §4.4, #41).
#[test]
fn an_edit_in_a_laid_worktree_is_judged_by_that_worktrees_manifest() {
    let root = repository("permit");
    let (code, out) = add(&root, &["t43"]);
    assert_eq!(code, 0, "{out}");
    let dir = worktree_of(&root, "t43");

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
}

/// Whatever its name, a branch is cut from `main`'s tip, which is what
/// `done` judges it against (harness §4.5, ADR-0009); and
/// `scripts/worktree-add.sh` is one `exec` of the instrument, so this one
/// runs through it.
#[test]
fn a_branch_is_cut_from_mains_tip_through_the_add_command() {
    let root = repository("solo");
    commit(&root, "ahead");
    let (code, out) = command(&root, "worktree-add.sh", &["solo"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        sh(&root, &["git", "rev-parse", "solo"]),
        sh(&root, &["git", "rev-parse", "main"]),
        "{out}"
    );
    assert!(out.contains("solo off main at"), "{out}");
    assert!(
        worktree_of(&root, "solo")
            .join("target/debug/xtask")
            .is_file(),
        "{out}"
    );
}

/// A worktree's `target/` is its own, seeded from the clone's `target/debug`
/// by `cp -c -R` before `xtask` is built there (harness-design.md §5,
/// "Worktrees"; #245, #251): the dependencies are blocks the two
/// directories share and the build compiles the workspace's own crates
/// alone, and what the worktree holds afterwards is a `target/debug/xtask`
/// built from its own sources, whose root is the worktree, with no
/// `CARGO_TARGET_DIR` set and no `.cargo/config.toml` written. The clone
/// here holds this workspace's dependency build, seeded the way the add
/// seeds, so on a volume without clonefile it is the cold branch that is
/// held: one line saying so and a worktree ready all the same. The retire
/// removes the seeded copy with the worktree and leaves the clone's.
#[test]
fn an_add_seeds_the_worktrees_target_from_the_clones_and_builds_the_workspace_alone() {
    let root = repository("seed");
    fs::create_dir_all(root.join("target")).unwrap();
    let cloned = Command::new("cp")
        .args(["-c", "-R"])
        .arg(workspace().join("target/debug"))
        .arg(root.join("target/debug"))
        .output()
        .unwrap();
    let warm = cloned.status.success();
    write(
        &root,
        "target/harness/log.jsonl",
        "{\"instrument\":\"the-clones-own\"}\n",
    );

    let (code, out) = add_building(&root, &["t44"]);
    assert_eq!(code, 0, "{out}");
    let dir = worktree_of(&root, "t44");
    let built = dir.join("target/debug/xtask");
    assert!(built.is_file(), "no built xtask: {out}");
    assert!(
        fs::symlink_metadata(dir.join("target/debug"))
            .unwrap()
            .file_type()
            .is_dir(),
        "the worktree's target/debug is a directory of its own, not a link to the clone's"
    );
    assert!(
        !root.join("target/worktrees").exists() && !dir.join("target/harness/log.jsonl").exists(),
        "no shared target directory, and the clone's readings are not the worktree's: {out}"
    );
    assert_eq!(
        sh(&dir, &["git", "status", "--porcelain"]).trim(),
        "",
        "nothing tracked is written, .cargo/config.toml among it: {out}"
    );

    // The binary's root is the worktree: run from elsewhere with no
    // `--root`, it answers for the branch it was built on, not the clone's
    // `main` and not this workspace's branch.
    let track = Command::new(&built)
        .args(["track", "--json"])
        .current_dir(&root)
        .env_remove("CARGO")
        .env_remove("CARGO_MANIFEST_DIR")
        .output()
        .unwrap();
    let facts: serde_json::Value =
        serde_json::from_slice(&track.stdout).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(facts["branch"], "t44", "{facts}");

    if warm {
        assert!(
            out.contains("seeded target/debug from the clone's"),
            "{out}"
        );
        assert!(
            out.contains(" and 0 dependencies:"),
            "the build compiled the workspace's own crates and no dependency: {out}"
        );
        assert!(
            !out.contains("compiling 0 crates of the workspace"),
            "the workspace's own crates are the worktree's build, not the clone's: {out}"
        );
    } else {
        assert_eq!(
            out.lines()
                .filter(|l| l.contains("the worktree starts cold"))
                .count(),
            1,
            "one line says the seeding failed: {out}"
        );
        assert!(
            !out.contains(" and 0 dependencies:"),
            "a cold worktree compiles its dependencies: {out}"
        );
    }
    assert!(
        out.contains(&format!("ready: cd {}", dir.display())),
        "{out}"
    );

    let (code, out) = retire(&root, &["t44"]);
    assert_eq!(code, 0, "{out}");
    assert!(
        !dir.exists(),
        "the seeded copy goes with the worktree: {out}"
    );
    assert_eq!(
        root.join("target/debug/xtask").is_file(),
        warm,
        "the clone's own build is not what retire removes: {out}"
    );
    assert!(
        fs::read_to_string(root.join("target/harness/log.migrated.jsonl"))
            .unwrap()
            .contains("the-clones-own"),
        "the clone's log stands, split into the log of each instrument and kept whole: {out}"
    );
}

/// The retire half: a worktree's readings live in its own `target/harness/`
/// and removing the worktree removes them, so the log is landed in the
/// main checkout's store first and its `status --section permits` counts
/// the retired agent's denials (harness-design.md §4.2). The store is the
/// one `Track::of` names, so a retire asked from a checkout that is not
/// the main one, here the worktree itself, lands the readings there too.
/// The per-key files beside the log stay with the worktree: each is a
/// scope's latest run, and the main checkout's own are not overwritten by
/// a tree it does not have, which is what the design says (§5,
/// "Worktrees"; #35).
#[test]
fn retiring_appends_the_readings_to_the_main_store_and_removes_the_worktree() {
    let root = repository("retire");
    let (code, out) = add(&root, &["t40"]);
    assert_eq!(code, 0, "{out}");
    let dir = worktree_of(&root, "t40");

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
    clippy.message = "the main checkout's clippy over xtask".to_owned();
    let mains_own = toml::to_string(&clippy).unwrap();
    write(&root, "target/harness/clippy/xtask.toml", &mains_own);

    // A removal that refuses has appended the readings already, so the
    // call that follows it must not count them a second time. Both calls
    // are asked from the worktree, which is not the main checkout.
    write(&dir, "uncommitted.rs", "fn left_behind() {}\n");
    let (code, dirty) = retire(&dir, &["t40"]);
    assert_eq!(code, 1, "{dirty}");
    assert!(dir.is_dir(), "{dirty}");
    fs::remove_file(dir.join("uncommitted.rs")).unwrap();

    let (code, out) = retire(&dir, &["t40"]);
    assert_eq!(code, 0, "{out}");
    assert_eq!(
        fs::read_to_string(root.join("target/harness/log/permit.jsonl"))
            .unwrap()
            .lines()
            .count(),
        1,
        "{dirty}{out}"
    );
    assert!(!dir.exists(), "the worktree is removed: {out}");
    assert_eq!(worktrees(&root).len(), 1, "{out}");

    let appended = fs::read_to_string(root.join("target/harness/log/permit.jsonl")).unwrap();
    assert!(appended.contains("the-retired-agent"), "{appended}");
    assert_eq!(
        fs::read_to_string(root.join("target/harness/clippy/xtask.toml")).unwrap(),
        mains_own,
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
        false,
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
        sh(&root, &["git", "branch", "--list", "t40"]).contains("t40"),
        "{out}"
    );
    assert!(out.contains("git branch -D t40"), "{out}");

    // The main worktree is not a thing this command retires, nor is a
    // branch no worktree is on; and no branch is a usage error.
    let (code, main) = retire(&root, &["main"]);
    assert_eq!(code, 1, "{main}");
    assert_eq!(worktrees(&root).len(), 1, "{main}");
    let (code, gone) = retire(&root, &["t40"]);
    assert_eq!(code, 1, "{gone}");
    let (code, usage) = retire(&root, &[]);
    assert_eq!(code, 2, "{usage}");
}

/// A worktree laid down under `.claude/worktrees/` before revision 56 is
/// found where git's own list says it is, never where one is laid down
/// now, so it is retired as any other.
#[test]
fn a_worktree_under_the_checkout_from_before_revision_56_is_retired_as_any_other() {
    let root = repository("retire-old");
    let dir = root.join(".claude/worktrees/t45");
    sh(
        &root,
        &[
            "git",
            "worktree",
            "add",
            "-q",
            "-b",
            "t45",
            dir.to_str().unwrap(),
            "main",
        ],
    );
    let (code, out) = retire(&root, &["t45"]);
    assert_eq!(code, 0, "{out}");
    assert!(!dir.exists(), "{out}");
    assert_eq!(worktrees(&root).len(), 1, "{out}");
}

/// `scripts/worktree-retire.sh` is one `exec` of the instrument, so a
/// worktree that wrote no readings is retired through it: nothing is
/// appended, the worktree is removed and the branch-delete line is a
/// person's.
#[test]
fn a_worktree_with_no_readings_is_retired_through_the_retire_command() {
    let root = repository("retire-none");
    let (code, out) = add(&root, &["t46"]);
    assert_eq!(code, 0, "{out}");
    let dir = worktree_of(&root, "t46");
    let (code, out) = command(&root, "worktree-retire.sh", &["t46"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("worktree retire: "), "{out}");
    assert!(out.contains("wrote no readings"), "{out}");
    assert!(!dir.exists(), "{out}");
    assert!(!root.join("target/harness/log.jsonl").exists(), "{out}");
    assert!(out.contains("git branch -D t46"), "{out}");
}

/// The branch's record names its ticket (#30): the add writes it on the
/// branch, `xtask track` in the worktree reads it by name, a second
/// branch on the same ticket is refused before anything is laid down, and
/// the retire keeps the record with the branch it keeps.
#[test]
fn an_add_records_the_ticket_and_a_retire_keeps_it_with_the_branch() {
    let root = repository("ticket");
    let (code, out) = add(&root, &["t30", "--ticket", "30"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("#30"), "the ticket is named: {out}");
    let dir = worktree_of(&root, "t30");
    let track = Command::new(dir.join("target/debug/xtask"))
        .args(["track", "--json"])
        .current_dir(&dir)
        .output()
        .unwrap();
    let facts: serde_json::Value = serde_json::from_slice(&track.stdout).unwrap();
    assert_eq!(facts["ticket"], 30, "{facts}");

    // A second branch on the same ticket is refused and nothing is laid down.
    let listed = worktrees(&root);
    let (code, again) = add(&root, &["t47", "--ticket", "30"]);
    assert_eq!(code, 1, "{again}");
    assert!(
        again.contains("t30"),
        "names the branch that has it: {again}"
    );
    assert_eq!(worktrees(&root), listed, "{again}");
    assert!(
        !sh(&root, &["git", "branch", "--list", "t47"]).contains("again"),
        "{again}"
    );

    let (code, out) = retire(&root, &["t30"]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("#30"), "the record is named as kept: {out}");
    assert_eq!(
        sh(&root, &["git", "config", "branch.t30.ticket"]).trim(),
        "30",
        "the record stays with the branch"
    );
    let (code, out) = worktree(&root, "add", &["t47", "--ticket", "30"]);
    assert_eq!(code, 1, "the retired branch still holds the ticket: {out}");
}

/// `xtask phpt` in `root` over one subset, held against `HEAD`: what it
/// printed.
fn phpt_against_head(root: &Path, subset: &str) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("--root")
        .arg(root)
        .args([
            "phpt",
            "--subset",
            subset,
            "--against",
            "HEAD",
            "--jobs",
            "1",
        ])
        .current_dir(root)
        .env_remove("CARGO")
        .env_remove("CARGO_MANIFEST_DIR")
        .output()
        .unwrap();
    joined(&out).1
}

/// The retire carries the worktree's kept phpt runs into the main
/// worktree's, where `phpt --against` looks for a base's run: a run is
/// keyed by the engine's tree, so one taken in a worktree at the base is
/// the run the comparison asks for, and before this it went with the
/// worktree (#1268). A run main keeps already is not overwritten.
#[test]
fn retiring_carries_the_worktrees_kept_phpt_runs_into_mains() {
    let manifest = Manifest::load(workspace()).unwrap();
    if Promise::Tree
        .or_skip(manifest.pin.source.tree())
        .unwrap()
        .is_none()
    {
        return;
    }
    let root = repository("retire-runs");
    let dir = worktree_of(&root, "t48");
    sh(
        &root,
        &[
            "git",
            "worktree",
            "add",
            "-q",
            "-b",
            "t48",
            dir.to_str().unwrap(),
            "main",
        ],
    );
    // No run is kept at HEAD, and the comparison's skip names the engine
    // tree a run would be kept under.
    let unkept = phpt_against_head(&root, "ext/pcre");
    assert!(unkept.contains("no run of ext/pcre is kept"), "{unkept}");
    let tree = unkept
        .split_once("(the engine's tree ")
        .and_then(|(_, rest)| rest.split_once(')'))
        .map_or_else(|| panic!("{unkept}"), |(tree, _)| tree.to_owned());

    let run = |engine: &str| {
        format!(
            "{{\"commit\": \"0000000\", \"corpus\": \"php-8.5.9\", \"engine\": \"{engine}\", \"verdicts\": {{}}}}\n"
        )
    };
    let runs = format!("target/phpt-runs/{tree}");
    write(
        &dir,
        &format!("{runs}/ext/pcre.json"),
        &run("the worktree's"),
    );
    write(
        &dir,
        &format!("{runs}/ext/ctype.json"),
        &run("the worktree's"),
    );
    write(&root, &format!("{runs}/ext/ctype.json"), &run("main's"));

    let (code, out) = retire(&root, &["t48"]);
    assert_eq!(code, 0, "{out}");
    assert!(!dir.exists(), "{out}");
    assert!(
        out.contains("carried 1 kept phpt run into")
            && out.contains("1 it keeps already stayed as they stood"),
        "{out}"
    );
    assert_eq!(
        fs::read_to_string(root.join(&runs).join("ext/pcre.json")).unwrap(),
        run("the worktree's")
    );
    assert_eq!(
        fs::read_to_string(root.join(&runs).join("ext/ctype.json")).unwrap(),
        run("main's"),
        "a run main keeps is not overwritten: {out}"
    );

    // The comparison in main now finds the carried run: what it gives no
    // verdict for is the engine this scratch repository has not built.
    let found = phpt_against_head(&root, "ext/pcre");
    assert!(!found.contains("no run of ext/pcre is kept"), "{found}");
    assert!(found.contains("this run measured no engine"), "{found}");
}

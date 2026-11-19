//! A ticket's worktree (harness-design.md §4.2, §5, §11): `xtask worktree
//! add` lays one down ready to be worked in and `xtask worktree retire`
//! retires it into the main checkout's store. Both run against a scratch repository
//! holding this working tree's tracked files, so the harness the add builds
//! in the worktree and asks for the local file is this one, and the store
//! the retire appends to is the real store.
#![cfg(tracked)]

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
use xtask::worktree::seed_build;

mod common {
    pub mod manifest;
}
use common::manifest::{Tracked, pin};

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
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
    let path = scratch::cargo(&bin, scratch::xtask(env!("CARGO_BIN_EXE_xtask"))).unwrap();
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
    let out = Command::new(scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
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

/// A scratch repository laid by [`scratch::repository`]: this working
/// tree's files, so the `xtask` a worktree of it builds is this one and the
/// manifest, the pin, the registry, the map, the tracked settings and the
/// hooks are the real ones; and one commit on `main`, which every ticket's
/// branch is cut from (ADR-0009).
fn repository(name: &str) -> Repository {
    let tree = Tree::new(&format!("worktree-{name}")).unwrap();
    let root = tree.join("repo");
    scratch::repository(workspace(), &root, &["."]).unwrap();
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
/// the worktrees' directory, waiting on none (§11).
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
    // nothing waits on it (revision 56).
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
/// checkout's rules (harness-design.md §4.4).
#[test]
fn an_edit_in_a_laid_worktree_is_judged_by_that_worktrees_manifest() {
    let root = repository("permit");
    let (code, out) = add(&root, &["t43"]);
    assert_eq!(code, 0, "{out}");
    let dir = worktree_of(&root, "t43");

    // The worktree's manifest differs from the main checkout's in what
    // the rule says, so the denial says which manifest judged.
    let marker = "edited by a person, says the worktree's manifest";
    Tracked::of(&dir)
        .with_rule("harness.self", |rule| {
            // The marker beside the rule's own words, whatever they are.
            let text = format!("{} Here: {marker}.", rule["text"].as_str().unwrap());
            rule.insert("text".to_owned(), text.into());
        })
        .write(&dir);

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
/// "Worktrees"): the dependencies are blocks the two
/// directories share and the build compiles the workspace's own crates
/// alone, and what the worktree holds afterwards is a `target/debug/xtask`
/// built from its own sources, whose root is the checkout it runs in, with
/// no `CARGO_TARGET_DIR` set and no `.cargo/config.toml` written. The clone
/// here holds a build of its own ([`warm_clone`]), so on a volume without
/// clonefile it is the cold branch that is held: one line saying so and a
/// worktree ready all the same. The retire removes the seeded copy with the
/// worktree and leaves the clone's.
#[test]
fn an_add_seeds_the_worktrees_target_from_the_clones_and_builds_the_workspace_alone() {
    let root = repository("seed");
    let warm = warm_clone(&root);
    write(
        &root,
        "target/harness/log/the-clones-own.jsonl",
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
        !root.join("target/worktrees").exists()
            && !dir.join("target/harness/log/the-clones-own.jsonl").exists(),
        "no shared target directory, and the clone's readings are not the worktree's: {out}"
    );
    assert_eq!(
        sh(&dir, &["git", "status", "--porcelain"]).trim(),
        "",
        "nothing tracked is written, .cargo/config.toml among it: {out}"
    );

    // The binary's root is the checkout it runs in, never this
    // workspace's: with no `--root` it answers for the worktree's
    // branch from inside it and for the clone's `main` from the clone.
    for (from, branch) in [(dir.as_path(), "t44"), (root.as_ref(), "main")] {
        let track = Command::new(&built)
            .args(["track", "--json"])
            .current_dir(from)
            .env_remove("CARGO")
            .env_remove("CARGO_MANIFEST_DIR")
            .output()
            .unwrap();
        let facts: serde_json::Value =
            serde_json::from_slice(&track.stdout).unwrap_or_else(|e| panic!("{e}: {out}"));
        assert_eq!(facts["branch"], branch, "{facts}");
    }

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
        // The clone holds no build, so the line is the add's "holds no
        // build, so it starts cold", worded so since the merge queue.
        assert_eq!(
            out.lines().filter(|l| l.contains("starts cold")).count(),
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
        fs::read_to_string(root.join("target/harness/log/the-clones-own.jsonl"))
            .unwrap()
            .contains("the-clones-own"),
        "the clone's log stands: {out}"
    );
}

/// The clone at `root` given a `target/debug` warm for the build the add
/// makes, where the volume clones: this workspace's copied in as a head
/// start and then `xtask`'s dependencies built in the clone, so what the
/// copy lacks the clone compiles here and the worktree does not. The copy
/// alone was once the clone's build, and it lacked every dependency
/// whenever a `cargo sweep --maxsize` had pruned this workspace's
/// `target/` during the run, the oldest artifacts first: the worktree then
/// compiled them all and the test failed on a tree it does not own. A
/// dependency's build does not read the sources of the crate that uses it,
/// so `xtask`'s two roots stand empty for the length of the clone's build
/// and are checked out again after: the add's build is the one that
/// compiles `xtask`. Whether the volume clones, which is the branch
/// the add takes; where it does not, the clone holds no build and the add
/// says it starts cold.
fn warm_clone(root: &Path) -> bool {
    if !clones(root) {
        return false;
    }
    // Seeded as the add seeds, so a workspace whose `target/debug` is a
    // link seeds a directory of the clone's own and the build below writes
    // there, not into the workspace's. A copy that fails, a sweep
    // mid-copy or a link to another volume, leaves the clone cold; the
    // build below compiles what it lacks, so the copy's result is not this
    // test's.
    seed_build(root, workspace(), "debug").ok();
    let roots = [
        ("xtask/src/lib.rs", "//! empty\n"),
        ("xtask/src/main.rs", "//! empty\nfn main() {}\n"),
    ];
    for (file, empty) in roots {
        write(root, file, empty);
    }
    let built = Command::new("cargo")
        .args(["build", "-p", "xtask"])
        .current_dir(root)
        .env_remove("CARGO")
        .env_remove("CARGO_MANIFEST_DIR")
        .output()
        .unwrap();
    sh(root, &["git", "checkout", "--", roots[0].0, roots[1].0]);
    assert!(
        built.status.success(),
        "the clone's build: {}",
        String::from_utf8_lossy(&built.stderr)
    );
    true
}

/// Whether the volume `root` stands on clones a file with `cp -c`, the
/// copy the add's and the queue's seeding make: macOS's `cp` on APFS
/// does, and GNU coreutils' `cp` refuses the option, so on the Linux
/// runner each build is seeded cold.
fn clones(root: &Path) -> bool {
    let target = root.join("target");
    fs::create_dir_all(&target).unwrap();
    let sample = target.join("clones");
    let clones = Command::new("cp")
        .arg("-c")
        .arg(root.join("Cargo.toml"))
        .arg(&sample)
        .output()
        .unwrap()
        .status
        .success();
    fs::remove_file(&sample).ok();
    clones
}

/// A seed from a `target/debug` that is a symbolic link copies what the
/// link names and not the link: `cp -R` copies a link named on
/// its command line as the link, and a seeded tree whose `target/debug`
/// were the source's link would relink the source's binaries when it
/// builds. The link here names a directory on the same volume, inside the
/// test's own tree, so where the volume clones the seed is warm, a
/// directory of the seeded tree's own holding the source's build; where
/// `cp -c` refuses, the seed is the error and nothing is left behind.
/// Either way the seeded tree's build makes a binary of its own and
/// leaves the source's as it was.
#[test]
fn a_seed_from_a_target_that_is_a_link_is_a_directory_of_its_own() {
    let tree = Tree::new("worktree-seed-link").unwrap();
    let package = |dir: &Path, says: &str| {
        write(
            dir,
            "Cargo.toml",
            "[package]\nname = \"seeded\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[workspace]\n",
        );
        write(
            dir,
            "src/main.rs",
            &format!("fn main() {{\n    print!(\"{says}\");\n}}\n"),
        );
    };
    let build = |dir: &Path| {
        let built = Command::new("cargo")
            .args(["build", "--offline"])
            .current_dir(dir)
            .env_remove("CARGO")
            .env_remove("CARGO_MANIFEST_DIR")
            .env_remove("CARGO_TARGET_DIR")
            .output()
            .unwrap();
        assert!(
            built.status.success(),
            "the build in {}: {}",
            dir.display(),
            String::from_utf8_lossy(&built.stderr)
        );
    };
    let says =
        |binary: &Path| String::from_utf8(Command::new(binary).output().unwrap().stdout).unwrap();

    let source = tree.join("source");
    let real = tree.join("elsewhere/debug");
    fs::create_dir_all(&real).unwrap();
    fs::create_dir_all(source.join("target")).unwrap();
    std::os::unix::fs::symlink(&real, source.join("target/debug")).unwrap();
    package(&source, "source");
    build(&source);
    let binary = real.join("seeded");
    let before = fs::read(&binary).unwrap();

    let seeded = tree.join("seeded");
    package(&seeded, "seeded");
    let seeding = seed_build(&seeded, &source, "debug");
    let to = seeded.join("target/debug");
    if clones(&source) {
        assert_eq!(seeding, Ok(true));
        assert!(
            fs::symlink_metadata(&to).unwrap().file_type().is_dir(),
            "the seeded target/debug is a directory of its own, not the source's link"
        );
        assert_eq!(
            fs::read(to.join("seeded")).unwrap(),
            before,
            "the seed holds the source's build"
        );
    } else {
        assert!(seeding.is_err(), "{seeding:?}");
        assert!(
            fs::symlink_metadata(&to).is_err(),
            "a seed that failed leaves nothing"
        );
    }

    build(&seeded);
    assert_eq!(says(&to.join("seeded")), "seeded");
    assert_eq!(
        fs::read(&binary).unwrap(),
        before,
        "the seeded tree's build left the source's binary as it was"
    );
    assert_eq!(says(&source.join("target/debug/seeded")), "source");
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
/// "Worktrees").
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
    write(
        &dir,
        "target/harness/log/permit.jsonl",
        &format!("{line}\n"),
    );
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
        status::Against::Checkout(&store),
        &root,
        Section::Permits,
        Format::Json,
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
    assert!(!root.join("target/harness/log").exists(), "{out}");
    assert!(out.contains("git branch -D t46"), "{out}");
}

/// The branch's record names its ticket: the add writes it on the
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
    let out = Command::new(scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
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
/// worktree. A run main keeps already is not overwritten.
#[test]
fn retiring_carries_the_worktrees_kept_phpt_runs_into_mains() {
    if Promise::Tree
        .or_skip(pin().source.tree())
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
    // No run is kept at HEAD.
    let unkept = phpt_against_head(&root, "ext/pcre");
    assert!(unkept.contains("no run of ext/pcre is kept"), "{unkept}");
    let tree = engine_tree(&root);

    let manifest = Manifest::load(&root).unwrap();
    let recipe = manifest.pin().unwrap().digest().to_owned();
    let run = |engine: &str| {
        format!(
            "{{\"commit\": \"0000000\", \"corpus\": \"php-8.5.9\", \"engine\": \"{engine}\", \"recipe\": \"{recipe}\", \"verdicts\": {{\"phpt:php-src/ext/pcre/tests/one.phpt\": \"pass\"}}}}\n"
        )
    };
    let runs = format!("target/harness/phpt-runs/{tree}");
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

/// A run CI's dashboard kept at a `main` commit, fetched by the line the
/// comparison's skip names, is found as that commit's base run.
/// The skip names the fetch before the call that takes a run; a stand-in
/// `gh` holds the upload as a run's artifact, the kept runs each under
/// its engine's tree, here a run of `ext/pcre` under the tree the skip
/// named: asked for the repository's artifacts it names the run that put
/// the upload, and asked to download `phpt-runs` from that run it lays
/// the upload in the directory named. The line run as it is printed lays
/// it in the checkout, and the comparison finds it there.
#[test]
fn a_run_ci_kept_fetched_by_the_skips_line_is_the_bases_run() {
    if Promise::Tree
        .or_skip(pin().source.tree())
        .unwrap()
        .is_none()
    {
        return;
    }
    let root = repository("ci-runs");
    let unkept = phpt_against_head(&root, "ext/pcre");
    assert!(unkept.contains("no run of ext/pcre is kept"), "{unkept}");
    let tree = engine_tree(&root);
    let (fetch, take) = unkept
        .split_once("fetch the runs CI's dashboard keeps with `")
        .and_then(|(_, rest)| rest.split_once("`, or take one with `"))
        .unwrap_or_else(|| panic!("the skip names the fetch before the take: {unkept}"));
    assert!(take.starts_with("cargo xtask phpt --base "), "{unkept}");

    let upload = Tree::new("worktree-ci-runs-upload").unwrap();
    write(
        &upload,
        &format!("{tree}/ext/pcre.json"),
        &format!(
            "{{\"commit\": \"0000000\", \"corpus\": \"php-8.5.9\", \"engine\": \"CI's\", \"recipe\": \"{}\", \"verdicts\": {{\"phpt:php-src/ext/pcre/tests/one.phpt\": \"pass\"}}}}\n",
            Manifest::load(&root).unwrap().pin().unwrap().digest()
        ),
    );
    let bin = Tree::new("worktree-ci-runs-gh").unwrap();
    write(
        &bin,
        "gh",
        &format!(
            "#!/bin/sh\ncase \"$1 $2\" in\n\"api \"*) echo 7 ;;\n\"run download\")\n  [ \"$3\" = 7 ] || exit 1\n  shift 3\n  while [ $# -gt 0 ]; do case $1 in --name) name=$2; shift 2 ;; --dir) dir=$2; shift 2 ;; *) shift ;; esac; done\n  [ \"$name\" = phpt-runs ] && mkdir -p \"$dir\" && cp -R '{}/.' \"$dir/\" ;;\n*) exit 2 ;;\nesac\n",
            upload.display()
        ),
    );
    executable(&bin.join("gh"));
    let path = std::env::join_paths(std::iter::once(bin.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let fetched = Command::new("sh")
        .args(["-c", fetch])
        .env("PATH", path)
        .env_remove("RSYNC_RSH")
        .output()
        .unwrap();
    assert!(fetched.status.success(), "{fetch}: {fetched:?}");

    let found = phpt_against_head(&root, "ext/pcre");
    assert!(!found.contains("no run of ext/pcre is kept"), "{found}");
    assert!(found.contains("this run measured no engine"), "{found}");
}

/// The crate directories of the manifest at `root`.
fn layout(root: &Path) -> xtask::manifest::Layout {
    xtask::manifest::Manifest::load(root).unwrap().layout()
}

/// The engine tree a run at `root`'s `HEAD` on a binary with no dep-info
/// is kept under (`xtask::phpt::kept_run`).
fn engine_tree(root: &Path) -> String {
    let head = sh(root, &["git", "rev-parse", "HEAD"]);
    let snapshot = xtask::key::Snapshot::commit(root, head.trim()).unwrap();
    let run = xtask::phpt::kept_run(&snapshot, &layout(root), "ext/pcre").unwrap();
    let tree = run
        .parent()
        .and_then(Path::parent)
        .and_then(Path::file_name);
    tree.unwrap().to_string_lossy().into_owned()
}

fn executable(path: &Path) {
    xtask::scratch::executable(path).unwrap();
}

/// A binary at `path` that hands every call to the pinned php `oracle`:
/// an engine the lane runs as the engine's own, not as its control.
fn engine_at(path: &Path, oracle: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        path,
        format!("#!/bin/sh\nexec '{}' \"$@\"\n", oracle.display()),
    )
    .unwrap();
    executable(path);
}

/// `xtask phpt` in the worktree at `dir` with `args`, `cargo` the file
/// at `cargo` and the corpus the tree at `corpus`.
fn phpt_in(dir: &Path, args: &[&str], cargo: &Path, corpus: &Path) -> (i32, String) {
    let out = Command::new(scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
        .arg("--root")
        .arg(dir)
        .arg("phpt")
        .args(args)
        .current_dir(dir)
        .env("CARGO", cargo)
        .env(xtask::oracle::TREE_VARIABLE, corpus)
        .env_remove("CARGO_MANIFEST_DIR")
        .output()
        .unwrap();
    joined(&out)
}

/// One call takes the merge base's run of each touched subset: it
/// lays a worktree at the base, builds the hosts there through `cargo`,
/// runs the subset, carries the run into the main worktree and removes
/// the worktree, and the comparison from the branch's worktree finds the
/// run by the base alone. The stand-in cargo answers the hosts' build by
/// laying a `php` that hands every call to the pinned one, and logs each
/// call; a second call finds the run kept and builds nothing.
#[test]
fn one_call_takes_the_merge_bases_run_and_keeps_it_in_the_main_worktree() {
    let pinned = pin().oracle.file().map(Path::to_path_buf);
    let Some(oracle) = Promise::Oracle.or_skip(pinned).unwrap() else {
        return;
    };
    let root = repository("base-run");
    let corpus = Tree::new("base-run-corpus").unwrap();
    write(
        &corpus,
        "ext/ctype/tests/one.phpt",
        "--TEST--\none\n--FILE--\n<?php echo \"one\";\n--EXPECT--\none\n",
    );
    let bin = Tree::new("base-run-cargo").unwrap();
    let (cargo, log) = (bin.join("cargo"), bin.join("calls"));
    write(
        &bin,
        "cargo",
        &format!(
            "#!/bin/sh\necho \"$*\" >> '{}'\n[ \"$1 $2\" = 'build --profile' ] || exit 1\nmkdir -p \"target/$3\" && printf '#!/bin/sh\\nexec %s \"$@\"\\n' \"'{}'\" > \"target/$3/php\" && chmod +x \"target/$3/php\"\n",
            log.display(),
            oracle.display()
        ),
    );
    executable(&cargo);

    // The branch touches ext/ctype, whose subset the per-commit lane runs.
    let dir = worktree_of(&root, "t49");
    let at = dir.to_str().unwrap();
    sh(
        &root,
        &["git", "worktree", "add", "-q", "-b", "t49", at, "main"],
    );
    let lib = dir.join("ext/ctype/src/lib.rs");
    fs::write(&lib, fs::read_to_string(&lib).unwrap() + "// touched\n").unwrap();
    sh(&dir, &["git", "add", "-A"]);
    commit(&dir, "touch ext/ctype");

    // A run of none kept at the base, as a scratch corpus's stood in main's
    // store, is no run, and the call takes one in its place.
    let none = format!(
        "target/harness/phpt-runs/{}/ext/ctype.json",
        engine_tree(&root)
    );
    let manifest = Manifest::load(&root).unwrap();
    let recipe = manifest.pin().unwrap().digest().to_owned();
    write(
        &root,
        &none,
        &format!(
            "{{\"commit\": \"0000000\", \"corpus\": \"php-8.5.9\", \"engine\": \"e\", \"recipe\": \"{recipe}\", \"verdicts\": {{}}}}\n"
        ),
    );

    // Before the call, the comparison names it.
    engine_at(&dir.join("target/release-da/php"), &oracle);
    let (_, unkept) = phpt_in(
        &dir,
        &["--subset", "ext/ctype", "--against", "main"],
        &cargo,
        &corpus,
    );
    // The base named as xtask names it: git's `--short=7` grows past seven
    // where the scratch's commit shares seven with an object of the
    // workspace it borrows.
    let base = sh(&root, &["git", "rev-parse", "main"]);
    let base = xtask::track::short(base.trim());
    let call = format!("cargo xtask phpt --base {base} --subset ext/ctype");
    assert!(unkept.contains("no run of ext/ctype is kept"), "{unkept}");
    assert!(unkept.contains(&call), "looked for `{call}` in: {unkept}");

    let (code, out) = phpt_in(&dir, &["--base", "--jobs", "2"], &cargo, &corpus);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("kept: ext/ctype at main"), "{out}");
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        "build --profile release-da -p php_sapi_cli -p php_sapi_cgi -p php_sapi_fpm -p php_sapi_phpdbg\n",
        "{out}"
    );
    assert_eq!(
        worktrees(&root).len(),
        2,
        "the base's worktree is gone: {out}"
    );
    let mut kept = Vec::new();
    for tree in fs::read_dir(root.join("target/harness/phpt-runs")).unwrap() {
        kept.push(tree.unwrap().path().join("ext/ctype.json"));
    }
    assert!(kept.iter().all(|run| run.is_file()), "{kept:?}: {out}");
    assert_eq!(kept.len(), 1, "{out}");
    let run: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(root.join(&none)).unwrap()).unwrap();
    assert_eq!(run["verdicts"].as_object().unwrap().len(), 1, "{out}");

    // The comparison from the branch's worktree finds the run by the base.
    engine_at(&dir.join("target/release-da/php"), &oracle);
    let (code, found) = phpt_in(
        &dir,
        &["--subset", "ext/ctype", "--against", "main"],
        &cargo,
        &corpus,
    );
    assert_eq!(code, 0, "{found}");
    assert!(!found.contains("no run of ext/ctype is kept"), "{found}");

    // A second call finds the run kept and lays nothing down.
    let (code, again) = phpt_in(&dir, &["--base"], &cargo, &corpus);
    assert_eq!(code, 0, "{again}");
    assert!(
        again.contains("a run of ext/ctype is kept at main"),
        "{again}"
    );
    assert_eq!(
        fs::read_to_string(&log).unwrap().lines().count(),
        1,
        "{again}"
    );
}

/// One half of `xtask worktree` run from `root` by `session`, as Claude
/// Code hands its session to a call's processes, with the build stood in.
fn by(session: &str, root: &Path, half: &str) -> (i32, String) {
    let (_bin, path) = stand_in();
    let out = Command::new(scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
        .arg("--root")
        .arg(root)
        .args(["worktree", half])
        .current_dir(root)
        .env("PATH", &path)
        .env(xtask::owner::SESSION_VARIABLE, session)
        .env_remove("CARGO_MANIFEST_DIR")
        .env("GIT_AUTHOR_NAME", "x")
        .env("GIT_AUTHOR_EMAIL", "x@x")
        .env("GIT_COMMITTER_NAME", "x")
        .env("GIT_COMMITTER_EMAIL", "x@x")
        .output()
        .unwrap();
    joined(&out)
}

/// Everything in `root` committed, unsigned and unhooked; the commit.
fn commit_all(root: &Path, message: &str) -> String {
    sh(root, &["git", "add", "-A"]);
    commit(root, message);
    sh(root, &["git", "rev-parse", "HEAD"]).trim().to_owned()
}

/// The merge queue's worktree: `queue` lays `_merge` down beside
/// the clone on `staging`, cut from `main`, owned by the calling session,
/// each build the merge lanes run seeded from main's with main's harness
/// binary taken out of it; the same session's second call claims it as it
/// stands, and another session's is refused. Where the volume does not
/// clone ([`clones`]), each build starts cold, one line saying so.
#[test]
fn the_queue_lays_the_merge_worktree_on_staging_owned_and_seeded_from_mains() {
    let root = repository("queue");
    let builds = ["debug", "release", "release-da"];
    for build in builds {
        write(&root, &format!("target/{build}/seeded"), build);
        write(&root, &format!("target/{build}/xtask"), "main's harness");
    }
    let clones = clones(&root);
    let (code, out) = by("the-coordinator", &root, "queue");
    assert_eq!(code, 0, "{out}");
    let dir = worktree_of(&root, "_merge");
    let head = |dir: &Path, what: &str| sh(dir, &["git", "rev-parse", what]);
    assert_eq!(
        sh(&dir, &["git", "symbolic-ref", "--short", "HEAD"]).trim(),
        "staging"
    );
    assert_eq!(head(&dir, "HEAD"), head(&root, "main"), "{out}");
    assert_eq!(
        xtask::owner::of(&dir).unwrap(),
        xtask::owner::SessionId::new("the-coordinator"),
        "{out}"
    );
    for build in builds {
        let seeded = dir.join(format!("target/{build}"));
        if clones {
            assert_eq!(fs::read_to_string(seeded.join("seeded")).unwrap(), build);
        } else {
            assert!(!seeded.join("seeded").exists(), "{build}: {out}");
            let cold = format!("target/{build} of the worktree starts cold");
            assert_eq!(out.matches(&cold).count(), 1, "{build}: {out}");
        }
        assert_ne!(
            fs::read_to_string(seeded.join("xtask")).ok().as_deref(),
            Some("main's harness"),
            "{build}: {out}"
        );
    }

    let (code, again) = by("the-coordinator", &root, "queue");
    assert_eq!(code, 0, "{again}");
    assert!(again.contains("stands on staging"), "{again}");
    assert_eq!(worktrees(&root).len(), 2, "{again}");
    let (code, other) = by("another-session", &root, "queue");
    assert_eq!(code, 1, "{other}");
    assert!(other.contains("is session the-coordinator's"), "{other}");
}

/// The judge `land` finds kept for main's tip in `root`, written as a
/// shell file at `target/judge/<tip>/xtask` with `body` after its first
/// line, so the land builds nothing; the tip.
fn judge_kept(root: &Path, body: &str) -> String {
    let tip = sh(root, &["git", "rev-parse", "main"]).trim().to_owned();
    let path = format!("target/judge/{tip}/xtask");
    write(root, &path, &format!("#!/bin/sh\n{body}"));
    xtask::scratch::executable(&root.join(&path)).unwrap();
    tip
}

/// [`judge_kept`] by the xtask under test, which is what a build of
/// main's tip is in a scratch repository of this working tree.
fn judged_by_this_build(root: &Path) -> String {
    let xtask = scratch::xtask(env!("CARGO_BIN_EXE_xtask"))
        .display()
        .to_string();
    judge_kept(root, &format!("exec '{xtask}' \"$@\"\n"))
}

/// `land` is the one step that moves `main`, run in main's
/// checkout and refused in `_merge`'s, whose xtask is the merge's own.
/// A harness source committed on `staging` with no reading over
/// it is unmet in `done`, as main's judges read it, so the land refuses
/// and `main` stands where it was, its store unwritten. Once `staging` is
/// reset to `main` with `--keep` and holds a change `done` meets, `main`
/// is fast-forwarded to it and `_merge`'s log is landed in main's store
/// and emptied, the worktree standing. A person's commit on `main`
/// refuses the next land, `main` untouched. Each verdict's reading names
/// land as its caller (Round 1, Q23, c11.b).
#[test]
fn a_red_merge_check_leaves_main_untouched_and_a_met_one_fast_forwards_it() {
    let root = repository("land");
    let (code, out) = by("the-coordinator", &root, "queue");
    assert_eq!(code, 0, "{out}");
    let merge = worktree_of(&root, "_merge");
    let main_at = || sh(&root, &["git", "rev-parse", "main"]);
    let before = main_at();
    judged_by_this_build(&root);

    write(&merge, "xtask/src/queued.rs", "pub fn queued() {}\n");
    commit_all(&merge, "a harness source no reading measured");
    let (code, there) = by("the-coordinator", &merge, "land");
    assert_eq!(code, 1, "{there}");
    assert!(there.contains("land runs in main's checkout"), "{there}");
    let (code, red) = by("the-coordinator", &root, "land");
    assert_eq!(code, 1, "{red}");
    assert!(red.contains("main's judges refuse"), "{red}");
    assert!(red.contains("main is untouched"), "{red}");
    assert_eq!(main_at(), before, "{red}");
    assert!(!root.join("target/harness/log").exists(), "{red}");
    let landed_by = |store: &Path| {
        let log = Store::of_workspace(store).history("done").unwrap();
        log.readings
            .into_iter()
            .next_back()
            .expect("a done reading")
    };
    let held = landed_by(&merge);
    assert_eq!(held.status, Status::Fail, "{held:#?}");
    assert!(held.message.starts_with("land: "), "{}", held.message);

    sh(&merge, &["git", "reset", "-q", "--keep", "main"]);
    write(&merge, "notes/queue.md", "A note no crate holds.\n");
    let met = commit_all(&merge, "a change done meets");
    let mut merged = Reading::new("clippy", "xtask");
    merged.message = "the merge worktree's reading".to_owned();
    let line = serde_json::to_string(&merged.to_json().unwrap()).unwrap();
    write(
        &merge,
        "target/harness/log/clippy.jsonl",
        &format!("{line}\n"),
    );
    let (code, green) = by("the-coordinator", &root, "land");
    assert_eq!(code, 0, "{green}");
    assert_eq!(main_at().trim(), met, "{green}");
    let landed = fs::read_to_string(root.join("target/harness/log/clippy.jsonl")).unwrap();
    assert!(landed.contains("the merge worktree's reading"), "{green}");
    assert_eq!(
        fs::read_to_string(merge.join("target/harness/log/clippy.jsonl")).unwrap(),
        "",
        "{green}"
    );
    assert!(merge.is_dir(), "the merge worktree stays: {green}");
    let met = landed_by(&root);
    assert_eq!(met.status, Status::Pass, "{met:#?}");
    assert!(met.message.starts_with("land: "), "{}", met.message);
    assert_eq!(worktrees(&root).len(), 2, "{green}");

    commit(&root, "a person's commit on main");
    let person = main_at();
    write(&merge, "notes/second.md", "A second note.\n");
    commit_all(&merge, "a second change");
    let (code, moved) = by("the-coordinator", &root, "land");
    assert_eq!(code, 1, "{moved}");
    assert!(moved.contains("main moved"), "{moved}");
    assert_eq!(main_at(), person, "{moved}");
}

/// The judges a land runs are main's tip's and never the branch's
/// (ruled 2026-11-09): a branch that changes `budget`'s source, and stands
/// a passing judge at its own head beside it, is judged by the one kept
/// for main's tip, which refuses here, and the call it was asked is
/// `_merge`'s judgement since that tip.
#[test]
fn a_branchs_change_to_a_judge_does_not_judge_that_branch() {
    let root = repository("land-judge");
    let (code, out) = by("the-coordinator", &root, "queue");
    assert_eq!(code, 0, "{out}");
    let merge = worktree_of(&root, "_merge");
    let before = sh(&root, &["git", "rev-parse", "main"]);
    let asked = root.with_file_name("asked");
    let tip = judge_kept(
        &root,
        &format!(
            "echo \"$*\" >> '{}'\necho \"main's judge refuses\" >&2\nexit 1\n",
            asked.display()
        ),
    );

    let budget = fs::read_to_string(merge.join("xtask/src/budget.rs")).unwrap();
    write(
        &merge,
        "xtask/src/budget.rs",
        &format!("{budget}\n// The branch's budget, which passes what it is handed.\n"),
    );
    let head = commit_all(&merge, "the branch's budget");
    for at in [&*root, &*merge] {
        write(
            at,
            &format!("target/judge/{head}/xtask"),
            "#!/bin/sh\nexit 0\n",
        );
    }
    let (code, out) = by("the-coordinator", &root, "land");
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("main's judge refuses"), "{out}");
    assert!(
        out.contains(&format!("main's xtask at {} refuses", &tip[..7])),
        "{out}"
    );
    assert!(out.contains("main is untouched"), "{out}");
    assert_eq!(sh(&root, &["git", "rev-parse", "main"]), before, "{out}");
    assert_eq!(
        fs::read_to_string(&asked).unwrap(),
        format!("--root {} worktree judge --base {tip}\n", merge.display()),
        "{out}"
    );
}

/// A merge whose `seams.toml` main's xtask cannot read is refused with
/// the two steps such a change lands in, `main` untouched: the
/// fixture is a table no reader at main's tip knows, the row of a grammar
/// change merged before the xtask that reads it, as one was.
#[test]
fn a_manifest_mains_xtask_cannot_read_is_refused_with_the_two_steps() {
    let root = repository("land-grammar");
    let (code, out) = by("the-coordinator", &root, "queue");
    assert_eq!(code, 0, "{out}");
    let merge = worktree_of(&root, "_merge");
    let before = sh(&root, &["git", "rev-parse", "main"]);
    judged_by_this_build(&root);
    let manifest = fs::read_to_string(merge.join("seams.toml")).unwrap();
    let row = fs::read_to_string(workspace().join("xtask/fixtures/land/grammar.toml")).unwrap();
    write(&merge, "seams.toml", &format!("{manifest}\n{row}"));
    commit_all(&merge, "a row of a grammar main's xtask does not read");
    let (code, out) = by("the-coordinator", &root, "land");
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("cannot read"), "{out}");
    assert!(out.contains("a_table_mains_xtask_does_not_read"), "{out}");
    assert!(out.contains("lands in two steps"), "{out}");
    assert!(
        out.contains("first the xtask that reads the new grammar"),
        "{out}"
    );
    assert!(
        out.contains("then, once main's xtask reads it, the row that does"),
        "{out}"
    );
    assert!(out.contains("main is untouched"), "{out}");
    assert_eq!(sh(&root, &["git", "rev-parse", "main"]), before, "{out}");
}

/// A merge that changes how the harness keys and writes its readings is
/// refused naming the case and the commands that take them by main's
/// xtask, and lands once they are taken. `xtask/src/key.rs` is
/// changed on `staging`; the harness's `clippy` readings stand over the
/// merged tree under another keying, a crate's `src` alone, as the
/// merge's own xtask would key them, and its `test` reading is written
/// in a version of the shape no reader at main's tip knows. Main's judge
/// holds the first stale and the second missing, as it must, since a
/// judge that read them by the merge's rules would be the merge's; the
/// refusal says so and names each command once, by the build that
/// judged, over `_merge`, and `main` is untouched. With each reading
/// taken over the key main's xtask computes, which in a scratch
/// repository of this working tree is this build's, the land
/// fast-forwards `main`. Those readings are stamped by this process and
/// the printed commands are not run here: `test --crate xtask` in a
/// scratch repository builds and runs the harness's whole suite. What
/// holds the commands' own run is outside this test: main's judge built
/// at 402d853c9, run with `--root` over this branch's worktree as `done
/// --peek --base 4731cc5f0`, read the worktree's readings and said every
/// condition of done was met, and a copy of this branch's xtask outside
/// its tree took `clippy --crate xtask` there the same way.
#[test]
fn a_merge_that_rekeys_its_readings_lands_once_mains_xtask_took_them() {
    let root = repository("land-keying");
    let (code, out) = by("the-coordinator", &root, "queue");
    assert_eq!(code, 0, "{out}");
    let merge = worktree_of(&root, "_merge");
    let main_at = || sh(&root, &["git", "rev-parse", "main"]);
    let before = main_at();
    judged_by_this_build(&root);
    let key = fs::read_to_string(merge.join("xtask/src/key.rs")).unwrap();
    write(
        &merge,
        "xtask/src/key.rs",
        &format!("{key}\n// The branch's keying, which hashes a crate's `src` alone.\n"),
    );
    let head = commit_all(&merge, "the branch's keying");

    let manifest = Manifest::load(&merge).unwrap();
    let harness = manifest.crate_named("xtask").unwrap();
    let store = Store::of_workspace(&merge);
    let snapshot = xtask::key::Snapshot::disk(&merge).unwrap();
    let taken = |instrument: &str, scope: &str, over: &Path| {
        let mut reading = Reading::stamped_over(instrument, scope, &snapshot, over).unwrap();
        reading.status = Status::Pass;
        reading
    };
    let lints = xtask::done::scopes_of(&merge, "clippy", harness).unwrap();
    let rekeyed: Vec<Reading> = (lints.iter())
        .map(|scope| taken("clippy", scope, Path::new("xtask/src")))
        .collect();
    store.record(&rekeyed).unwrap();
    write(
        &merge,
        "target/harness/test/xtask.toml",
        "v = 3\ninstrument = \"test\"\nscope = \"xtask\"\nstatus = \"pass\"\n",
    );

    let (code, out) = by("the-coordinator", &root, "land");
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("clippy: the reading for xtask is stale"),
        "{out}"
    );
    assert!(out.contains("test: no reading for xtask"), "{out}");
    assert!(
        out.contains("the merge changes the harness's own sources"),
        "{out}"
    );
    assert!(
        out.contains("lands on readings main's xtask takes"),
        "{out}"
    );
    let judge = scratch::xtask(env!("CARGO_BIN_EXE_xtask"));
    for ask in ["clippy --crate xtask", "test --crate xtask"] {
        let line = format!("  {} --root {} {ask}\n", judge.display(), merge.display());
        assert_eq!(out.matches(&line).count(), 1, "{line}{out}");
    }
    assert!(out.contains("then land again"), "{out}");
    assert!(out.contains("main is untouched"), "{out}");
    assert_eq!(main_at(), before, "{out}");

    let mut retaken: Vec<Reading> = (lints.iter())
        .map(|scope| taken("clippy", scope, &harness.path()))
        .collect();
    retaken.push(taken("test", "xtask", &harness.path()));
    store.record(&retaken).unwrap();
    let (code, out) = by("the-coordinator", &root, "land");
    assert_eq!(code, 0, "{out}");
    assert_eq!(main_at().trim(), head, "{out}");
}

/// A merge of the harness whose lock fails main's `api-check` is told,
/// beside `done`'s count, the instrument, the two things the failure can
/// be and the two steps, `main` untouched:
/// `xtask/src/api_check/mod.rs` is changed on `staging` and `xtask.lock`
/// committed with its lines in the other order. What this holds is that
/// the text is said and names the tree's own case first. It does not
/// stand the case the two steps are for, a lock the merge's own xtask
/// passes and main's fails: the judge here is the build under test, so
/// the merge's xtask and main's are one, and a lock out of order fails
/// both. Two builds that render a lock differently, and the steps
/// themselves, no test stands yet.
#[test]
fn a_lock_mains_xtask_renders_otherwise_is_refused_with_the_two_steps() {
    let root = repository("land-lock");
    let (code, out) = by("the-coordinator", &root, "queue");
    assert_eq!(code, 0, "{out}");
    let merge = worktree_of(&root, "_merge");
    let before = sh(&root, &["git", "rev-parse", "main"]);
    judged_by_this_build(&root);
    let source = fs::read_to_string(merge.join("xtask/src/api_check/mod.rs")).unwrap();
    write(
        &merge,
        "xtask/src/api_check/mod.rs",
        &format!("{source}\n// The branch's rendering, a lock's lines in the other order.\n"),
    );
    let lock = fs::read_to_string(merge.join("xtask/xtask.lock")).unwrap();
    let mut lines: Vec<&str> = lock.lines().collect();
    lines.reverse();
    write(&merge, "xtask/xtask.lock", &(lines.join("\n") + "\n"));
    commit_all(&merge, "the branch's rendering of a lock");

    let (code, out) = by("the-coordinator", &root, "land");
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("fail api-check harness"), "{out}");
    assert!(
        out.contains("main's xtask, which judges, fails api-check here"),
        "{out}"
    );
    let own = out
        .find("where the merge's own xtask fails it over this tree as well")
        .expect("the tree's own case");
    let steps = out
        .find("is meant to land in two steps")
        .expect("the two steps");
    assert!(own < steps, "{out}");
    assert!(
        out.contains("first the xtask that changes the instrument"),
        "{out}"
    );
    assert!(
        out.contains("which that xtask has to pass as well"),
        "{out}"
    );
    assert!(
        out.contains("then, once main's xtask is that one, what the tree changes by"),
        "{out}"
    );
    assert!(out.contains("main is untouched"), "{out}");
    assert_eq!(sh(&root, &["git", "rev-parse", "main"]), before, "{out}");
}

/// A merge that leaves `xtask/` alone is told neither text, though
/// `done` counts the harness as touched: `docs/divergence.toml`
/// is a file xtask's tests read (`key::READ_BY_TESTS`), so a merge that
/// changes it alone is asked the harness's readings, missing here, and
/// its xtask is the one main has, which keys and writes them as the
/// judge does. The refusal is `done`'s list and count, and the list's
/// own commands clear it.
#[test]
fn a_merge_that_changes_only_a_file_the_harnesss_tests_read_is_told_neither_text() {
    let root = repository("land-read-by-tests");
    let (code, out) = by("the-coordinator", &root, "queue");
    assert_eq!(code, 0, "{out}");
    let merge = worktree_of(&root, "_merge");
    let before = sh(&root, &["git", "rev-parse", "main"]);
    judged_by_this_build(&root);
    let registry = fs::read_to_string(merge.join("docs/divergence.toml")).unwrap();
    write(
        &merge,
        "docs/divergence.toml",
        &format!("{registry}\n# A comment no row reads.\n"),
    );
    commit_all(&merge, "a comment in the registry");

    let (code, out) = by("the-coordinator", &root, "land");
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("test: no reading for xtask"), "{out}");
    assert!(out.contains("of done unmet"), "{out}");
    for told in [
        "the merge changes the harness's own sources",
        "then land again",
        "in two steps",
    ] {
        assert!(!out.contains(told), "{told}: {out}");
    }
    assert!(out.contains("main is untouched"), "{out}");
    assert_eq!(sh(&root, &["git", "rev-parse", "main"]), before, "{out}");
}

/// `land` from main's checkout with a `cargo-deny` on `PATH` whose body
/// is `body`, ahead of the build's stand-in.
fn land_with_deny(root: &Path, body: &str) -> (i32, String) {
    let (_bin, path) = stand_in();
    let deny = Tree::new("worktree-deny").unwrap();
    write(&deny, "cargo-deny", &format!("#!/bin/sh\n{body}"));
    xtask::scratch::executable(&deny.join("cargo-deny")).unwrap();
    let mut on = OsString::from(deny.as_os_str());
    on.push(":");
    on.push(&path);
    let out = Command::new(scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
        .arg("--root")
        .arg(root)
        .args(["worktree", "land"])
        .current_dir(root)
        .env("PATH", &on)
        .env(xtask::owner::SESSION_VARIABLE, "the-coordinator")
        .env_remove("CARGO_MANIFEST_DIR")
        .output()
        .unwrap();
    joined(&out)
}

/// Where `Cargo.lock` differs from main's, the land runs `cargo deny
/// check advisories sources` under main's `deny.toml`, never the merge's
/// (ruled 2026-11-09): a release other than the one nightly.yml installs
/// refuses, and so does a check that finds anything; one that finds
/// nothing hands the merge on to `done`. The stand-in refuses
/// `check --config` with a usage error, exit 2, as 0.20.2 does, so the
/// argv is held to `--config <path> check advisories sources`, and a
/// usage error refuses as a check that judged nothing, never as a
/// finding. A merge that leaves the lock alone runs no check.
#[test]
fn a_merge_that_moves_the_lock_is_held_to_cargo_deny_under_mains_config() {
    let root = repository("land-deny");
    let (code, out) = by("the-coordinator", &root, "queue");
    assert_eq!(code, 0, "{out}");
    let merge = worktree_of(&root, "_merge");
    let before = sh(&root, &["git", "rev-parse", "main"]);
    judged_by_this_build(&root);
    let read = |path: &str| fs::read_to_string(merge.join(path)).unwrap();
    let mains = read("deny.toml");
    let lock = read("Cargo.lock");
    write(
        &merge,
        "Cargo.lock",
        &format!("{lock}# a lock the merge moved\n"),
    );
    write(
        &merge,
        "deny.toml",
        &format!("{mains}\n# the merge's own config\n"),
    );
    commit_all(&merge, "a lock and a config the merge moved");
    let seen = root.with_file_name("seen");
    let version = "if [ \"$1\" = --version ]; then echo 'cargo-deny 0.20.2'; exit 0; fi\n";
    // As 0.20.2 does: `--config` is cargo-deny's own option, and after
    // the subcommand it is a usage error, exit 2.
    let usage = "case \" $* \" in *\" check \"*\" --config \"*) echo \"error: unexpected argument '--config' found\" >&2; exit 2;; esac\n";
    let check = |exit: u8| {
        format!(
            "{version}{usage}echo \"$*\" > '{seen}'\nwhile [ $# -gt 0 ]; do [ \"$1\" = --config ] && cat \"$2\" >> '{seen}'; shift; done\nexit {exit}\n",
            seen = seen.display()
        )
    };

    let (code, old) = land_with_deny(&root, "echo 'cargo-deny 0.18.3'\n");
    assert_eq!(code, 1, "{old}");
    assert!(old.contains("not cargo-deny 0.20.2"), "{old}");
    assert!(old.contains("land refuses rather than pass"), "{old}");

    let (code, found) = land_with_deny(&root, &check(1));
    assert_eq!(code, 1, "{found}");
    assert!(
        found.contains("finds what main's deny.toml refuses"),
        "{found}"
    );
    let asked = fs::read_to_string(&seen).unwrap();
    assert!(asked.starts_with("--config "), "{asked}");
    assert!(asked.contains(" check advisories sources\n"), "{asked}");
    assert!(asked.ends_with(&mains), "main's config: {asked}");
    assert!(!asked.contains("the merge's own config"), "{asked}");
    assert_eq!(sh(&root, &["git", "rev-parse", "main"]), before, "{found}");

    // A usage error judged nothing, and says so rather than a finding.
    let (code, unjudged) = land_with_deny(&root, &check(2));
    assert_eq!(code, 1, "{unjudged}");
    assert!(unjudged.contains("cargo-deny judged nothing"), "{unjudged}");
    assert!(
        !unjudged.contains("finds what main's deny.toml refuses"),
        "{unjudged}"
    );

    let (_, passed) = land_with_deny(&root, &check(0));
    assert!(passed.contains("Cargo.lock moved since"), "{passed}");
    assert!(
        !passed.contains("land refuses rather than pass"),
        "{passed}"
    );

    sh(&merge, &["git", "reset", "-q", "--keep", "main"]);
    write(&merge, "notes/deny.md", "A note no lock holds.\n");
    commit_all(&merge, "a change that leaves the lock alone");
    fs::remove_file(&seen).unwrap();
    let (_, alone) = land_with_deny(&root, &check(1));
    assert!(!alone.contains("Cargo.lock moved since"), "{alone}");
    assert!(!seen.exists(), "{alone}");
}

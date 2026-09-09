//! `xtask track` (harness-design.md §4.5, §5): the three things every
//! reader of a track asks, answered for one worktree without a reading.
//! One test per shape over a scratch repository of the kind the `done`
//! tests use: a branch on a track, a branch whose prefixes name no branch,
//! the track itself, a detached `HEAD`, a branch not yet made (what the
//! add command asks for), and a linked worktree, whose store is the main
//! worktree's.

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::done::Base;
use xtask::track::{Found, MAIN, Track};

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

fn rev(root: &Path, name: &str) -> String {
    sh(root, &["git", "rev-parse", name]).trim().to_owned()
}

/// A scratch repository on `main` by name, one commit in, as the `done`
/// tests lay theirs down. The path is canonical, since that is what git
/// prints of it.
fn scratch(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("xtask-track-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    sh(&root, &["git", "init", "-q", "-b", "main"]);
    commit(&root, "scratch");
    std::fs::canonicalize(root).unwrap()
}

/// The scratch repository on `t-opus-5/witness`, one commit past the track
/// `t-opus-5`, itself one commit past `main` and `origin`'s branch alone,
/// for the reason the `done` tests give: git's ref store refuses
/// `refs/heads/t-opus-5` beside `refs/heads/t-opus-5/witness`.
fn on_a_track(name: &str) -> PathBuf {
    let root = scratch(name);
    sh(&root, &["git", "checkout", "-q", "-b", "the-track"]);
    commit(&root, "the track's own change");
    let tip = rev(&root, "HEAD");
    sh(
        &root,
        &["git", "update-ref", "refs/remotes/origin/t-opus-5", &tip],
    );
    sh(&root, &["git", "checkout", "-q", "main"]);
    sh(&root, &["git", "branch", "-q", "-D", "the-track"]);
    sh(
        &root,
        &[
            "git",
            "checkout",
            "-q",
            "-b",
            "t-opus-5/witness",
            "origin/t-opus-5",
        ],
    );
    commit(&root, "the turn's change");
    root
}

/// The binary, from `root`: exit code, stdout, stderr.
fn xtask(root: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
fn a_branch_on_a_track_names_it_and_is_measured_since_its_tip() {
    let root = on_a_track("track");
    let track = Track::of(&root, None).unwrap();
    assert_eq!(track.branch.as_deref(), Some("t-opus-5/witness"));
    assert_eq!(track.found, Found::Prefix);
    assert_eq!(track.name, "t-opus-5");
    assert_eq!(track.at, "origin/t-opus-5", "the track is origin's branch");
    assert_eq!(track.base, rev(&root, "origin/t-opus-5"));
    assert_eq!(track.store, root);

    let (code, out, err) = xtask(&root, &["track"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("branch: t-opus-5/witness\n"), "{out}");
    assert!(out.contains("track: t-opus-5"), "{out}");
    assert!(
        out.contains(&format!("base: {}", &track.base[..7])),
        "{out}"
    );
    assert!(out.contains(&format!("store: {}", root.display())), "{out}");
    assert!(
        !root.join("target/harness").exists(),
        "track writes no reading: it measures nothing"
    );
}

#[test]
fn a_branch_whose_prefixes_name_no_branch_is_on_main_and_says_so() {
    let root = scratch("none");
    sh(&root, &["git", "checkout", "-q", "-b", "pcre/preg-match"]);
    commit(&root, "preg_match");
    let track = Track::of(&root, None).unwrap();
    assert_eq!(track.branch.as_deref(), Some("pcre/preg-match"));
    assert_eq!(track.found, Found::NoPrefix);
    assert_eq!(track.name, MAIN);
    assert_eq!(track.at, MAIN);
    assert_eq!(track.base, rev(&root, "main"));

    let (code, out, _) = xtask(&root, &["track"]);
    assert_eq!(code, 0);
    assert!(out.contains("track: main"), "{out}");
    assert!(out.contains("name no branch"), "stated plainly: {out}");
}

#[test]
fn the_track_itself_and_main_are_on_main() {
    let root = scratch("itself");
    let track = Track::of(&root, None).unwrap();
    assert_eq!(track.name, MAIN);
    assert_eq!(track.found, Found::NoPrefix);
    assert_eq!(track.base, rev(&root, "HEAD"));
    let (_, out, _) = xtask(&root, &["track"]);
    assert!(out.contains("branch: main, the track itself"), "{out}");
}

#[test]
fn a_detached_head_is_on_no_track_and_is_stated_not_refused() {
    let root = on_a_track("detached");
    sh(&root, &["git", "checkout", "-q", "--detach"]);
    let track = Track::of(&root, None).unwrap();
    assert_eq!(track.branch, None);
    assert_eq!(track.found, Found::Detached);
    assert_eq!(track.name, MAIN);
    assert_eq!(track.base, rev(&root, "main"));

    let (code, out, err) = xtask(&root, &["track"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("branch: none"), "{out}");
    assert!(out.contains("detached"), "stated plainly: {out}");
    assert!(err.is_empty(), "{err}");
}

/// What the add command asks: the track of a branch not yet made, whose
/// base is the track's tip, where the branch is taken off.
#[test]
fn a_branch_not_yet_made_has_its_track_and_the_tracks_tip_as_its_base() {
    let root = on_a_track("not-yet");
    sh(&root, &["git", "checkout", "-q", "main"]);
    let track = Track::of(&root, Some("t-opus-5/phpt")).unwrap();
    assert_eq!(track.branch.as_deref(), Some("t-opus-5/phpt"));
    assert_eq!(track.found, Found::Prefix);
    assert_eq!(track.name, "t-opus-5");
    assert_eq!(track.at, "origin/t-opus-5");
    assert_eq!(track.base, rev(&root, "origin/t-opus-5"));

    let solo = Track::of(&root, Some("solo")).unwrap();
    assert_eq!(solo.found, Found::NoPrefix);
    assert_eq!(solo.base, rev(&root, "main"));

    let (code, out, _) = xtask(&root, &["track", "--branch", "t-opus-5/phpt"]);
    assert_eq!(code, 0);
    assert!(out.contains("track: t-opus-5 (origin/t-opus-5)"), "{out}");
}

#[test]
fn a_linked_worktrees_store_is_the_main_worktrees() {
    let root = on_a_track("linked");
    let dir = root.parent().unwrap().join(format!(
        "{}-wt",
        root.file_name().unwrap().to_string_lossy()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    sh(
        &root,
        &[
            "git",
            "worktree",
            "add",
            "-q",
            "-b",
            "t-opus-5/phpt",
            dir.to_str().unwrap(),
            "origin/t-opus-5",
        ],
    );
    let track = Track::of(&dir, None).unwrap();
    assert_eq!(track.name, "t-opus-5");
    assert_eq!(
        track.store, root,
        "the store is the track's root, not the worktree's"
    );
    assert_eq!(track.base, rev(&root, "origin/t-opus-5"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn json_is_the_same_facts_as_one_object() {
    let root = on_a_track("json");
    let (code, out, err) = xtask(&root, &["track", "--json"]);
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(value["branch"], "t-opus-5/witness");
    assert_eq!(value["track"], "t-opus-5");
    assert_eq!(value["found"], "prefix");
    assert_eq!(value["at"], "origin/t-opus-5");
    assert_eq!(value["base"], rev(&root, "origin/t-opus-5"));
    assert_eq!(value["store"], root.to_str().unwrap());

    sh(&root, &["git", "checkout", "-q", "--detach"]);
    let (_, out, _) = xtask(&root, &["track", "--json"]);
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["branch"], serde_json::Value::Null);
    assert_eq!(value["found"], "detached");
    assert_eq!(value["track"], "main");
}

/// `done`'s base is the track's, taken through the same function.
#[test]
fn dones_base_is_the_tracks() {
    let root = on_a_track("base");
    let base = Base::of(&root, None).unwrap();
    let track = Track::of(&root, None).unwrap();
    assert_eq!(base.since, track.name);
    assert_eq!(base.commit, track.base);
    let given = Base::of(&root, Some("main")).unwrap();
    assert_eq!(given.since, "main");
    assert_eq!(given.commit, rev(&root, "main"));
}

#[test]
fn track_is_listed_by_the_help() {
    let (code, out, _) = xtask(Path::new("."), &["--help"]);
    assert_eq!(code, 0);
    assert!(out.contains("\n  track "), "{out}");
    assert!(xtask::cli::instruments().contains(&"track".to_owned()));
}

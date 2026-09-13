//! `xtask track new` (harness-design.md §5, §11): a track laid down by one
//! command, against a scratch repository with a scratch `origin`, a bare
//! repository beside it, as the worktree tests run against a scratch of
//! this working tree's files, so the `xtask` the command builds in the
//! clone is this one.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::scratch::Tree;

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

/// Whether `name` is a ref at `root`, which for the bare origin is
/// whether the track's branch was pushed.
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

/// The command, this crate's own binary, run against the scratch root.
fn track_new(root: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("--root")
        .arg(root)
        .args(["track", "new"])
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
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.code().unwrap_or(-1), text)
}

/// A scratch repository of this working tree's files, one commit on
/// `main`, with a bare `origin` beside it that `main` is pushed to: the
/// repository and its remote, both under one scratch tree, which comes
/// back with them and takes both away when the test ends. A scratch
/// tree's path is canonical, so the clone's origin compares equal to the
/// remote's URL on a system whose temporary directory is a symbolic link.
fn repository(name: &str) -> (Tree, PathBuf, PathBuf) {
    let base = Tree::new(&format!("track-new-{name}")).unwrap();
    let root = base.join("repo");
    let origin = base.join("origin.git");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&origin).unwrap();
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
    sh(&root, &["git", "push", "-q", "origin", "main"]);
    (base, root, origin)
}

/// The one command: `origin/<name>` at `main`, a clone beside the
/// repository detached at it and recording its track and its model,
/// `xtask` built there, the console's row, the trust line printed; and
/// `xtask track` in the clone, detached and in a branch of it, reads the
/// track as the clone's.
#[test]
fn a_track_is_laid_down_by_one_command() {
    let (_base, root, origin) = repository("lay");
    let (code, out) = track_new(
        &root,
        &[
            "t-y",
            "--model",
            "claude-opus-5",
            "--subagent-model",
            "claude-sonnet-5",
        ],
    );
    assert_eq!(code, 0, "{out}");

    // origin/<name> is main.
    assert_eq!(rev(&origin, "t-y"), rev(&root, "main"), "{out}");
    assert_eq!(rev(&root, "origin/t-y"), rev(&root, "main"), "{out}");
    assert!(
        !has_ref(&root, "refs/heads/t-y"),
        "the track is origin's branch, never the clone's own: {out}"
    );

    // The clone is beside the repository, detached at the track.
    let clone = root.parent().unwrap().join("repo-t-y");
    assert!(clone.is_dir(), "{}: {out}", clone.display());
    assert_eq!(rev(&clone, "HEAD"), rev(&root, "main"), "{out}");
    assert!(
        !has_ref(&clone, "refs/heads/t-y"),
        "the clone has no branch of the track's name: {out}"
    );
    let head = Command::new("git")
        .args(["-C", clone.to_str().unwrap(), "symbolic-ref", "-q", "HEAD"])
        .output()
        .unwrap();
    assert!(
        !head.status.success(),
        "detached, so HEAD is no symbolic ref: {out}"
    );
    assert_eq!(
        sh(&clone, &["git", "remote", "get-url", "origin"]).trim(),
        origin.to_str().unwrap(),
        "the clone's origin is the repository's: {out}"
    );

    // The clone records its track and its model, one key per role.
    assert_eq!(
        sh(&clone, &["git", "config", "--get-regexp", "^track\\."]),
        "track.name t-y\ntrack.model.session claude-opus-5\ntrack.model.subagent claude-sonnet-5\n",
        "{out}"
    );
    assert!(out.contains("track.model.session=claude-opus-5"), "{out}");

    // xtask is built in the clone; detached, the clone reads the track it
    // records and its model, and a branch of the track in it reads the
    // track as <name>, both with the clone as the store.
    let built = clone.join("target/debug/xtask");
    assert!(built.is_file(), "no built xtask: {out}");
    let track = Command::new(&built)
        .args(["track", "--json"])
        .current_dir(&clone)
        .output()
        .unwrap();
    let facts: serde_json::Value =
        serde_json::from_slice(&track.stdout).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(facts["branch"], serde_json::Value::Null, "{facts}");
    assert_eq!(facts["found"], "record", "{facts}");
    assert_eq!(facts["track"], "t-y", "not main: {facts}");
    assert_eq!(facts["at"], "origin/t-y", "{facts}");
    assert_eq!(facts["store"], clone.to_str().unwrap(), "{facts}");
    assert_eq!(facts["model"]["session"], "claude-opus-5", "{facts}");
    assert_eq!(facts["model"]["subagent"], "claude-sonnet-5", "{facts}");
    sh(
        &clone,
        &["git", "checkout", "-q", "-b", "t-y/x", "origin/t-y"],
    );
    let track = Command::new(&built)
        .args(["track", "--json"])
        .current_dir(&clone)
        .output()
        .unwrap();
    let facts: serde_json::Value =
        serde_json::from_slice(&track.stdout).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(facts["branch"], "t-y/x", "{facts}");
    assert_eq!(facts["track"], "t-y", "{facts}");
    assert_eq!(facts["at"], "origin/t-y", "{facts}");
    assert_eq!(facts["store"], clone.to_str().unwrap(), "{facts}");
    assert_eq!(facts["model"]["session"], "claude-opus-5", "{facts}");

    // The console's track list carries the row, after the rows it had.
    let list = fs::read_to_string(root.join("console/tracks.json")).unwrap();
    let rows: serde_json::Value =
        serde_json::from_str(&list).unwrap_or_else(|e| panic!("{e}: {list}"));
    assert_eq!(rows["t-y"], "../repo-t-y", "{list}");
    assert!(
        list.trim_start().starts_with("{\n  \"main\": \".\""),
        "main stays first: {list}"
    );
    assert!(out.contains("console/tracks.json"), "{out}");

    // The grant is a person's, printed.
    assert!(
        out.contains(&format!("computed trust {}", clone.display())),
        "{out}"
    );
    assert!(out.contains("a person runs, once"), "{out}");
}

/// A branch the repository or `origin` has, and a directory that exists,
/// refuse before anything is pushed, cloned or written.
#[test]
fn an_existing_branch_or_directory_refuses_and_changes_nothing() {
    let (_base, root, origin) = repository("refuse");
    let list = fs::read_to_string(root.join("console/tracks.json")).unwrap();
    let unchanged = |name: &str, out: &str| {
        assert!(!has_ref(&origin, &format!("refs/heads/{name}")), "{out}");
        assert!(
            !root.parent().unwrap().join(format!("repo-{name}")).exists(),
            "{out}"
        );
        assert_eq!(
            fs::read_to_string(root.join("console/tracks.json")).unwrap(),
            list,
            "{out}"
        );
    };

    // The repository's own branch.
    sh(&root, &["git", "branch", "t-local"]);
    let (code, out) = track_new(&root, &["t-local", "--model", "claude-opus-5"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("t-local"), "{out}");
    unchanged("t-local", &out);

    // origin's branch, which the repository has not fetched: origin is
    // asked, not the remote-tracking refs.
    sh(
        &root,
        &["git", "push", "-q", "origin", "main:refs/heads/t-remote"],
    );
    sh(
        &root,
        &["git", "update-ref", "-d", "refs/remotes/origin/t-remote"],
    );
    let (code, out) = track_new(&root, &["t-remote", "--model", "claude-opus-5"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("t-remote"), "{out}");
    assert!(
        !root.parent().unwrap().join("repo-t-remote").exists(),
        "{out}"
    );
    assert_eq!(
        fs::read_to_string(root.join("console/tracks.json")).unwrap(),
        list,
        "{out}"
    );

    // The directory.
    fs::create_dir_all(root.parent().unwrap().join("repo-t-dir")).unwrap();
    let (code, out) = track_new(&root, &["t-dir", "--model", "claude-opus-5"]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("repo-t-dir"), "{out}");
    assert!(!has_ref(&origin, "refs/heads/t-dir"), "{out}");
    assert_eq!(
        fs::read_to_string(root.join("console/tracks.json")).unwrap(),
        list,
        "{out}"
    );

    // A name with a slash is not a track's, the track being the prefix.
    let (code, out) = track_new(&root, &["t/dir", "--model", "claude-opus-5"]);
    assert_eq!(code, 1, "{out}");
    unchanged("t/dir", &out);

    // No name is a usage error, and so is no model: a track laid down by
    // the command records one.
    let (code, out) = track_new(&root, &["--model", "claude-opus-5"]);
    assert_eq!(code, 2, "{out}");
    let (code, out) = track_new(&root, &["t-z"]);
    assert_eq!(code, 2, "{out}");
    unchanged("t-z", &out);
}

//! `xtask track sync`, a track caught up with main (#80; harness-design.md
//! §5): held against a scratch repository shaped as a track's clone,
//! detached at `origin/t-x` with the track recorded, a bare `origin`
//! beside it whose `main` a test moves from a branch of the clone, as a
//! commit landing on main does.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::scratch::{self, Tree};

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

/// The scratch: the track's clone at `root`, its bare `origin.git`, and
/// the `PATH` the sync runs under, with the build stood in by
/// [`scratch::cargo`].
struct Scratch {
    /// The tree both of them are under, held for as long as the test
    /// reads them and dropped with it, which is what takes them away.
    #[expect(dead_code, reason = "held to own the tree, never read")]
    dir: Tree,
    root: PathBuf,
    origin: PathBuf,
    path: OsString,
}

impl Scratch {
    /// A track's clone of this working tree's files: one commit on
    /// `main`, pushed to `origin` as `main` and as `t-x`, the clone
    /// detached at `origin/t-x` recording `track.name`.
    fn lay(name: &str) -> Scratch {
        let dir = Tree::new(&format!("track-sync-{name}")).unwrap();
        let root = dir.join("repo");
        let origin = dir.join("origin.git");
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
        commit(&root, "scratch");
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
                "main:refs/heads/t-x",
            ],
        );
        sh(&root, &["git", "checkout", "-q", "--detach", "origin/t-x"]);
        sh(&root, &["git", "config", "track.name", "t-x"]);
        let path =
            scratch::cargo(&dir.join("bin"), Path::new(env!("CARGO_BIN_EXE_xtask"))).unwrap();
        Scratch {
            dir,
            root,
            origin,
            path,
        }
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
        commit(&self.root, &format!("{file} on {branch}"));
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

    /// The command, this crate's own binary, run against the clone.
    fn sync(&self) -> (i32, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
            .arg("--root")
            .arg(&self.root)
            .args(["track", "sync"])
            .current_dir(&self.root)
            .env("PATH", &self.path)
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

    fn clean(&self) -> bool {
        sh(&self.root, &["git", "status", "--porcelain"]).is_empty()
            && !self.root.join(".git/MERGE_HEAD").exists()
    }
}

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
            "-m",
            message,
        ],
    );
}

/// The whole path, twice: main moved with the track still at main's old
/// tip, so the track fast-forwards to main; then the track moved too, so
/// main is merged into it as one commit whose parents are the track's
/// tip and main's. Each time the branch is pushed to `origin`, HEAD
/// follows it, `xtask` is built and one line says what moved.
#[test]
fn main_ahead_is_merged_pushed_redetached_and_built() {
    let scratch = Scratch::lay("moved");
    let old = rev(&scratch.root, "origin/t-x");
    let main = scratch.land("main", "landed.txt", "on main\n");
    let (code, out) = scratch.sync();
    assert_eq!(code, 0, "{out}");
    assert_eq!(rev(&scratch.origin, "t-x"), main, "fast-forwarded: {out}");
    assert_eq!(rev(&scratch.root, "origin/t-x"), main, "{out}");
    assert_eq!(rev(&scratch.root, "HEAD"), main, "HEAD follows: {out}");
    assert!(scratch.root.join("landed.txt").is_file(), "{out}");
    assert!(scratch.clean(), "{out}");
    assert!(
        out.lines().any(|line| line
            == format!(
                "track sync: t-x caught up with main ({}): fast-forwarded {} to {}; pushed to origin, HEAD follows, xtask built",
                &main[..7],
                &old[..7],
                &main[..7]
            )),
        "{out}"
    );
    assert!(
        scratch.root.join("target/debug/xtask").is_file(),
        "no built xtask: {out}"
    );

    let track = scratch.land("t-x", "track.txt", "on the track\n");
    let main = scratch.land("main", "landed.txt", "on main, again\n");
    let (code, out) = scratch.sync();
    assert_eq!(code, 0, "{out}");
    let tip = rev(&scratch.origin, "t-x");
    assert_ne!(tip, track, "{out}");
    assert_eq!(rev(&scratch.root, "origin/t-x"), tip, "{out}");
    assert_eq!(rev(&scratch.root, "HEAD"), tip, "HEAD follows: {out}");
    assert_eq!(
        sh(&scratch.root, &["git", "rev-parse", "HEAD^1", "HEAD^2"]),
        format!("{track}\n{main}\n"),
        "one merge commit, the track's tip first: {out}"
    );
    assert_eq!(
        fs::read_to_string(scratch.root.join("landed.txt")).unwrap(),
        "on main, again\n"
    );
    assert!(scratch.root.join("track.txt").is_file(), "{out}");
    assert!(scratch.clean(), "{out}");
    assert!(
        out.lines().any(|line| line
            == format!(
                "track sync: t-x caught up with main ({}): merged it into {} as {}; pushed to origin, HEAD follows, xtask built",
                &main[..7],
                &track[..7],
                &tip[..7]
            )),
        "{out}"
    );
}

/// A track that has main's tip already moves nothing and builds nothing.
#[test]
fn a_track_with_mains_tip_already_moves_nothing() {
    let scratch = Scratch::lay("already");
    let tip = rev(&scratch.root, "origin/t-x");
    let (code, out) = scratch.sync();
    assert_eq!(code, 0, "{out}");
    assert_eq!(rev(&scratch.origin, "t-x"), tip, "{out}");
    assert_eq!(rev(&scratch.root, "HEAD"), tip, "{out}");
    assert!(
        out.contains(&format!(
            "track sync: t-x has main's tip ({}) already; nothing moved",
            &tip[..7]
        )),
        "{out}"
    );
    assert!(
        !scratch.root.join("target/debug/xtask").exists(),
        "nothing built: {out}"
    );

    // The track ahead of main is the same shape.
    let ahead = scratch.land("t-x", "track.txt", "on the track\n");
    let (code, out) = scratch.sync();
    assert_eq!(code, 0, "{out}");
    assert_eq!(rev(&scratch.origin, "t-x"), ahead, "{out}");
    assert!(out.contains("nothing moved"), "{out}");
    assert!(
        !scratch.root.join("target/debug/xtask").exists(),
        "nothing built: {out}"
    );
}

/// A merge that conflicts refuses, names the conflict, and leaves the
/// branch, `origin` and the clone as they were: no merge in progress,
/// nothing built.
#[test]
fn a_conflict_refuses_and_changes_nothing() {
    let scratch = Scratch::lay("conflict");
    let track = scratch.land("t-x", "README.md", "the track's\n");
    let main = scratch.land("main", "README.md", "main's\n");
    let (code, out) = scratch.sync();
    assert_eq!(code, 1, "{out}");
    assert!(
        out.contains("main does not merge into t-x: conflicts in README.md"),
        "{out}"
    );
    assert!(out.contains("as they were"), "{out}");
    assert_eq!(rev(&scratch.origin, "t-x"), track, "{out}");
    assert_eq!(rev(&scratch.origin, "main"), main, "{out}");
    assert_eq!(rev(&scratch.root, "HEAD"), track, "{out}");
    assert!(scratch.clean(), "{out}");
    assert!(
        !scratch.root.join("target/debug/xtask").exists(),
        "nothing built: {out}"
    );
}

/// A clone on `main`, which is no track's, has nothing to catch up with.
#[test]
fn main_has_nothing_to_catch_up_with() {
    let scratch = Scratch::lay("main");
    sh(&scratch.root, &["git", "config", "--unset", "track.name"]);
    let tip = rev(&scratch.root, "HEAD");
    let (code, out) = scratch.sync();
    assert_eq!(code, 0, "{out}");
    assert!(
        out.contains(
            "track sync: the track is main, which has nothing to catch up with; nothing moved"
        ),
        "{out}"
    );
    assert_eq!(rev(&scratch.root, "HEAD"), tip, "{out}");
}

/// A HEAD on a branch of the track is a worktree or a person's checkout,
/// so the sync refuses before anything moves; a HEAD detached elsewhere
/// than the track's tip is re-detached at the new tip, since the build
/// that follows is of the checkout.
#[test]
fn a_head_on_a_branch_refuses_and_a_detached_head_elsewhere_follows() {
    let scratch = Scratch::lay("head");
    let old = rev(&scratch.root, "origin/t-x");
    let main = scratch.land("main", "landed.txt", "on main\n");
    sh(
        &scratch.root,
        &["git", "checkout", "-q", "-b", "t-x/person", "origin/t-x"],
    );
    let (code, out) = scratch.sync();
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("HEAD is on t-x/person"), "{out}");
    assert!(out.contains("nothing moved"), "{out}");
    assert_eq!(rev(&scratch.origin, "t-x"), old, "{out}");
    assert_eq!(rev(&scratch.root, "HEAD"), old, "{out}");

    // Detached behind the track, which moved on origin meanwhile.
    sh(&scratch.root, &["git", "checkout", "-q", "--detach", &old]);
    sh(&scratch.root, &["git", "branch", "-q", "-D", "t-x/person"]);
    let track = scratch.land("t-x", "track.txt", "on the track\n");
    sh(&scratch.root, &["git", "checkout", "-q", "--detach", &old]);
    let (code, out) = scratch.sync();
    assert_eq!(code, 0, "{out}");
    let tip = rev(&scratch.origin, "t-x");
    assert_eq!(
        sh(&scratch.root, &["git", "rev-parse", "HEAD^1", "HEAD^2"]),
        format!("{track}\n{main}\n"),
        "{out}"
    );
    assert_eq!(rev(&scratch.root, "HEAD"), tip, "HEAD re-detached: {out}");
    assert!(out.contains("HEAD follows, xtask built"), "{out}");
}

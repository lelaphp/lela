//! `xtask track` (harness-design.md §5, ADR-0009): what every reader of
//! a worktree asks, answered without a reading: the branch, the commit it
//! was taken off `main` at, the main checkout whose `target/harness` is
//! the one store, and the ticket the branch records. One test per shape
//! over a scratch repository of the kind the `done` tests use: a ticket's
//! branch, `main` itself, a detached `HEAD`, a branch not yet made (what
//! the add command asks for), a linked worktree, whose store is the main
//! worktree's, `main` known only as `origin`'s, and the ticket record.

use std::path::Path;
use std::process::Command;

use xtask::scratch::Tree;
use xtask::track::{Ticket, Track};

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
/// tests lay theirs down. A scratch tree's path is canonical, which is
/// what git prints of it.
fn scratch(name: &str) -> Tree {
    let root = Tree::new(&format!("track-{name}")).unwrap();
    sh(&root, &["git", "init", "-q", "-b", "main"]);
    commit(&root, "scratch");
    root
}

/// The scratch repository on `t30`, a ticket's branch cut from `main`
/// with one commit of its own, while `main` moves on by one more: the
/// base is where the branch was taken off, not `main`'s tip.
fn on_a_branch(name: &str) -> Tree {
    let root = scratch(name);
    sh(&root, &["git", "checkout", "-q", "-b", "t30"]);
    commit(&root, "the ticket's change");
    sh(&root, &["git", "checkout", "-q", "main"]);
    commit(&root, "main moves on");
    sh(&root, &["git", "checkout", "-q", "t30"]);
    root
}

/// The binary, from `root`: exit code, stdout, stderr.
fn xtask(root: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
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
fn a_tickets_branch_is_measured_since_where_it_was_taken_off_main() {
    let root = on_a_branch("branch");
    let taken_off = rev(&root, "t30^");
    let track = Track::of(&root, None).unwrap();
    assert_eq!(track.branch.as_deref(), Some("t30"));
    assert_eq!(track.base, taken_off, "the merge base, not main's tip");
    assert_eq!(track.store, root.path());
    assert_eq!(track.ticket, None);

    let (code, out, err) = xtask(&root, &["track"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        out,
        format!(
            "branch: t30\nbase: {}, where the branch was taken off main\nstore: {}, whose target/harness is the one store\nticket: none; the branch records no ticket, and none is guessed from its name\n",
            &taken_off[..7],
            root.display()
        )
    );
    assert!(!out.contains("track:"), "no track is printed: {out}");
    assert!(
        !root.join("target/harness").exists(),
        "track writes no reading: it measures nothing"
    );
}

#[test]
fn main_is_its_own_base() {
    let root = scratch("main");
    let track = Track::of(&root, None).unwrap();
    assert_eq!(track.branch.as_deref(), Some("main"));
    assert_eq!(track.base, rev(&root, "HEAD"));
    let (_, out, _) = xtask(&root, &["track"]);
    assert!(out.starts_with("branch: main\n"), "{out}");
}

#[test]
fn a_detached_head_is_stated_not_refused() {
    let root = on_a_branch("detached");
    sh(&root, &["git", "checkout", "-q", "--detach"]);
    let track = Track::of(&root, None).unwrap();
    assert_eq!(track.branch, None);
    assert_eq!(track.base, rev(&root, "HEAD^"));
    assert_eq!(track.ticket, None);

    let (code, out, err) = xtask(&root, &["track"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("branch: none; HEAD is detached"), "{out}");
    assert!(err.is_empty(), "{err}");
}

/// What the add command asks: a branch not yet made is taken off
/// `main`'s tip.
#[test]
fn a_branch_not_yet_made_is_taken_off_mains_tip() {
    let root = on_a_branch("not-yet");
    let track = Track::of(&root, Some("t31")).unwrap();
    assert_eq!(track.branch.as_deref(), Some("t31"));
    assert_eq!(track.base, rev(&root, "main"));

    let (code, out, _) = xtask(&root, &["track", "--branch", "t31"]);
    assert_eq!(code, 0);
    assert!(out.starts_with("branch: t31\n"), "{out}");
}

/// A clone whose `main` is `origin`'s alone measures since that.
#[test]
fn main_known_only_as_origins_is_the_base_all_the_same() {
    let root = on_a_branch("origin");
    let main = rev(&root, "main");
    sh(
        &root,
        &["git", "update-ref", "refs/remotes/origin/main", &main],
    );
    sh(&root, &["git", "branch", "-q", "-D", "main"]);
    let track = Track::of(&root, Some("t31")).unwrap();
    assert_eq!(track.base, main);
}

#[test]
fn a_linked_worktrees_store_is_the_main_worktrees() {
    let root = on_a_branch("linked");
    sh(&root, &["git", "checkout", "-q", "main"]);
    // The linked worktree stands beside the repository, in a scratch tree
    // of its own: `git worktree add` writes the directory itself, and one
    // that is already there and not empty is refused.
    let beside = Tree::new("track-linked-worktree").unwrap();
    let dir = beside.join("wt");
    sh(
        &root,
        &["git", "worktree", "add", "-q", dir.to_str().unwrap(), "t30"],
    );
    let track = Track::of(&dir, None).unwrap();
    assert_eq!(track.branch.as_deref(), Some("t30"));
    assert_eq!(
        track.store,
        root.path(),
        "the store is the main checkout's, not the worktree's"
    );
    assert_eq!(track.base, rev(&root, "t30^"));
}

#[test]
fn json_is_the_same_facts_as_one_object() {
    let root = on_a_branch("json");
    sh(&root, &["git", "config", "branch.t30.ticket", "30"]);
    let (code, out, err) = xtask(&root, &["track", "--json"]);
    assert_eq!(code, 0, "{err}");
    let value: serde_json::Value =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(
        value,
        serde_json::json!({
            "branch": "t30",
            "base": rev(&root, "t30^"),
            "store": root.to_str().unwrap(),
            "ticket": 30,
        })
    );

    sh(&root, &["git", "checkout", "-q", "--detach"]);
    let (_, out, _) = xtask(&root, &["track", "--json"]);
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["branch"], serde_json::Value::Null);
    assert_eq!(value["ticket"], serde_json::Value::Null);
}

#[test]
fn track_is_listed_by_the_help() {
    let (code, out, _) = xtask(Path::new("."), &["--help"]);
    assert_eq!(code, 0);
    assert!(out.contains("\n  track "), "{out}");
}

/// A track is a label on the tracker, laid down and caught up by nothing
/// (ADR-0009): `track` has no `new` and no `sync`.
#[test]
fn track_lays_no_track_down() {
    let (code, out, _) = xtask(Path::new("."), &["track", "--help"]);
    assert_eq!(code, 0);
    for command in ["next", "chains", "work"] {
        assert!(out.contains(&format!("\n  {command} ")), "{out}");
    }
    assert!(!out.contains("\n  new "), "{out}");
    assert!(!out.contains("\n  sync "), "{out}");
}

/// The ticket is a record on the branch, read by name (#30): a branch
/// with none says so, and nothing is guessed from a branch name that
/// carries a number.
#[test]
fn a_branch_names_its_ticket_by_record_and_never_by_its_name() {
    let root = on_a_branch("ticket");
    let none = Track::of(&root, None).unwrap();
    assert_eq!(none.ticket, None, "a number in the name is not a ticket");
    let (code, out, _) = xtask(&root, &["track"]);
    assert_eq!(code, 0);
    assert!(out.contains("ticket: none"), "stated plainly: {out}");

    sh(&root, &["git", "config", "branch.t30.ticket", "30"]);
    let recorded = Track::of(&root, None).unwrap();
    assert_eq!(recorded.ticket, Some(Ticket::new(30)));
    let (code, out, _) = xtask(&root, &["track"]);
    assert_eq!(code, 0);
    assert!(
        out.contains("ticket: #30, what the branch's work is for"),
        "{out}"
    );

    // A record that is not a number is an error, not a branch with none.
    sh(&root, &["git", "config", "branch.t30.ticket", "thirty"]);
    let err = Track::of(&root, Some("t30")).unwrap_err();
    assert!(format!("{err:#}").contains("thirty"), "{err:#}");
}

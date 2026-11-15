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

mod common {
    pub mod repo;
    pub mod run;
}
use common::repo::Repo;
use common::run::xtask;

fn rev(root: &Path, name: &str) -> String {
    Repo::at(root).git(&["rev-parse", name])
}

/// A scratch repository on `main` by name, one commit in, as the `done`
/// tests lay theirs down. A scratch tree's path is canonical, which is
/// what git prints of it.
fn scratch(name: &str) -> Tree {
    let root = Tree::new(&format!("track-{name}")).unwrap();
    Repo::init(&root).commit("scratch");
    root
}

/// The scratch repository on `t30`, a ticket's branch cut from `main`
/// with one commit of its own, while `main` moves on by one more: the
/// base is where the branch was taken off, not `main`'s tip.
fn on_a_branch(name: &str) -> Tree {
    let root = scratch(name);
    let repo = Repo::at(&root);
    repo.git(&["checkout", "-q", "-b", "t30"]);
    repo.commit("the ticket's change");
    repo.git(&["checkout", "-q", "main"]);
    repo.commit("main moves on");
    repo.git(&["checkout", "-q", "t30"]);
    root
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

    let out = xtask(&root, &["track"]).ok();
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
    let out = xtask(&root, &["track"]).ok();
    assert!(out.starts_with("branch: main\n"), "{out}");
}

#[test]
fn a_detached_head_is_stated_not_refused() {
    let root = on_a_branch("detached");
    Repo::at(&root).git(&["checkout", "-q", "--detach"]);
    let track = Track::of(&root, None).unwrap();
    assert_eq!(track.branch, None);
    assert_eq!(track.base, rev(&root, "HEAD^"));
    assert_eq!(track.ticket, None);

    let ran = xtask(&root, &["track"]);
    assert!(ran.stderr.is_empty(), "{ran}");
    let out = ran.ok();
    assert!(out.contains("branch: none; HEAD is detached"), "{out}");
}

/// What the add command asks: a branch not yet made is taken off
/// `main`'s tip.
#[test]
fn a_branch_not_yet_made_is_taken_off_mains_tip() {
    let root = on_a_branch("not-yet");
    let track = Track::of(&root, Some("t31")).unwrap();
    assert_eq!(track.branch.as_deref(), Some("t31"));
    assert_eq!(track.base, rev(&root, "main"));

    let out = xtask(&root, &["track", "--branch", "t31"]).ok();
    assert!(out.starts_with("branch: t31\n"), "{out}");
}

/// A clone whose `main` is `origin`'s alone measures since that.
#[test]
fn main_known_only_as_origins_is_the_base_all_the_same() {
    let root = on_a_branch("origin");
    let main = rev(&root, "main");
    Repo::at(&root).git(&["update-ref", "refs/remotes/origin/main", &main]);
    Repo::at(&root).git(&["branch", "-q", "-D", "main"]);
    let track = Track::of(&root, Some("t31")).unwrap();
    assert_eq!(track.base, main);
}

#[test]
fn a_linked_worktrees_store_is_the_main_worktrees() {
    let root = on_a_branch("linked");
    Repo::at(&root).git(&["checkout", "-q", "main"]);
    // The linked worktree stands beside the repository, in a scratch tree
    // of its own: `git worktree add` writes the directory itself, and one
    // that is already there and not empty is refused.
    let beside = Tree::new("track-linked-worktree").unwrap();
    let dir = beside.join("wt");
    Repo::at(&root).git(&["worktree", "add", "-q", dir.to_str().unwrap(), "t30"]);
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
    Repo::at(&root).git(&["config", "branch.t30.ticket", "30"]);
    let out = xtask(&root, &["track", "--json"]).ok();
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

    Repo::at(&root).git(&["checkout", "-q", "--detach"]);
    let out = xtask(&root, &["track", "--json"]).ok();
    let value: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(value["branch"], serde_json::Value::Null);
    assert_eq!(value["ticket"], serde_json::Value::Null);
}

/// The runner every integration test runs the binary through: a
/// run held to an end it did not reach fails the test with its command
/// line, how it ended and both streams, all a reader of the failure has.
#[test]
fn a_run_that_ends_otherwise_than_asked_prints_its_line_its_exit_and_its_streams() {
    let root = scratch("ran");
    let ran = xtask(&root, &["no-such-instrument"]);
    assert_eq!(ran.code(), 2, "{ran}");
    let printed = ran.to_string();
    let line = format!("--root {} no-such-instrument\nexit 2\n", root.display());
    assert!(printed.contains(&line), "{printed}");
    assert!(
        printed.contains("--- stderr\nerror: unrecognized subcommand"),
        "{printed}"
    );

    let held = std::panic::catch_unwind(|| xtask(&root, &["no-such-instrument"]).ok());
    let said = held.expect_err("a failure held to success fails the test");
    let said = said.downcast_ref::<String>().expect("the panic's message");
    assert!(said.contains(&printed), "{said}");
    let err = xtask(&root, &["no-such-instrument"]).failed();
    assert!(err.starts_with("error: unrecognized subcommand"), "{err}");
}

#[test]
fn track_is_listed_by_the_help() {
    let out = xtask(Path::new("."), &["--help"]).ok();
    assert!(out.contains("\n  track "), "{out}");
}

/// A track is a label on the tracker, laid down and caught up by nothing
/// (ADR-0009): `track` has no `new` and no `sync`.
#[test]
fn track_lays_no_track_down() {
    let out = xtask(Path::new("."), &["track", "--help"]).ok();
    for command in ["next", "chains", "work"] {
        assert!(out.contains(&format!("\n  {command} ")), "{out}");
    }
    assert!(!out.contains("\n  new "), "{out}");
    assert!(!out.contains("\n  sync "), "{out}");
}

/// The ticket is a record on the branch, read by name: a branch
/// with none says so, and nothing is guessed from a branch name that
/// carries a number.
#[test]
fn a_branch_names_its_ticket_by_record_and_never_by_its_name() {
    let root = on_a_branch("ticket");
    let none = Track::of(&root, None).unwrap();
    assert_eq!(none.ticket, None, "a number in the name is not a ticket");
    let out = xtask(&root, &["track"]).ok();
    assert!(out.contains("ticket: none"), "stated plainly: {out}");

    Repo::at(&root).git(&["config", "branch.t30.ticket", "30"]);
    let recorded = Track::of(&root, None).unwrap();
    assert_eq!(recorded.ticket, Some(Ticket::new(30)));
    let out = xtask(&root, &["track"]).ok();
    assert!(
        out.contains("ticket: #30, what the branch's work is for"),
        "{out}"
    );

    // A record that is not a number is an error, not a branch with none.
    Repo::at(&root).git(&["config", "branch.t30.ticket", "thirty"]);
    let err = Track::of(&root, Some("t30")).unwrap_err();
    assert!(format!("{err:#}").contains("thirty"), "{err:#}");
}

/// With no `--root`, the binary measures the workspace it runs in, the
/// nearest `Cargo.toml` holding `[workspace]` from its working directory
/// up, else from `CARGO_MANIFEST_DIR` as cargo sets it; never the one it
/// was compiled in, which a shared target directory can make another
/// checkout's.
#[test]
fn with_no_root_the_binary_measures_the_workspace_it_runs_in() {
    let root = scratch("runs-in");
    std::fs::write(root.join("Cargo.toml"), "[workspace]\nmembers = []\n").unwrap();
    std::fs::create_dir_all(root.join("xtask/src")).unwrap();
    let elsewhere = Tree::new("track-runs-in-elsewhere").unwrap();
    for (dir, manifest_dir) in [
        (root.join("xtask/src"), None),
        (elsewhere.to_path_buf(), Some(root.join("xtask"))),
    ] {
        let mut command = Command::new(env!("CARGO_BIN_EXE_xtask"));
        command
            .arg("track")
            .current_dir(&dir)
            .env_remove("CARGO_MANIFEST_DIR");
        if let Some(manifest_dir) = manifest_dir {
            command.env("CARGO_MANIFEST_DIR", manifest_dir);
        }
        let out = String::from_utf8(command.output().unwrap().stdout).unwrap();
        let store = format!("store: {}, whose", root.display());
        assert!(out.contains(&store), "from {}: {out}", dir.display());
    }
}

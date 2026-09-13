//! `xtask steer`, the sanctioned writer of the harness's own files
//! (harness-design.md §4.4, §11): a proposal that only adds is applied, one
//! that changes what already binds is refused and writes nothing, and a
//! file that is not the harness's own is refused before anything is read.
//!
//! The judge itself is held by the unit tests beside it; what is held here
//! is what the instrument does to the file on disk, which is the half that
//! matters when the judge is wrong.

use std::fs;

use xtask::reading::Status;
use xtask::scratch::Tree;
use xtask::steer;

/// A scratch workspace carrying one of the harness's own files, a git
/// repository with one commit so that a reading can be stamped.
fn scratch(name: &str, pin: &str) -> Tree {
    let dir = Tree::new(&format!("steer-{name}")).expect("the scratch workspace");
    fs::write(dir.join("oracle.toml"), pin).expect("the pin");
    let who = ["-c", "user.name=x", "-c", "user.email=x@x"];
    for args in [
        &["init", "-q"][..],
        &[&who[..], &["add", "."]].concat(),
        &[&who[..], &["commit", "-q", "-m", "the pin"]].concat(),
    ] {
        let run = std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .expect("git runs");
        assert!(run.status.success(), "git {args:?}");
    }
    dir
}

const PIN: &str =
    "[oracle]\nversion = \"8.5.9\"\n\n[regime.jit-off]\nini = [\"opcache.enable=0\"]\n";

#[test]
fn a_proposal_that_only_adds_is_written() {
    let root = scratch("adds", PIN);
    let proposal = root.join("proposed.toml");
    let added = format!("{PIN}\n[regime.dump]\nini = [\"opcache.opt_debug_level=0x10000\"]\n");
    fs::write(&proposal, &added).expect("the proposal");

    let reading = steer::run(&root, "oracle.toml", &proposal, false).expect("the instrument runs");
    assert_eq!(reading.status, Status::Pass);
    assert_eq!(reading.instrument, "steer");
    assert_eq!(reading.scope, "oracle.toml");
    assert!(
        reading.message.contains("regime.dump"),
        "the reading names what it added: {}",
        reading.message
    );
    assert_eq!(
        fs::read_to_string(root.join("oracle.toml")).expect("the pin"),
        added,
        "the proposal is the file now"
    );
}

#[test]
fn a_dry_run_judges_and_writes_nothing() {
    let root = scratch("dry", PIN);
    let proposal = root.join("proposed.toml");
    fs::write(&proposal, format!("{PIN}\n[regime.dump]\nini = []\n")).expect("the proposal");

    let reading = steer::run(&root, "oracle.toml", &proposal, true).expect("the instrument runs");
    assert_eq!(reading.status, Status::Pass);
    assert_eq!(
        fs::read_to_string(root.join("oracle.toml")).expect("the pin"),
        PIN,
        "a dry run leaves the file as it was"
    );
}

#[test]
fn a_changed_recipe_is_refused_and_the_file_stands() {
    let root = scratch("refused", PIN);
    let proposal = root.join("proposed.toml");
    let moved = PIN.replace("opcache.enable=0", "opcache.enable=1");
    fs::write(&proposal, &moved).expect("the proposal");

    let reading = steer::run(&root, "oracle.toml", &proposal, false).expect("the instrument runs");
    assert_eq!(reading.status, Status::Fail);
    assert!(
        reading.message.contains("regime.jit-off.ini"),
        "the reading names what would have changed: {}",
        reading.message
    );
    assert_eq!(
        fs::read_to_string(root.join("oracle.toml")).expect("the pin"),
        PIN,
        "a refused proposal writes nothing"
    );
}

#[test]
fn a_file_that_is_not_the_harnesss_own_is_refused() {
    let root = scratch("elsewhere", PIN);
    let proposal = root.join("proposed.toml");
    fs::write(&proposal, PIN).expect("the proposal");
    fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("a crate manifest");

    let refused = steer::run(&root, "Cargo.toml", &proposal, false);
    let message = refused
        .expect_err("steer writes the harness's own files alone")
        .to_string();
    assert!(
        message.contains("Cargo.toml") && message.contains("Edit"),
        "the refusal says where the file belongs instead: {message}"
    );
}

#[test]
fn every_steerable_file_is_one_permit_denies() {
    // The list this instrument may write is the scope of `harness.self`,
    // less `.claude/**`, which is no TOML and carries nothing to judge.
    for file in steer::STEERABLE {
        let extension = std::path::Path::new(file).extension();
        assert!(
            extension.is_some_and(|ext| ext.eq_ignore_ascii_case("toml")),
            "{file} is judged as TOML, so it must be TOML"
        );
    }
}

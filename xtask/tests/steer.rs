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
    scratch_at(name, "oracle.toml", pin)
}

/// The same, for a workspace whose steerable file is another of the three.
fn scratch_at(name: &str, file: &str, body: &str) -> Tree {
    let dir = Tree::new(&format!("steer-{name}")).expect("the scratch workspace");
    if let Some(parent) = dir.join(file).parent() {
        fs::create_dir_all(parent).expect("the tracked file's directory");
    }
    fs::write(dir.join(file), body).expect("the tracked file");
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
    // Stamped over what steer left, a snapshot taken after the write, so
    // `permit --after` can tell steer's write from one beside it (#1516,
    // #2163).
    let now = xtask::key::Snapshot::disk(&root).expect("the tree");
    assert_eq!(
        reading.key(),
        now.key(std::path::Path::new("oracle.toml"))
            .expect("the key")
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

/// One of the proposals under `xtask/fixtures/steer/`, beside the
/// `tracked.toml` every one of them is judged against.
fn fixture(name: &str) -> String {
    let path = xtask::package_dir().join("fixtures/steer").join(name);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("the fixture {}: {e}", path.display()))
}

#[test]
fn a_key_added_to_a_rule_row_the_file_carries_is_written_and_named() {
    // The widening the maintainer settled on #387 with its risk named:
    // `exempt` on a `[[rule]]` row is an addition, so it lands, and the
    // reading is where a person reads that it did (ledger 51).
    let tracked = fixture("tracked.toml");
    let root = scratch_at("added-key", "seams.toml", &tracked);
    let proposal = root.join("proposed.toml");
    let added = fixture("added-key.toml");
    fs::write(&proposal, &added).expect("the proposal");

    let reading = steer::run(&root, "seams.toml", &proposal, false).expect("the instrument runs");
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert!(
        reading
            .message
            .contains("rule[witness.exit-authored].exempt"),
        "the reading names the key it added: {}",
        reading.message
    );
    assert_eq!(reading.numbers["added"], 1.into());
    assert_eq!(
        fs::read_to_string(root.join("seams.toml")).expect("the manifest"),
        added,
        "the proposal is the file now"
    );
}

#[test]
fn every_negative_control_is_refused_at_the_path_it_moved_and_writes_nothing() {
    // The four shapes the widening does not admit, one fixture each, each
    // named by the path the refusal prints (xtask/fixtures/steer/README.md).
    let controls = [
        ("changed-value.toml", "crate[ext/standard].budget"),
        ("dropped-key.toml", "crate[ext/standard].budget"),
        ("dropped-row.toml", "rule[lanes.nightly] dropped"),
        ("moved-row.toml", "rule[lanes.nightly] moved"),
    ];
    let tracked = fixture("tracked.toml");
    for (name, path) in controls {
        let root = scratch_at(name.trim_end_matches(".toml"), "seams.toml", &tracked);
        let proposal = root.join("proposed.toml");
        fs::write(&proposal, fixture(name)).expect("the proposal");

        let reading =
            steer::run(&root, "seams.toml", &proposal, false).expect("the instrument runs");
        assert_eq!(reading.status, Status::Fail, "{name}: {}", reading.message);
        assert!(
            reading.message.contains(path),
            "{name} is refused at {path}: {}",
            reading.message
        );
        assert_eq!(
            fs::read_to_string(root.join("seams.toml")).expect("the manifest"),
            tracked,
            "{name}: a refused proposal writes nothing"
        );
    }
}

/// A covering row (harness-design.md §4.3, revision 62) proposed beside the
/// tracked registry: one reason for three tests of its kind.
const COVERING: &str = "\n[[divergence]]\nid = \"phpt:fixture-cause\"\nkind = \"phpt\"\n\
reason = \"One reason (fixture).\"\nstatus = \"accepted\"\nsince = \"2026-09-28\"\n\
covers = [\"phpt:php-src/ext/fixture/tests/a.phpt\", \"phpt:php-src/ext/fixture/tests/b.phpt\", \
\"phpt:php-src/ext/fixture/tests/c.phpt\"]\n";

#[test]
fn a_new_row_with_covers_is_one_addition_to_the_registry() {
    let tracked = fs::read_to_string(
        xtask::package_dir()
            .join("..")
            .join(xtask::divergence::FILE),
    )
    .expect("the tracked registry");
    let root = scratch_at("covers", xtask::divergence::FILE, &tracked);
    let proposal = root.join("proposed.toml");
    fs::write(&proposal, format!("{tracked}{COVERING}")).expect("the proposal");

    let reading =
        steer::run(&root, xtask::divergence::FILE, &proposal, true).expect("the instrument runs");
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert_eq!(reading.numbers["added"], 1.into(), "{}", reading.message);
    assert!(
        reading.message.contains("divergence[phpt:fixture-cause]"),
        "the row is the addition, its covers inside it: {}",
        reading.message
    );
    assert_eq!(
        fs::read_to_string(root.join(xtask::divergence::FILE)).expect("the registry"),
        tracked,
        "a dry run writes nothing"
    );

    // What the registry would refuse at load, steer refuses at the write:
    // a covered id without its kind's prefix or of another kind, and one
    // another row holds.
    let unprefixed = COVERING.replace(
        "\"phpt:php-src/ext/fixture/tests/b.phpt\"",
        "\"php-src/ext/fixture/tests/b.phpt\"",
    );
    let other_kind = COVERING.replace(
        "\"phpt:php-src/ext/fixture/tests/c.phpt\"",
        "\"trace:nested-entry-return\"",
    );
    let doubled = COVERING.replace(
        "\"phpt:php-src/ext/fixture/tests/c.phpt\"",
        "\"phpt:php-src/ext/opcache/tests/gh9164.phpt\"",
    );
    for (row, named) in [
        (unprefixed.as_str(), "whose ids start with \"phpt:\""),
        (other_kind.as_str(), "trace:nested-entry-return"),
        (doubled.as_str(), "two rows"),
    ] {
        fs::write(&proposal, format!("{tracked}{row}")).expect("the proposal");
        let reading = steer::run(&root, xtask::divergence::FILE, &proposal, false)
            .expect("the instrument runs");
        assert_eq!(reading.status, Status::Fail, "{}", reading.message);
        assert!(
            reading.message.contains(named) && reading.message.contains("Nothing was written"),
            "{named}: {}",
            reading.message
        );
        assert_eq!(
            fs::read_to_string(root.join(xtask::divergence::FILE)).expect("the registry"),
            tracked,
            "a refused proposal writes nothing"
        );
    }
}

/// A proposal is held to the reader the harness reads its file with, where
/// the tracked file loads through it: a table the pin's reader does not
/// know is refused and written nowhere, since written it would stop every
/// instrument that reads the pin (#1161's `[sapi]`, which the reader
/// learned in the same change), and a table it knows is an addition.
#[test]
fn a_table_the_pins_reader_does_not_know_is_refused_and_one_it_knows_is_written() {
    let workspace = xtask::package_dir().join("..");
    let whole = fs::read_to_string(workspace.join("oracle.toml")).expect("the workspace's pin");
    // The pin without its own `[sapi]`, so the proposal below adds the table.
    let pin = match whole.find("\n[sapi]\n") {
        Some(at) => format!("{}\n", whole[..at].trim_end()),
        None => whole,
    };
    let root = scratch("reader", &pin);
    let proposal = root.join("proposed.toml");

    fs::write(
        &proposal,
        format!("{pin}\n[hosts_nobody_reads]\ncgi = \"x\"\n"),
    )
    .expect("the proposal");
    let refused = steer::run(&root, "oracle.toml", &proposal, false).expect("the instrument runs");
    assert_eq!(refused.status, Status::Fail, "{}", refused.message);
    assert!(
        refused.message.contains("would not load"),
        "{}",
        refused.message
    );
    assert_eq!(
        fs::read_to_string(root.join("oracle.toml")).expect("the pin"),
        pin
    );

    let sapi = "\n[sapi]\ncgi = { path = \"/bin/php-cgi\", md5 = \"00\" }\n";
    fs::write(&proposal, format!("{pin}{sapi}")).expect("the proposal");
    let written = steer::run(&root, "oracle.toml", &proposal, false).expect("the instrument runs");
    assert_eq!(written.status, Status::Pass, "{}", written.message);
    assert!(
        fs::read_to_string(root.join("oracle.toml"))
            .expect("the pin")
            .ends_with(sapi)
    );
}

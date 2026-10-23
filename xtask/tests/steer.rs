//! `xtask steer`, the sanctioned writer of the harness's own files
//! (harness-design.md §4.4, §11): a proposal that only adds is applied, one
//! that changes what already binds is refused and writes nothing, and a
//! file that is not the harness's own is refused before anything is read.
//!
//! The judge itself is held by the unit tests beside it; what is held here
//! is what the instrument does to the file on disk, which is the half that
//! matters when the judge is wrong.

use std::fs;

use xtask::manifest::Manifest;
use xtask::reading::Status;
use xtask::scratch::Tree;
use xtask::steer;

mod common {
    pub mod repo;
}
use common::repo::Repo;

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
    Repo::init(&dir).commit("the pin");
    dir
}

/// A manifest whose `harness.self` names `files` as the harness's own,
/// with the pin row naming `oracle.toml`: what steer reads its files from.
fn naming(files: &[&str]) -> Manifest {
    let scope = files
        .iter()
        .map(|f| format!("{f:?}"))
        .collect::<Vec<_>>()
        .join(", ");
    Manifest::parse(&format!(
        "[oracle]\npin = \"oracle.toml\"\n[budget]\ntotal = 1\nnew = 1\ndocs = 1\nharness = 1\nlock_packages = 1\nclean_build_release_s = 1\napi_items = 1\nbindings = 1\n[[rule]]\nid = \"harness.self\"\ntool = [\"Edit\", \"Write\"]\nscope = [{scope}]\ndeny = true\ntext = \"harness.self\"\n"
    ))
    .expect("the manifest")
}

/// The manifest as dax-php's `harness.self` names its three TOML files.
fn harness() -> Manifest {
    naming(&["seams.toml", "oracle.toml", xtask::divergence::FILE])
}

const PIN: &str =
    "[oracle]\nversion = \"8.5.9\"\n\n[regime.jit-off]\nini = [\"opcache.enable=0\"]\n";

#[test]
fn a_proposal_that_only_adds_is_written() {
    let root = scratch("adds", PIN);
    let proposal = root.join("proposed.toml");
    let added = format!("{PIN}\n[regime.dump]\nini = [\"opcache.opt_debug_level=0x10000\"]\n");
    fs::write(&proposal, &added).expect("the proposal");

    let reading = steer::run(&root, &harness(), "oracle.toml", &proposal, false)
        .expect("the instrument runs");
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

    let reading =
        steer::run(&root, &harness(), "oracle.toml", &proposal, true).expect("the instrument runs");
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

    let reading = steer::run(&root, &harness(), "oracle.toml", &proposal, false)
        .expect("the instrument runs");
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

    let refused = steer::run(&root, &harness(), "Cargo.toml", &proposal, false);
    let message = refused
        .expect_err("steer writes the harness's own files alone")
        .to_string();
    assert!(
        message.contains("Cargo.toml") && message.contains("Edit"),
        "the refusal says where the file belongs instead: {message}"
    );
}

/// The files steer writes are the ones the manifest's `harness.self`
/// names (#3183): a manifest naming another file has steer write that one
/// and refuse `oracle.toml`, which it no longer names.
#[test]
fn steer_writes_the_files_the_manifests_harness_self_names_and_no_other() {
    let root = scratch_at("named", "harness.toml", "[a]\nb = 1\n");
    let manifest = naming(&["harness.toml", ".claude/**"]);
    let proposal = root.join("proposed.toml");
    fs::write(&proposal, "[a]\nb = 1\nc = 2\n").expect("the proposal");
    let reading = steer::run(&root, &manifest, "harness.toml", &proposal, false)
        .expect("the instrument runs");
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert_eq!(
        fs::read_to_string(root.join("harness.toml")).expect("the file"),
        "[a]\nb = 1\nc = 2\n"
    );
    fs::write(root.join("oracle.toml"), PIN).expect("a pin");
    for file in ["oracle.toml", ".claude/settings.toml"] {
        let refused = steer::run(&root, &manifest, file, &proposal, false)
            .expect_err("a file the scope does not name is refused")
            .to_string();
        assert!(
            refused.contains(file) && refused.contains("harness.self"),
            "{refused}"
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

    let reading =
        steer::run(&root, &harness(), "seams.toml", &proposal, false).expect("the instrument runs");
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

        let reading = steer::run(&root, &harness(), "seams.toml", &proposal, false)
            .expect("the instrument runs");
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

/// The registry the covering row is proposed beside, in place of the
/// tracked one, whose rows a person keeps (#3091): one row, whose id a
/// covering row may not cover again.
const REGISTRY: &str = "# The divergence registry (fixture).\n\n[[divergence]]\n\
id = \"phpt:php-src/ext/opcache/tests/gh9164.phpt\"\nkind = \"phpt\"\n\
reason = \"The fixture's one reason (fixture).\"\nstatus = \"accepted\"\nsince = \"2026-09-07\"\n";

/// A covering row (harness-design.md §4.3, revision 62) proposed beside
/// [`REGISTRY`]: one reason for three tests of its kind.
const COVERING: &str = "\n[[divergence]]\nid = \"phpt:fixture-cause\"\nkind = \"phpt\"\n\
reason = \"One reason (fixture).\"\nstatus = \"accepted\"\nsince = \"2026-09-28\"\n\
covers = [\"phpt:php-src/ext/fixture/tests/a.phpt\", \"phpt:php-src/ext/fixture/tests/b.phpt\", \
\"phpt:php-src/ext/fixture/tests/c.phpt\"]\n";

#[test]
fn a_new_row_with_covers_is_one_addition_to_the_registry() {
    let tracked = REGISTRY;
    let root = scratch_at("covers", xtask::divergence::FILE, tracked);
    let proposal = root.join("proposed.toml");
    fs::write(&proposal, format!("{tracked}{COVERING}")).expect("the proposal");

    let reading = steer::run(&root, &harness(), xtask::divergence::FILE, &proposal, true)
        .expect("the instrument runs");
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
        let reading = steer::run(&root, &harness(), xtask::divergence::FILE, &proposal, false)
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

/// A trace-dump row's digests are part of the row a proposal adds
/// (#3291): steer writes them as the proposal spells them, and refuses a
/// row naming one side's alone, as the registry would at load.
#[test]
fn a_dump_rows_digests_are_written_with_the_row_and_one_alone_is_refused() {
    use xtask::divergence::{Digests, FILE, Registry};
    let root = scratch_at("digests", FILE, REGISTRY);
    let proposal = root.join("proposed.toml");
    let fixtures = xtask::package_dir().join("fixtures/trace-dump");
    let read = |name: &str| fs::read_to_string(fixtures.join(name)).expect("the fixture");
    let row = read("fib-moved.toml");
    let half: String = row
        .lines()
        .filter(|l| !l.starts_with("oracle = "))
        .flat_map(|l| [l, "\n"])
        .collect();
    for (rows, status, named) in [
        (
            &half,
            Status::Fail,
            "names one side's digest and not the other's",
        ),
        (&row, Status::Pass, "divergence[trace:dump/fib]"),
    ] {
        fs::write(&proposal, format!("{REGISTRY}\n{rows}")).expect("the proposal");
        let reading =
            steer::run(&root, &harness(), FILE, &proposal, false).expect("the instrument runs");
        assert_eq!(reading.status, status, "{}", reading.message);
        assert!(
            reading.message.contains(named),
            "{named}: {}",
            reading.message
        );
    }
    let written = Registry::load(&root).expect("the registry steer wrote");
    let pinned = Digests::of(
        read("oracle.lines").as_bytes(),
        read("engine-pinned.lines").as_bytes(),
    );
    assert_eq!(
        written.row("trace:dump/fib").and_then(|r| r.digests),
        Some(pinned)
    );
}

/// A proposal is held to the reader the harness reads its file with, where
/// the tracked file loads through it: a table the pin's reader does not
/// know is refused and written nowhere, since written it would stop every
/// instrument that reads the pin (#1161's `[sapi]`, which the reader
/// learned in the same change), and a table it knows is an addition.
#[cfg(tracked)]
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
    let refused = steer::run(&root, &harness(), "oracle.toml", &proposal, false)
        .expect("the instrument runs");
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
    let written = steer::run(&root, &harness(), "oracle.toml", &proposal, false)
        .expect("the instrument runs");
    assert_eq!(written.status, Status::Pass, "{}", written.message);
    assert!(
        fs::read_to_string(root.join("oracle.toml"))
            .expect("the pin")
            .ends_with(sapi)
    );
}

/// A scratch workspace standing in a conflicted merge of the registry:
/// `merge/base.toml` committed, `merged` (a fixture under `merge/`) on a
/// branch beside it, `merge/head.toml` on `HEAD`, and `git merge` of the
/// branch stopped on the conflict both appends make, markers in the
/// working file.
fn merging(name: &str, merged: &str) -> Tree {
    let sides = [fixture("merge/head.toml"), fixture(merged)];
    merging_over(name, &fixture("merge/base.toml"), sides)
}

/// The same over the registry `base`, `head` on `HEAD` and `merged` on
/// the branch.
fn merging_over(name: &str, base: &str, [head, merged]: [String; 2]) -> Tree {
    let file = xtask::divergence::FILE;
    let root = scratch_at(name, file, base);
    let repo = Repo::at(&root);
    let side = |branch: &str, body: &str| {
        fs::write(root.join(file), body).expect("the registry");
        repo.git(&["commit", "-q", "-am", branch]);
    };
    repo.git(&["checkout", "-q", "-b", "merged"]);
    side("merged", &merged);
    repo.git(&["checkout", "-q", "-"]);
    side("head", &head);
    // The merge stops on the conflict.
    repo.refused(&["merge", "-q", "merged"]);
    assert!(
        fs::read_to_string(root.join(file))
            .expect("the registry")
            .contains("<<<<<<<"),
        "the working file holds the conflict's markers"
    );
    root
}

/// The union steered and written, the reading naming each side's row.
fn steered_as_the_union(root: &Tree, union: &str) {
    let file = xtask::divergence::FILE;
    let proposal = root.join("proposed.toml");
    fs::write(&proposal, union).expect("the proposal");
    let reading =
        steer::run(root, &harness(), file, &proposal, false).expect("the instrument runs");
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert_eq!(reading.numbers["added"], 1.into(), "{}", reading.message);
    for named in [
        "judged against HEAD's file since a merge is in progress",
        "divergence[trace:fixture-head]",
        "divergence[trace:fixture-merged]",
    ] {
        assert!(
            reading.message.contains(named),
            "the reading names {named}: {}",
            reading.message
        );
    }
    let written = fs::read_to_string(root.join(file)).expect("the registry");
    assert_eq!(written, union, "the union is the file now");
    assert!(!written.contains("<<<<<<<"), "no conflict marker stands");
}

#[test]
fn a_conflicted_merge_of_two_additions_is_resolved_by_their_union() {
    let root = merging("union", "merge/merged.toml");
    steered_as_the_union(&root, &fixture("merge/union.toml"));
}

/// A merge base whose registry holds no row, as a scratch's empty one
/// does, each side appending one (#3141): the whole array is new on
/// both sides, and the union is still each side's row by its id, not a
/// row `MERGE_HEAD` carries named as carried by neither.
#[test]
fn a_conflicted_merge_over_a_base_of_no_row_is_resolved_by_their_union() {
    let empty = xtask::package_dir().join("fixtures/registry/empty.toml");
    let empty = fs::read_to_string(empty).expect("the empty registry");
    let base = fixture("merge/base.toml");
    let row = |side: &str| {
        let whole = fixture(side);
        whole
            .strip_prefix(base.as_str())
            .expect("a side appends to the base")
            .to_owned()
    };
    let (head, merged) = (row("merge/head.toml"), row("merge/merged.toml"));
    let sides = [format!("{empty}{head}"), format!("{empty}{merged}")];
    let root = merging_over("union-over-no-row", &empty, sides);
    steered_as_the_union(&root, &format!("{empty}{head}{merged}"));
}

#[test]
fn a_conflicted_merge_refuses_a_reworded_reason_and_a_row_neither_side_carries() {
    // The negative controls of the union (xtask/fixtures/steer/README.md):
    // a side that rewords a reason the base carries, a proposal that does,
    // and a row neither side carries; each is refused at the path it
    // names, and the conflicted file stands.
    let file = xtask::divergence::FILE;
    for (merged, proposal, path) in [
        (
            "merge/merged-reworded.toml",
            "merge/union.toml",
            "divergence[trace:fixture-base].reason on MERGE_HEAD",
        ),
        (
            "merge/merged.toml",
            "merge/changed-reason.toml",
            "divergence[trace:fixture-base].reason",
        ),
        (
            "merge/merged.toml",
            "merge/neither.toml",
            "divergence[trace:fixture-neither], which neither side carries",
        ),
    ] {
        let name = format!("{merged}-{proposal}").replace(['/', '.'], "-");
        let root = merging(&name, merged);
        let conflicted = fs::read_to_string(root.join(file)).expect("the registry");
        let at = root.join("proposed.toml");
        fs::write(&at, fixture(proposal)).expect("the proposal");

        let reading = steer::run(&root, &harness(), file, &at, false).expect("the instrument runs");
        assert_eq!(reading.status, Status::Fail, "{name}: {}", reading.message);
        assert!(
            reading.message.contains(path) && reading.message.contains("Nothing was written"),
            "{name} is refused at {path}: {}",
            reading.message
        );
        assert_eq!(
            fs::read_to_string(root.join(file)).expect("the registry"),
            conflicted,
            "{name}: a refused proposal writes nothing"
        );
    }
}

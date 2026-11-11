//! `xtask steer`, the sanctioned writer of the harness's own files
//! (harness-design.md §4.4, §11): a proposal that only adds is applied, one
//! that changes what already binds is refused and writes nothing, and a
//! file that is not the harness's own is refused before anything is read.
//!
//! The judge itself is held by the unit tests beside it; what is held here
//! is what the instrument does to the file on disk, which is the half that
//! matters when the judge is wrong.

use std::fs;
use std::path::Path;

use xtask::manifest::Manifest;
use xtask::reading::Status;
use xtask::scratch::Tree;
use xtask::steer;

mod common {
    pub mod repo;
    pub mod run;
}
use common::repo::{Repo, scratch_workspace};
use common::run::{Run, xtask};

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
        "[oracle]\npin = \"oracle.toml\"\n[budget]\ntotal = 1\nnew = 1\ndocs = 1\nharness = 1\nlock_packages = 1\nclean_build_release_s = 1\napi_items = 1\nbindings = 1\n[[rule]]\nid = \"harness.self\"\ntool = [\"Edit\", \"Write\"]\nscope = [{scope}]\ndeny = true\ntext = \"harness.self\"\nremedy = \"steer\"\n"
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

/// A row's `proven_at`, the regime its cause is proven at (#3369), is
/// part of the row a proposal adds: steer writes it, and the registry
/// reads it back; on a row holding an id that is no fence, which no lane
/// runs again under a regime, it is refused as the registry would at load.
#[test]
fn a_proven_rows_regime_is_written_with_the_row_and_one_on_no_fence_is_refused() {
    use xtask::divergence::{FILE, Registry};
    let root = scratch_at("proven-at", FILE, REGISTRY);
    let proposal = root.join("proposed.toml");
    let proven = "\n[[divergence]]\nid = \"trace:fixture-proven\"\nkind = \"trace\"\n\
                  cause = \"by-design\"\nreason = \"r (fixture).\"\nstatus = \"accepted\"\n\
                  since = \"2026-10-26\"\ncovers = [\"witness:abc\"]\nproven_at = \"optimizer-off\"\n";
    let on_a_test = format!("{COVERING}proven_at = \"optimizer-off\"\n");
    for (row, status, named) in [
        (
            on_a_test.as_str(),
            Status::Fail,
            "names `proven_at`, and an id it holds is no fence",
        ),
        (proven, Status::Pass, "divergence[trace:fixture-proven]"),
    ] {
        fs::write(&proposal, format!("{REGISTRY}{row}")).expect("the proposal");
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
    let row = written
        .row("trace:fixture-proven")
        .and_then(|r| r.proven_at.clone());
    assert_eq!(row.as_deref(), Some("optimizer-off"));
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
    steered_bringing(root, union, &[]);
}

/// The same, the write bringing `taken` from `MERGE_HEAD` beside its row.
fn steered_bringing(root: &Tree, union: &str, taken: &[&str]) {
    let file = xtask::divergence::FILE;
    let proposal = root.join("proposed.toml");
    fs::write(&proposal, union).expect("the proposal");
    let reading =
        steer::run(root, &harness(), file, &proposal, false).expect("the instrument runs");
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    let brought = xtask::reading::Number::count(1 + taken.len());
    assert_eq!(reading.numbers["added"], brought, "{}", reading.message);
    for named in [
        "judged against HEAD's file since a merge is in progress",
        "divergence[trace:fixture-head]",
        "divergence[trace:fixture-merged]",
    ]
    .iter()
    .chain(taken)
    {
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

/// A side that rewords a reason the base carries, while each side appends
/// a row: that side's reason is part of the union and written (#3633).
#[test]
fn a_conflicted_merge_takes_a_reason_one_side_reworded_over_the_base() {
    let root = merging("union-reworded", "merge/merged-reworded.toml");
    let union = fixture("merge/union-reworded.toml");
    steered_bringing(&root, &union, &["divergence[trace:fixture-base].reason"]);
}

#[test]
fn a_conflicted_merge_refuses_a_reworded_reason_and_a_row_neither_side_carries() {
    // The negative controls of the union (xtask/fixtures/steer/README.md):
    // a proposal that puts back a reason one side reworded, one that
    // rewords it again, one that rewords a reason neither side touched,
    // and a row neither side carries; each is refused at the path it
    // names, and the conflicted file stands.
    let file = xtask::divergence::FILE;
    for (merged, proposal, path) in [
        (
            "merge/merged-reworded.toml",
            "merge/union.toml",
            "divergence[trace:fixture-base].reason",
        ),
        (
            "merge/merged-reworded.toml",
            "merge/changed-reason.toml",
            "divergence[trace:fixture-base].reason",
        ),
        (
            "merge/merged.toml",
            "merge/changed-reason.toml",
            "divergence[trace:fixture-base].reason",
        ),
        (
            "merge/merged.toml",
            "merge/neither.toml",
            "divergence[trace:fixture-neither], which neither side adds over the base",
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

/// A scratch workspace standing in a merge of the registry that git took
/// cleanly and did not commit, as pre-merge-commit leaves one it refused
/// (#4441): `base` committed at `root`, `merged` on a branch beside it,
/// `head` on `HEAD`, and `git merge --no-commit` of the branch with `how`,
/// the strategy options it was given. The file is staged, unmerged nowhere.
fn cleanly(root: &Path, base: &str, [head, merged]: [&str; 2], how: &[&str]) -> Repo {
    let file = xtask::divergence::FILE;
    let repo = Repo::at(root);
    let side = |branch: &str, body: &str| {
        fs::write(root.join(file), body).expect("the registry");
        repo.git(&["commit", "-q", "-am", branch]);
    };
    side("base", base);
    repo.git(&["checkout", "-q", "-b", "merged"]);
    side("merged", merged);
    repo.git(&["checkout", "-q", "-"]);
    side("head", head);
    let merge = [
        &["merge", "-q", "--no-ff", "--no-commit"][..],
        how,
        &["merged"],
    ]
    .concat();
    repo.git(&merge);
    assert!(
        repo.git(&["ls-files", "--unmerged"]).is_empty(),
        "git merged the registry cleanly"
    );
    repo
}

/// `permit --diff`, pre-commit's judge of what is staged, as an agent's
/// commit runs it (`CLAUDE_CODE_SESSION_ID` set): its exit and what it
/// printed.
fn agents_diff(root: &Path) -> (i32, String) {
    let ran = Run::at(root)
        .args(["permit", "--diff"])
        .env(xtask::owner::SESSION_VARIABLE, "an-agent")
        .go();
    (ran.code(), format!("{ran}"))
}

/// The clean merge #4441 met, end to end through the binaries the hooks
/// run: a person's commit rewords a reason on `HEAD` and the branch adds a
/// steered row, which git merges with no conflict. Pre-commit refuses the
/// agent's merge (#3644), the one step merging.md gives, steer over a copy
/// of the merged file, writes a reading that names what each side brings
/// and is stamped over the staged blob, and pre-commit then admits it.
#[test]
fn a_cleanly_merged_harness_file_is_covered_by_steering_its_merged_copy() {
    let file = xtask::divergence::FILE;
    let root = Tree::new("steer-clean-commit").expect("the scratch workspace");
    scratch_workspace(xtask::package_dir().parent().expect("the workspace"), &root);
    let reworded = fixture("merge/head-reworded.toml");
    let merged = fixture("merge/merged.toml");
    let repo = cleanly(
        &root,
        &fixture("merge/base.toml"),
        [&reworded, &merged],
        &[],
    );
    let clean = fixture("merge/clean.toml");
    assert_eq!(
        fs::read_to_string(root.join(file)).expect("the registry"),
        clean,
        "git's merge is HEAD's reason and the branch's row"
    );
    let (code, said) = agents_diff(&root);
    assert_eq!(code, 1, "the merge is refused before steer: {said}");
    assert!(said.contains("#3644"), "{said}");

    let copy = Tree::new("steer-clean-copy").expect("the scratchpad");
    let at = copy.join("divergence.toml");
    fs::copy(root.join(file), &at).expect("the merged file's copy");
    xtask(
        &root,
        &["steer", file, "--from", at.to_str().expect("UTF-8")],
    )
    .ok();
    let reading = xtask::reading::Store::of_workspace(&root)
        .read("steer", file)
        .expect("the store")
        .expect("steer's reading");
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert_eq!(reading.numbers["added"], 1.into(), "{}", reading.message);
    for named in [
        "HEAD (divergence[trace:fixture-base].reason)",
        "as git's three-way merge of the three files gives it cleanly",
        "divergence[trace:fixture-merged]",
    ] {
        assert!(
            reading.message.contains(named),
            "the reading names {named}: {}",
            reading.message
        );
    }
    assert_eq!(
        fs::read_to_string(root.join(file)).expect("the registry"),
        clean
    );
    repo.git(&["add", "--", file]);
    let (code, said) = agents_diff(&root);
    assert_eq!(code, 0, "steer's reading covers what is staged: {said}");
    repo.git(&["commit", "-q", "-m", "Merge merged"]);
    repo.git(&["rev-parse", "--verify", "HEAD^2"]);
}

/// The negative controls of a clean merge (xtask/fixtures/steer/README.md),
/// each refused at what it names with nothing written: a proposal that
/// puts back the reason HEAD's person reworded, one that drops the
/// branch's row, one that changes a value neither side changed, one that
/// is the union in another text than git merged,
/// and the working copy of a merge `-X theirs` took, which dropped HEAD's
/// row where git's three-way merge of the three files conflicts.
#[test]
fn a_clean_merge_refuses_a_side_undone_and_a_text_git_did_not_merge() {
    let file = xtask::divergence::FILE;
    let base = fixture("merge/base.toml");
    let reworded = fixture("merge/head-reworded.toml");
    let merged = fixture("merge/merged.toml");
    let clean = fixture("merge/clean.toml");
    let head_row = fixture("merge/head.toml")
        .strip_prefix(base.as_str())
        .expect("HEAD appends to the base")
        .to_owned();
    let both = format!("{reworded}{head_row}");
    // One control: the merge of `sides` with `how`, steered with
    // `proposal`, or the staged file where it is `None`.
    let refused =
        |name: &str, sides: [&str; 2], how: &[&str], proposal: Option<&str>, named: &[&str]| {
            let root = scratch_at(&format!("clean-{name}"), file, "# no row yet\n");
            cleanly(&root, &base, sides, how);
            let staged = fs::read_to_string(root.join(file)).expect("the registry");
            let at = root.join("proposed.toml");
            fs::write(&at, proposal.unwrap_or(&staged)).expect("the proposal");

            let reading =
                steer::run(&root, &harness(), file, &at, false).expect("the instrument runs");
            assert_eq!(reading.status, Status::Fail, "{name}: {}", reading.message);
            for path in named {
                assert!(
                    reading.message.contains(path)
                        && reading.message.contains("Nothing was written"),
                    "{name} is refused at {path}: {}",
                    reading.message
                );
            }
            assert_eq!(
                fs::read_to_string(root.join(file)).expect("the registry"),
                staged,
                "{name}: a refused proposal writes nothing"
            );
        };
    let sides = [reworded.as_str(), merged.as_str()];
    refused(
        "undone",
        sides,
        &[],
        Some(&merged),
        &["divergence[trace:fixture-base].reason"],
    );
    refused(
        "dropped",
        sides,
        &[],
        Some(&reworded),
        &["divergence[trace:fixture-merged] dropped"],
    );
    // A value neither side carries, the branch's row's date moved in the
    // merged file before it was steered.
    let neither = clean.replace("since = \"2026-10-18\"", "since = \"2026-11-11\"");
    refused(
        "neither",
        sides,
        &[],
        Some(&neither),
        &[
            "divergence[trace:fixture-merged].since",
            "is not the text git's three-way merge",
        ],
    );
    let retyped = format!("{clean}# retyped\n");
    refused(
        "retyped",
        sides,
        &[],
        Some(&retyped),
        &["is not the text git's three-way merge"],
    );
    refused(
        "theirs",
        [&both, &merged],
        &["-X", "theirs"],
        None,
        &[
            "divergence[trace:fixture-head] dropped",
            "a strategy option took one side's hunk",
        ],
    );
}

/// A clean merge whose `MERGE_HEAD` changed the file in a comment alone,
/// as main merged into a branch that steered a row brings a person's
/// comment: no value is brought to HEAD's file, and the text git merged,
/// neither side's commit, is covered by a passing reading of no addition
/// that names what HEAD carries.
#[test]
fn a_clean_merge_whose_merged_side_changed_a_comment_alone_is_covered() {
    let file = xtask::divergence::FILE;
    let base = fixture("merge/base.toml");
    let commented = base.replacen("# A slice", "# A reworded slice", 1);
    let root = scratch_at("clean-comment", file, "# no row yet\n");
    cleanly(
        &root,
        &base,
        [&fixture("merge/merged.toml"), &commented],
        &[],
    );
    let staged = fs::read_to_string(root.join(file)).expect("the registry");
    let at = root.join("proposed.toml");
    fs::write(&at, &staged).expect("the proposal");

    let reading = steer::run(&root, &harness(), file, &at, false).expect("the instrument runs");
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert_eq!(reading.numbers["added"], 0.into(), "{}", reading.message);
    for named in [
        "HEAD (divergence[trace:fixture-merged])",
        "MERGE_HEAD bringing no value to HEAD's file",
    ] {
        assert!(
            reading.message.contains(named),
            "{named}: {}",
            reading.message
        );
    }
    assert_eq!(
        fs::read_to_string(root.join(file)).expect("the registry"),
        staged
    );
}

/// A criss-cross merge, whose `HEAD` and `MERGE_HEAD` have two merge
/// bases, merged cleanly into a text neither side carries: git merged it
/// over a base no commit carries, so steer refuses it, the merged copy
/// and a row added to it alike, and writes nothing.
#[test]
fn a_criss_cross_merge_of_a_file_both_sides_changed_is_refused() {
    let file = xtask::divergence::FILE;
    let root = scratch_at("clean-crossed", file, &fixture("merge/base.toml"));
    let repo = Repo::at(&root);
    let commit = |name: &str, body: &str| {
        fs::write(root.join(name), body).expect("the file");
        repo.git(&["add", "--", name]);
        repo.git(&["commit", "-q", "-m", name]);
    };
    // Two bases: each side's first commit, merged into the other.
    repo.git(&["checkout", "-q", "-b", "merged"]);
    commit("merged.txt", "the branch's first\n");
    repo.git(&["checkout", "-q", "-"]);
    commit("head.txt", "HEAD's first\n");
    repo.git(&["tag", "head-first"]);
    repo.git(&["merge", "-q", "--no-edit", "merged"]);
    repo.git(&["checkout", "-q", "merged"]);
    repo.git(&["merge", "-q", "--no-edit", "head-first"]);
    commit(file, &fixture("merge/merged.toml"));
    repo.git(&["checkout", "-q", "-"]);
    commit(file, &fixture("merge/head-reworded.toml"));
    repo.git(&["merge", "-q", "--no-ff", "--no-commit", "merged"]);
    let bases = repo.git(&["merge-base", "--all", "HEAD", "MERGE_HEAD"]);
    assert_eq!(bases.lines().count(), 2, "a criss-cross: {bases}");
    let staged = fs::read_to_string(root.join(file)).expect("the registry");
    assert_eq!(staged, fixture("merge/clean.toml"), "git merged it cleanly");

    let row = "\n[[divergence]]\nid = \"trace:fixture-crossed\"\nkind = \"trace\"\n\
               cause = \"php-ub\"\nreason = \"r (fixture).\"\nstatus = \"accepted\"\n\
               since = \"2026-11-11\"\n";
    for proposal in [staged.clone(), format!("{staged}{row}")] {
        let at = root.join("proposed.toml");
        fs::write(&at, &proposal).expect("the proposal");
        let reading = steer::run(&root, &harness(), file, &at, false).expect("the instrument runs");
        assert_eq!(reading.status, Status::Fail, "{}", reading.message);
        assert!(
            reading.message.contains("several merge bases"),
            "{}",
            reading.message
        );
        assert_eq!(
            fs::read_to_string(root.join(file)).expect("the registry"),
            staged
        );
    }
}

/// An `[[owed]]` row named by its instrument (#3636): `held_at` added to
/// two rows is written, and the negative controls beside it, a changed
/// `when` and a second row of one instrument, are refused with nothing
/// written (xtask/fixtures/steer/README.md).
#[test]
fn a_key_added_to_an_owed_row_is_written_and_a_change_or_a_second_row_is_refused() {
    let tracked = fixture("owed/tracked.toml");
    let steered = |name: &str| {
        let root = scratch_at(&format!("owed-{name}"), "seams.toml", &tracked);
        let proposal = root.join("proposed.toml");
        fs::write(&proposal, fixture(&format!("owed/{name}.toml"))).expect("the proposal");
        let reading = steer::run(&root, &harness(), "seams.toml", &proposal, false)
            .expect("the instrument runs");
        let now = fs::read_to_string(root.join("seams.toml")).expect("the manifest");
        (reading, now)
    };
    let (reading, now) = steered("held-at");
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert!(
        reading
            .message
            .ends_with("owed[phpt].held_at, owed[crosscheck].held_at"),
        "{}",
        reading.message
    );
    assert_eq!(
        now,
        fixture("owed/held-at.toml"),
        "the proposal is the file now"
    );
    for (name, path) in [
        ("changed-when", "owed[crosscheck].when"),
        ("second-row", "owed[phpt] twice"),
    ] {
        let (reading, now) = steered(name);
        assert_eq!(reading.status, Status::Fail, "{name}: {}", reading.message);
        assert!(
            reading.message.contains(path),
            "{name} is refused at {path}: {}",
            reading.message
        );
        assert_eq!(now, tracked, "{name}: a refused proposal writes nothing");
    }
}

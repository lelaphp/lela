//! The negative controls of `budget` and `unsafe-audit` (harness-design.md
//! §4.1, §12 item 4): one fixture per rule these two instruments evaluate,
//! each a workspace under `xtask/fixtures/` that must fail, run through the
//! instrument's library entry. The rules are the manifest's whose
//! predicate one of the two reads: `allow` (unsafe-audit) and `generated`
//! (budget); the ceilings, which are `[[crate]]` rows and not rules, have
//! their fixtures under `xtask/fixtures/budget/`. Rules other instruments
//! evaluate have their fixtures beside those instruments. The licence
//! header's rule is `budget`'s by its `check`, `licence-header`: its own
//! fixture is under `rules/`, and one under `budget/licence-*` for each
//! other thing its sensor holds.
#![cfg(tracked)]

use std::path::{Path, PathBuf};

use xtask::manifest::Manifest;
use xtask::reading::{Reading, Status};
use xtask::{budget, unsafe_audit};

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

fn fixture(kind: &str, name: &str) -> PathBuf {
    let dir = xtask::package_dir().join("fixtures").join(kind).join(name);
    assert!(dir.is_dir(), "no fixture at {}", dir.display());
    dir
}

fn failures(readings: &[Reading]) -> Vec<&Reading> {
    readings
        .iter()
        .filter(|r| r.status == Status::Fail)
        .collect()
}

/// An instrument's library entry.
type Instrument = fn(&Path, &Manifest) -> anyhow::Result<Vec<Reading>>;

/// The predicates of the rules `budget` and `unsafe-audit` evaluate, and
/// which instrument evaluates each.
const EVALUATED: [(&str, Instrument); 2] =
    [("allow", unsafe_audit::run), ("generated", budget::run)];

#[test]
fn a_crate_one_line_over_its_ceiling_fails_with_the_ceiling_and_the_design_text() {
    let root = fixture("budget", "ceiling");
    let manifest = Manifest::load(&root).unwrap();
    let readings = budget::run(&root, &manifest).unwrap();
    let heap = readings.iter().find(|r| r.scope == "php_heap").unwrap();
    assert_eq!(heap.status, Status::Fail, "{heap:#?}");
    assert_eq!(heap.numbers["lines"], 3.into());
    assert_eq!(heap.numbers["ceiling"], 2.into());
    assert!(
        heap.message.contains("3 lines over the ceiling of 2"),
        "{}",
        heap.message
    );
    assert!(heap.message.contains("design §4.1"), "{}", heap.message);
    assert!(
        heap.message.contains("size is a gate"),
        "the principle prints with the failure: {}",
        heap.message
    );
}

/// `scripts/**` against `[budget] scripts`: the hook's three lines
/// are over a ceiling of two, the message says what makes the count up
/// file by file, and the generator the row names is not in it, so its six
/// lines would not have rescued or sunk the reading either way.
#[test]
fn scripts_over_their_ceiling_fail_naming_each_counted_file_and_the_generator_left_out() {
    let root = fixture("budget", "scripts");
    let manifest = Manifest::load(&root).unwrap();
    let readings = budget::run(&root, &manifest).unwrap();
    let scripts = readings.iter().find(|r| r.scope == "scripts").unwrap();
    assert_eq!(scripts.status, Status::Fail, "{scripts:#?}");
    assert_eq!(scripts.numbers["lines"], 3.into());
    assert_eq!(scripts.numbers["files"], 1.into());
    assert_eq!(scripts.numbers["ceiling"], 2.into());
    assert!(
        scripts
            .message
            .contains("3 lines over the ceiling of 2, made up of scripts/hooks/pre-commit 3"),
        "{}",
        scripts.message
    );
    assert!(
        scripts
            .message
            .contains("generators not counted: scripts/generate.sh"),
        "{}",
        scripts.message
    );
    assert!(
        scripts.message.contains("design §4.1"),
        "{}",
        scripts.message
    );
    assert!(!scripts.key().is_empty(), "stamped over scripts/");
}

/// `docs/perf/workloads/**` against `[budget] workloads`: the
/// program's three lines are over a row of two, the workspace reading
/// fails naming the directory, and `docs` no longer counts them.
#[test]
fn workloads_over_their_row_fail_the_workspace_and_leave_docs() {
    let root = fixture("budget", "workloads");
    let manifest = Manifest::load(&root).unwrap();
    assert_eq!(manifest.budget.workloads, Some(2));
    let readings = budget::run(&root, &manifest).unwrap();
    let workspace = readings.iter().find(|r| r.scope == "workspace").unwrap();
    assert_eq!(workspace.status, Status::Fail, "{workspace:#?}");
    assert_eq!(workspace.numbers["workloads"], 3.into());
    assert_eq!(workspace.numbers["workloads_ceiling"], 2.into());
    assert_eq!(workspace.numbers["docs"], 0.into());
    assert!(
        workspace
            .message
            .contains("docs/perf/workloads/** lines: 3 over the ceiling of 2"),
        "{}",
        workspace.message
    );
}

#[test]
fn committed_generated_code_outside_the_declared_exception_fails_the_workspace() {
    let root = fixture("budget", "generated-code");
    let manifest = Manifest::load(&root).unwrap();
    let readings = budget::run(&root, &manifest).unwrap();
    let workspace = readings.iter().find(|r| r.scope == "workspace").unwrap();
    assert_eq!(workspace.status, Status::Fail, "{workspace:#?}");
    assert!(
        workspace
            .message
            .contains("crates/php_unit/src/generated/ops.rs is committed generated code"),
        "{}",
        workspace.message
    );
}

/// A `build.rs` that bakes its crate's directory in fails the workspace;
/// a module that happens to be named `build.rs`, with no
/// `Cargo.toml` beside it, is not one cargo runs and is not named.
#[test]
fn a_build_rs_baking_its_crate_dir_in_fails_the_workspace() {
    let root = fixture("budget", "build-rs-dir");
    let manifest = Manifest::load(&root).unwrap();
    let readings = budget::run(&root, &manifest).unwrap();
    let workspace = readings.iter().find(|r| r.scope == "workspace").unwrap();
    assert_eq!(workspace.status, Status::Fail, "{workspace:#?}");
    let message = &workspace.message;
    assert!(
        message.contains(
            "crates/php_unit/build.rs reads env!(\"CARGO_MANIFEST_DIR\") when it compiles"
        ),
        "{message}"
    );
    assert!(!message.contains("src/build.rs"), "{message}");
}

#[test]
fn an_unsafe_block_outside_the_homes_fails_naming_the_file_and_the_rule() {
    let root = fixture("rules", "unsafe.homes");
    let manifest = Manifest::load(&root).unwrap();
    let readings = unsafe_audit::run(&root, &manifest).unwrap();
    let heap = readings.iter().find(|r| r.scope == "php_heap").unwrap();
    assert_eq!(heap.status, Status::Pass, "a home passes: {heap:#?}");
    // The block, the block whose brace stands on the next line, which the
    // audit sees only while it reads the file whole, and the unsafe
    // attribute: a site as a block is, in the fixture itself now
    // that the fixture's own manifest judges an edit to it.
    assert_eq!(heap.numbers["sites"], 3.into());
    let pcre = readings.iter().find(|r| r.scope == "ext/pcre").unwrap();
    assert_eq!(
        pcre.status,
        Status::Pass,
        "an extension with bindings is deny with the allowance on ffi: {pcre:#?}"
    );
    assert_eq!(
        pcre.numbers["sites"],
        2.into(),
        "the block and the attribute are two sites"
    );
    let unit = readings.iter().find(|r| r.scope == "php_unit").unwrap();
    assert_eq!(unit.status, Status::Fail, "{unit:#?}");
    assert_eq!(unit.rule, "unsafe.homes");
    assert_eq!(unit.adr, "0006");
    assert!(
        unit.message.contains("crates/php_unit/src/lib.rs:4"),
        "the file and line: {}",
        unit.message
    );
    assert!(
        unit.message.contains("(fixture)") && unit.message.contains("ADR-0006"),
        "the rule text and its ADR: {}",
        unit.message
    );
    // The rule's `group` puts php_heap in the engine's homes and the
    // bindings in none, so the shares are not counted and the run fails.
    let shares = readings.iter().find(|r| r.scope == "workspace").unwrap();
    assert_eq!(shares.status, Status::Fail, "{shares:#?}");
    assert!(
        shares
            .message
            .contains("the home ext/*/src/ffi/ is in no group"),
        "{}",
        shares.message
    );
}

#[test]
fn a_hand_edited_generated_file_fails_under_generated_committed() {
    let root = fixture("rules", "generated.committed");
    let manifest = Manifest::load(&root).unwrap();
    let readings = budget::run(&root, &manifest).unwrap();
    let clippy = readings
        .iter()
        .find(|r| r.scope == "ext/clippy.toml")
        .unwrap();
    assert_eq!(clippy.status, Status::Fail, "{clippy:#?}");
    assert_eq!(clippy.rule, "generated.committed");
    assert!(
        clippy.message.contains("differs from its rendering")
            && clippy.message.contains("(fixture)"),
        "{}",
        clippy.message
    );
}

/// What a line counter reads as code is what the `rust` module blanks
/// around: a character literal holding a quote and a raw string
/// holding a comment marker. The counter `budget` read through before took
/// the quote in `'"'` for a string's start and the `/*` inside a raw
/// string for a comment, and counted four of the fixture's six lines.
#[test]
fn a_quote_in_a_character_literal_and_a_comment_marker_in_a_raw_string_are_counted_as_code() {
    let root = fixture("budget", "quoted");
    let manifest = Manifest::load(&root).unwrap();
    let readings = budget::run(&root, &manifest).unwrap();
    let heap = readings.iter().find(|r| r.scope == "php_heap").unwrap();
    assert_eq!(heap.numbers["lines"], 6.into(), "{heap:#?}");
    assert_eq!(heap.status, Status::Fail, "{heap:#?}");
}

/// The generated file's reading in a fixture whose copy is current, so
/// what fails it is what stands between the copy and the crates below it.
fn current_generated_reading(name: &str) -> Reading {
    let root = fixture("budget", name);
    let manifest = Manifest::load(&root).unwrap();
    let readings = budget::run(&root, &manifest).unwrap();
    let clippy = readings
        .into_iter()
        .find(|r| r.scope == "ext/clippy.toml")
        .unwrap();
    assert_eq!(clippy.status, Status::Fail, "{clippy:#?}");
    assert_eq!(clippy.rule, "generated.committed");
    assert!(
        !clippy.message.contains("differs from its rendering"),
        "the fixture's copy is the rendering: {}",
        clippy.message
    );
    clippy
}

/// Clippy reads the nearest configuration alone and merges nothing, a
/// `.clippy.toml` before a `clippy.toml` in one directory: a crate
/// carrying either of its own is held to none of the rules the generated
/// file above it carries, and each such file fails the generated file's
/// reading, naming itself and every rule it hides.
#[test]
fn a_clippy_configuration_nearer_than_a_generated_one_fails_naming_the_file_and_the_rules_it_hides()
{
    let clippy = current_generated_reading("nearer-clippy");
    for named in [
        "ext/pcre/clippy.toml",
        "ext/json/.clippy.toml",
        "ext.no-locks",
        "fork.threads",
        "(fixture)",
    ] {
        assert!(
            clippy.message.contains(named),
            "{named}: {}",
            clippy.message
        );
    }
}

/// `CLIPPY_CONF_DIR` points clippy at one directory for every crate, past
/// each generated file, and cargo hands its `[env]` to clippy-driver
/// whatever the environment says: a cargo configuration of the
/// checkout that sets it fails the generated file's reading, naming it.
#[test]
fn a_cargo_configuration_setting_clippy_conf_dir_fails_naming_it() {
    let clippy = current_generated_reading("clippy-conf-dir");
    for named in [".cargo/config.toml", "CLIPPY_CONF_DIR", "ext.no-locks"] {
        assert!(
            clippy.message.contains(named),
            "{named}: {}",
            clippy.message
        );
    }
}

/// The licence reading of a fixture, which fails, with every clause in
/// `named` in its message and none in `unnamed`.
fn licence_reading(kind: &str, name: &str, named: &[&str], unnamed: &[&str]) -> Reading {
    let root = fixture(kind, name);
    let manifest = Manifest::load(&root).unwrap();
    let readings = budget::run(&root, &manifest).unwrap();
    let reading = readings.iter().find(|r| r.scope == "licence").unwrap();
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(reading.rule, "licence.header");
    for clause in named {
        assert!(
            reading.message.contains(clause),
            "{clause}: {}",
            reading.message
        );
    }
    for clause in unnamed {
        assert!(
            !reading.message.contains(clause),
            "{clause}: {}",
            reading.message
        );
    }
    reading.clone()
}

/// The rule's own fixture: a file that carries the four lines and that
/// the rule's list does not name fails saying so, and of the files the
/// list names, one with the expression and no copyright line under it and
/// a C file that begins with a block comment each fail naming the line
/// that differs and the header's. The listed file that begins with the
/// four lines, the crate root that states no licence and the vendored
/// file the rule's `exempt` names are not named, and the rule's text and
/// remedy close the message.
#[test]
fn a_file_states_no_licence_unless_the_list_names_it_and_then_it_begins_with_the_four_lines() {
    let reading = licence_reading(
        "rules",
        "licence.header",
        &[
            "crates/php_unit/src/unlisted.rs states \"MIT AND BSD-3-Clause\" where it begins and the rule's allow does not name it; a file carries the header only where that list names its path",
            "crates/php_unit/src/two_lines.rs's line 2 is \"// Derived in part from php-src, used under BSD-3-Clause: see NOTICE.\" and the header's is \"// Copyright © The PHP Group and Contributors.\"",
            "crates/php_unit/src/shim.c's line 1 is",
            "(seams.toml rule licence.header: A Rust, C or C++ file under crates/, ext/ or sapi/ states no licence where it begins, and one the rule's allow names begins with the four lines of the licence header and an empty one. Instead: take the licence lines off a file the list does not name, and begin one it names with the four lines.)",
        ],
        &["held.rs", "lib.rs", "vendored"],
    );
    assert_eq!(reading.numbers["files"], 5.into(), "{reading:#?}");
    assert_eq!(reading.numbers["headed"], 3.into(), "{reading:#?}");
}

/// The expression of a file the rule's list names is
/// `MIT AND BSD-3-Clause` and what its row adds: one that says `MIT` alone
/// fails, and one that adds a licence no row gives it fails, each naming
/// both expressions. The listed file that carries the four lines and the
/// crate root, which no list names and which states none, are not named.
#[test]
fn an_expression_that_is_not_the_files_fails_naming_both() {
    licence_reading(
        "budget",
        "licence-expression",
        &[
            "crates/php_unit/src/alone.rs states \"MIT\" and its expression is \"MIT AND BSD-3-Clause\"",
            "crates/php_unit/src/wide.rs states \"MIT AND BSD-3-Clause AND Zlib\" and its expression is \"MIT AND BSD-3-Clause\"",
        ],
        &["listed.rs", "lib.rs"],
    );
}

/// A file the rule's list and a row of `THIRD-PARTY.md`'s ports both name
/// is held to the row: the line that says whose it is, each notice the
/// row names, byte for byte as `LICENSES/` holds it, and the line under it
/// that names that file. A file that says it is a port under no row
/// fails, and so does a row whose file is not there. The licensed
/// component's file and the public-domain one's, which begin as their
/// rows ask, are not named, and neither is the file a row of the next
/// table names. A component's file the list does not name carries none of
/// it and is not named either, and its row's licence is still one its
/// crate's `license` covers: the crate's that lacks it fails.
#[test]
fn a_components_file_is_held_to_its_row_and_its_notice_to_licenses() {
    licence_reading(
        "budget",
        "licence-component",
        &[
            "crates/php_unit/src/unsaid.rs's line 5 is \"\" and the header's is \"// A port of unsaid, which is in the public domain: see THIRD-PARTY.md.\"",
            "crates/php_unit/src/open.rs's line 10 is \"\" and the header's is \"// It stands, byte for byte, in LICENSES/sha-crypt-256.txt.\"",
            "crates/php_unit/src/altered.rs's line 9 is \"//      Released into the public domain by Ulrich Drepper <drepper@redhat.com>.  */\" and the header's is \"//      Released into the Public Domain by Ulrich Drepper <drepper@redhat.com>.  */\"",
            "crates/php_unit/src/claims.rs says it is a port and no row of THIRD-PARTY.md's ports names it",
            "THIRD-PARTY.md's ports name crates/php_unit/src/absent.rs, which is no file the rule covers",
            "crates/php_unit/Cargo.toml says license = \"MIT AND BSD-3-Clause AND Zlib\" and its files are under TCL",
        ],
        &["held.rs", "free.rs", "lib.rs", "bare.rs"],
    );
}

/// A file under an LGPL licence says that it was changed and when: lines
/// that name no day fail.
#[test]
fn an_lgpl_file_whose_change_lines_name_no_day_fails() {
    licence_reading(
        "budget",
        "licence-lgpl",
        &[
            "crates/php_unit/src/num.rs is under an LGPL licence and changed on ",
            "; `// Changed from ` lines under its fifth line name each day it changed, and none names ",
        ],
        &["lib.rs"],
    );
}

/// A crate's `license` covers the licences of its files the rule covers,
/// read through the workspace's where the crate inherits it, though no
/// file states them: one that says `MIT` over a file under BSD-3-Clause
/// fails, and so does a crate with no file the rule covers that inherits
/// a `license` wider than `MIT`. The crate that inherits what its file is
/// under is not named.
#[test]
fn a_crates_license_covers_the_licences_of_its_files() {
    licence_reading(
        "budget",
        "licence-manifest",
        &[
            "crates/php_unit/Cargo.toml says license = \"MIT\" and its files are under BSD-3-Clause",
            "lanes/a/Cargo.toml inherits the workspace's license = \"MIT AND BSD-3-Clause\" and the rule covers no file of the crate; such a crate says license = \"MIT\" itself",
        ],
        &["php_heap"],
    );
}

/// The tests whose subject is the tracked workspace itself: each reads the
/// `seams.toml`, `oracle.toml`, registry or sources standing in the
/// workspace, so a person's edit there is what may turn one red.
/// Every other test here judges by a manifest it builds, or by a
/// scratch's byte-for-byte copy of the tracked one where the binary's
/// hook or `permit` refuses any other (`built.rs`).
mod tracked {
    use super::*;

    #[test]
    fn every_rule_the_two_instruments_evaluate_has_a_fixture_that_fails_under_it() {
        let manifest = Manifest::load(workspace()).unwrap();
        let mut seen = 0;
        for rule in &manifest.rules {
            let predicate = rule.predicate.named()[0];
            let Some((_, instrument)) = EVALUATED.iter().find(|(p, _)| *p == predicate) else {
                continue;
            };
            seen += 1;
            let root = fixture("rules", &rule.id);
            let fixture_manifest = Manifest::load(&root).unwrap();
            let readings = instrument(&root, &fixture_manifest).unwrap();
            let failed = failures(&readings);
            assert!(
                failed.iter().any(|r| r.rule == rule.id),
                "fixture {} does not fail under rule {}: {readings:#?}",
                root.display(),
                rule.id
            );
        }
        assert!(
            seen >= 2,
            "the manifest's allow and generated rules are covered"
        );
    }
}

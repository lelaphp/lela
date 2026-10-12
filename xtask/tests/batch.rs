//! `xtask clippy` and `xtask test` measure every crate in one cargo run
//! (#2165). The fixture at `xtask/fixtures/test/batch` is nine crates:
//! one clean, one carrying a lint, one built on it, one that does not
//! compile, one built on that, a proc-macro crate, and one whose lint is
//! behind a feature (#140), and two hosts whose binaries turn a feature of
//! `php_heap` on and off (#2606). Each crate's reading out of the one run is the
//! reading a run over it alone gives: a crate's lint is its own and blocks
//! none above it, and a crate that does not compile stops only the crates
//! built on it.

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::manifest::Manifest;
use xtask::reading::{Number, Reading, Status};
use xtask::scratch::Tree;
use xtask::{clippy, test};

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

fn fixture() -> PathBuf {
    xtask::package_dir().join("fixtures/test/batch")
}

const CRATES: [&str; 9] = [
    "php_heap",
    "php_unit",
    "php_syntax",
    "php_compile",
    "php_engine",
    "php_macros",
    "php_link",
    "sapi/cli",
    "sapi/makala",
];

/// The fixture as a repository of its own, committed once, with the
/// tracked pin and registry beside its manifest: the instruments stamp
/// each reading through git.
fn repository(name: &str) -> Tree {
    committed(&fixture(), name)
}

/// Every file under `dir`, relative to it.
fn files_under(dir: &Path, at: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir.join(at)).unwrap() {
        let path = at.join(entry.unwrap().file_name());
        if dir.join(&path).is_dir() {
            files_under(dir, &path, out);
        } else {
            out.push(path);
        }
    }
}

/// A fixture workspace whole as a repository of its own, as
/// [`repository`] lays the batch fixture down.
fn committed(fixture: &Path, name: &str) -> Tree {
    let root = Tree::new(name).unwrap();
    let mut files = Vec::new();
    files_under(fixture, Path::new(""), &mut files);
    for file in &files {
        std::fs::create_dir_all(root.join(file).parent().unwrap()).unwrap();
        std::fs::copy(fixture.join(file), root.join(file)).unwrap();
    }
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::copy(workspace().join("oracle.toml"), root.join("oracle.toml")).unwrap();
    std::fs::copy(
        workspace().join("docs/divergence.toml"),
        root.join("docs/divergence.toml"),
    )
    .unwrap();
    let manifest = std::fs::read_to_string(root.join("seams.toml")).unwrap();
    std::fs::write(
        root.join("seams.toml"),
        manifest.replace("../../../../oracle.toml", "oracle.toml"),
    )
    .unwrap();
    std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["add", "-A"],
        &[
            "-c",
            "user.name=x",
            "-c",
            "user.email=x@x",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/var/empty",
            "commit",
            "-q",
            "-m",
            "scratch",
        ],
    ] {
        let out = Command::new("git")
            .args(args)
            .current_dir(&root)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}");
    }
    root
}

fn of<'a>(readings: &'a [Reading], scope: &str) -> &'a Reading {
    readings
        .iter()
        .find(|r| r.scope == scope)
        .unwrap_or_else(|| panic!("no reading of {scope}: {readings:#?}"))
}

fn int(reading: &Reading, key: &str) -> i64 {
    match reading.numbers.get(key) {
        Some(Number::Int(n)) => *n,
        other => panic!("{key} is {other:?}: {reading:#?}"),
    }
}

/// What a reading says, without the time it was taken at: what the
/// one run and a run over the crate alone must agree on.
fn verdict(reading: &Reading) -> (Status, String, Vec<(String, String)>) {
    let first = reading
        .message
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    let numbers = reading
        .numbers
        .iter()
        .map(|(k, v)| (k.clone(), v.to_string()))
        .collect();
    (reading.status, first, numbers)
}

#[test]
fn one_test_run_reads_each_crate_as_a_run_over_it_alone() {
    let root = repository("batch-test");
    let manifest = Manifest::load(&root).unwrap();
    let all = test::run(&root, &manifest, &[]).unwrap();
    assert_eq!(all.len(), CRATES.len(), "{all:#?}");

    let heap = of(&all, "php_heap");
    assert_eq!(heap.status, Status::Pass, "{heap:#?}");
    assert_eq!(int(heap, "passed"), 3, "two tests and a doctest: {heap:#?}");
    assert_eq!(of(&all, "php_unit").status, Status::Pass);
    assert_eq!(of(&all, "php_syntax").status, Status::Pass);
    let macros = of(&all, "php_macros");
    assert_eq!(
        macros.status,
        Status::Pass,
        "a proc-macro crate's tests find the toolchain's libstd: {macros:#?}"
    );
    assert_eq!(int(macros, "passed"), 1, "{macros:#?}");
    assert_eq!(
        of(&all, "php_link").status,
        Status::Pass,
        "a test run turns on no feature, so the lint behind one is not built"
    );
    // Each host's test starts its binary and holds it to the features its
    // own package turns on: makala's, built beside cli's, would carry
    // cli's `loud` (#2606).
    for host in ["sapi/cli", "sapi/makala"] {
        let reading = of(&all, host);
        assert_eq!(reading.status, Status::Pass, "{reading:#?}");
        assert_eq!(int(reading, "passed"), 1, "{reading:#?}");
    }
    for broken in ["php_compile", "php_engine"] {
        let reading = of(&all, broken);
        assert_eq!(reading.status, Status::Fail, "{reading:#?}");
        assert!(
            reading
                .message
                .starts_with(&format!("{broken}: the tests did not build")),
            "{}",
            reading.message
        );
    }
    assert!(
        of(&all, "php_compile").message.contains("mismatched types"),
        "the crate that does not compile says why: {}",
        of(&all, "php_compile").message
    );

    for krate in CRATES {
        let alone = test::run(&root, &manifest, &[krate.to_owned()]).unwrap();
        assert_eq!(
            verdict(of(&all, krate)),
            verdict(of(&alone, krate)),
            "{krate}"
        );
    }
}

#[test]
fn one_clippy_run_reads_each_crate_as_a_run_over_it_alone() {
    let root = repository("batch-clippy");
    let manifest = Manifest::load(&root).unwrap();
    let all = clippy::run(&root, &manifest, &[]).unwrap();
    assert_eq!(all.len(), CRATES.len(), "{all:#?}");

    assert_eq!(of(&all, "php_heap").status, Status::Pass);
    let unit = of(&all, "php_unit");
    assert_eq!(unit.status, Status::Fail, "{unit:#?}");
    assert_eq!(int(unit, "warnings"), 1, "{unit:#?}");
    assert!(unit.message.contains("needless_return"), "{}", unit.message);
    let syntax = of(&all, "php_syntax");
    assert_eq!(
        syntax.status,
        Status::Pass,
        "the lint of the crate it is built on is not its own: {syntax:#?}"
    );
    let link = of(&all, "php_link");
    assert_eq!(
        link.status,
        Status::Fail,
        "a lint behind a feature is linted: {link:#?}"
    );
    assert_eq!(int(link, "warnings"), 1, "{link:#?}");
    assert!(link.message.contains("let_and_return"), "{}", link.message);
    let compile = of(&all, "php_compile");
    assert_eq!(compile.status, Status::Fail);
    assert!(int(compile, "errors") > 0, "{compile:#?}");
    let engine = of(&all, "php_engine");
    assert_eq!(engine.status, Status::Fail);
    assert!(
        engine.message.contains("did not check it"),
        "{}",
        engine.message
    );

    for krate in CRATES {
        let alone = clippy::run(&root, &manifest, &[krate.to_owned()]).unwrap();
        assert_eq!(
            verdict(of(&all, krate)),
            verdict(of(&alone, krate)),
            "{krate}"
        );
    }
}

/// The release profile's lint (#2392): a crate whose sources name
/// `debug_assertions` is linted again in release on its library and
/// binaries, as `<crate>/release`, and fails there on a lint the dev run
/// cannot see; a crate naming the word in a comment or a string is not.
#[test]
fn a_crate_whose_sources_name_debug_assertions_is_linted_again_in_release() {
    let fixture = xtask::package_dir().join("fixtures/clippy/release-cfg");
    let root = committed(&fixture, "clippy-release");
    let manifest = Manifest::load(&root).unwrap();
    let all = clippy::run(&root, &manifest, &[]).unwrap();
    let scopes: Vec<&str> = all.iter().map(|r| r.scope.as_str()).collect();
    let dev = ["php_heap", "php_unit", "php_syntax"];
    assert_eq!(
        scopes,
        [&dev[..], &["php_heap/release", "php_syntax/release"]].concat()
    );
    for passing in ["php_heap", "php_unit", "php_syntax", "php_syntax/release"] {
        assert_eq!(of(&all, passing).status, Status::Pass, "{all:#?}");
    }
    let heap = of(&all, "php_heap/release");
    assert_eq!(heap.status, Status::Fail, "{heap:#?}");
    assert!(
        heap.message.contains("does not need to be mutable"),
        "{}",
        heap.message
    );
}

/// A fixture's rule, the crates that fail with what each message names,
/// and the crates that pass.
type Expected<'a> = (&'a str, &'a [(&'a str, &'a str)], &'a [&'a str]);

/// The sensor of the manifest's `deny_lints` rules (#1917): every such
/// rule has a fixture workspace under `xtask/fixtures/rules/<id>/` whose
/// clippy readings carry the rule, fail where a source the scope covers
/// sits under no module that denies the rule's lints, or under one that
/// lifts a denial again (an inner `allow`, or one on the `mod` item),
/// naming the source and the lints, and pass where the crate root, a
/// module above the source or the crate's Cargo `[lints]` denies them.
#[test]
fn each_deny_lints_rule_fails_a_crate_whose_covered_source_no_module_denies_it_at() {
    let manifest = Manifest::load(workspace()).unwrap();
    let rules: Vec<_> = manifest
        .rules
        .iter()
        .filter(|r| r.predicate.deny_lints.is_some())
        .collect();
    assert!(rules.len() >= 2, "lints.no-panic and unsafe.one-op");
    let expected: [Expected; 2] = [
        (
            "lints.no-panic",
            &[
                (
                    "php_unit",
                    "crates/php_unit/src/lib.rs: clippy::expect_used, clippy::panic, clippy::todo, clippy::unimplemented, clippy::unreachable not denied",
                ),
                (
                    "php_syntax",
                    "crates/php_syntax/src/main.rs: clippy::expect_used",
                ),
                (
                    "php_link",
                    "crates/php_link/src/lifted.rs and 1 more: clippy::panic, clippy::unwrap_used not denied",
                ),
            ],
            &["php_heap", "php_sapi"],
        ),
        (
            "unsafe.one-op",
            &[(
                "ext/pcre",
                "ext/pcre/src/ffi/inner.rs and 1 more: clippy::multiple_unsafe_ops_per_block, clippy::undocumented_unsafe_blocks not denied",
            )],
            &["ext/ctype"],
        ),
    ];
    for rule in rules {
        let (_, failing, passing) = expected
            .iter()
            .find(|(id, _, _)| *id == rule.id)
            .unwrap_or_else(|| panic!("rule {} has no fixture this test reads", rule.id));
        let fixture = xtask::package_dir().join("fixtures/rules").join(&rule.id);
        let root = committed(&fixture, &format!("deny-lints-{}", rule.id));
        let manifest = Manifest::load(&root).unwrap();
        let readings = clippy::run(&root, &manifest, &[]).unwrap();
        for (krate, named) in *failing {
            let reading = of(&readings, krate);
            assert_eq!(reading.status, Status::Fail, "{reading:#?}");
            assert_eq!(reading.rule, rule.id);
            assert!(
                reading.message.contains(named) && reading.message.contains("(fixture)"),
                "{}",
                reading.message
            );
        }
        for krate in *passing {
            let reading = of(&readings, krate);
            assert_eq!(reading.status, Status::Pass, "{reading:#?}");
            assert_eq!(reading.rule, rule.id);
        }
    }
}

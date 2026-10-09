//! `xtask clippy` and `xtask test` measure every crate in one cargo run
//! (#2165). The fixture at `xtask/fixtures/test/batch` is seven crates:
//! one clean, one carrying a lint, one built on it, one that does not
//! compile, one built on that, a proc-macro crate, and one whose lint is
//! behind a feature (#140). Each crate's reading out of the one run is the
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
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/test/batch")
}

const CRATES: [&str; 7] = [
    "php_heap",
    "php_unit",
    "php_syntax",
    "php_compile",
    "php_engine",
    "php_macros",
    "php_link",
];

/// The fixture as a repository of its own, committed once, with the
/// tracked pin and registry beside its manifest: the instruments stamp
/// each reading through git.
fn repository(name: &str) -> Tree {
    let root = Tree::new(name).unwrap();
    let fixture = fixture();
    let mut files = vec![
        PathBuf::from("seams.toml"),
        PathBuf::from("Cargo.toml"),
        PathBuf::from("Cargo.lock"),
    ];
    for krate in CRATES {
        files.push(Path::new("crates").join(krate).join("Cargo.toml"));
        files.push(Path::new("crates").join(krate).join("src/lib.rs"));
    }
    files.push(PathBuf::from("crates/php_heap/src/extra.rs"));
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

//! The one rule beside the tally's ladder, held at the command line: an
//! instrument that set out for readings it did not take exits
//! `exit::UNFINISHED`, and writes the readings it writes today, each the
//! skip that says why. `phpt`'s runs are held in `phpt.rs`; here each of
//! `witness`, `compile-identity`, `crosscheck`, `fuzz` and `hostile` is run
//! over a scratch workspace whose pin names an oracle that is not there,
//! so every run is one that could not take what it set out for, on any
//! machine, and nothing is built or served.
#![cfg(tracked)]

use std::fs;
use std::path::Path;

use xtask::oracle::Promise;
use xtask::reading::{Reading, Status, Store};
use xtask::scratch::Tree;

mod common {
    pub mod manifest;
    pub mod repo;
    pub mod run;
}
use common::manifest::pin;
use common::repo::scratch_workspace;
use common::run::{Ran, Run};

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

/// A scratch workspace holding the tracked manifest and a pin whose
/// oracle is at `nowhere/php` under it, which no machine has.
fn without_oracle(name: &str) -> Tree {
    let root = Tree::new(&format!("unfinished-{name}")).unwrap();
    scratch_workspace(workspace(), &root);
    let file = root.join("oracle.toml");
    let text = fs::read_to_string(&file).unwrap();
    let path = format!("path = \"{}\"", pin().oracle.path.display());
    assert!(text.contains(&path), "{text}");
    let nowhere = format!("path = \"{}\"", root.join("nowhere/php").display());
    fs::write(&file, text.replacen(&path, &nowhere, 1)).unwrap();
    root
}

/// The run exited as one that could not run to its end, and said so.
fn unfinished(ran: &Ran, instrument: &str) {
    assert_eq!(ran.code(), i32::from(xtask::exit::UNFINISHED), "{ran}");
    assert!(
        ran.stderr
            .contains(&format!("{instrument} could not run to its end")),
        "the status is said: {ran}"
    );
}

/// The reading `instrument` wrote of `scope` under `root`: the skip that
/// names the oracle's absence, as the run wrote before it took the rule.
fn skipped(root: &Path, instrument: &str, scope: &str) -> Reading {
    let reading = Store::of_workspace(root)
        .read(instrument, scope)
        .unwrap()
        .unwrap_or_else(|| panic!("no {instrument} reading of {scope}"));
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert!(
        reading.message.contains("nowhere/php"),
        "the skip names where the pin looked: {}",
        reading.message
    );
    reading
}

/// A crate's witnesses with no oracle to run them: the crate's skip is
/// written, counting the witness not run, and the run exits 4.
#[test]
fn witness_with_no_oracle_writes_its_skip_and_exits_as_unfinished() {
    let root = without_oracle("witness");
    fs::create_dir_all(root.join("ext/ctype/src")).unwrap();
    fs::write(
        root.join("ext/ctype/Cargo.toml"),
        "[package]\nname = \"php_ext_ctype\"\nedition = \"2024\"\n",
    )
    .unwrap();
    fs::write(
        root.join("ext/ctype/src/lib.rs"),
        "/// ```php\n/// echo 1;\n/// ```\n/// ```expect\n/// 1\n/// ```\npub fn native() {}\n",
    )
    .unwrap();
    let ran = Run::at(&root)
        .args(["witness", "--crate", "ext/ctype"])
        .go();
    unfinished(&ran, "witness");
    skipped(&root, "witness", "ext/ctype");
}

/// A corpus with no oracle to dump it under: the corpus's skip is
/// written, every file counted and none dumped, and the run exits 4.
#[test]
fn compile_identity_with_no_oracle_writes_its_skip_and_exits_as_unfinished() {
    let root = without_oracle("compile-identity");
    let corpus = root.join("corpus");
    fs::create_dir_all(&corpus).unwrap();
    fs::write(corpus.join("plain.php"), "<?php echo 1;\n").unwrap();
    let ran = Run::at(&root)
        .args(["compile-identity", "--corpus"])
        .args([&corpus])
        .go();
    unfinished(&ran, "compile-identity");
    let printed = ran
        .stdout
        .lines()
        .find(|l| l.starts_with("skip compile-identity "));
    assert!(printed.is_some(), "the skip is printed: {ran}");
}

/// An extension the binary renders, in a workspace whose binary cannot be
/// built, having no `Cargo.toml`: the oracle renders it and the engine's
/// side is served by nothing, so each column's reading fails as it did,
/// naming why, and the run exits 4, a rendering it set out for not taken.
#[test]
fn crosscheck_with_no_engine_binary_writes_its_failure_and_exits_as_unfinished() {
    let promised = Promise::Oracle.or_skip(pin().oracle.file().map(Path::to_path_buf));
    if promised.unwrap().is_none() {
        return;
    }
    let root = Tree::new("unfinished-crosscheck-unbuilt").unwrap();
    scratch_workspace(workspace(), &root);
    let ran = Run::at(&root)
        .args(["crosscheck", "--ext", "standard"])
        .go();
    unfinished(&ran, "crosscheck");
    let reading = Store::of_workspace(&root)
        .read("crosscheck", "ext/standard")
        .unwrap()
        .expect("the failing reading");
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert!(
        reading.message.contains("the engine rendered nothing"),
        "{}",
        reading.message
    );
}

/// An extension with no oracle to render it: both columns' skips are
/// written and the run exits 4.
#[test]
fn crosscheck_with_no_oracle_writes_its_skips_and_exits_as_unfinished() {
    let root = without_oracle("crosscheck");
    let ran = Run::at(&root).args(["crosscheck", "--ext", "ctype"]).go();
    unfinished(&ran, "crosscheck");
    for scope in ["ext/ctype", "ext/ctype/ri"] {
        skipped(&root, "crosscheck", scope);
    }
}

/// A hunt with no oracle to hold programs to: every owing crate's skip is
/// written and the run exits 4, building nothing.
#[test]
fn fuzz_with_no_oracle_writes_its_skips_and_exits_as_unfinished() {
    let root = without_oracle("fuzz");
    let ran = Run::at(&root).args(["fuzz"]).go();
    unfinished(&ran, "fuzz");
    let owing = [
        "php_syntax",
        "php_heap",
        "php_unit",
        "php_compile",
        "php_engine",
        "php_jit",
    ];
    for scope in owing {
        skipped(&root, "fuzz", scope);
    }
}

/// The hostile corpus with no oracle to hold it to: every owing crate's
/// skip is written and the run exits 4, building nothing.
#[test]
fn hostile_with_no_oracle_writes_its_skips_and_exits_as_unfinished() {
    let root = without_oracle("hostile");
    fs::create_dir_all(root.join("data")).unwrap();
    fs::copy(
        workspace().join("data/hostile.toml"),
        root.join("data/hostile.toml"),
    )
    .unwrap();
    let ran = Run::at(&root).args(["hostile"]).go();
    unfinished(&ran, "hostile");
    for scope in xtask::hostile::OWING {
        skipped(&root, "hostile", scope);
    }
}

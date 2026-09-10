//! `xtask vocab`, the glossary as a sensor (harness-design.md §5): the
//! `_Avoid_` lists of `CONTEXT.md` against public item names, rustdoc prose
//! scoped so that `Result` is not `result`. Every test here goes through the
//! instrument, `check` in process or the binary, and reads the reading it
//! writes; how the glossary itself is read is held beside the glossary.

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::manifest::Manifest;
use xtask::reading::{Number, Reading, Status, Store};
use xtask::vocab;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn fixture(rule: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/rules")
        .join(rule)
}

/// A scratch workspace: the tracked manifest, pin and glossary, the fixture
/// at `from` as one of the manifest's crates, and one commit so a reading
/// can be stamped.
fn scratch(name: &str, krate: &str, from: &Path) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xtask-vocab-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for file in ["seams.toml", "oracle.toml", "CONTEXT.md"] {
        std::fs::copy(workspace().join(file), dir.join(file)).unwrap();
    }
    let target = dir.join(krate);
    std::fs::create_dir_all(target.join("src")).unwrap();
    std::fs::copy(from.join("src/lib.rs"), target.join("src/lib.rs")).unwrap();
    // A crate is one by its Cargo.toml, which is what its reading's stamp
    // asks; the package name is the last segment of the crate's path.
    let package = krate.rsplit('/').next().unwrap();
    std::fs::write(
        target.join("Cargo.toml"),
        format!("[package]\nname = \"{package}\"\nedition = \"2024\"\n"),
    )
    .unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["add", "."],
        vec![
            "-c",
            "user.name=xtask",
            "-c",
            "user.email=xtask@localhost",
            "commit",
            "-q",
            "-m",
            "fixture",
        ],
    ] {
        let ok = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(args)
            .status()
            .unwrap()
            .success();
        assert!(ok);
    }
    dir
}

/// The one reading of `php_ext` over the `vocab.avoid` fixture standing as
/// `crates/php_ext`, through the instrument's library entry.
fn fixture_reading(name: &str) -> Reading {
    let root = scratch(name, "crates/php_ext", &fixture("vocab.avoid"));
    let manifest = Manifest::load(&root).unwrap();
    let readings = vocab::check(&root, &manifest, &["php_ext".to_owned()]).unwrap();
    assert_eq!(readings.len(), 1, "{readings:#?}");
    readings.into_iter().next().unwrap()
}

/// The findings a reading lists, one per line after the rule's text.
fn findings(reading: &Reading) -> Vec<&str> {
    reading.message.lines().skip(1).collect()
}

fn xtask(root: &Path, args: &[&str]) -> (bool, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("--root")
        .arg(root)
        .args(args)
        .output()
        .unwrap();
    (
        out.status.success(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

#[test]
fn a_public_item_named_by_an_avoided_word_fails_naming_the_word_and_the_term() {
    let reading = fixture_reading("items");
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(reading.rule, "vocab.avoid");
    let found = findings(&reading);
    let hit = found
        .iter()
        .find(|f| f.starts_with("src/lib.rs:6: \"run\" names "))
        .unwrap_or_else(|| panic!("{found:#?}"));
    assert!(hit.contains("the glossary says \"Serve\""), "{hit}");

    // Case does not hide a name: a type, a field and a variant.
    for line in [
        "src/lib.rs:16: \"registry\" names ",
        "src/lib.rs:18: \"globals\" names ",
        "src/lib.rs:24: \"handle\" names ",
    ] {
        assert!(
            found.iter().any(|f| f.starts_with(line)),
            "{line}: {found:#?}"
        );
    }
    assert!(
        !found.iter().any(|f| f.starts_with("src/lib.rs:23:")),
        "a variant named by a glossary term passes: {found:#?}"
    );
}

#[test]
fn prose_using_an_avoided_word_fails_and_the_same_word_as_a_type_name_passes() {
    let reading = fixture_reading("prose");
    let found = findings(&reading);
    for line in [
        "src/lib.rs:5: \"result\" in rustdoc prose",
        "src/lib.rs:5: \"run\" in rustdoc prose",
    ] {
        assert!(
            found.iter().any(|f| f.starts_with(line)),
            "{line}: {found:#?}"
        );
    }
    assert!(
        !found.iter().any(|f| f.starts_with("src/lib.rs:8:")),
        "`Result` in a code span and Result bare are the type, not the word: {found:#?}"
    );
    assert!(
        !found.iter().any(
            |f| (f.starts_with("src/lib.rs:10:") || f.starts_with("src/lib.rs:13:"))
                && f.contains(" names ")
        ),
        "a type named Result or Return is not the avoided word: {found:#?}"
    );
    assert_eq!(
        reading.numbers["hits"],
        Number::count(found.len()),
        "one line per hit: {reading:#?}"
    );
}

#[test]
fn the_instrument_writes_a_reading_per_crate_with_instrument_vocab() {
    let root = scratch("reading", "crates/php_ext", &fixture("vocab.avoid"));
    let (ok, out, err) = xtask(&root, &["vocab"]);
    assert!(!ok, "the fixture must fail: {out}{err}");
    assert!(out.contains("Serve"), "{out}");

    let store = Store::of_workspace(&root);
    let reading = store.read("vocab", "php_ext").unwrap().unwrap();
    assert_eq!(reading.instrument, "vocab");
    assert_eq!(reading.rule, "vocab.avoid");
    assert_eq!(reading.status, Status::Fail);
    assert!(reading.message.contains("run"), "{}", reading.message);
    assert!(!reading.tree().is_empty() && !reading.commit.is_empty());
    let hits = reading.numbers["hits"].to_string().parse::<i64>().unwrap();
    assert!(hits >= 3, "items and prose: {reading:#?}");

    // A lock under the crate is not read.
    let other = scratch("nolock", "crates/php_heap", &fixture("vocab.avoid"));
    std::fs::write(other.join("crates/php_heap/heap.lock"), "heap::run\n").unwrap();
    let (ok, _, _) = xtask(&other, &["vocab", "--crate", "php_heap"]);
    assert!(!ok);
    let reading = Store::of_workspace(&other)
        .read("vocab", "php_heap")
        .unwrap()
        .unwrap();
    assert!(!reading.numbers.contains_key("lock"), "{reading:#?}");
    assert!(
        !reading.message.contains("heap::run"),
        "{}",
        reading.message
    );
}

#[test]
fn a_crate_that_does_not_exist_yet_gets_no_reading() {
    let root = scratch("absent", "crates/php_ext", &fixture("vocab.avoid"));
    let (ok, _, err) = xtask(&root, &["vocab", "--crate", "php_heap"]);
    assert!(ok, "{err}");
    assert!(
        Store::of_workspace(&root)
            .read("vocab", "php_heap")
            .unwrap()
            .is_none()
    );
}

/// Every rule `vocab` evaluates (the manifest's `avoid_from` rules, harness
/// §4.1) has a negative-control fixture under `fixtures/rules/<id>/` that
/// must fail under the instrument. The rules of the other instruments are
/// theirs to control.
#[test]
fn every_rule_vocab_evaluates_has_a_fixture_that_fails() {
    let manifest = Manifest::load(workspace()).unwrap();
    let rules: Vec<_> = manifest
        .rules
        .iter()
        .filter(|r| r.predicate.avoid_from.is_some())
        .collect();
    assert!(!rules.is_empty());
    for rule in rules {
        let dir = fixture(&rule.id);
        assert!(
            dir.is_dir(),
            "rule {} has no fixture at {}",
            rule.id,
            dir.display()
        );
        let root = scratch(&format!("rule-{}", rule.id), "crates/php_ext", &dir);
        let readings = vocab::check(
            &root,
            &Manifest::load(&root).unwrap(),
            &["php_ext".to_owned()],
        )
        .unwrap();
        assert!(
            readings
                .iter()
                .any(|r| r.status == Status::Fail && r.rule == rule.id),
            "the fixture for {} passes: {readings:#?}",
            rule.id
        );
    }
}

//! `xtask vocab`, the glossary as a sensor (harness-design.md §5): the
//! `_Avoid_` lists of `CONTEXT.md` against public item names, rustdoc prose
//! scoped so that `Result` is not `result`.

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::glossary::Glossary;
use xtask::manifest::Manifest;
use xtask::reading::{Status, Store};
use xtask::vocab::{self, Finding, Where};

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn fixture(rule: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/rules")
        .join(rule)
}

fn glossary() -> Glossary {
    Glossary::load(&workspace().join("CONTEXT.md")).unwrap()
}

fn fixture_findings() -> Vec<Finding> {
    vocab::scan_crate(&fixture("vocab.avoid"), &glossary())
        .unwrap()
        .findings
}

#[test]
fn the_glossary_reads_each_term_with_its_avoided_phrases() {
    let glossary = glossary();
    let serve = glossary.term("Serve").unwrap();
    let avoided: Vec<&str> = serve.avoid.iter().map(|a| a.text.as_str()).collect();
    assert_eq!(avoided, ["run", "execute", "handle"]);

    // A parenthetical explanation and a qualifier are not part of the phrase.
    let linked = glossary.term("Linked table").unwrap();
    let avoided: Vec<&str> = linked.avoid.iter().map(|a| a.text.as_str()).collect();
    assert_eq!(avoided, ["class table", "registry"]);
    let native = glossary.term("Parameter / Argument").unwrap();
    let avoided: Vec<&str> = native.avoid.iter().map(|a| a.text.as_str()).collect();
    assert_eq!(avoided, ["arg", "param", "Ret"]);

    // A phrase avoided under two terms names both.
    let phrases = glossary.avoided_phrases();
    let registry = phrases
        .iter()
        .find(|a| a.phrase.text == "registry")
        .unwrap();
    assert_eq!(registry.terms, ["Linked table", "Bundle"]);

    // The glossary's own carve-out: `result (which means `Result` in Rust)`.
    let result = phrases.iter().find(|a| a.phrase.text == "result").unwrap();
    assert_eq!(result.phrase.code, ["Result"]);
}

#[test]
fn a_public_item_named_by_an_avoided_word_fails_naming_the_word_and_the_term() {
    let findings = fixture_findings();
    let hit = findings
        .iter()
        .find(|f| f.site == Where::Item && f.word == "run")
        .unwrap_or_else(|| panic!("{findings:#?}"));
    assert_eq!(hit.terms, ["Serve"]);
    assert_eq!(hit.file, Path::new("src/lib.rs"));
    assert_eq!(hit.line, 6);
    let text = hit.to_string();
    assert!(text.contains("run") && text.contains("Serve"), "{text}");

    // Case does not hide a name: a type, a field and a variant.
    let items: Vec<(&str, usize)> = findings
        .iter()
        .filter(|f| f.site == Where::Item)
        .map(|f| (f.word.as_str(), f.line))
        .collect();
    assert!(items.contains(&("registry", 16)), "{items:?}");
    assert!(items.contains(&("globals", 18)), "{items:?}");
    assert!(items.contains(&("handle", 24)), "{items:?}");
    assert!(
        !items.iter().any(|(_, line)| *line == 23),
        "a variant named by a glossary term passes: {items:?}"
    );
}

#[test]
fn prose_using_an_avoided_word_fails_and_the_same_word_as_a_type_name_passes() {
    let findings = fixture_findings();
    let prose: Vec<(&str, usize)> = findings
        .iter()
        .filter(|f| f.site == Where::Prose)
        .map(|f| (f.word.as_str(), f.line))
        .collect();
    assert!(prose.contains(&("result", 5)), "{prose:?}");
    assert!(prose.contains(&("run", 5)), "{prose:?}");
    assert!(
        !prose.iter().any(|(_, line)| *line == 8),
        "`Result` in a code span and Result bare are the type, not the word: {prose:?}"
    );
    assert!(
        !findings
            .iter()
            .any(|f| f.site == Where::Item && (f.line == 10 || f.line == 13)),
        "a type named Result or Return is not the avoided word: {findings:#?}"
    );
}

/// A scratch workspace: the tracked manifest, pin and glossary, the fixture
/// as one of the manifest's crates, and one commit so a reading can be stamped.
fn scratch(name: &str, krate: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("xtask-vocab-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    for file in ["seams.toml", "oracle.toml", "CONTEXT.md"] {
        std::fs::copy(workspace().join(file), dir.join(file)).unwrap();
    }
    let target = dir.join(krate);
    std::fs::create_dir_all(target.join("src")).unwrap();
    let from = fixture("vocab.avoid");
    std::fs::copy(from.join("src/lib.rs"), target.join("src/lib.rs")).unwrap();
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
fn the_instrument_writes_a_reading_per_crate_with_instrument_vocab() {
    let root = scratch("reading", "crates/php_ext");
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
    let other = scratch("nolock", "crates/php_heap");
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
    let root = scratch("absent", "crates/php_ext");
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
/// must fail. The rules of the other instruments are theirs to control.
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
        let glossary =
            Glossary::load(&workspace().join(rule.predicate.avoid_from.as_deref().unwrap()))
                .unwrap();
        let scan = vocab::scan_crate(&dir, &glossary).unwrap();
        assert!(
            !scan.findings.is_empty(),
            "the fixture for {} passes",
            rule.id
        );
    }
}

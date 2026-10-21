//! `xtask vocab`, the glossary as a sensor (harness-design.md §5): the
//! `_Avoid_` lists of `CONTEXT.md` against public item names, rustdoc prose
//! scoped so that `Result` is not `result`. Every test here goes through the
//! instrument, `check` in process or the binary, and reads the reading it
//! writes; how the glossary itself is read is held beside the glossary.

use std::path::{Path, PathBuf};

use xtask::manifest::{AvoidFrom, Manifest, Rule};
use xtask::reading::{Number, Reading, Status, Store};
use xtask::scratch::Tree;
use xtask::vocab;

mod common {
    pub mod repo;
    pub mod run;
}
use common::repo::Repo;
use common::run::xtask;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

fn fixture(rule: &str) -> PathBuf {
    xtask::package_dir().join("fixtures/rules").join(rule)
}

/// A scratch workspace: the tracked manifest, pin and glossary, the fixture
/// at `from` as one of the manifest's crates, and one commit so a reading
/// can be stamped.
fn scratch(name: &str, krate: &str, from: &Path) -> Tree {
    let dir = Tree::new(&format!("vocab-{name}")).unwrap();
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
    Repo::init(&dir).commit("fixture");
    dir
}

/// The one reading of `php_ext` over the `vocab.avoid` fixture standing as
/// `crates/php_ext`, through the instrument's library entry.
fn fixture_reading(name: &str) -> Reading {
    let root = scratch(name, "crates/php_ext", &fixture("vocab.avoid"));
    let manifest = Manifest::load(&root).unwrap();
    let readings = vocab::hold(&root, &manifest, &["php_ext".to_owned()]).unwrap();
    assert_eq!(readings.len(), 1, "{readings:#?}");
    readings.into_iter().next().unwrap()
}

/// The findings a reading lists, one per line after the rule's text.
fn findings(reading: &Reading) -> Vec<&str> {
    reading.message.lines().skip(1).collect()
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

/// The glossary's Ticket entry (#27): the two words that hide whether a
/// thing found and not done was filed, `leftover` and `follow-up`, fire in
/// a public name and in prose, and the finding names the term. A hyphen
/// in the glossary's phrase is a word boundary, so `follow-up` in prose is
/// the phrase as written.
#[test]
fn a_thing_found_and_not_done_is_a_ticket_and_the_words_that_hide_it_fail() {
    let reading = fixture_reading("ticket");
    let found = findings(&reading);
    for line in [
        "src/lib.rs:28: \"follow-up\" in rustdoc prose; the glossary says \"Ticket\"",
        "src/lib.rs:29: \"leftover\" names `pub struct Leftover`; the glossary says \"Ticket\"",
    ] {
        assert!(found.contains(&line), "{line}: {found:#?}");
    }
    assert!(
        !found.iter().any(|f| f.starts_with("src/lib.rs:27:")),
        "the words the entry replaces them with pass: {found:#?}"
    );
}

#[test]
fn the_instrument_writes_a_reading_per_crate_with_instrument_vocab() {
    let root = scratch("reading", "crates/php_ext", &fixture("vocab.avoid"));
    let ran = xtask(&root, &["vocab"]);
    assert_ne!(ran.code(), 0, "the fixture must fail: {ran}");
    assert!(ran.stdout.contains("Serve"), "{ran}");

    let store = Store::of_workspace(&root);
    let reading = store.read("vocab", "php_ext").unwrap().unwrap();
    assert_eq!(reading.instrument, "vocab");
    assert_eq!(reading.rule, "vocab.avoid");
    assert_eq!(reading.status, Status::Fail);
    assert!(reading.message.contains("run"), "{}", reading.message);
    assert!(!reading.key().is_empty() && !reading.commit.is_empty());
    let hits = reading.numbers["hits"].to_string().parse::<i64>().unwrap();
    assert!(hits >= 3, "items and prose: {reading:#?}");

    // A lock under the crate is not read.
    let other = scratch("nolock", "crates/php_heap", &fixture("vocab.avoid"));
    std::fs::write(other.join("crates/php_heap/heap.lock"), "heap::run\n").unwrap();
    xtask(&other, &["vocab", "--crate", "php_heap"]).failed();
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
    xtask(&root, &["vocab", "--crate", "php_heap"]).ok();
    assert!(
        Store::of_workspace(&root)
            .read("vocab", "php_heap")
            .unwrap()
            .is_none()
    );
}

/// A copy of the fixture at `from` in a scratch directory, whole, with the
/// tracked pin beside it where the fixture's manifest points, and one
/// commit: a fixture that carries its own `seams.toml` is a workspace of
/// its own (#57), and the pin it names by path is the real one.
fn scratch_workspace(name: &str, from: &Path) -> (Tree, PathBuf) {
    let base = Tree::new(&format!("vocab-{name}")).unwrap();
    let dir = base.join("a/b/c/d");
    copy_tree(from, &dir);
    std::fs::copy(workspace().join("oracle.toml"), base.join("oracle.toml")).unwrap();
    Repo::init(&dir).commit("fixture");
    (base, dir)
}

fn copy_tree(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// The readings of `vocab` over the fixture at `dir` under the manifest
/// the fixture carries.
fn fixture_readings(name: &str, dir: &Path) -> Vec<Reading> {
    let (_base, root) = scratch_workspace(name, dir);
    let manifest = Manifest::load(&root).unwrap();
    vocab::hold(&root, &manifest, &[]).unwrap()
}

/// A rule whose `avoid_from` names a section holds its scope to the terms
/// under that heading alone (#63): over the `vocab.harness` fixture, whose
/// glossary has an Engine section and a Harness section, the Harness
/// words fire in prose and in a name, the Engine words (`run`, `registry`)
/// do not, `Result` is the code spelling the section's own entry marks,
/// and the fixture under the crate's `fixtures/` is not read. Without the
/// grammar the fixture's manifest does not load.
#[test]
fn a_section_holds_its_scope_to_that_sections_terms_and_a_fixture_under_the_crate_is_not_read() {
    let readings = fixture_readings("section", &fixture("vocab.harness"));
    assert_eq!(readings.len(), 1, "{readings:#?}");
    let reading = &readings[0];
    assert_eq!(reading.scope, "xtask");
    assert_eq!(reading.rule, "vocab.harness");
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    let found = findings(reading);
    assert_eq!(
        found,
        [
            "src/lib.rs:6: \"scoreboard\" in rustdoc prose; the glossary says \"Tally\"",
            "src/lib.rs:10: \"summary\" names `pub fn summary`; the glossary says \"Tally\"",
            "src/lib.rs:15: \"leftover\" in rustdoc prose; the glossary says \"Ticket\"",
            "src/lib.rs:15: \"follow-up\" in rustdoc prose; the glossary says \"Ticket\"",
            "src/lib.rs:16: \"leftover\" names `pub fn leftover`; the glossary says \"Ticket\"",
        ],
        "{}",
        reading.message
    );
    assert_eq!(reading.numbers["hits"], Number::count(5));
    assert_eq!(
        reading.numbers["items"],
        Number::count(4),
        "the fixture's items are not counted"
    );
}

/// Two `avoid_from` rules whose scopes cover one crate are refused by
/// name: the store holds one vocab reading per crate.
#[test]
fn two_rules_over_one_crate_are_refused() {
    let dir = fixture("vocab.harness");
    let (_base, root) = scratch_workspace("two", &dir);
    let mut manifest = Manifest::load(&root).unwrap();
    let mut second = Rule::named("vocab.whole", "xtask");
    second.predicate.deny = None;
    second.predicate.avoid_from = Some(AvoidFrom::File("CONTEXT.md".to_owned()));
    manifest.rules.push(second);
    let err = vocab::hold(&root, &manifest, &[]).unwrap_err().to_string();
    assert!(
        err.contains("2 rules") && err.contains("vocab.harness, vocab.whole"),
        "{err}"
    );
}

/// The tracked manifest with the row a person adds (#63): a second
/// `avoid_from` rule over `xtask` that reads the glossary's Harness section
/// alone, built through the constructors while no tracked rule covers the
/// harness. Once one does this adds nothing.
fn with_the_harness_rule(mut manifest: Manifest) -> Manifest {
    let covered = manifest
        .rules
        .iter()
        .any(|r| r.predicate.avoid_from.is_some() && r.covers(Path::new("xtask")));
    if !covered {
        let mut rule = Rule::named("vocab.harness", "xtask");
        rule.predicate.deny = None;
        rule.predicate.avoid_from = Some(AvoidFrom::Section {
            file: "CONTEXT.md".to_owned(),
            section: "Harness".to_owned(),
        });
        "The Harness section's _Avoid_ words appear in no public item name or rustdoc prose of the harness."
            .clone_into(&mut rule.text);
        manifest.rules.push(rule);
    }
    manifest
}

/// The tests whose subject is the tracked workspace itself: each reads the
/// `seams.toml`, `oracle.toml`, registry or sources standing in the
/// workspace, so a person's edit there is what may turn one red (#3091).
/// Every other test here builds the manifest it judges.
mod tracked {
    use super::*;

    /// Every rule `vocab` evaluates (the manifest's `avoid_from` rules, harness
    /// §4.1) has a negative-control fixture under `fixtures/rules/<id>/` that
    /// must fail under the instrument: judged by the manifest the fixture
    /// carries when it carries one (#57), and standing as `crates/php_ext`
    /// under the tracked manifest otherwise. The rules of the other
    /// instruments are theirs to control.
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
            let name = format!("rule-{}", rule.id);
            let readings = if dir.join("seams.toml").is_file() {
                fixture_readings(&name, &dir)
            } else {
                let root = scratch(&name, "crates/php_ext", &dir);
                vocab::hold(
                    &root,
                    &Manifest::load(&root).unwrap(),
                    &["php_ext".to_owned()],
                )
                .unwrap()
            };
            assert!(
                readings
                    .iter()
                    .any(|r| r.status == Status::Fail && r.rule == rule.id),
                "the fixture for {} passes: {readings:#?}",
                rule.id
            );
        }
    }

    /// The harness held to the glossary's Harness section alone (#63): under
    /// the row `with_the_harness_rule` builds, `vocab` over `xtask` is one
    /// passing reading of rule `vocab.harness`, its rustdoc prose read and
    /// clean, and its public items clean, the names that are a grammar's
    /// included: the instrument `api-check`'s module, the manifest's `check`,
    /// `tool` and `[telemetry]` keys, the pin's `script_name`, which is php's,
    /// and the hook's `tool_name` and `tool_input` are the spellings the
    /// Harness section's parentheticals mark as code.
    #[test]
    fn the_harness_reads_clean_under_the_harness_section() {
        let manifest = with_the_harness_rule(Manifest::load(workspace()).unwrap());
        let readings = vocab::hold(workspace(), &manifest, &["xtask".to_owned()]).unwrap();
        assert_eq!(readings.len(), 1, "{readings:#?}");
        let reading = &readings[0];
        assert_eq!(reading.scope, "xtask");
        assert_eq!(reading.rule, "vocab.harness");
        let count = |n: &str| reading.numbers[n].to_string().parse::<i64>().unwrap();
        assert!(
            count("items") > 100 && count("prose") > 1000,
            "{reading:#?}"
        );
        // Each finding less its `file:line:` head, which moves with the sources.
        let found: Vec<&str> = findings(reading)
            .into_iter()
            .filter_map(|f| f.split_once(": ").map(|(_, rest)| rest))
            .collect();
        let prose: Vec<&&str> = found
            .iter()
            .filter(|f| f.contains("in rustdoc prose"))
            .collect();
        assert!(prose.is_empty(), "{prose:#?}");
        assert!(found.is_empty(), "{}", reading.message);
        assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    }
}

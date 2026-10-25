//! `xtask test` (harness-design.md §4.5 condition 1, #191): the crate's
//! own tests as a reading. The negative control is the fixture at
//! `xtask/fixtures/test/failing`, one crate with two tests of which one
//! fails; it is run through the instrument's library entry, through the
//! binary, and then through `done` over a scratch repository, which is
//! the reading the ticket is for: a crate with a failing test is not done.
#![cfg(tracked)]

use std::path::{Path, PathBuf};

use xtask::divergence::Registry;
use xtask::done;
use xtask::manifest::Manifest;
use xtask::oracle::SKIPPED_VARIABLE;
use xtask::reading::{Number, Reading, Status, Store, epoch_seconds};
use xtask::scratch::Tree;
use xtask::test;

mod common {
    pub mod manifest;
    pub mod repo;
    pub mod run;
}
use common::manifest::Tracked;
use common::repo::Repo;
use common::run::{Run, xtask};

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

fn fixture() -> PathBuf {
    let dir = xtask::package_dir().join("fixtures/test/failing");
    assert!(dir.is_dir(), "no fixture at {}", dir.display());
    dir
}

fn int(reading: &Reading, key: &str) -> i64 {
    match reading.numbers.get(key) {
        Some(Number::Int(n)) => *n,
        other => panic!("{key}: {other:?}"),
    }
}

/// The fixture's one reading: `fail`, with the passed test and the failed
/// one counted and the failed one named.
fn the_failing_reading(readings: &[Reading]) -> &Reading {
    assert_eq!(readings.len(), 1, "{readings:#?}");
    let reading = &readings[0];
    assert_eq!(reading.instrument, "test");
    assert_eq!(reading.scope, "php_heap");
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(int(reading, "passed"), 1, "{reading:#?}");
    assert_eq!(int(reading, "failed"), 1, "{reading:#?}");
    assert_eq!(int(reading, "ignored"), 0, "{reading:#?}");
    assert_eq!(int(reading, "filtered"), 0, "{reading:#?}");
    assert!(
        reading.message.starts_with("php_heap: 1 test failed\n"),
        "{}",
        reading.message
    );
    assert!(
        reading.message.contains("tests::one_is_two"),
        "the failing test is named: {}",
        reading.message
    );
    // What the panic said is kept where the log keeps the reading, so a
    // failure under the pre-commit hook says itself there (#976).
    assert!(
        reading
            .message
            .contains("the failing test the fixture is for"),
        "the panic's own message is kept: {}",
        reading.message
    );
    reading
}

#[test]
fn a_crate_with_a_failing_test_reads_fail_with_the_passed_and_the_failed_counted() {
    let root = repository("test-library");
    let manifest = Manifest::load(&root).unwrap();
    let readings = test::run(&root, &manifest, &[]).unwrap();
    the_failing_reading(&readings);
}

#[test]
fn the_binary_writes_the_reading_and_exits_nonzero_on_a_failure() {
    let root = repository("test-binary");
    let ran = xtask(&root, &["test", "--crate", "php_heap"]);
    assert_ne!(ran.code(), 0, "{ran}");
    let stdout = ran.stdout;
    assert!(
        stdout.starts_with("fail test php_heap: php_heap: 1 test failed"),
        "{stdout}"
    );
    let store = Store::of_workspace(root.path());
    let reading = store.read("test", "php_heap").unwrap().unwrap();
    the_failing_reading(std::slice::from_ref(&reading));
}

/// A crate the manifest does not name is refused before anything runs.
#[test]
fn a_crate_the_manifest_does_not_name_is_refused() {
    let root = fixture();
    let manifest = Manifest::load(&root).unwrap();
    let err = test::run(&root, &manifest, &["php_engine".to_owned()]).unwrap_err();
    assert!(
        err.to_string().contains("no crate named \"php_engine\""),
        "{err}"
    );
}

/// The fixture copied into a scratch tree of its own, with the tracked
/// pin and a registry with no row beside its manifest, so two tests running at once
/// build in two `target/` directories and neither removes the other's
/// mid-run; the tree takes itself away (#194). The instrument reads a
/// crate's files through `git ls-files`, so a test runs it over
/// [`repository`], never over the tracked fixture.
fn scratch(name: &str) -> Tree {
    let root = Tree::new(name).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    std::fs::create_dir_all(root.join("crates/php_heap/src")).unwrap();
    std::fs::copy(workspace().join("oracle.toml"), root.join("oracle.toml")).unwrap();
    std::fs::copy(
        xtask::package_dir().join("fixtures/registry/empty.toml"),
        root.join("docs/divergence.toml"),
    )
    .unwrap();
    let fixture = fixture();
    for file in [
        "seams.toml",
        "Cargo.toml",
        "Cargo.lock",
        "crates/php_heap/Cargo.toml",
        "crates/php_heap/src/lib.rs",
    ] {
        std::fs::copy(fixture.join(file), root.join(file)).unwrap();
    }
    // The fixture's pin is by a path relative to the fixture; here the
    // pin stands beside the manifest, and what `done` asks of a crate is
    // the workspace's `[[owed]]` rows (#3206), those whose every pattern
    // names `php_heap`, the one crate here, as the manifest's reader holds.
    let text = std::fs::read_to_string(workspace().join("seams.toml")).unwrap();
    let rows = text.parse::<toml::Table>().unwrap()["owed"].clone();
    let heap = |pattern: &toml::Value| matches!(pattern.as_str(), Some("*" | "php_heap"));
    let names_heap = |row: &&toml::Table| {
        let patterns = |key: &str| match row.get(key) {
            Some(toml::Value::Array(list)) => list.iter().all(heap),
            Some(one) => heap(one),
            None => true,
        };
        patterns("crates") && patterns("exempt")
    };
    let rows = rows
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row.as_table().unwrap());
    let rows = rows.filter(names_heap);
    let tracked = Tracked::of(&root).with_pin(Path::new("oracle.toml"));
    rows.fold(tracked, |tracked, row| {
        tracked.with_row("owed", row.clone())
    })
    .write(&root);
    root
}

/// The scratch tree as a repository, committed once, so an edit of the
/// crate after is the diff `done` judges with `--base HEAD`.
fn repository(name: &str) -> Tree {
    committed(scratch(name))
}

/// `root` committed once.
fn committed(root: Tree) -> Tree {
    std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
    Repo::init(&root).commit("scratch");
    root
}

/// The reading `done` reads: over a crate whose tests do not all pass,
/// with every other reading condition 1 asks fresh and passing, the one
/// unmet condition is the `test` reading, with the instrument's message.
/// Fixing the test and measuring again clears it.
#[test]
fn done_is_red_for_a_crate_with_a_failing_test_and_green_once_it_passes() {
    let root = repository("test-done");
    let lib = root.join("crates/php_heap/src/lib.rs");
    let text = std::fs::read_to_string(&lib).unwrap();
    // A change, so the crate is in the diff.
    std::fs::write(
        &lib,
        format!("{text}\n/// Changed.\npub fn two() -> u8 {{ 2 }}\n"),
    )
    .unwrap();

    let manifest = Manifest::load(&root).unwrap();
    let store = Store::of_workspace(&root);
    let judge = || judged(&root, &manifest, &store);
    let fresh_rest = || fresh_but_test(&root, &manifest, &store);

    fresh_rest();
    let readings = test::run(&root, &manifest, &[]).unwrap();
    the_failing_reading(&readings);
    store.record(&readings).unwrap();
    let verdict = judge();
    assert_eq!(verdict.crates, ["php_heap"], "{verdict:#?}");
    assert_eq!(verdict.unmet.len(), 1, "{verdict:#?}");
    assert!(
        verdict.unmet[0].starts_with("test: php_heap fail: php_heap: 1 test failed"),
        "{verdict:#?}"
    );

    // The test is fixed; the readings are stale until measured again, and
    // measured again the crate is done.
    let text = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(
        &lib,
        text.replace("assert_eq!(super::one(), 2", "assert_eq!(super::one(), 1"),
    )
    .unwrap();
    let verdict = judge();
    assert!(
        verdict.unmet.contains(
            &"test: the reading for php_heap is stale; php_heap changed since it was taken"
                .to_owned()
        ),
        "{verdict:#?}"
    );
    fresh_rest();
    let readings = test::run(&root, &manifest, &[]).unwrap();
    assert_eq!(readings[0].status, Status::Pass, "{readings:#?}");
    assert_eq!(int(&readings[0], "passed"), 2, "{readings:#?}");
    assert_eq!(readings[0].message, "php_heap: 2 tests passed");
    store.record(&readings).unwrap();
    let verdict = judge();
    assert!(verdict.met(), "{verdict:#?}");
}

/// What `done` says of `root`'s changes since `HEAD`.
fn judged(root: &Path, manifest: &Manifest, store: &Store) -> done::Verdict {
    let registry = Registry::load(root).expect("the registry");
    let snapshot = xtask::key::Snapshot::disk(root).expect("the tree");
    done::judge(&snapshot, manifest, &registry, store, Some("HEAD"), &[]).expect("the verdict")
}

/// A fresh passing reading of `php_heap` for each instrument condition 1
/// asks but `test`, so the test reading is the one `done` judges.
fn fresh_but_test(root: &Path, manifest: &Manifest, store: &Store) {
    for row in &manifest.owed {
        let instrument = row.instrument.name();
        if instrument == "test" || instrument == "phpt" || instrument == "port-map" {
            continue;
        }
        let snapshot = xtask::key::Snapshot::disk(root).expect("the tree");
        let mut reading =
            Reading::for_scope(instrument, "php_heap", manifest, &snapshot).expect("a reading");
        if xtask::reading::UNDER_RECIPE.contains(&instrument) {
            reading = reading.under(manifest.pin().unwrap());
        }
        reading.status = Status::Pass;
        reading.message = String::from("php_heap: measured");
        store.record(&[reading]).expect("the reading is recorded");
    }
}

/// The fixture's crate with three tests, two of which skip for a fact
/// the machine did not promise, one of them twice, each line written to
/// the file the variable names as `Promise::or_skip` and
/// `MysqlServer::with` write it.
const SKIPS: &str = r#"#[cfg(test)]
mod tests {
    fn skip() {
        use std::io::Write as _;
        let file = std::env::var_os("VARIABLE").expect("the file xtask test names");
        let test = std::thread::current().name().expect("libtest's name").to_owned();
        let mut out = std::fs::OpenOptions::new().create(true).append(true).open(file).expect("the file");
        out.write_all(format!("{test}\tmysql\n").as_bytes()).expect("the skip");
    }

    #[test]
    fn a() {
        skip();
        skip();
    }

    #[test]
    fn b() {
        skip();
    }

    #[test]
    fn c() {}
}
"#;

/// A reading counts the tests that skipped for a fact the machine did
/// not promise, each once, says how many in its message, and `done`
/// notes the number beside the passing reading (#3085).
#[test]
fn a_reading_counts_the_tests_that_skipped_and_done_notes_them() {
    let root = scratch("test-skipped");
    let lib = root.join("crates/php_heap/src/lib.rs");
    std::fs::write(&lib, SKIPS.replace("VARIABLE", SKIPPED_VARIABLE)).expect("the tests that skip");
    let root = committed(root);
    std::fs::write(
        &lib,
        format!("{SKIPS}\n/// Changed.\npub fn two() {{}}\n").replace("VARIABLE", SKIPPED_VARIABLE),
    )
    .expect("the crate changed");
    let manifest = Manifest::load(&root).expect("the manifest");
    let store = Store::of_workspace(&root);
    fresh_but_test(&root, &manifest, &store);
    let readings = test::run(&root, &manifest, &[]).expect("the readings");
    assert_eq!(readings[0].status, Status::Pass, "{readings:#?}");
    assert_eq!(int(&readings[0], "passed"), 3, "{readings:#?}");
    assert_eq!(int(&readings[0], "skipped_unpromised"), 2, "{readings:#?}");
    assert_eq!(
        readings[0].message,
        "php_heap: 3 tests passed, 2 of them skipped for a fact this machine did not promise"
    );
    store.record(&readings).expect("the readings are recorded");
    let verdict = judged(&root, &manifest, &store);
    assert!(verdict.met(), "{verdict:#?}");
    assert!(
        verdict.notes.contains(
            &"test: php_heap passed with 2 tests skipped for a fact this machine did not promise"
                .to_owned()
        ),
        "{verdict:#?}"
    );
}

/// An integration test of the fixture's crate that writes when it ran,
/// in milliseconds, to the scratch directory the crate's integration
/// tests share, `CARGO_TARGET_TMPDIR`, as `<NAME>.span`.
const SPAN: &str = r#"#[test]
fn spans() {
    let at = || std::time::UNIX_EPOCH.elapsed().unwrap().as_millis();
    let from = at();
    std::thread::sleep(std::time::Duration::from_millis(700));
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("NAME.span"), format!("{from} {}", at())).unwrap();
}
"#;

/// `xtask test` over the fixture's crate with three integration tests,
/// `a`, `b` and `c`, its `Cargo.toml` ending in `declared`, at most three
/// executables at once: the reading, and when each test ran, from and to.
fn spans(name: &str, declared: &str) -> (Reading, [(u128, u128); 3]) {
    let root = scratch(name);
    let heap = root.join("crates/php_heap");
    std::fs::write(heap.join("src/lib.rs"), "//! Nothing to test.\n").unwrap();
    std::fs::create_dir_all(heap.join("tests")).unwrap();
    for test in ["a", "b", "c"] {
        std::fs::write(
            heap.join(format!("tests/{test}.rs")),
            SPAN.replace("NAME", test),
        )
        .unwrap();
    }
    let toml = std::fs::read_to_string(heap.join("Cargo.toml")).unwrap();
    std::fs::write(heap.join("Cargo.toml"), format!("{toml}{declared}")).unwrap();
    let root = committed(root);
    Run::at(&root)
        .args(["test", "--crate", "php_heap"])
        .env("CARGO_BUILD_JOBS", "3")
        .go()
        .ok();
    let reading = Store::of_workspace(root.path())
        .read("test", "php_heap")
        .unwrap()
        .unwrap();
    assert_eq!(int(&reading, "passed"), 3, "{reading:#?}");
    let span = |test: &str| {
        let text = std::fs::read_to_string(root.join(format!("target/tmp/{test}.span"))).unwrap();
        let (from, to) = text.split_once(' ').unwrap();
        (from.parse().unwrap(), to.parse().unwrap())
    };
    (reading, [span("a"), span("b"), span("c")])
}

fn overlap((a_from, a_to): (u128, u128), (b_from, b_to): (u128, u128)) -> bool {
    a_from < b_to && b_from < a_to
}

/// A crate's test executables run one at a time unless its `Cargo.toml`
/// says they share nothing; then they run side by side, but for those
/// that hold one key apart, which never overlap. The reading is stamped
/// once the last has ended, `ms` says how long they ran, and a group per
/// executable how long each did (#2869).
#[test]
fn executables_run_side_by_side_where_declared_and_apart_where_they_share_a_key() {
    let declared = "\n[package.metadata.xtask.test]\nexecutables = \"side-by-side\"\n\
                    apart = { a = [\"shared\"], b = [\"shared\"] }\n";
    let (reading, [a, b, c]) = spans("test-beside", declared);
    assert!(!overlap(a, b), "a and b hold one key: {a:?} {b:?}");
    assert!(
        overlap(a, c) || overlap(b, c),
        "c runs beside one: {a:?} {b:?} {c:?}"
    );
    assert!(int(&reading, "ms") >= 1400, "{reading:#?}");
    for test in ["test a", "test b", "test c"] {
        let ms = reading.groups.get(test).map(|ms| ms.as_f64());
        assert!(
            ms.is_some_and(|ms| ms >= 700.0),
            "each one's time: {reading:#?}"
        );
    }
    let ended = a.1.max(b.1).max(c.1) / 1000;
    assert!(
        epoch_seconds(&reading.at).unwrap() >= i64::try_from(ended).unwrap(),
        "{reading:#?}"
    );

    let (reading, [a, b, c]) = spans("test-one-at-a-time", "");
    assert!(
        !overlap(a, b) && !overlap(a, c) && !overlap(b, c),
        "{a:?} {b:?} {c:?}"
    );
    assert!(int(&reading, "ms") >= 2100, "{reading:#?}");
}

/// Under `--staged` an executable the crate names in `own` runs only
/// where a path of its own is staged, and a pass with one left out is not
/// recorded, since `done` asks the whole crate's run (#2982); staged, it
/// runs, and the reading is the whole crate's.
#[test]
fn staged_leaves_out_an_executable_whose_own_paths_are_not_staged() {
    let root = scratch("test-own");
    let heap = root.join("crates/php_heap");
    std::fs::write(heap.join("src/lib.rs"), "//! Nothing to test.\n").unwrap();
    std::fs::create_dir_all(heap.join("tests")).unwrap();
    for test in ["a", "b"] {
        let body = SPAN.replace("NAME", test);
        std::fs::write(heap.join(format!("tests/{test}.rs")), body).unwrap();
    }
    let declared = "\n[package.metadata.xtask.test]\nexecutables = \"side-by-side\"\n\
                    own = { b = [\"tests/b.rs\"] }\n";
    let toml = std::fs::read_to_string(heap.join("Cargo.toml")).unwrap();
    std::fs::write(heap.join("Cargo.toml"), format!("{toml}{declared}")).unwrap();
    let root = committed(root);
    let staged = |path: &str, line: &str| {
        let file = heap.join(path);
        let text = std::fs::read_to_string(&file).unwrap();
        std::fs::write(&file, format!("{text}{line}\n")).unwrap();
        Repo::at(&root).git(&["add", "--", &format!("crates/php_heap/{path}")]);
        xtask(&root, &["test", "--staged"]).ok()
    };
    let ran = |test: &str| root.join(format!("target/tmp/{test}.span")).exists();
    let read = || {
        Store::of_workspace(root.path())
            .read("test", "php_heap")
            .unwrap()
    };

    let said = staged("src/lib.rs", "//! Changed.");
    assert!(said.contains("under --staged b left out"), "{said}");
    assert!(ran("a") && !ran("b"));
    assert!(read().is_none(), "{said}");

    let said = staged("tests/b.rs", "// Changed.");
    assert!(ran("b"), "{said}");
    let reading = read().unwrap();
    assert_eq!(int(&reading, "passed"), 2, "{reading:#?}");
}

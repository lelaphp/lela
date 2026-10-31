//! `xtask done`, the definition of done (harness-design.md §4.5, §13 D.16,
//! D.18): the six conditions over a scratch repository, each provoked by a
//! fixture and read back from the verdict; the Stop hook shapes; and the
//! wiring of the Stop hook (the two git hooks run in `tests/hooks.rs`).
//! The instruments whose readings `done` reads have their own fixtures
//! beside them; here a reading is
//! written by hand into the store, since what is under test is how `done`
//! reads it, not how it was measured.
#![cfg(tracked)]

use std::path::{Path, PathBuf};

use xtask::divergence::Registry;
use xtask::done::Base;
use xtask::done::{self, Verdict};
use xtask::manifest::Manifest;
use xtask::oracle::Promise;
use xtask::reading::{Reading, Status, Store, UNDER_RECIPE};
use xtask::scratch::Tree;

mod common {
    pub mod manifest;
    pub mod registry;
    pub mod repo;
    pub mod run;
    pub mod tracker;
}
use common::manifest::Tracked;
use common::registry::Row;
use common::repo::Repo;
use common::run::{Ran, Run, xtask};
use common::tracker::{Tracker, issue};

/// What condition 1 asks of `ext/pcre`, the crate most of these tests
/// change: the manifest's `[[owed]]` instruments less `port-map`, which is
/// asked of the crate
/// `api.lock` lives with and of no other (§4.5 condition 1, #148), less
/// `miri`, asked of `php_heap` alone (#2416), and
/// with `crosscheck`, asked of a default extension (#269) at both its
/// scopes, the crate's name and `ext/<name>/ri` (#361). That this is
/// the rows less those two is asserted in
/// `the_crate_the_lock_lives_with_is_asked_for_a_port_map_reading_and_no_other_crate_is`.
const ASKED: [&str; 8] = [
    "clippy",
    "test",
    "witness",
    "api-check",
    "budget",
    "unsafe-audit",
    "phpt",
    "crosscheck",
];

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

fn fixtures() -> PathBuf {
    xtask::package_dir().join("fixtures")
}

fn read(path: &str) -> String {
    std::fs::read_to_string(workspace().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, text).unwrap();
}

/// A scratch repository: the tracked manifest and pin, a registry with no
/// row, and the fixture's two crates, `ext/pcre` and `crates/php_ext`,
/// committed once. Every change the tests make after this is the diff
/// `done` judges with `--base HEAD`. The manifest is the tracked file byte
/// for byte, since `done --hook` refuses one its build was not compiled
/// against; a test that needs a row otherwise builds it with [`Tracked`]
/// and judges without `--hook`.
fn scratch(name: &str) -> Tree {
    repository(name, &fixtures().join("registry/empty.toml"))
}

/// The same, with the fixture's registry: one open row on ext/pcre's phpt
/// subset, committed in the base so the registry itself is not in the diff.
fn scratch_with_an_open_row(name: &str) -> Tree {
    repository(name, &fixtures().join("done/divergence.toml"))
}

/// The branch is `main` by name and not by default: `git init` takes it
/// from `init.defaultBranch`, which is `main` on a workstation that set it
/// and `master` on a runner that did not, and a scratch repository on
/// `master` has no `main` to take a merge base with. The tests that pass
/// no `--base` read the branch this line pins.
fn repository(name: &str, registry: &Path) -> Tree {
    let root = Tree::new(&format!("done-{name}")).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    for file in ["seams.toml", "oracle.toml"] {
        std::fs::copy(workspace().join(file), root.join(file)).unwrap();
    }
    std::fs::copy(registry, root.join("docs/divergence.toml")).unwrap();
    let fixture = fixtures().join("done");
    for file in ["Cargo.toml", "src/lib.rs"] {
        let text = std::fs::read_to_string(fixture.join("ext/pcre").join(file)).unwrap();
        write(&root, &format!("ext/pcre/{file}"), &text);
    }
    // The crate `api.lock` lives with, with the lock in its tree: the one
    // crate condition 1 asks for a `port-map` reading (§4.5, #148).
    for file in ["Cargo.toml", "src/lib.rs", "api.lock"] {
        let text = std::fs::read_to_string(fixture.join("crates/php_ext").join(file)).unwrap();
        write(&root, &format!("crates/php_ext/{file}"), &text);
    }
    write(&root, ".gitignore", "target/\n");
    Repo::init(&root).commit("scratch");
    root
}

fn judge(root: &Path) -> Verdict {
    judge_since(root, Some("HEAD"))
}

fn judge_since(root: &Path, base: Option<&str>) -> Verdict {
    let manifest = Manifest::load(root).unwrap();
    let registry = Registry::load(root).unwrap();
    let store = Store::of_workspace(root);
    done::judge(
        &xtask::key::Snapshot::disk(root).unwrap(),
        &manifest,
        &registry,
        &store,
        base,
        &[],
    )
    .unwrap()
}

/// `done --hook` over `root` with `event` on stdin, as the Stop hook runs
/// it, the stopping session and agent having written in `root` as permit
/// records it (#3619).
fn hook(root: &Path, event: &str) -> Ran {
    if let Ok(stop) = serde_json::from_str::<serde_json::Value>(event) {
        let session = stop["session_id"].as_str().unwrap_or_default();
        wrote_in(root, root, session, stop["agent_id"].as_str());
    }
    stop_hook(root, event)
}

/// `done --hook` over `root` with `event` on stdin, and nothing recorded.
fn stop_hook(root: &Path, event: &str) -> Ran {
    Run::at(root).args(["done", "--hook"]).stdin(event).go()
}

/// A writing call of `agent` of `session` in `checkout`, admitted by the
/// permit of `root`, which records where it wrote.
fn wrote_in(root: &Path, checkout: &Path, session: &str, agent: Option<&str>) {
    let call = serde_json::json!({"session_id": session, "agent_id": agent, "tool_name": "Bash",
        "cwd": checkout.to_str().unwrap(), "tool_input": {"command": "touch written"}});
    // A build that cannot judge records nothing, as the hook's would not.
    Run::at(root).args(["permit"]).stdin(call.to_string()).go();
}

/// A passing reading of `instrument` over `scope`, stamped through the
/// constructor with the scope's tree now.
fn fresh(root: &Path, instrument: &str, scope: &str) -> Reading {
    let manifest = Manifest::load(root).unwrap();
    let beside = scope
        .strip_suffix("/ri")
        .or_else(|| scope.strip_suffix("/release"));
    let mut reading = match beside {
        // The `--ri` column's scope and the release lint's map to no
        // crate; each is stamped over the crate's tree as `crosscheck`
        // and `clippy` stamp it (#361, #2392).
        Some(name) => {
            let krate = manifest.crate_named(name).unwrap();
            Reading::stamped_over(
                instrument,
                scope,
                &xtask::key::Snapshot::disk(root).unwrap(),
                &krate.path(),
            )
            .unwrap()
        }
        None => Reading::for_scope(
            instrument,
            scope,
            &manifest,
            &xtask::key::Snapshot::disk(root).unwrap(),
        )
        .unwrap(),
    };
    if UNDER_RECIPE.contains(&instrument) {
        reading = reading.under(manifest.pin().unwrap());
    }
    reading.status = Status::Pass;
    reading.message = format!("{scope}: measured");
    reading.commit = "0000000".into();
    reading
}

/// What condition 1 reads for `name` at `root`, instrument by scope:
/// every row of [`ASKED`] at the crate's name, and `crosscheck` at
/// `ext/<name>/ri` too (`done::scopes_of`, ledger 46, #361).
fn asked_of(root: &Path, name: &str) -> Vec<(&'static str, String)> {
    let manifest = Manifest::load(root).unwrap();
    let krate = manifest.crate_named(name).unwrap();
    ASKED
        .into_iter()
        .flat_map(|instrument| {
            done::scopes_of(root, instrument, krate)
                .unwrap()
                .into_iter()
                .map(move |scope| (instrument, scope))
        })
        .collect()
}

/// What meets everything `name` is asked ([`met`]).
fn record_asked(store: &Store, root: &Path, name: &str) {
    for (instrument, scope) in asked_of(root, name) {
        met(store, root, instrument, &scope);
    }
}

/// What meets one row of [`asked_of`]: a fresh passing reading, or for
/// `phpt` a passing run kept at the base, `HEAD`, and one at the tree now,
/// which condition 1 diffs in place of a reading (#725).
fn met(store: &Store, root: &Path, instrument: &str, scope: &str) {
    if instrument == "phpt" {
        keep_run(root, Some("HEAD"), scope, "pass");
        keep_run(root, None, scope, "pass");
    } else {
        store.record(&[fresh(root, instrument, scope)]).unwrap();
    }
}

/// The line condition 1 gives a subset with no run kept at the tree now.
fn unrun(scope: &str) -> impl Fn(&String) -> bool {
    let head = format!("phpt: no run of {scope} is kept at this tree's engine (");
    move |line| line.starts_with(&head)
}

/// The crate directories of the manifest at `root`.
fn layout(root: &Path) -> xtask::manifest::Layout {
    xtask::manifest::Manifest::load(root).unwrap().layout()
}

/// A run of `scope` holding one test at `verdict`, or none where it is
/// empty, kept at `at`'s engine tree, the tree now where `at` is `None`,
/// as `xtask phpt` keeps one.
fn keep_run(root: &Path, at: Option<&str>, scope: &str, verdict: &str) {
    let snapshot = match at {
        Some(rev) => {
            let commit = Repo::at(root).git(&["rev-parse", rev]);
            xtask::key::Snapshot::commit(root, commit.trim()).unwrap()
        }
        None => std::sync::Arc::new(xtask::key::Snapshot::disk(root).unwrap()),
    };
    let file = xtask::phpt::kept_run(&snapshot, &layout(root), scope).unwrap();
    let mut verdicts = serde_json::Map::new();
    if !verdict.is_empty() {
        verdicts.insert(format!("phpt:php-src/{scope}/tests/a.phpt"), verdict.into());
    }
    let recipe = Manifest::load(root)
        .unwrap()
        .pin()
        .unwrap()
        .digest()
        .to_owned();
    let run = serde_json::json!({"commit": "0", "corpus": "c", "engine": "e", "recipe": recipe, "verdicts": verdicts});
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, run.to_string()).unwrap();
}

// ---------------------------------------------------------------- 1, 2

/// Over a diff in xtask, the harness's own sources, witness and phpt are
/// not on the list, and the five that are pass when fresh; which five is
/// `done`'s own unit test. `test` is among them: the harness has tests
/// as every crate has (#191).
#[test]
fn the_harness_crate_is_asked_for_clippy_test_api_check_budget_and_unsafe_audit_only() {
    let root = scratch("harness");
    write(
        &root,
        "xtask/Cargo.toml",
        "[package]\nname = \"xtask\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    );
    write(&root, "xtask/src/lib.rs", "pub fn measure() {}\n");
    let verdict = judge(&root);
    assert!(
        verdict
            .unmet
            .iter()
            .all(|u| !u.starts_with("witness") && !u.starts_with("phpt")),
        "{verdict:#?}"
    );
    assert_eq!(verdict.unmet.len(), 5, "{verdict:#?}");
    let store = Store::of_workspace(&root);
    for instrument in ["clippy", "test", "api-check", "budget", "unsafe-audit"] {
        store.record(&[fresh(&root, instrument, "xtask")]).unwrap();
    }
    assert!(judge(&root).unmet.is_empty(), "{:#?}", judge(&root));
}

/// A commit that changes `seams.toml` alone touches xtask, whose tests
/// copy the manifest (`key::READ_BY_TESTS`, #3646): its `test` reading,
/// taken before, no longer stands, and `done` names the file.
#[test]
fn a_change_to_the_manifest_alone_stales_the_harness_test_reading() {
    let root = scratch("read-by-tests");
    let package = "[package]\nname = \"xtask\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";
    write(&root, "xtask/Cargo.toml", package);
    write(&root, "xtask/src/lib.rs", "pub fn measure() {}\n");
    Repo::at(&root).commit("the harness");
    let store = Store::of_workspace(&root);
    store.record(&[fresh(&root, "test", "xtask")]).unwrap();
    assert!(judge(&root).crates.is_empty(), "{:#?}", judge(&root));

    write(
        &root,
        "seams.toml",
        &format!("{}# moved\n", read("seams.toml")),
    );
    Repo::at(&root).commit("a steer");
    let verdict = judge_since(&root, Some("HEAD~1"));
    assert_eq!(verdict.crates, ["xtask"]);
    let stale =
        "test: the reading for xtask is stale; xtask or seams.toml changed since it was taken";
    assert!(
        verdict.unmet.iter().any(|u| u.starts_with(stale)),
        "{verdict:#?}"
    );
}

/// A change to `ops.toml` alone, which `php_engine`'s `build.rs` reads
/// (`key::READ_BY_BUILDS`, #3654), touches `php_engine` and stales its
/// reading, naming the file, and stales the reading of `ext/reflection`,
/// built from it, though nothing under either crate changed.
#[test]
fn a_change_to_a_file_read_at_build_stales_the_crates_built_from_it() {
    let root = Tree::new("done-read-by-builds").unwrap();
    xtask::scratch::repository(&fixtures().join("closure"), &root, &["."]).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    for file in ["seams.toml", "oracle.toml", "ops.toml"] {
        std::fs::copy(workspace().join(file), root.join(file)).unwrap();
    }
    let registry = fixtures().join("registry/empty.toml");
    std::fs::copy(registry, root.join("docs/divergence.toml")).unwrap();
    write(&root, ".gitignore", "target/\n");
    write(
        &root,
        "ext/reflection/src/lib.rs",
        "pub fn reflection() { }\n",
    );
    Repo::at(&root).commit("the harness's own files and the opcodes");
    let store = Store::of_workspace(&root);
    let readings = [("test", "php_engine"), ("witness", "ext/reflection")];
    for (instrument, scope) in readings {
        store.record(&[fresh(&root, instrument, scope)]).unwrap();
    }
    let stands = |u: &String| !u.contains("is stale");
    assert!(judge_since(&root, Some("HEAD~1")).unmet.iter().all(stands));

    write(&root, "ops.toml", &format!("{}# moved\n", read("ops.toml")));
    let verdict = judge(&root);
    assert_eq!(verdict.crates, ["php_engine"]);
    let engine = "test: the reading for php_engine is stale; php_engine or ops.toml changed";
    assert!(
        verdict.unmet.iter().any(|u| u.starts_with(engine)),
        "{verdict:#?}"
    );
    let verdict = judge_since(&root, Some("HEAD~1"));
    let built = "witness: the reading for ext/reflection is stale";
    assert!(
        verdict.unmet.iter().any(|u| u.starts_with(built)),
        "{verdict:#?}"
    );
}

#[test]
fn with_no_readings_every_instrument_of_the_touched_crate_is_unmet_and_the_exit_is_1() {
    let root = scratch("none");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let verdict = judge(&root);
    assert_eq!(verdict.crates, ["ext/pcre"]);
    assert_eq!(verdict.changed.len(), 1, "{verdict:#?}");
    for (instrument, scope) in asked_of(&root, "ext/pcre") {
        let line = format!("{instrument}: no reading for {scope}");
        let unmet = if instrument == "phpt" {
            verdict.unmet.iter().any(unrun(&scope))
        } else {
            verdict.unmet.contains(&line)
        };
        assert!(unmet, "{line}\n{verdict:#?}");
    }
    assert_eq!(
        verdict.unmet.len(),
        asked_of(&root, "ext/pcre").len(),
        "{verdict:#?}"
    );
    assert!(verdict.notes.is_empty(), "{verdict:#?}");

    let ran = xtask(&root, &["done", "--base", "HEAD"]);
    assert_eq!(ran.code(), 1, "{ran}");
    let out = ran.stdout;
    let err = ran.stderr;
    assert!(out.contains("unmet:"), "{out}");
    assert!(out.contains("- phpt: no run of ext/pcre is kept"), "{out}");
    assert!(
        out.contains("- crosscheck: no reading for ext/pcre\n"),
        "{out}"
    );
    assert!(
        out.contains("- crosscheck: no reading for ext/pcre/ri"),
        "{out}"
    );
    assert!(err.contains("9 conditions of done unmet"), "{err}");
}

/// `port-map` writes exactly one reading, at the crate `api.lock` lives
/// with, so condition 1 asks that crate for one and asks no other crate
/// for a reading nothing would ever write (§4.5 condition 1, #148). The
/// lock is in that crate's tree, which is what makes the ask sharp: an
/// edit of `api.lock` alone stales the reading and the condition says so.
#[test]
fn the_crate_the_lock_lives_with_is_asked_for_a_port_map_reading_and_no_other_crate_is() {
    let manifest = Manifest::load(workspace()).unwrap();
    let asked_of_every_other_crate: Vec<&str> = (manifest.owed.iter())
        .map(|row| row.instrument.name())
        .filter(|i| !matches!(*i, "port-map" | "miri"))
        .collect();
    assert_eq!(asked_of_every_other_crate, ASKED);

    let root = scratch("port-map");
    write(&root, "crates/php_ext/src/lib.rs", "pub fn state() {}\n");
    let verdict = judge(&root);
    assert_eq!(verdict.crates, ["php_ext"], "{verdict:#?}");
    assert!(
        verdict
            .unmet
            .contains(&"port-map: no reading for php_ext".to_owned()),
        "{verdict:#?}"
    );

    // The crate the tests change is not asked, however much it changes.
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let verdict = judge(&root);
    assert!(
        !verdict
            .unmet
            .iter()
            .any(|u| u == "port-map: no reading for ext/pcre"),
        "{verdict:#?}"
    );

    // Fresh readings of everything php_ext is asked, and it is done.
    let store = Store::of_workspace(&root);
    let manifest = Manifest::load(&root).unwrap();
    let pcre = manifest.crate_named("ext/pcre").unwrap();
    for instrument in ASKED.into_iter().chain(["port-map"]) {
        store
            .record(&[fresh(&root, instrument, "php_ext")])
            .unwrap();
        for scope in done::scopes_of(&root, instrument, pcre).unwrap() {
            met(&store, &root, instrument, &scope);
        }
    }
    let verdict = judge(&root);
    assert!(verdict.met(), "{verdict:#?}");

    // The lock is in the crate's tree, so an edit of it alone stales the
    // reading the join is held by.
    write(&root, "crates/php_ext/api.lock", "api::Value\napi::Zval\n");
    let verdict = judge(&root);
    assert!(
        verdict.unmet.iter().any(|u| u
            == "port-map: the reading for php_ext is stale; php_ext changed since it was taken"),
        "{verdict:#?}"
    );
}

/// A reading is keyed on its crate and the crates it is built from
/// (#1318): over `xtask/fixtures/closure`, a pass over `ext/reflection`
/// taken after its own change stands, and a change to `php_engine`, which
/// it is built from through `php_ext`, reports it stale and names both.
#[test]
fn a_dependents_reading_is_stale_once_a_crate_in_its_closure_changes() {
    let root = Tree::new("done-closure").unwrap();
    xtask::scratch::repository(&fixtures().join("closure"), &root, &["."]).unwrap();
    std::fs::create_dir_all(root.join("docs")).unwrap();
    for file in ["seams.toml", "oracle.toml"] {
        std::fs::copy(workspace().join(file), root.join(file)).unwrap();
    }
    std::fs::copy(
        fixtures().join("registry/empty.toml"),
        root.join("docs/divergence.toml"),
    )
    .unwrap();
    write(&root, ".gitignore", "target/\n");
    Repo::at(&root).commit("the harness's own files");
    let stale = "witness: the reading for ext/reflection is stale";

    write(
        &root,
        "ext/reflection/src/lib.rs",
        "pub fn reflection() { }\n",
    );
    let store = Store::of_workspace(&root);
    store
        .record(&[fresh(&root, "witness", "ext/reflection")])
        .unwrap();
    let verdict = judge(&root);
    assert!(
        verdict.unmet.iter().all(|u| !u.starts_with(stale)),
        "{verdict:#?}"
    );

    write(
        &root,
        "crates/php_engine/src/lib.rs",
        "pub fn execute() { }\n",
    );
    let verdict = judge(&root);
    let named = format!("{stale}; ext/reflection or php_engine changed since it was taken");
    assert!(verdict.unmet.contains(&named), "{verdict:#?}");
}

/// Condition 10 (#3372): a diff that changes `.cargo/config.toml` asks for
/// a fresh passing `fold` reading of `sapi/cli`, and one taken before the
/// file changed is stale, since `sapi/cli`'s key covers the files rustc is
/// built under; a failing one is unmet with its message, a skip, a host
/// the sensor cannot read, is met and a note, and a diff that changes none
/// of those files asks for no reading.
#[test]
fn a_change_to_what_rustc_is_built_under_asks_for_a_fold_reading() {
    let root = scratch("fold");
    let asked = |verdict: &Verdict| -> Vec<String> {
        let fold = verdict.unmet.iter().filter(|u| u.starts_with("fold: "));
        fold.cloned().collect()
    };
    write(
        &root,
        "sapi/cli/Cargo.toml",
        "[package]\nname = \"php_sapi_cli\"\nversion = \"0.0.0\"\n\n[dependencies]\nphp_bundle = { path = \"../../crates/php_bundle\" }\n",
    );
    assert_eq!(asked(&judge(&root)), Vec::<String>::new());
    let store = Store::of_workspace(&root);
    store.record(&[fresh(&root, "fold", "sapi/cli")]).unwrap();

    write(
        &root,
        ".cargo/config.toml",
        "[target.'cfg(all())']\nrustflags = []\n",
    );
    assert_eq!(
        asked(&judge(&root)),
        [
            "fold: no reading for sapi/cli over the tree now, and .cargo/config.toml changed; `cargo xtask fold` takes it, building the release sapi/cli first (minutes)"
        ]
    );
    let mut skipped = fresh(&root, "fold", "sapi/cli");
    skipped.status = Status::Skip;
    skipped.message = "sapi/cli: not held; this host is x86_64".into();
    store.record(&[skipped]).unwrap();
    let verdict = judge(&root);
    assert_eq!(asked(&verdict), Vec::<String>::new());
    let note = "fold: sapi/cli not held on this host: sapi/cli: not held; this host is x86_64";
    assert!(verdict.notes.contains(&note.to_owned()), "{verdict:#?}");
    let mut failing = fresh(&root, "fold", "sapi/cli");
    failing.status = Status::Fail;
    failing.message = "sapi/cli: hoisted".into();
    store.record(&[failing]).unwrap();
    assert_eq!(
        asked(&judge(&root)),
        ["fold: sapi/cli fail: sapi/cli: hoisted"]
    );
    store.record(&[fresh(&root, "fold", "sapi/cli")]).unwrap();
    assert_eq!(asked(&judge(&root)), Vec::<String>::new());

    // The manifest's profiles stale the reading, and a table of it that
    // decides no codegen does not; the configuration's deployment target
    // stales it as its rustflags do (#3356).
    let unread = "[workspace.metadata]\nnote = 1\n";
    write(&root, "Cargo.toml", unread);
    assert_eq!(asked(&judge(&root)), Vec::<String>::new());
    write(
        &root,
        "Cargo.toml",
        &format!("{unread}[profile.release]\nlto = false\n"),
    );
    assert_eq!(
        asked(&judge(&root)),
        [
            "fold: no reading for sapi/cli over the tree now, and .cargo/config.toml changed; `cargo xtask fold` takes it, building the release sapi/cli first (minutes)"
        ]
    );
    store.record(&[fresh(&root, "fold", "sapi/cli")]).unwrap();
    write(
        &root,
        ".cargo/config.toml",
        "[target.'cfg(all())']\nrustflags = []\n[env]\nMACOSX_DEPLOYMENT_TARGET = \"11.0\"\n",
    );
    assert_eq!(asked(&judge(&root)).len(), 1);
}

// ------------------------------------------- the base is main

/// A scratch repository on `t9`, cut from `main`, with `main` one commit
/// past the cut: `main`'s commit changes `ext/pcre`, the branch's changes
/// `xtask`, so which crates the verdict names says which base it took.
/// Returns the root and the commit the branch was cut at.
fn on_a_ticket_branch(name: &str) -> (Tree, String) {
    let root = scratch(name);
    let cut = Repo::at(&root).git(&["rev-parse", "HEAD"]);
    Repo::at(&root).git(&["checkout", "-q", "-b", "t9"]);
    write(&root, "xtask/src/lib.rs", "pub fn measure() {}\n");
    Repo::at(&root).commit("the turn's change");
    Repo::at(&root).git(&["checkout", "-q", "main"]);
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    Repo::at(&root).commit("main's own change");
    Repo::at(&root).git(&["checkout", "-q", "t9"]);
    (root, cut)
}

/// A row held at the merge (#3618 ruling 2) is the merge check's: a
/// ticketed checkout renders it as delegated, and main's holds it.
#[test]
fn a_row_held_at_the_merge_is_delegated_in_a_ticketed_checkout_and_held_in_mains() {
    let root = scratch("held-at-merge");
    let text = std::fs::read_to_string(root.join("seams.toml")).unwrap();
    let row = "instrument = \"crosscheck\"\n";
    assert!(text.contains(row));
    // The workspace's manifest holds the row at the merge once a person
    // has landed the ruling; a second `held_at` would be a duplicate key.
    let held = if text.contains(&format!("{row}held_at = \"merge\"")) {
        text
    } else {
        text.replace(row, &format!("{row}held_at = \"merge\"\n"))
    };
    std::fs::write(root.join("seams.toml"), held).unwrap();
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let crosscheck =
        |verdict: &Verdict| verdict.unmet.iter().any(|u| u.starts_with("crosscheck: "));

    let on_main = judge(&root);
    assert!(crosscheck(&on_main), "{on_main:#?}");
    assert!(on_main.delegated.is_empty(), "{on_main:#?}");

    Repo::at(&root).git(&["checkout", "-q", "-b", "t9"]);
    Repo::at(&root).git(&["config", "branch.t9.ticket", "9"]);
    let ticketed = judge(&root);
    assert!(!crosscheck(&ticketed), "{ticketed:#?}");
    assert!(
        ticketed
            .delegated
            .iter()
            .any(|d| d == "crosscheck for ext/pcre"),
        "{ticketed:#?}"
    );
    // Other rows the manifest holds at the merge may come first.
    let rendered = ticketed.render();
    let list = rendered.split("delegated to the merge check:\n").nth(1);
    assert!(
        list.is_some_and(|list| list.contains("- crosscheck for ext/pcre\n")),
        "{rendered}"
    );
}

/// A bare `done` measures since the merge base with `main` (ADR-0009,
/// harness-design.md §4.5), so `main`'s commits since the cut are not
/// the branch's.
#[test]
fn on_a_ticket_branch_a_bare_done_measures_since_its_merge_base_with_main() {
    let (root, cut) = on_a_ticket_branch("branch");
    let verdict = judge_since(&root, None);
    assert_eq!(verdict.base.since, "main");
    assert_eq!(verdict.base.commit, cut, "{verdict:#?}");
    assert_eq!(
        verdict.crates,
        ["xtask"],
        "main's own change is not this turn's: {verdict:#?}"
    );
    let ran = xtask(&root, &["done"]);
    assert_eq!(ran.code(), 1, "{ran}");
    let out = ran.stdout;
    assert!(
        out.contains(&format!("since main ({})", &cut[..7])),
        "{out}"
    );
}

/// A checkout that has only `origin`'s `main`, as a clone that never
/// checked it out has, measures since that.
#[test]
fn a_checkout_without_a_local_main_measures_since_origins() {
    let (root, cut) = on_a_ticket_branch("origin-main");
    let main = Repo::at(&root).git(&["rev-parse", "main"]);
    Repo::at(&root).git(&["update-ref", "refs/remotes/origin/main", &main]);
    Repo::at(&root).git(&["branch", "-q", "-D", "main"]);
    let verdict = judge_since(&root, None);
    assert_eq!(verdict.base.since, "main");
    assert_eq!(verdict.base.commit, cut, "{verdict:#?}");
    assert_eq!(verdict.crates, ["xtask"], "{verdict:#?}");
}

#[test]
fn on_main_the_base_is_mains_tip() {
    let (root, _) = on_a_ticket_branch("on-main");
    Repo::at(&root).git(&["checkout", "-q", "main"]);
    let verdict = judge_since(&root, None);
    assert_eq!(verdict.base.since, "main");
    assert_eq!(
        verdict.base.commit,
        Repo::at(&root).git(&["rev-parse", "main"])
    );
    assert!(verdict.crates.is_empty(), "{verdict:#?}");
}

/// A detached `HEAD` is on no branch and measures since `main` all the
/// same.
#[test]
fn a_detached_head_measures_since_its_merge_base_with_main() {
    let (root, cut) = on_a_ticket_branch("detached");
    Repo::at(&root).git(&["checkout", "-q", "--detach"]);
    let verdict = judge_since(&root, None);
    assert_eq!(verdict.base.since, "main");
    assert_eq!(verdict.base.commit, cut, "{verdict:#?}");
    assert_eq!(verdict.crates, ["xtask"], "{verdict:#?}");
}

/// The head line names where the base came from: the ref and the commit
/// it resolved to, and the commit alone when the ref given was that
/// commit, which is what pre-push passes per ref.
#[test]
fn the_head_line_names_the_ref_and_the_commit_and_a_commit_only_once() {
    let commit = "0f1e2d3c4b5a69788796a5b4c3d2e1f001234567";
    let named = Base {
        since: "main".to_owned(),
        commit: commit.to_owned(),
    };
    assert_eq!(named.to_string(), "main (0f1e2d3)");
    for since in [commit, &commit[..7]] {
        let a_commit = Base {
            since: since.to_owned(),
            commit: commit.to_owned(),
        };
        assert_eq!(a_commit.to_string(), "0f1e2d3");
    }
    // A ref whose name is a prefix of the commit is still a ref.
    let prefix = Base {
        since: "0f".to_owned(),
        commit: commit.to_owned(),
    };
    assert_eq!(prefix.to_string(), "0f (0f1e2d3)");
}

#[test]
fn base_overrides_main() {
    let (root, cut) = on_a_ticket_branch("base-wins");
    let head = Repo::at(&root).git(&["rev-parse", "HEAD"]);
    let verdict = judge_since(&root, Some("HEAD"));
    assert_eq!(verdict.base.since, "HEAD");
    assert_eq!(verdict.base.commit, head);
    assert!(verdict.crates.is_empty(), "{verdict:#?}");
    let ran = xtask(&root, &["done", "--base", &cut]);
    assert_eq!(ran.code(), 1, "{ran}");
    let out = ran.stdout;
    assert!(out.contains(&format!("since {}", &cut[..7])), "{out}");
}

/// The Stop hook passes no base (harness-design.md §4.5, callers), so
/// it holds a turn against `main`'s.
#[test]
fn the_stop_hook_holds_a_turn_against_mains_base() {
    let (root, _) = on_a_ticket_branch("hook");
    let out = hook(&root, "{\"session_id\":\"s\",\"stop_hook_active\":false}").ok();
    let block: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let reason = block["reason"].as_str().unwrap();
    assert!(reason.contains("since main ("), "{reason}");
    assert!(
        !reason.contains("ext/pcre"),
        "main's own change is not this turn's: {reason}"
    );
}

/// The Stop hook walks the built binaries before cargo, as permit's does
/// (#2982), so a build apart from the manifest in front of it judges no
/// turn: it exits 1, prints no verdict and names the manifest, and the
/// walk's next candidate holds the turn.
#[test]
fn a_stale_build_judges_no_turn_and_hands_it_on() {
    let root = scratch("hook-stale");
    let stale = std::fs::read_to_string(fixtures().join("stale/seams.toml")).unwrap();
    std::fs::write(root.join("seams.toml"), stale).unwrap();
    let stdin = "{\"session_id\":\"s\",\"stop_hook_active\":false}";
    let ran = hook(&root, stdin);
    assert_eq!(ran.code(), 1, "{ran}");
    let out = ran.stdout;
    let err = ran.stderr;
    assert!(out.trim().is_empty(), "{out}");
    assert!(err.contains("seams.toml"), "{err}");
}

#[test]
fn an_unchanged_tree_is_done() {
    let root = scratch("unchanged");
    let verdict = judge(&root);
    assert!(verdict.met(), "{verdict:#?}");
    assert!(verdict.crates.is_empty());
    let out = xtask(&root, &["done", "--base", "HEAD"]).ok();
    assert!(out.contains("every condition of done is met"), "{out}");
}

/// CI's `done` while the oracle job is skipped (#2477): the instruments
/// that job runs are named `--not-run`, so their missing readings are a
/// note that they did not run, the exit is not red for their absence, and
/// the head line and the reading say a pass is over what ran and nothing
/// else. A reading of one that the store does hold is judged as any other.
#[test]
fn an_instrument_named_not_run_is_a_note_and_never_a_silent_pass() {
    const ORACLE_JOB: [&str; 4] = ["--not-run", "witness", "--not-run", "crosscheck"];
    let root = scratch("not-run");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let store = Store::of_workspace(&root);
    for (instrument, scope) in asked_of(&root, "ext/pcre") {
        if instrument != "witness" && instrument != "crosscheck" {
            met(&store, &root, instrument, &scope);
        }
    }
    let ran = xtask(&root, &["done", "--base", "HEAD"]);
    assert_eq!(ran.code(), 1, "{ran}");
    let out = ran.stdout;
    assert!(out.contains("- witness: no reading for ext/pcre"), "{out}");

    let mut args = vec!["done", "--base", "HEAD"];
    args.extend(ORACLE_JOB);
    let out = xtask(&root, &args).ok();
    assert!(
        out.contains("every condition of done is met over what ran; witness and crosscheck did not run in this run\n"),
        "{out}"
    );
    assert!(
        out.contains("judged none of the readings it asks for: witness for ext/pcre, crosscheck for ext/pcre, crosscheck for ext/pcre/ri"),
        "{out}"
    );
    let reading = store.read("done", "workspace").unwrap().unwrap();
    assert_eq!(reading.status, Status::Pass);
    assert_eq!(reading.numbers["not_run"], 2.into());
    assert!(
        reading
            .message
            .ends_with("; witness and crosscheck did not run in this run"),
        "{}",
        reading.message
    );

    // A reading the store holds is judged: the lane ran and failed.
    let mut failed = fresh(&root, "witness", "ext/pcre");
    failed.status = Status::Fail;
    failed.message = "1 fence failed".into();
    store.record(&[failed]).unwrap();
    let ran = xtask(&root, &args);
    assert_eq!(ran.code(), 1, "{ran}");
    let out = ran.stdout;
    assert!(
        out.contains("- witness: ext/pcre fail: 1 fence failed"),
        "{out}"
    );

    // With no crate touched the note still says what did not run.
    let unchanged = scratch("not-run-unchanged");
    let mut args = vec!["done", "--base", "HEAD"];
    args.extend(ORACLE_JOB);
    let out = xtask(&unchanged, &args).ok();
    assert!(out.contains("no touched crate is asked for one"), "{out}");

    // What `--not-run` names is an instrument an `[[owed]]` row names
    // (#3206), and the refusal names the rows' instruments.
    let ran = xtask(&unchanged, &["done", "--not-run", "oracle"]);
    assert_eq!(ran.code(), 1, "{ran}");
    assert!(
        ran.stderr
            .contains("--not-run oracle: no [[owed]] row names it")
            && ran.stderr.contains("the rows name clippy, test, witness"),
        "{ran}"
    );
}

#[test]
fn fresh_passing_readings_for_the_touched_crate_pass_and_an_edit_after_them_is_stale() {
    let root = scratch("fresh");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let store = Store::of_workspace(&root);
    record_asked(&store, &root, "ext/pcre");
    let verdict = judge(&root);
    assert!(verdict.met(), "{verdict:#?}");

    write(
        &root,
        "ext/pcre/src/lib.rs",
        "pub fn preg_match() -> bool { true }\n",
    );
    let verdict = judge(&root);
    assert_eq!(
        verdict.unmet.len(),
        asked_of(&root, "ext/pcre").len(),
        "{verdict:#?}"
    );
    // The subset's run here is keyed by the engine's tree, which moved.
    for line in &verdict.unmet {
        assert!(
            line.contains("is stale; ext/pcre changed since it was taken")
                || unrun("ext/pcre")(line),
            "{line}\n{verdict:#?}"
        );
    }
}

/// A refused merge commit measures the tree it would have taken and
/// leaves its readings in the scope's files; once that tree is gone, the
/// passes over the tree now, still in the log, stand and are not shadowed
/// by the later readings over the tree that never landed (#1312).
#[test]
fn a_refused_commits_readings_over_another_tree_do_not_shadow_the_pass_over_the_tree_now() {
    let root = scratch("shadowed");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let store = Store::of_workspace(&root);
    record_asked(&store, &root, "ext/pcre");

    write(
        &root,
        "ext/pcre/src/lib.rs",
        "pub fn preg_match() -> u8 { 0 }\n",
    );
    let mut refused = fresh(&root, "clippy", "ext/pcre");
    refused.status = Status::Fail;
    refused.message = "ext/pcre: clippy is not clean".into();
    store.record(&[refused]).unwrap();
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");

    let verdict = judge(&root);
    assert!(verdict.met(), "{verdict:#?}");
}

/// The constructor and `done` agree on the path a scope's tree is hashed
/// over: a reading written through it is fresh for its scope, and one
/// byte under that scope makes every one of them stale (harness-design.md
/// §4.5 condition 1, §13 D.18). The crate's readings go through
/// `for_crate`; the `--ri` column's and the release lint's through
/// `stamped_over`. The subset is held by its kept runs and not a reading
/// (#725), and the byte moves the engine's tree they are kept at.
#[test]
fn a_reading_through_the_constructor_is_fresh_for_its_scope_and_stale_after_one_byte() {
    let root = scratch("one-byte");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let manifest = Manifest::load(&root).unwrap();
    let krate = manifest.crate_named("ext/pcre").unwrap();
    let store = Store::of_workspace(&root);
    for (instrument, scope) in asked_of(&root, "ext/pcre") {
        if instrument == "phpt" {
            met(&store, &root, instrument, &scope);
            continue;
        }
        let mut reading = if scope == "ext/pcre" {
            Reading::for_crate(
                instrument,
                krate,
                &xtask::key::Snapshot::disk(&root).unwrap(),
            )
            .unwrap()
        } else {
            Reading::stamped_over(
                instrument,
                &scope,
                &xtask::key::Snapshot::disk(&root).unwrap(),
                &krate.path(),
            )
            .unwrap()
        };
        if UNDER_RECIPE.contains(&instrument) {
            reading = reading.under(manifest.pin().unwrap());
        }
        reading.status = Status::Pass;
        reading.message = "measured".into();
        store.record(std::slice::from_ref(&reading)).unwrap();
    }
    let verdict = judge(&root);
    assert!(verdict.met(), "{verdict:#?}");

    let file = root.join("ext/pcre/src/lib.rs");
    let mut text = std::fs::read(&file).unwrap();
    text.push(b'\n');
    std::fs::write(&file, text).unwrap();
    let verdict = judge(&root);
    let stale: Vec<&str> = verdict
        .unmet
        .iter()
        .filter(|l| l.ends_with("is stale; ext/pcre changed since it was taken"))
        .map(|l| l.split(':').next().unwrap())
        .collect();
    let asked: Vec<&str> = asked_of(&root, "ext/pcre")
        .iter()
        .map(|(i, _)| *i)
        .filter(|i| *i != "phpt")
        .collect();
    assert_eq!(stale, asked, "{verdict:#?}");
    assert!(verdict.unmet.iter().any(unrun("ext/pcre")), "{verdict:#?}");
}

/// The touched crate's readings and kept runs met, then the pin rewritten
/// by `edit`: what `done` calls stale for it naming the pin as what
/// changed is the crate's instruments of [`UNDER_RECIPE`], its kept phpt
/// run among them (#2725), where `stales`, and nothing where not.
fn after_the_pin_moves(name: &str, edit: impl Fn(&str) -> String, stales: bool) -> Tree {
    let root = scratch(name);
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    record_asked(&Store::of_workspace(&root), &root, "ext/pcre");
    assert!(judge(&root).met(), "{:#?}", judge(&root));

    let pin = root.join("oracle.toml");
    let text = std::fs::read_to_string(&pin).unwrap();
    let changed = edit(&text);
    assert_ne!(changed, text, "the edit changes the pin");
    std::fs::write(&pin, changed).unwrap();
    let verdict = judge(&root);
    let mut stale: Vec<&str> = verdict
        .unmet
        .iter()
        .filter(|l| l.ends_with("is stale; oracle.toml changed since it was taken"))
        .map(|l| l.split(':').next().unwrap())
        .collect();
    stale.sort_unstable();
    stale.dedup();
    let mut under: Vec<&str> = asked_of(&root, "ext/pcre")
        .iter()
        .map(|(i, _)| *i)
        .filter(|i| UNDER_RECIPE.contains(i))
        .collect();
    under.sort_unstable();
    under.dedup();
    assert!(under.contains(&"phpt"));
    under.retain(|_| stales);
    assert_eq!(stale, under, "{verdict:#?}");
    root
}

/// A reading taken under the oracle's recipe is stale once the pin's values
/// move, as one taken over a crate is once its tree does (#1387): an entry
/// laid under `oracle.toml`'s `[recipe.env]` stales the touched crate's
/// `witness` and `crosscheck` readings and its kept phpt run, which
/// carries the pin's digest (#2725), and nothing else, each naming the pin
/// as what changed. Before the change all are fresh; after it, a run taken
/// again here leaves the base's stale, naming the call that takes it.
#[test]
fn a_reading_under_the_recipe_is_stale_once_the_recipe_changes() {
    let entry = read("xtask/fixtures/witness/recipe-env/env.toml");
    let env = |text: &str| text.replacen("[recipe.env]\n", &format!("[recipe.env]\n{entry}"), 1);
    let root = after_the_pin_moves("recipe", env, true);

    // The run here taken again under the new recipe, and the base's still
    // under the old: the base's is stale and names the call that takes it
    // again (#2725).
    keep_run(&root, None, "ext/pcre", "pass");
    let base = Repo::at(&root).git(&["rev-parse", "--short", "HEAD"]);
    let base = base.trim();
    let line = format!(
        "phpt: the run of ext/pcre kept at {base} is stale; oracle.toml changed since it was taken; take one with `cargo xtask phpt --base {base} --subset ext/pcre`"
    );
    assert!(judge(&root).unmet.contains(&line), "{:#?}", judge(&root));
}

#[test]
fn a_failing_or_skipped_reading_is_unmet_with_its_message() {
    let root = scratch("failing");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let store = Store::of_workspace(&root);
    record_asked(&store, &root, "ext/pcre");
    let mut reading = fresh(&root, "budget", "ext/pcre");
    reading.status = Status::Fail;
    reading.message = "ext/pcre: 7000 lines over the ceiling of 6000".into();
    store.record(std::slice::from_ref(&reading)).unwrap();
    // The subset's own reading is the port's meter, never read (#725).
    let mut reading = fresh(&root, "phpt", "ext/pcre");
    reading.status = Status::Fail;
    store.record(std::slice::from_ref(&reading)).unwrap();
    let verdict = judge(&root);
    assert_eq!(
        verdict.unmet,
        ["budget: ext/pcre fail: ext/pcre: 7000 lines over the ceiling of 6000"],
        "{verdict:#?}"
    );
}

/// The pin's digest leaves out a build's provenance and nothing else
/// (#2969): a Linux build's `image` changed keeps the touched crate's
/// readings under the recipe and its kept run fresh.
#[test]
fn changing_a_builds_image_keeps_the_runs_under_the_recipe_fresh() {
    let image = |text: &str| text.replacen("image = \"sha256:", "image = \"sha256:0", 1);
    after_the_pin_moves("image", image, false);
}

/// A variant build's `md5` is its binary's identity, not its provenance:
/// changing it stales every run under the recipe (#2969).
#[test]
fn changing_a_variant_builds_md5_stales_every_run_under_the_recipe() {
    let md5 = |text: &str| {
        let (head, variants) = text.split_at(text.find("[variant.").unwrap());
        format!(
            "{head}{}",
            variants.replacen(", md5 = \"", ", md5 = \"0", 1)
        )
    };
    after_the_pin_moves("variant-md5", md5, true);
}

/// `[oracle]`'s `md5`, a `[closure]` keg and a `[recipe]` ini line are
/// values of the pin, each digested: changing one stales every run under
/// the recipe (#2969). The keg's move re-pins the macOS build against the
/// moved record, as a person re-pins it, and that is its provenance.
#[test]
fn changing_the_oracles_md5_a_closure_keg_or_a_recipe_ini_stales_every_run() {
    let oracle = |text: &str| text.replacen("\nmd5 = \"", "\nmd5 = \"0", 1);
    after_the_pin_moves("oracle-md5", oracle, true);
    let keg = |text: &str| {
        let moved = text.replacen("\nzstd = \"", "\nzstd = \"0", 1);
        let parsed = xtask::oracle::Pin::parse(&moved).unwrap();
        let refused = parsed.defects().first().unwrap().clone();
        let (_, at) = refused.split_once("made against the closure ").unwrap();
        let (was, at) = at.split_once(", and the pin's [closure] is ").unwrap();
        moved.replace(was, at.split(',').next().unwrap())
    };
    after_the_pin_moves("closure-keg", keg, true);
    let ini = |text: &str| text.replacen("\"precision=14\"", "\"precision=13\"", 1);
    after_the_pin_moves("recipe-ini", ini, true);
}

/// A touched crate's subset is held to no regression against the base
/// (harness §4.5 condition 1, #725), from the run kept at the base's
/// engine tree and the one kept at this tree's, which `done` diffs itself:
/// a test met at the base and failed or skipped here is unmet, one met
/// nowhere at the base is held to nothing, and a base with no run kept
/// is unmet naming how to take one.
#[test]
fn a_subset_is_held_to_no_regression_against_the_runs_kept_at_the_base() {
    let root = scratch("regressed");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let store = Store::of_workspace(&root);
    record_asked(&store, &root, "ext/pcre");
    assert!(judge(&root).met(), "{:#?}", judge(&root));
    let base = Repo::at(&root).git(&["rev-parse", "--short", "HEAD"]);
    for (then, now, held) in [
        ("pass", "fail", true),
        ("diverged", "skip", true),
        ("pass", "diverged", false),
        ("skip", "fail", false),
        ("fail", "bork", false),
    ] {
        keep_run(&root, Some("HEAD"), "ext/pcre", then);
        keep_run(&root, None, "ext/pcre", now);
        let regressed = format!(
            "phpt: ext/pcre: 1 of 1 tests met at {} are not met here: ext/pcre/tests/a.phpt",
            base.trim()
        );
        let unmet = if held { vec![regressed] } else { vec![] };
        assert_eq!(judge(&root).unmet, unmet, "{then} then, {now} now");
    }
    let kept = xtask::phpt::kept_run(
        &xtask::key::Snapshot::commit(&root, &Repo::at(&root).git(&["rev-parse", "HEAD"])).unwrap(),
        &layout(&root),
        "ext/pcre",
    )
    .unwrap();
    std::fs::remove_file(kept).unwrap();
    let verdict = judge(&root);
    assert!(
        verdict.unmet.len() == 1
            && verdict.unmet[0].starts_with("phpt: no run of ext/pcre is kept at ")
            && verdict.unmet[0].contains("cargo xtask phpt --base "),
        "{verdict:#?}"
    );
    // A run of none is no run (#3143), here as at the base.
    keep_run(&root, None, "ext/pcre", "");
    keep_run(&root, Some("HEAD"), "ext/pcre", "");
    let verdict = judge(&root);
    assert!(
        verdict.unmet.len() == 1 && unrun("ext/pcre")(&verdict.unmet[0]),
        "{verdict:#?}"
    );
}

/// An extension with no tests of its own in the pinned tree, as
/// `ext/lexbor`, keeps no run (#3143), and condition 1 holds it to nothing
/// with none kept, as it held the run of none kept before (#725).
#[test]
fn an_extension_with_no_tests_of_its_own_is_met_with_no_run_kept() {
    let manifest = Manifest::load(workspace()).unwrap();
    let tree = manifest.pin().unwrap().source.tree();
    if Promise::Tree.or_skip(tree).unwrap().is_none() {
        return;
    }
    let root = scratch("testless");
    let toml = "[package]\nname = \"php_ext_lexbor\"\nversion = \"0.0.0\"\nedition = \"2024\"\n";
    write(&root, "ext/lexbor/Cargo.toml", toml);
    write(&root, "ext/lexbor/src/lib.rs", "pub fn parse() {}\n");
    let store = Store::of_workspace(&root);
    let asked = asked_of(&root, "ext/lexbor");
    assert!(asked.iter().any(|(i, _)| *i == "phpt"), "{asked:?}");
    for (instrument, scope) in asked.iter().filter(|(i, _)| *i != "phpt") {
        met(&store, &root, instrument, scope);
    }
    assert!(judge(&root).met(), "{:#?}", judge(&root));
}

/// A crate the manifest marks `porting = true` has its `crosscheck`
/// readings read as the port's meter and not as a condition of done
/// (harness §4.5 condition 1, ledger 47; #383): a touched extension with
/// both columns red is met, and the key excuses those readings alone — a
/// red `budget` of the marked crate is unmet, and so is a regression of
/// its subset, the key's `phpt` half having gone with #725. The readings
/// are still asked for: one missing is unmet whatever the key says.
#[test]
fn a_porting_crates_crosscheck_readings_are_the_ports_meter() {
    let root = scratch("porting");
    write(
        &root,
        "ext/standard/Cargo.toml",
        "[package]\nname = \"php_ext_standard\"\n",
    );
    write(&root, "ext/standard/src/lib.rs", "pub fn str_repeat() {}\n");
    // The key `xtask steer` writes (harness §4.5 condition 1, §11).
    Tracked::of(&root)
        .with_crate("ext/standard", |row| row.porting = true)
        .write(&root);
    Repo::at(&root).commit("ext/standard, its port partial");
    write(
        &root,
        "ext/standard/src/lib.rs",
        "pub fn str_repeat() {}\npub fn implode() {}\n",
    );
    let store = Store::of_workspace(&root);
    record_asked(&store, &root, "ext/standard");
    assert!(judge(&root).met(), "{:#?}", judge(&root));

    let red = |instrument: &str, scope: &str, message: &str| {
        let mut reading = fresh(&root, instrument, scope);
        reading.status = Status::Fail;
        reading.message = message.to_owned();
        store.record(std::slice::from_ref(&reading)).unwrap();
    };
    red("crosscheck", "ext/standard", "ext/standard: 4758 lines");
    red("crosscheck", "ext/standard/ri", "ext/standard/ri: 40 bytes");
    red("phpt", "ext/standard", "ext/standard: 753 of 3821 failed");
    assert!(judge(&root).met(), "{:#?}", judge(&root));

    red("budget", "ext/standard", "ext/standard: 31000 lines");
    keep_run(&root, None, "ext/standard", "fail");
    let unmet = judge(&root).unmet;
    assert_eq!(unmet.len(), 2, "{unmet:#?}");
    assert_eq!(
        unmet[0],
        "budget: ext/standard fail: ext/standard: 31000 lines"
    );
    assert!(unmet[1].starts_with("phpt: ext/standard: 1 of 1 tests met at "));

    std::fs::remove_file(store.path("crosscheck", "ext/standard").unwrap()).unwrap();
    assert!(
        judge(&root)
            .unmet
            .contains(&"crosscheck: no reading for ext/standard".to_owned())
    );
}

#[test]
fn a_kernel_or_native_without_a_witness_is_unmet_from_the_witness_reading() {
    let root = scratch("missing");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let store = Store::of_workspace(&root);
    record_asked(&store, &root, "ext/pcre");
    let mut reading = fresh(&root, "witness", "ext/pcre");
    reading.count("missing", 2).unwrap();
    store.record(std::slice::from_ref(&reading)).unwrap();
    let verdict = judge(&root);
    assert_eq!(
        verdict.unmet,
        ["witness: 2 kernels or natives in ext/pcre without a witness"],
        "{verdict:#?}"
    );
}

// ---------------------------------------------------------------- 3

#[test]
fn a_lock_that_differs_must_be_in_the_diff_and_a_lock_in_the_diff_needs_its_reading() {
    let root = scratch("locks");
    let store = Store::of_workspace(&root);
    // The seam's name is the scope; the mapping stamps it over the lock's
    // home, `crates/php_heap`, which is what `done` hashes.
    let differs = |root: &Path| {
        let manifest = Manifest::load(root).unwrap();
        let mut reading = Reading::for_scope(
            "api-check",
            "heap",
            &manifest,
            &xtask::key::Snapshot::disk(root).unwrap(),
        )
        .unwrap();
        reading.status = Status::Fail;
        reading.message =
            "2 public items of php_heap have no line in crates/php_heap/heap.lock: Gc, Zval".into();
        reading
    };
    store.record(&[differs(&root)]).unwrap();
    let about_locks = |verdict: &Verdict| -> Vec<String> {
        verdict
            .unmet
            .iter()
            .filter(|l| l.contains(".lock"))
            .cloned()
            .collect()
    };
    let verdict = judge(&root);
    assert_eq!(
        about_locks(&verdict),
        [
            "crates/php_heap/heap.lock: 2 public items of php_heap have no line in crates/php_heap/heap.lock: Gc, Zval; the lock is not in the diff"
        ],
        "{verdict:#?}"
    );

    // The lock lives in its crate, so writing it makes the seam reading
    // stale, which condition 3 leaves alone: php_heap is now a touched
    // crate and condition 1 asks it for an api-check reading (a crate
    // reading, which nothing wrote here). A fresh seam reading that still
    // fails is unmet without the suffix.
    write(&root, "crates/php_heap/heap.lock", "Gc\nZval\n");
    let verdict = judge(&root);
    assert!(about_locks(&verdict).is_empty(), "{verdict:#?}");
    assert!(
        verdict
            .unmet
            .contains(&"api-check: no reading for php_heap".into()),
        "{verdict:#?}"
    );
    store.record(&[differs(&root)]).unwrap();
    let verdict = judge(&root);
    let lines = about_locks(&verdict);
    assert_eq!(lines.len(), 1, "{verdict:#?}");
    assert!(lines[0].ends_with("Gc, Zval"), "{verdict:#?}");

    write(&root, "crates/php_unit/unit.lock", "Unit\n");
    let verdict = judge(&root);
    assert!(
        about_locks(&verdict).contains(
            &"crates/php_unit/unit.lock is in the diff and has no api-check reading".into()
        ),
        "{verdict:#?}"
    );
}

// ---------------------------------------------------------------- 4

#[test]
fn an_open_divergence_row_on_a_changed_scope_fails_and_on_another_scope_does_not() {
    let root = scratch_with_an_open_row("open");
    write(&root, "crates/php_heap/src/lib.rs", "pub struct Zval;\n");
    let verdict = judge(&root);
    assert!(
        verdict.unmet.iter().all(|l| !l.contains("open divergence")),
        "the open row names ext/pcre, not php_heap: {verdict:#?}"
    );

    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let verdict = judge(&root);
    let line = verdict
        .unmet
        .iter()
        .find(|l| l.starts_with("open divergence"))
        .unwrap_or_else(|| panic!("{verdict:#?}"));
    assert!(
        line.contains("phpt:php-src/ext/pcre/tests/preg_match_basic.phpt touches ext/pcre"),
        "{line}"
    );
    assert!(line.contains("a person writes the reason"), "{line}");
}

#[test]
fn a_row_the_diff_itself_proposes_touches_it_whatever_crate_it_names() {
    let root = scratch("proposed");
    let mut registry = std::fs::read_to_string(root.join("docs/divergence.toml")).unwrap();
    registry.push('\n');
    registry.push_str(&Row::open("phpt:php-src/ext/spl/tests/heap_top.phpt").toml());
    write(&root, "docs/divergence.toml", &registry);
    let verdict = judge(&root);
    let line = verdict
        .unmet
        .iter()
        .find(|l| l.starts_with("open divergence phpt:php-src/ext/spl"))
        .unwrap_or_else(|| panic!("{verdict:#?}"));
    assert!(
        line.contains("docs/divergence.toml, proposed in this diff"),
        "{line}"
    );
    assert!(
        verdict
            .unmet
            .iter()
            .filter(|l| l.starts_with("open divergence"))
            .count()
            == 1,
        "the accepted rows of the base are not touched: {verdict:#?}"
    );
}

// ---------------------------------------------------------------- 5, 6

#[test]
fn permit_over_the_diff_denies_a_lock_in_an_extension_and_notes_the_harness_files() {
    let root = scratch("permit");
    write(
        &root,
        "ext/pcre/src/lib.rs",
        "use std::sync::Mutex;\npub static CACHE: Mutex<u8> = Mutex::new(0);\n",
    );
    // A person's edit to a row this diff has no other business with.
    Tracked::of(&root)
        .with_crate("php_heap", |row| row.budget += 1)
        .write(&root);
    let verdict = judge(&root);
    let denied = verdict
        .unmet
        .iter()
        .find(|l| l.starts_with("permit denies ext/pcre/src/lib.rs"))
        .unwrap_or_else(|| panic!("{verdict:#?}"));
    assert!(denied.contains("ext.no-locks"), "{denied}");
    assert!(denied.contains("ADR-0005"), "{denied}");
    assert!(
        verdict.unmet.iter().all(|l| !l.contains("seams.toml")),
        "the manifest is a note, not unmet: {verdict:#?}"
    );
    assert_eq!(
        verdict.notes,
        [
            "seams.toml is in the diff: changed by a person, or not; the hook cannot tell (harness.self)"
        ],
        "{verdict:#?}"
    );
    let ran = xtask(&root, &["done", "--base", "HEAD"]);
    assert_eq!(ran.code(), 1, "{ran}");
    let out = ran.stdout;
    assert!(out.contains("notes:\n- seams.toml is in the diff"), "{out}");
}

/// A binary file in the diff `done` judges is judged by its path alone,
/// as `permit --diff` judges a staged one: one committed in the base and
/// changed since, and one untracked, each spelling a lock in bytes git
/// reads as binary, deny nothing, and one at a harness path is that
/// path's note.
#[test]
fn permit_over_the_diff_judges_a_binary_file_by_its_path_alone() {
    const BINARY: &[u8] = b"\xcf\xfa\xed\xfe\x00\xff php_ext::__private\n";
    let root = scratch("binary");
    let repo = Repo::at(&root);
    repo.write("ext/pcre/src/committed.so", b"\x00\x01\x02")
        .commit("a binary");
    repo.write("ext/pcre/src/committed.so", BINARY)
        .write("ext/pcre/src/untracked.so", BINARY)
        .write(".claude/hooks/stand-in", BINARY);
    let verdict = judge(&root);
    assert!(
        verdict
            .unmet
            .iter()
            .all(|l| !l.starts_with("permit denies")),
        "{verdict:#?}"
    );
    assert_eq!(
        verdict.notes,
        [
            ".claude/hooks/stand-in is in the diff: changed by a person, or not; the hook cannot tell (harness.self)"
        ],
        "{verdict:#?}"
    );
}

/// The `unless` qualifier over the diff (#105): a crate whose only `exit=`
/// is the one `xtask witness --accept` wrote is done, and the same tree
/// with no witness reading behind it is denied with the rule id. The
/// reading is written by hand, as every reading here is; that `--accept`
/// writes the line and then stamps the reading over it is held beside
/// `permit`.
#[test]
fn a_crate_whose_only_exit_is_one_accept_wrote_is_done() {
    let root = scratch("captured-exit");
    // The status stands in the line as `--accept` would have written it;
    // it is built from the number here so that the row under test does not
    // read this fixture as a witness of the harness's own.
    let status = 3;
    write(
        &root,
        "ext/pcre/src/lib.rs",
        &format!(
            "/// ```php\n/// exit({status});\n/// ```\n/// ```expect exit={status}\n/// ```\npub fn preg_match() {{}}\n"
        ),
    );
    let store = Store::of_workspace(&root);
    record_asked(&store, &root, "ext/pcre");
    let verdict = judge(&root);
    assert!(verdict.met(), "{verdict:#?}");

    std::fs::remove_file(store.path("witness", "ext/pcre").unwrap()).unwrap();
    let verdict = judge(&root);
    let denied = verdict
        .unmet
        .iter()
        .find(|l| l.starts_with("permit denies ext/pcre/src/lib.rs"))
        .unwrap_or_else(|| panic!("{verdict:#?}"));
    assert!(denied.contains("witness.exit-authored"), "{denied}");
    assert!(denied.contains("never authored"), "{denied}");
}

// ---------------------------------------------------------------- the hook

#[test]
fn the_hook_blocks_with_the_list_on_the_recorded_stop_stdin_and_passes_when_active() {
    let root = scratch("hook");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let stop = std::fs::read_to_string(fixtures().join("hooks/stop.json")).unwrap();
    let out = hook(&root, &stop).ok();
    let block: serde_json::Value =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    let shape: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures().join("hooks/stop.stdout.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(block["decision"], shape["decision"]);
    assert_eq!(
        block.as_object().unwrap().keys().collect::<Vec<_>>(),
        shape.as_object().unwrap().keys().collect::<Vec<_>>()
    );
    let reason = block["reason"].as_str().unwrap();
    assert!(
        reason.contains("unmet:\n- clippy: no reading for ext/pcre"),
        "{reason}"
    );

    let active = std::fs::read_to_string(fixtures().join("hooks/stop-active.json")).unwrap();
    let out = hook(&root, &active).ok();
    assert_eq!(out, "", "the second Stop passes");

    let out = hook(&root, "not json").ok();
    assert!(out.contains("\"decision\":\"block\""), "{out}");
    assert!(
        out.contains("could not read the Stop hook's stdin"),
        "{out}"
    );
}

#[test]
fn the_hook_prints_nothing_when_done_is_met() {
    let root = scratch("hook-met");
    let stop = std::fs::read_to_string(fixtures().join("hooks/stop.json")).unwrap();
    let out = hook(&root, &stop).ok();
    assert_eq!(out, "");
}

/// The Stop holds what the session wrote and nothing else (#3618 ruling
/// 1): a change in the checkout that no call of the stopping session wrote
/// passes it, whoever made the change.
#[test]
fn a_session_that_wrote_in_no_checkout_passes_the_stop() {
    let root = scratch("hook-wrote-nothing");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let stop = "{\"session_id\":\"quiet\",\"stop_hook_active\":false}";
    assert_eq!(
        stop_hook(&root, stop).ok(),
        "",
        "the change is not this session's"
    );
    let reading = Store::of_workspace(&root)
        .read("done", "workspace")
        .unwrap()
        .unwrap();
    assert!(
        reading
            .message
            .starts_with("stop: 0 unmet over the 0 checkouts"),
        "{}",
        reading.message
    );

    let out = hook(&root, stop).ok();
    assert!(
        out.contains("no reading for ext/pcre"),
        "once it wrote there: {out}"
    );
}

/// A subagent's writes are its own (#3619): they hold its own Stop and
/// never its parent's, whose Stop with no write of its own passes.
#[test]
fn a_subagents_writes_hold_its_own_stop_and_not_its_parents() {
    let root = scratch("hook-subagent");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    wrote_in(&root, &root, "s", Some("builder"));
    let parent = "{\"session_id\":\"s\",\"hook_event_name\":\"Stop\"}";
    assert_eq!(
        stop_hook(&root, parent).ok(),
        "",
        "the parent wrote nothing"
    );
    let own =
        "{\"session_id\":\"s\",\"agent_id\":\"builder\",\"hook_event_name\":\"SubagentStop\"}";
    let out = stop_hook(&root, own).ok();
    assert!(out.contains("no reading for ext/pcre"), "{out}");
    let reading = Store::of_workspace(&root)
        .read("done", "workspace")
        .unwrap()
        .unwrap();
    assert!(
        reading.message.starts_with("subagent-stop: "),
        "{}",
        reading.message
    );
}

/// A checkout the session wrote in other than the one the hook stands in
/// is judged by that checkout's own build, and a checkout with none that
/// judges holds the turn naming it, never a silent pass (#3619).
#[test]
fn a_checkout_written_elsewhere_is_handed_to_its_own_build() {
    let root = scratch("hook-elsewhere");
    let tree = Tree::new("done-hook-elsewhere").unwrap();
    let other = tree.join("t1");
    Repo::at(&root).git(&["worktree", "add", "-q", "-b", "t1", other.to_str().unwrap()]);
    wrote_in(&root, &other, "s", None);
    // A directory in no checkout of the repository is none the Stop holds.
    wrote_in(&root, tree.path(), "s", None);
    let out = stop_hook(&root, "{\"session_id\":\"s\"}").ok();
    let other = other.canonicalize().unwrap();
    assert!(
        out.contains(&format!("done could not judge {}", other.display())),
        "{out}"
    );
}

/// The second Stop releases a held turn and records that it did, with the
/// count the block had (#3618 M4).
#[test]
fn a_block_the_second_stop_released_is_a_reading_with_its_count() {
    let root = scratch("hook-released");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let stop = std::fs::read_to_string(fixtures().join("hooks/stop.json")).unwrap();
    assert!(hook(&root, &stop).ok().contains("block"));
    let store = Store::of_workspace(&root);
    let held = store.read("done", "workspace").unwrap().unwrap();
    let active = std::fs::read_to_string(fixtures().join("hooks/stop-active.json")).unwrap();
    assert_eq!(stop_hook(&root, &active).ok(), "");
    let release = store
        .history("done")
        .unwrap()
        .readings
        .into_iter()
        .rfind(|r| r.scope == "released");
    let release = release.expect("the release was recorded");
    assert_eq!(release.status, Status::Fail);
    assert_eq!(release.numbers["unmet"], held.numbers["unmet"]);
    assert!(
        release
            .message
            .starts_with("stop: the second Stop released"),
        "{}",
        release.message
    );
}

/// Each reading names its caller (#3619): pre-push says so on the command
/// line, and a run that names none is a hand's outside CI.
#[test]
fn a_done_reading_carries_its_caller() {
    let root = scratch("caller");
    let store = Store::of_workspace(&root);
    xtask(&root, &["done", "--base", "HEAD", "--caller", "pre-push"]).ok();
    let reading = store.read("done", "workspace").unwrap().unwrap();
    assert!(
        reading.message.starts_with("pre-push: "),
        "{}",
        reading.message
    );
    if std::env::var_os("GITHUB_ACTIONS").is_none() {
        xtask(&root, &["done", "--base", "HEAD"]).ok();
        let reading = store.read("done", "workspace").unwrap().unwrap();
        assert!(reading.message.starts_with("hand: "), "{}", reading.message);
    }
}

// ---------------------------------------------------------------- the reading

#[test]
fn every_verdict_is_one_reading_of_done_over_the_workspace() {
    let root = scratch("reading");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let ran = xtask(&root, &["done", "--base", "HEAD"]);
    assert_eq!(ran.code(), 1, "{ran}");
    let store = Store::of_workspace(&root);
    let reading = store.read("done", "workspace").unwrap().unwrap();
    assert_eq!(reading.status, Status::Fail);
    assert_eq!(reading.numbers["unmet"], 9.into());
    assert_eq!(reading.numbers["crates"], 1.into());
    assert_eq!(reading.numbers["changed"], 1.into());
    assert!(
        reading.message.contains(": 9 unmet since "),
        "{}",
        reading.message
    );
    assert_eq!(reading.key().len(), 40);
    assert_eq!(reading.commit.len(), 7);

    let stop = std::fs::read_to_string(fixtures().join("hooks/stop.json")).unwrap();
    hook(&root, &stop);
    assert_eq!(
        store.history("done").unwrap().readings.len(),
        2,
        "the hook writes one too"
    );
}

// ---------------------------------------------------------------- the wiring

#[test]
fn the_stop_hook_runs_done_with_hook() {
    let hooks: serde_json::Value =
        serde_json::from_str(&read("harness-plugins/harness/hooks/hooks.json")).unwrap();
    let stop = &hooks["hooks"]["Stop"];
    let entries = stop
        .as_array()
        .unwrap_or_else(|| panic!("no Stop entry: {hooks}"));
    assert_eq!(entries.len(), 1);
    let hook = &entries[0]["hooks"][0];
    assert_eq!(hook["type"], "command");
    let command = hook["command"].as_str().unwrap();
    assert!(command.contains("done --hook"), "{command}");
    assert!(command.contains("cd \"$CLAUDE_PROJECT_DIR\""), "{command}");
    assert!(
        entries[0].get("matcher").is_none(),
        "Stop takes no matcher: {}",
        entries[0]
    );
}

/// CI's per-commit job, where the harness's own tests run, holds the
/// function-hook module to the committed declarations (harness-design.md
/// §1 revision 8, §6; #62): `tsc -p harness-plugins` reads
/// `harness-plugins/types`, the tracked file the committer's Claude Code
/// rewrites at pre-commit, before the crates' tests; no Claude Code is installed on the runner and nothing writes
/// the declarations there. The comments are stripped first, so a step
/// named in prose does not stand in for the step, and a step that left
/// is not found in prose either.
#[test]
fn ci_typechecks_the_module_against_the_committed_declarations_before_the_tests() {
    let text = read(".github/workflows/harness.yml");
    let job = &text[text.find("\n  per-commit:").unwrap()..];
    let steps = job
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    let mut last = 0;
    for step in ["tsc -p harness-plugins", "run: cargo xtask test"] {
        let at = steps
            .find(step)
            .unwrap_or_else(|| panic!("the test job lacks {step}: {steps}"));
        assert!(at > last, "{step} is out of order: {steps}");
        last = at;
    }
    for gone in [
        "@anthropic-ai/claude-code",
        "/plugin-types",
        "plugin validate",
        "run: claude",
    ] {
        assert!(
            !steps.contains(gone),
            "the test job still runs {gone}: {steps}"
        );
    }
}

/// CI per commit (harness-design.md §7): the same instruments over the
/// workspace, then `done`; the reading store is the only thing the run
/// keeps; what it renders is the permits section. The instruments run
/// on two runners, the build runner's and the oracle runner's (#2253),
/// and each job stashes its own store for `done`'s job, which joins them
/// and stashes the one store the run keeps, `harness` (#3554).
#[test]
fn ci_per_commit_runs_the_instruments_then_done_and_uploads_only_the_store() {
    let text = read(".github/workflows/harness.yml");
    let mut last = 0;
    for step in [
        "cargo fmt --check",
        "xtask clippy",
        "xtask test",
        "xtask api-check",
        "xtask budget",
        "xtask unsafe-audit",
        "xtask vocab",
        "xtask crosscheck",
        "xtask done",
        "status --section permits",
    ] {
        let at = text
            .find(step)
            .unwrap_or_else(|| panic!("CI lacks {step}: {text}"));
        assert!(at > last, "{step} is out of order: {text}");
        last = at;
    }
    // Every put to the stash is a store and nothing else, less what a job
    // excludes of it (the phpt runs a job that takes none would carry,
    // #2299), and one of them is the run's, `harness`; a ticket is closed by
    // the session that worked it, so no job closes one on a push (#290).
    // The stash, not a GitHub artifact, which counts against the account's
    // storage (#3554).
    assert!(
        !text.contains("upload-artifact") && !text.contains("download-artifact"),
        "the hand-offs are the stash's: {text}"
    );
    let puts = text
        .match_indices("uses: ./.github/actions/stash")
        .map(|(at, _)| {
            let step = &text[at..];
            &step[..step.find("\n      - ").unwrap_or(step.len())]
        })
        .filter(|step| step.contains("put: "))
        .collect::<Vec<_>>();
    assert_eq!(
        puts.len(),
        4,
        "the three jobs' stores and the run's: {text}"
    );
    let mut kept = 0;
    for step in puts {
        assert!(
            step.contains("path: target/harness\n"),
            "the put is the store: {step}"
        );
        assert!(
            step.lines()
                .map(str::trim)
                .filter_map(|l| l.strip_prefix("exclude: "))
                .all(|l| l == "phpt-runs/"),
            "a put leaves out the phpt runs and nothing else: {step}"
        );
        kept += usize::from(step.contains("put: harness\n"));
    }
    assert_eq!(kept, 1, "one store is the run's: {text}");
    assert!(
        text.contains("fetch-depth: 0"),
        "done needs the merge base with main: {text}"
    );
}

/// CI per commit runs on a push to `main` alone, since branch tracks are
/// gone and a ticket's branch reaches CI as a pull request (ADR-0009,
/// #1757), and judges each since the base its Stop hook took
/// (harness-design.md §4.5, §7). A push passes no base, so `done` takes
/// the merge base with `main`; a pull request's checkout is a detached
/// merge commit, so the step passes the ref the pull request targets.
#[test]
fn ci_runs_the_per_commit_job_on_main_and_judges_since_the_same_base() {
    let text = read(".github/workflows/harness.yml");
    let on = &text[text.find("on:").unwrap()..text.find("jobs:").unwrap()];
    let branches = on
        .lines()
        .find(|l| l.trim_start().starts_with("branches:"))
        .unwrap_or_else(|| panic!("no push branches: {on}"));
    assert_eq!(
        branches.split('#').next().map(str::trim),
        Some("branches: [main]"),
        "a push to main alone: {branches}"
    );
    assert!(
        on.lines().any(|l| l.trim() == "pull_request:"),
        "a ticket's branch reaches CI as a pull request: {on}"
    );
    let step = &text[text.find("- name: done").unwrap()..];
    let step = &step[..step.find("\n      - ").unwrap_or(step.len())];
    assert!(
        step.contains("GITHUB_BASE_REF"),
        "a pull request is judged since the ref it targets: {step}"
    );
    assert!(
        step.contains("done --base \"$GITHUB_BASE_REF\""),
        "the base is passed as the ref, which merge_base resolves on origin: {step}"
    );
    assert!(
        step.lines()
            .any(|l| l.contains("cargo xtask done") && !l.contains("--base")),
        "a push passes nothing and gets the merge base with main: {step}"
    );
}

/// While `DAX_BUILD_RUNNER` is set the oracle job is skipped, and the done
/// step names as `--not-run` exactly the instruments of condition 1 that
/// job's steps run, so a lane added there is named here or `done` is red
/// for its absence (#2477). No instrument without an owed row is named.
#[test]
fn a_skipped_oracle_job_has_done_name_every_instrument_it_runs_not_run() {
    let manifest = Manifest::load(workspace()).unwrap();
    let text = read(".github/workflows/harness.yml");
    let oracle = &text[text.find("\n  oracle:\n").unwrap()..text.find("\n  done:\n").unwrap()];
    assert!(
        oracle.contains("    if: ${{ !vars.DAX_BUILD_RUNNER }}\n"),
        "the oracle job is skipped while the variable is set: {oracle}"
    );
    let runs: std::collections::BTreeSet<&str> = oracle
        .lines()
        .filter_map(|l| l.trim().strip_prefix("- run: cargo xtask "))
        .filter_map(|l| l.split_whitespace().next())
        .filter(|i| manifest.owing(i).is_some())
        .collect();
    assert_eq!(
        runs,
        ["crosscheck", "witness"].into(),
        "the oracle job's instruments: {oracle}"
    );
    let step = &text[text.find("- name: done").unwrap()..];
    let step = &step[..step.find("\n      - ").unwrap_or(step.len())];
    assert!(
        step.contains("ORACLE: ${{ needs.oracle.result }}")
            && step.contains("if [ \"$ORACLE\" = skipped ]; then"),
        "the step reads whether the oracle job was skipped: {step}"
    );
    let named: std::collections::BTreeSet<&str> = step
        .split("--not-run ")
        .skip(1)
        .filter_map(|rest| rest.split_whitespace().next())
        .collect();
    assert_eq!(named, runs, "{step}");
    let empty = step
        .find("set --\n")
        .expect("no instrument is excluded before the oracle job is tested");
    assert!(
        empty < step.find("if [ \"$ORACLE\" = skipped ]").unwrap(),
        "{step}"
    );
    assert!(
        step.contains("::warning title=the oracle lane did not run::"),
        "{step}"
    );
}

/// #18's other half: the row is what asks, so a crate that is no
/// extension and carries one is unmet without its reading, the way
/// `ext/pcre` is.
#[test]
fn a_crate_with_a_row_and_no_reading_is_unmet_the_way_an_extension_is() {
    let root = scratch("row");
    Tracked::of(&root)
        .with_crate("php_heap", |row| row.phpt = vec!["Zend/tests".to_owned()])
        .write(&root);
    Repo::at(&root).commit("the row");

    write(
        &root,
        "crates/php_heap/src/lib.rs",
        "pub fn allocate() {}\n",
    );
    let verdict = judge(&root);
    assert_eq!(verdict.crates, ["php_heap"], "{verdict:#?}");
    assert!(verdict.unmet.iter().any(unrun("php_heap")), "{verdict:#?}");
}

// ---------------------------------------------------------------- the reply (#32)

/// The row #32 proposes for a person to add: the word list as a
/// `deny_pattern` at the Stop, read over the reply sentence by sentence.
const STOP_RULE: &str = "ticket.reply-unfiled";
const STOP_RULE_ROW: &str = r#"
tool = "Stop"
deny_pattern = '(?i)\b(left (for|to) a person|for a person to|handed (off )?to a person|a person (decides|must|should|will|would|has to)|is a person.?s( alone)?|worth (filing|a ticket)|(needs|deserves|wants) (a|its own) ticket|should be filed|not (yet )?filed|left open|leaves? open|open question|still to (do|decide)|not (yet )?done|remains? (to be|to do|undone|open)|in a later (commit|turn|session|ticket)|next turn|future work|follow[- ]?ups?|leftovers?|out of scope|todo)\b'
text = "A session that closed a ticket names what it found and did not do in a ticket, a divergence row or a rule proposed in a commit message, never in its reply alone (CONTEXT.md, Ticket): a reply that hands work to a person, leaves a decision open or calls a thing worth filing is held once. A heuristic beside the close's own conditions; the second Stop passes."
"#;

/// `manifest` with the proposed row, as the tracked file carries it or
/// added.
fn with_the_stop_row(manifest: Tracked) -> Tracked {
    manifest.with_rule(STOP_RULE, |rule| {
        if !rule.contains_key("tool") {
            rule.extend(STOP_RULE_ROW.parse::<toml::Table>().unwrap());
        }
    })
}

/// The scratch repository with the proposed row in its manifest, as the
/// tracked file carries it or appended and committed here, so the row is
/// in the base and not in the diff.
fn with_the_stop_rule(root: &Path) {
    if Manifest::load(root).unwrap().rule(STOP_RULE).is_none() {
        with_the_stop_row(Tracked::of(root)).write(root);
        Repo::at(root).commit("the row");
    }
}

/// A `ticket` reading of a close, as `xtask ticket close` writes it: it
/// counts `opens`, passes, and names the session the call named.
fn closed(root: &Path, ticket: &str, session: &str) {
    let mut reading = Reading::stamped_over(
        "ticket",
        ticket,
        &xtask::key::Snapshot::disk(root).unwrap(),
        Path::new("."),
    )
    .unwrap();
    reading.status = Status::Pass;
    reading.count("criteria", 3).unwrap();
    reading.count("opens", 0).unwrap();
    session.clone_into(&mut reading.session);
    reading.message = format!("#{ticket}: 3 of 3 criteria ticked; opens nothing");
    Store::of_workspace(root).record(&[reading]).unwrap();
}

fn stop_fixture(name: &str) -> String {
    std::fs::read_to_string(fixtures().join("rules").join(STOP_RULE).join(name)).unwrap()
}

/// The session the fixtures name, the one the recorded Stop stdin carries.
const SESSION: &str = "3f1c0d2a-9b7e-4c55-8f1a-2d6e7b9c0a11";

/// A session that closed a ticket and whose reply carries the language is
/// held once, with the sentences it matched and what to do; the second
/// Stop passes; the hold is a reading of `done` over the rule, carrying
/// the session; and with the list unmet as well, the block carries both.
#[test]
fn the_hook_holds_a_session_that_closed_a_ticket_to_its_reply_once() {
    let root = scratch("reply");
    with_the_stop_rule(&root);
    closed(&root, "28", SESSION);
    let out = hook(&root, &stop_fixture("deny.json")).ok();
    let block: serde_json::Value =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    assert_eq!(block["decision"], "block");
    let reason = block["reason"].as_str().unwrap();
    assert!(
        reason.starts_with("A session that closed a ticket names what it found"),
        "the rule's text: {reason}"
    );
    assert!(reason.contains(&format!("({STOP_RULE})")), "{reason}");
    assert!(reason.contains("This session closed #28;"), "{reason}");
    assert!(
        reason.contains("- \"Left for a person: the two seams.toml rows the commit proposes, and the design sentence in §4.4.\""),
        "{reason}"
    );
    assert!(
        reason.contains(
            "- \"The MCP gap is worth filing but I did not file it; a follow-up can take it.\""
        ),
        "{reason}"
    );
    assert!(
        !reason.contains("Closed #28 through"),
        "a sentence without the language is not named: {reason}"
    );
    assert!(
        reason.contains("File each as a ticket") && reason.contains("the second Stop passes"),
        "what to do: {reason}"
    );
    assert!(
        !reason.contains("done:"),
        "the list is met and not rendered: {reason}"
    );

    let store = Store::of_workspace(&root);
    let reading = store.read("done", STOP_RULE).unwrap().unwrap();
    assert_eq!(reading.status, Status::Fail);
    assert_eq!(reading.rule, STOP_RULE);
    assert_eq!(reading.session, SESSION);
    assert_eq!(reading.numbers["sentences"], 2.into());
    assert_eq!(reading.numbers["closed"], 1.into());
    assert!(
        reading.message.starts_with("the reply after closing #28 reads as work left unfiled in 2 sentences: Left for a person"),
        "{}",
        reading.message
    );

    let store_session = store.read("done", "workspace").unwrap().unwrap().session;
    assert_eq!(
        store_session, SESSION,
        "the Stop's verdict carries the session"
    );

    let active = stop_fixture("deny.json")
        .replace("\"stop_hook_active\": false", "\"stop_hook_active\": true");
    assert_ne!(active, stop_fixture("deny.json"));
    let out = hook(&root, &active).ok();
    assert_eq!(out, "", "the second Stop passes");

    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    closed(&root, "29", SESSION);
    let out = hook(&root, &stop_fixture("deny.json")).stdout;
    let block: serde_json::Value = serde_json::from_str(&out).unwrap();
    let reason = block["reason"].as_str().unwrap();
    let list = reason
        .find("unmet:\n- clippy: no reading for ext/pcre")
        .unwrap();
    // #28's close is this turn's only while the first Stop above fell in
    // the same second as it, the grain `at` has; #29's always is.
    let reply = reason.find("#29; its last reply").unwrap();
    assert!(reason.contains("This session closed #"), "{reason}");
    assert!(list < reply, "the list, then the reply: {reason}");
}

/// A close is this turn's while no Stop of this session has passed since
/// it: a close the previous Stop read is not held again three turns
/// later, whatever the later reply says, and a close by this session
/// after that Stop is.
#[test]
fn a_close_the_previous_stop_read_is_not_held_again_on_a_later_turn() {
    let root = scratch("reply-turns");
    with_the_stop_rule(&root);
    let mut old = Reading::new("ticket", "28");
    old.status = Status::Pass;
    old.count("opens", 0).unwrap();
    SESSION.clone_into(&mut old.session);
    Store::of_workspace(&root).record(&[old]).unwrap();
    let plain = std::fs::read_to_string(fixtures().join("hooks/stop.json")).unwrap();
    let out = hook(&root, &plain).ok();
    assert_eq!(out, "", "the earlier turn's Stop, with a plain reply");
    let out = hook(&root, &stop_fixture("deny.json")).ok();
    assert_eq!(
        out, "",
        "the close at the epoch was read by the Stop before"
    );
    closed(&root, "29", SESSION);
    let out = hook(&root, &stop_fixture("deny.json")).stdout;
    assert!(
        out.contains("This session closed #29;"),
        "a close since that Stop is this turn's: {out}"
    );
}

/// A session that closed no ticket is not read at all: the same reply
/// passes with no close in the store, with a close by another session,
/// with a close that names no session, and with a read of the ticket by
/// this session that closed nothing.
#[test]
fn a_session_that_closed_no_ticket_has_its_reply_read_by_nothing() {
    let root = scratch("reply-none");
    with_the_stop_rule(&root);
    let passes = |what: &str| {
        let out = hook(&root, &stop_fixture("deny.json")).ok();
        assert_eq!(out, "", "{what}");
        assert!(
            Store::of_workspace(&root)
                .read("done", STOP_RULE)
                .unwrap()
                .is_none(),
            "{what}: no reading over the rule"
        );
    };
    passes("no close in the store");
    closed(&root, "28", "another-session");
    passes("a close by another session");
    closed(&root, "29", "");
    passes("a close that names no session");
    let mut read = Reading::new("ticket", "30");
    read.status = Status::Pass;
    read.count("criteria", 3).unwrap();
    SESSION.clone_into(&mut read.session);
    Store::of_workspace(&root).record(&[read]).unwrap();
    passes("a read by this session");
}

/// A reply that names the ticket it opened does not fire: naming a filed
/// thing is what the sensor wants, so a sentence with a number other than
/// the closed ticket's is not read, whatever else it says.
#[test]
fn a_reply_that_names_the_ticket_it_opened_does_not_fire() {
    let root = scratch("reply-opened");
    with_the_stop_rule(&root);
    closed(&root, "28", SESSION);
    let out = hook(&root, &stop_fixture("allow.json")).ok();
    assert_eq!(out, "", "the reply names #40 and #41");
}

/// One fixture per rule at the Stop, tracked or proposed: a Stop stdin
/// whose reply must fire, `deny.json`, and one that must not,
/// `allow.json`, each held over a session that closed a ticket.
#[test]
fn every_rule_at_the_stop_has_a_reply_that_fires_and_one_that_does_not() {
    let root = scratch("reply-fixtures");
    with_the_stop_rule(&root);
    closed(&root, "28", SESSION);
    let manifest = Manifest::load(&root).unwrap();
    let at_the_stop: Vec<&str> = manifest
        .rules
        .iter()
        .filter(|r| !xtask::permit::judged_at_the_call(r) && r.predicate.deny_pattern.is_some())
        .map(|r| r.id.as_str())
        .collect();
    assert_eq!(at_the_stop, [STOP_RULE]);
    for id in at_the_stop {
        let dir = fixtures().join("rules").join(id);
        for (name, fires) in [("deny.json", true), ("allow.json", false)] {
            let stdin = std::fs::read_to_string(dir.join(name)).unwrap();
            let out = hook(&root, &stdin).ok();
            assert_eq!(!out.is_empty(), fires, "{id}/{name}: {out}");
        }
    }
}

// ---------------------------------------------------------------- 7 (#31)

/// The ticket `number` at a [`Tracker`] with the fixture body named, so
/// the `ticket` reading `done` reads is the one the instrument writes,
/// and nothing reaches the tracker.
fn ticket_read(root: &Path, number: u64, body: &str) {
    ticket_act(root, number, body, xtask::ticket::Act::Read);
}

/// The same tracker under any act of the instrument: a close goes through
/// the way the tracker would take it, and the reading recorded is the
/// close's.
fn ticket_act(root: &Path, number: u64, body: &str, act: xtask::ticket::Act) -> Reading {
    let tracker = Tracker::empty("done-ticket");
    tracker.with_issue(&xtask::ticket::Issue {
        body: std::fs::read_to_string(fixtures().join("done/ticket").join(body)).unwrap(),
        ..issue(number)
    });
    let reading = xtask::ticket::run(
        root,
        &xtask::manifest::Manifest::load(root).unwrap(),
        &tracker.gh(),
        act,
        xtask::track::Ticket::new(number),
        None,
    )
    .unwrap();
    Store::of_workspace(root)
        .record(std::slice::from_ref(&reading))
        .unwrap();
    reading
}

fn about_the_ticket(verdict: &Verdict) -> Vec<String> {
    verdict
        .unmet
        .iter()
        .filter(|l| l.starts_with("ticket"))
        .cloned()
        .collect()
}

/// Condition 7 over a branch that records its ticket: no reading is
/// unmet and says what to run; the fixture with an unticked box is unmet
/// with the box named; the fully ticked one is met; and one byte of the
/// working tree after the read makes it stale, since the reading is
/// stamped over the workspace and `done` reaches no tracker.
#[test]
fn the_tracks_ticket_is_held_to_its_boxes_from_the_store_and_stale_after_an_edit() {
    let root = scratch("ticket");
    Repo::at(&root).git(&["config", "branch.main.ticket", "31"]);
    let verdict = judge(&root);
    assert_eq!(verdict.ticket, Some(xtask::track::Ticket::new(31)));
    assert_eq!(
        about_the_ticket(&verdict),
        ["ticket: no reading for #31; run `cargo xtask ticket read`"],
        "{verdict:#?}"
    );
    assert!(!verdict.met());
    let ran = xtask(&root, &["done", "--base", "HEAD"]);
    assert_eq!(ran.code(), 1, "{ran}");
    let out = ran.stdout;
    assert!(out.contains("for #31\n"), "the head line names it: {out}");
    assert!(out.contains("- ticket: no reading for #31"), "{out}");

    ticket_read(&root, 31, "unticked.md");
    let verdict = judge(&root);
    let lines = about_the_ticket(&verdict);
    assert_eq!(lines.len(), 1, "{verdict:#?}");
    assert!(
        lines[0].starts_with("ticket: #31: 1 of 3 criteria ticked; unticked: (1) the thing is measured; (2) harness §4.5 gains the sentence, by a person"),
        "{lines:?}"
    );
    assert!(!verdict.met());

    ticket_read(&root, 31, "ticked.md");
    let verdict = judge(&root);
    assert!(verdict.met(), "{verdict:#?}");
    xtask(&root, &["done", "--base", "HEAD"]).ok();

    // The reading is over the tree; the work changed since, so the claim
    // is re-asserted by reading again, and not by `done` asking gh.
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let verdict = judge(&root);
    assert_eq!(
        about_the_ticket(&verdict),
        [
            "ticket: the reading for #31 is stale; the tree changed since it was read; run `cargo xtask ticket read`"
        ],
        "{verdict:#?}"
    );
    ticket_read(&root, 31, "ticked.md");
    let verdict = judge(&root);
    assert!(about_the_ticket(&verdict).is_empty(), "{verdict:#?}");
}

/// A reading of another ticket is not the branch's: the scope is the
/// number, and #30's reading says nothing for #31.
#[test]
fn a_reading_of_another_ticket_does_not_answer_for_the_branchs_own() {
    let root = scratch("other-ticket");
    Repo::at(&root).git(&["config", "branch.main.ticket", "31"]);
    ticket_read(&root, 30, "ticked.md");
    let verdict = judge(&root);
    assert_eq!(
        about_the_ticket(&verdict),
        ["ticket: no reading for #31; run `cargo xtask ticket read`"],
        "{verdict:#?}"
    );
}

/// A branch that records no ticket, and a detached `HEAD`, are not asked
/// and say nothing about tickets, a reading in the store or not.
#[test]
fn a_branch_that_records_no_ticket_is_not_asked_and_says_nothing_about_tickets() {
    let root = scratch("no-ticket");
    ticket_read(&root, 31, "unticked.md");
    let verdict = judge(&root);
    assert_eq!(verdict.ticket, None);
    assert!(verdict.met(), "{verdict:#?}");
    let out = xtask(&root, &["done", "--base", "HEAD"]).ok();
    assert!(!out.contains("ticket"), "{out}");
    let out = xtask(&root, &["done", "--base", "HEAD", "--hook"]).stdout;
    assert!(!out.contains("ticket"), "{out}");

    Repo::at(&root).git(&["config", "branch.main.ticket", "31"]);
    Repo::at(&root).git(&["checkout", "-q", "--detach"]);
    let verdict = judge(&root);
    assert_eq!(verdict.ticket, None);
    assert!(verdict.met(), "{verdict:#?}");
}

/// The Stop hook holds the turn on the ticket's boxes as it holds it on
/// every other condition, from the store alone.
#[test]
fn the_stop_hook_holds_a_turn_on_the_tickets_unticked_boxes() {
    let root = scratch("hook-ticket");
    Repo::at(&root).git(&["config", "branch.main.ticket", "31"]);
    ticket_read(&root, 31, "unticked.md");
    let stop = std::fs::read_to_string(fixtures().join("hooks/stop.json")).unwrap();
    let out = hook(&root, &stop).ok();
    let block: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let reason = block["reason"].as_str().unwrap();
    assert!(
        reason
            .contains("- ticket: #31: 1 of 3 criteria ticked; unticked: (1) the thing is measured"),
        "{reason}"
    );
    ticket_read(&root, 31, "ticked.md");
    let out = hook(&root, &stop).stdout;
    assert_eq!(out, "", "every box ticked: the turn ends");
}

/// The boxes are the condition and not the close: a `close` the tracker
/// refused with every box ticked is a failed reading and is met here,
/// and a ticket with no box is held, since it cannot be shown done.
#[test]
fn a_refused_close_with_every_box_ticked_is_met_and_a_ticket_with_no_box_is_not() {
    let root = scratch("refused-close");
    Repo::at(&root).git(&["config", "branch.main.ticket", "31"]);
    let store = Store::of_workspace(&root);
    let mut refused = Reading::stamped_over(
        "ticket",
        "31",
        &xtask::key::Snapshot::disk(&root).unwrap(),
        Path::new("."),
    )
    .unwrap();
    refused.status = Status::Fail;
    refused.count("criteria", 3).unwrap();
    refused.count("ticked", 3).unwrap();
    refused.count("unticked", 0).unwrap();
    refused.count("opens", 0).unwrap();
    refused.message =
        "#31: 3 of 3 criteria ticked; closed already, nothing changed at the tracker".into();
    store.record(&[refused]).unwrap();
    let verdict = judge(&root);
    assert!(verdict.met(), "{verdict:#?}");

    let mut boxless = Reading::stamped_over(
        "ticket",
        "31",
        &xtask::key::Snapshot::disk(&root).unwrap(),
        Path::new("."),
    )
    .unwrap();
    boxless.status = Status::Fail;
    boxless.count("criteria", 0).unwrap();
    boxless.count("ticked", 0).unwrap();
    boxless.count("unticked", 0).unwrap();
    boxless.message = "#31 has no box under `## Acceptance criteria`; nothing to hold it to".into();
    store.record(&[boxless]).unwrap();
    let verdict = judge(&root);
    assert_eq!(
        about_the_ticket(&verdict),
        ["ticket: #31 has no box under `## Acceptance criteria`; nothing to hold it to"],
        "{verdict:#?}"
    );
}

/// A withdrawal of the branch's own ticket meets condition 7 (#221): the
/// reading `xtask ticket close --withdrawn` writes passes with every box
/// unticked and `withdrawn` set, the instrument having settled the ticket,
/// and `done` reads `withdrawn` before the boxes. It would be held under
/// the rule this replaces, which read the boxes alone and saw two
/// unticked; the assertion that the verdict is met is what fails on that
/// behaviour. The Stop hook ends the turn on it. A withdrawn ticket with
/// no box at all is met the same way and not held for carrying none; a
/// withdrawal the instrument refused, a box being ticked, is a failing
/// reading and stays held, naming the box.
#[test]
fn a_withdrawn_branch_ticket_is_met_and_a_refused_withdrawal_is_not() {
    use xtask::ticket::{Act, Reach, Successors, Verdict as Close};
    let root = scratch("withdrawn");
    Repo::at(&root).git(&["config", "branch.main.ticket", "31"]);
    let reach = Reach::of(&root).unwrap();
    let withdrawn = |reason: &str| {
        Act::Close(
            Close::Withdrawn(reason.to_owned()),
            Successors::Nothing,
            reach.clone(),
        )
    };

    let reading = ticket_act(
        &root,
        31,
        "withdrawn.md",
        withdrawn("measured on HEAD: not in the tree"),
    );
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert_eq!(reading.numbers["withdrawn"], 1.into(), "{reading:#?}");
    assert_eq!(reading.numbers["unticked"], 2.into(), "{reading:#?}");
    let verdict = judge(&root);
    assert!(about_the_ticket(&verdict).is_empty(), "{verdict:#?}");
    assert!(verdict.met(), "{verdict:#?}");
    xtask(&root, &["done", "--base", "HEAD"]).ok();
    let stop = std::fs::read_to_string(fixtures().join("hooks/stop.json")).unwrap();
    let out = hook(&root, &stop).stdout;
    assert_eq!(out, "", "withdrawn: the turn ends");

    // No box at all, withdrawn: the instrument settled it, so the
    // ticket is not held for carrying no box.
    let store = Store::of_workspace(&root);
    let mut boxless = Reading::stamped_over(
        "ticket",
        "31",
        &xtask::key::Snapshot::disk(&root).unwrap(),
        Path::new("."),
    )
    .unwrap();
    boxless.status = Status::Pass;
    boxless.count("criteria", 0).unwrap();
    boxless.count("ticked", 0).unwrap();
    boxless.count("unticked", 0).unwrap();
    boxless.count("withdrawn", 1).unwrap();
    boxless.message =
        "#31: 0 of 0 criteria ticked; withdrawn: landed by #135; opens nothing".into();
    store.record(&[boxless]).unwrap();
    let verdict = judge(&root);
    assert!(about_the_ticket(&verdict).is_empty(), "{verdict:#?}");

    // Refused: one box of `unticked.md` is ticked, and work half done is
    // not withdrawn, so the reading fails and the condition holds.
    let reading = ticket_act(&root, 31, "unticked.md", withdrawn("nothing here"));
    assert_eq!(reading.status, Status::Fail, "{}", reading.message);
    assert_eq!(reading.numbers["withdrawn"], 1.into(), "{reading:#?}");
    let verdict = judge(&root);
    let lines = about_the_ticket(&verdict);
    assert_eq!(lines.len(), 1, "{verdict:#?}");
    assert!(
        lines[0].starts_with("ticket: #31: 1 of 3 criteria ticked;")
            && lines[0].contains("the thing is built")
            && lines[0].contains("not closed, nothing changed at the tracker"),
        "{lines:?}"
    );
    assert!(!verdict.met(), "{verdict:#?}");
}

/// A withdrawal is renewed past the commits after it (#3476): a bundle
/// branch withdraws its recorded ticket first and then lands its other
/// tickets' commits, as t3459 withdrew #3459 at e61851ebe and landed
/// #3440's and #3449's after it. The first commit stales the withdrawal's
/// reading, and the `ticket read` `done` names reads the ticket the
/// tracker now holds closed and the store's reading of the close, so it
/// writes a passing reading that carries the withdrawal and the condition
/// is met again, and so does a read after that read. A read used to hold
/// the unticked boxes, and a second withdrawal is refused as closed
/// already, so nothing renewed it. A refused withdrawal is no close, and
/// stays unmet.
#[test]
fn a_withdrawal_is_renewed_by_a_read_past_a_later_commit_and_a_refused_one_is_not() {
    use xtask::ticket::{Act, Reach, Successors, Verdict as Close};
    let withdrawn = |root: &Path, reason: &str| {
        let reach = Reach::of(root).unwrap();
        Act::Close(
            Close::Withdrawn(reason.to_owned()),
            Successors::Nothing,
            reach,
        )
    };
    let commit = |root: &Path, name: &str| {
        std::fs::write(root.join(name), "another ticket's work\n").unwrap();
        let repo = Repo::at(root);
        repo.git(&["add", "--", name]);
        repo.git(&["commit", "-q", "-m", name]);
    };
    // The tracker after the close: the ticket closed. A read asks it
    // nothing else, the withdrawal being the store's.
    let read_closed = |root: &Path| {
        let tracker = Tracker::empty("done-withdrawn");
        tracker.with_issue(&xtask::ticket::Issue {
            body: std::fs::read_to_string(fixtures().join("done/ticket/withdrawn.md")).unwrap(),
            state: xtask::ticket::State::Closed,
            ..issue(31)
        });
        let reading = xtask::ticket::run(
            root,
            &xtask::manifest::Manifest::load(root).unwrap(),
            &tracker.gh(),
            Act::Read,
            xtask::track::Ticket::new(31),
            None,
        )
        .unwrap();
        Store::of_workspace(root)
            .record(std::slice::from_ref(&reading))
            .unwrap();
        reading
    };

    let root = scratch("withdrawn-then-commit");
    Repo::at(&root).git(&["config", "branch.main.ticket", "31"]);
    let measured = withdrawn(&root, "measured on HEAD: not in the tree");
    let reading = ticket_act(&root, 31, "withdrawn.md", measured);
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    commit(&root, "after-the-withdrawal.txt");
    let lines = about_the_ticket(&judge(&root));
    assert!(lines.len() == 1 && lines[0].contains("stale"), "{lines:?}");
    let read = read_closed(&root);
    assert_eq!(read.status, Status::Pass, "{}", read.message);
    assert_eq!(read.numbers["withdrawn"], 1.into(), "{read:#?}");
    let said = format!("withdrawn by the close at {}: ", reading.commit);
    assert!(
        read.message.contains(&said) && read.message.contains("measured on HEAD"),
        "{}",
        read.message
    );
    assert!(about_the_ticket(&judge(&root)).is_empty());
    // A read after the read, and after a withdrawal refused since,
    // still says the close.
    commit(&root, "after-the-read.txt");
    let again = ticket_act(&root, 31, "unticked.md", withdrawn(&root, "nothing here"));
    assert_eq!(again.status, Status::Fail, "{}", again.message);
    let again = read_closed(&root);
    assert_eq!(again.status, Status::Pass, "{}", again.message);
    assert_eq!(again.message, read.message, "the close is said again");
    assert!(about_the_ticket(&judge(&root)).is_empty());

    // A withdrawal refused and none taken: the read holds the boxes.
    let root = scratch("withdrawn-refused");
    Repo::at(&root).git(&["config", "branch.main.ticket", "31"]);
    let refused = ticket_act(&root, 31, "unticked.md", withdrawn(&root, "nothing here"));
    assert_eq!(refused.status, Status::Fail, "{}", refused.message);
    commit(&root, "after-the-refusal.txt");
    let read = read_closed(&root);
    assert_eq!(read.status, Status::Fail, "{}", read.message);
    let lines = about_the_ticket(&judge(&root));
    assert!(
        lines.len() == 1 && lines[0].contains("0 of 2 criteria ticked"),
        "{lines:?}"
    );
}

/// The phpt skip the missing engine binary causes is met, and `done`
/// reaches green for a crate that owns a subset on a checkout with no
/// engine built (#132): with no run kept here, a fresh such skip is the
/// one excuse the comparison takes (#725).
///
/// The causes the decision did not excuse are held as before, so the
/// excuse does not spread: a phpt skip of a crate not yet created or one
/// whose every test skipped writes `engine_absent` zero, and a witness
/// skip is held to its status whatever its numbers say, the linked engine
/// having served every snippet there was one for (#119).
#[test]
fn the_phpt_skip_the_missing_engine_binary_causes_is_met_and_the_others_are_not() {
    let root = scratch("no-binary");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let store = Store::of_workspace(&root);
    let skip = |instrument: &str, numbers: &[(&str, u64)]| {
        let mut reading = fresh(&root, instrument, "ext/pcre");
        reading.status = Status::Skip;
        reading.message = format!("{instrument}: the engine's binary is not built");
        for (key, n) in numbers {
            reading.count(key, *n).unwrap();
        }
        reading
    };
    record_asked(&store, &root, "ext/pcre");
    let here = xtask::phpt::kept_run(
        &xtask::key::Snapshot::disk(&root).unwrap(),
        &layout(&root),
        "ext/pcre",
    );
    std::fs::remove_file(here.unwrap()).unwrap();

    // The corpus could not run at all, and nothing else is unmet: the
    // crate is done.
    store
        .record(&[skip(
            "phpt",
            &[
                ("engine_absent", 1),
                ("failed", 0),
                ("diverged", 0),
                ("borked", 0),
            ],
        )])
        .unwrap();
    let verdict = judge(&root);
    assert!(verdict.met(), "{verdict:#?}");

    // A phpt skip of a crate not yet created, or one whose every test
    // skipped: the lane says the binary was there, so this is not the rung.
    store
        .record(&[skip(
            "phpt",
            &[
                ("engine_absent", 0),
                ("failed", 0),
                ("diverged", 0),
                ("borked", 0),
            ],
        )])
        .unwrap();
    let verdict = judge(&root);
    assert_eq!(verdict.unmet.len(), 1, "{verdict:#?}");
    assert!(unrun("ext/pcre")(&verdict.unmet[0]), "{verdict:#?}");

    // A witness skip is unmet on the numbers that once excused it: the
    // binary is no longer why a witness reading is a skip, so what is left
    // is the pinned oracle's absence or a control, and neither is met.
    store
        .record(&[
            skip(
                "phpt",
                &[
                    ("engine_absent", 1),
                    ("failed", 0),
                    ("diverged", 0),
                    ("borked", 0),
                ],
            ),
            skip("witness", &[("run", 6), ("failed", 0), ("missing", 0)]),
        ])
        .unwrap();
    let verdict = judge(&root);
    assert_eq!(verdict.unmet.len(), 1, "{verdict:#?}");
    assert!(
        verdict.unmet[0].starts_with("witness: ext/pcre skip:"),
        "{verdict:#?}"
    );
}

/// The tests whose subject is the tracked workspace itself: each reads the
/// `seams.toml`, `oracle.toml`, registry or sources standing in the
/// workspace, so a person's edit there is what may turn one red (#3091).
/// Every other test here judges by a manifest it builds, or by a
/// scratch's byte-for-byte copy of the tracked one where the binary's
/// hook or `permit` refuses any other (`built.rs`).
mod tracked {
    use super::*;

    /// The `--ri` column is a condition of done as the `--re` one is (harness
    /// §4.5 condition 1, ledger 46; #361): a default extension with a red
    /// `ext/<name>/ri` reading is unmet naming that scope, one with both
    /// columns green is met, and a crate outside `ext/` is asked for no such
    /// scope.
    #[test]
    fn a_default_extension_with_a_red_ri_crosscheck_reading_is_unmet_and_with_both_green_is_met() {
        let manifest = Manifest::load(workspace()).unwrap();
        let scopes = |instrument, name| {
            let krate = manifest.crate_named(name).unwrap();
            done::scopes_of(workspace(), instrument, krate).unwrap()
        };
        assert_eq!(
            scopes("crosscheck", "ext/pcre"),
            ["ext/pcre", "ext/pcre/ri"]
        );
        assert_eq!(scopes("clippy", "ext/pcre"), ["ext/pcre"]);
        assert_eq!(scopes("crosscheck", "php_ext"), ["php_ext"]);

        let root = scratch("ri");
        write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
        let store = Store::of_workspace(&root);
        record_asked(&store, &root, "ext/pcre");
        assert!(judge(&root).met(), "{:#?}", judge(&root));

        let mut red = fresh(&root, "crosscheck", "ext/pcre/ri");
        red.status = Status::Fail;
        red.message = "ext/pcre/ri: the oracle rendered 200 bytes under `php -n --ri pcre`".into();
        store.record(std::slice::from_ref(&red)).unwrap();
        let verdict = judge(&root);
        assert_eq!(
            verdict.unmet,
            [
                "crosscheck: ext/pcre/ri fail: ext/pcre/ri: the oracle rendered 200 bytes under `php -n --ri pcre`"
            ],
            "{verdict:#?}"
        );

        std::fs::remove_file(store.path("crosscheck", "ext/pcre/ri").unwrap()).unwrap();
        let verdict = judge(&root);
        assert_eq!(
            verdict.unmet,
            ["crosscheck: no reading for ext/pcre/ri"],
            "{verdict:#?}"
        );
    }

    /// A crate whose library sources name `debug_assertions` is asked for a
    /// second `clippy` reading, the release profile's at `<name>/release`, and
    /// is unmet without it (#2392); a mention in a comment or a string asks
    /// nothing. `php_heap` and `php_engine` carry the split in the tree.
    #[test]
    fn a_crate_whose_sources_name_debug_assertions_is_asked_for_a_release_clippy_reading() {
        let manifest = Manifest::load(workspace()).unwrap();
        for name in ["php_heap", "php_engine"] {
            let krate = manifest.crate_named(name).unwrap();
            let scopes = done::scopes_of(workspace(), "clippy", krate).unwrap();
            assert_eq!(scopes, [name.to_owned(), format!("{name}/release")]);
        }

        let root = scratch("release");
        let commented =
            "// debug_assertions\npub fn f() -> &'static str { \"debug_assertions\" }\n";
        write(&root, "ext/pcre/src/lib.rs", commented);
        assert!(!asked_of(&root, "ext/pcre").contains(&("clippy", "ext/pcre/release".into())));
        write(
            &root,
            "ext/pcre/src/lib.rs",
            "#[cfg(debug_assertions)]\npub fn f() {}\n",
        );
        let store = Store::of_workspace(&root);
        record_asked(&store, &root, "ext/pcre");
        assert!(judge(&root).met(), "{:#?}", judge(&root));
        std::fs::remove_file(store.path("clippy", "ext/pcre/release").unwrap()).unwrap();
        let verdict = judge(&root);
        assert_eq!(
            verdict.unmet,
            ["clippy: no reading for ext/pcre/release"],
            "{verdict:#?}"
        );
    }

    /// The row is not `permit`'s to judge: a rule at the Stop is left out of
    /// the rules judged at the call, so the call fixtures are not asked of it.
    #[test]
    fn a_rule_at_the_stop_is_not_judged_at_the_call() {
        let manifest = Manifest::parse(&with_the_stop_row(Tracked::workspace()).text()).unwrap();
        let rule = manifest.rule(STOP_RULE).unwrap();
        assert!(!xtask::permit::judged_at_the_call(rule));
    }
}

//! `xtask done`, the definition of done (harness-design.md §4.5, §13 D.16,
//! D.18): the six conditions over a scratch repository, each provoked by a
//! fixture and read back from the verdict; the Stop hook shapes; and the
//! wiring of the Stop hook (the two git hooks run in `tests/hooks.rs`).
//! The instruments whose readings `done` reads have their own fixtures
//! beside them; here a reading is
//! written by hand into the store, since what is under test is how `done`
//! reads it, not how it was measured.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use xtask::divergence::Registry;
use xtask::done::{self, FRESH, Verdict};
use xtask::manifest::Manifest;
use xtask::reading::{Reading, Status, Store};
use xtask::track::Base;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn read(path: &str) -> String {
    std::fs::read_to_string(workspace().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

fn write(root: &Path, path: &str, text: &str) {
    let file = root.join(path);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, text).unwrap();
}

fn sh(root: &Path, args: &[&str]) -> String {
    let out = Command::new(args[0])
        .args(&args[1..])
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "x")
        .env("GIT_AUTHOR_EMAIL", "x@x")
        .env("GIT_COMMITTER_NAME", "x")
        .env("GIT_COMMITTER_EMAIL", "x@x")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

/// One commit of everything in the working tree.
fn commit(root: &Path, message: &str) {
    sh(root, &["git", "add", "-A"]);
    sh(
        root,
        &[
            "git",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            message,
        ],
    );
}

/// A scratch repository: the tracked manifest, pin and registry, and the
/// fixture's `ext/pcre` crate, committed once. Every change the tests make
/// after this is the diff `done` judges with `--base HEAD`.
fn scratch(name: &str) -> PathBuf {
    repository(name, &workspace().join("docs/divergence.toml"))
}

/// The same, with the fixture's registry: one open row on ext/pcre's phpt
/// subset, committed in the base so the registry itself is not in the diff.
fn scratch_with_an_open_row(name: &str) -> PathBuf {
    repository(name, &fixtures().join("done/divergence.toml"))
}

/// The branch is `main` by name and not by default: `git init` takes it
/// from `init.defaultBranch`, which is `main` on a workstation that set it
/// and `master` on a runner that did not, and a scratch repository on
/// `master` is on no track, falls back to `main`, and has no `main` to
/// take a merge base with. The three tests that pass no `--base` read the
/// branch this line pins.
fn repository(name: &str, registry: &Path) -> PathBuf {
    let root = std::env::temp_dir().join(format!("xtask-done-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
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
    write(&root, ".gitignore", "target/\n");
    sh(&root, &["git", "init", "-q", "-b", "main"]);
    commit(&root, "scratch");
    root
}

fn judge(root: &Path) -> Verdict {
    judge_since(root, Some("HEAD"))
}

fn judge_since(root: &Path, base: Option<&str>) -> Verdict {
    let manifest = Manifest::load(root).unwrap();
    let registry = Registry::load(root).unwrap();
    let store = Store::of_workspace(root);
    done::judge(root, &manifest, &registry, &store, base).unwrap()
}

/// A passing reading of `instrument` over `scope`, stamped through the
/// constructor with the scope's tree now.
fn fresh(root: &Path, instrument: &str, scope: &str) -> Reading {
    let manifest = Manifest::load(root).unwrap();
    let mut reading = Reading::for_scope(instrument, scope, &manifest, root).unwrap();
    reading.status = Status::Pass;
    reading.message = format!("{scope}: measured");
    reading.commit = "0000000".into();
    reading
}

fn xtask(root: &Path, args: &[&str], stdin: Option<&str>) -> (i32, String, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_xtask"))
        .arg("--root")
        .arg(root)
        .args(args)
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    if let (Some(text), Some(mut pipe)) = (stdin, child.stdin.take()) {
        use std::io::Write;
        pipe.write_all(text.as_bytes()).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

// ---------------------------------------------------------------- 1, 2

/// Over a diff in xtask, the harness's own sources, witness and phpt are
/// not on the list, and the four that are pass when fresh; which four is
/// `done`'s own unit test.
#[test]
fn the_harness_crate_is_asked_for_clippy_api_check_budget_and_unsafe_audit_only() {
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
    assert_eq!(verdict.unmet.len(), 4, "{verdict:#?}");
    let store = Store::of_workspace(&root);
    for instrument in ["clippy", "api-check", "budget", "unsafe-audit"] {
        store.record(&[fresh(&root, instrument, "xtask")]).unwrap();
    }
    assert!(judge(&root).unmet.is_empty(), "{:#?}", judge(&root));
}

#[test]
fn with_no_readings_every_instrument_of_the_touched_crate_is_unmet_and_the_exit_is_1() {
    let root = scratch("none");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let verdict = judge(&root);
    assert_eq!(verdict.crates, ["ext/pcre"]);
    assert_eq!(verdict.changed.len(), 1, "{verdict:#?}");
    for instrument in FRESH {
        let line = format!("{instrument}: no reading for ext/pcre");
        assert!(verdict.unmet.contains(&line), "{line}\n{verdict:#?}");
    }
    assert_eq!(verdict.unmet.len(), FRESH.len(), "{verdict:#?}");
    assert!(verdict.notes.is_empty(), "{verdict:#?}");

    let (code, out, err) = xtask(&root, &["done", "--base", "HEAD"], None);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("unmet:"), "{out}");
    assert!(out.contains("- phpt: no reading for ext/pcre"), "{out}");
    assert!(err.contains("6 conditions of done unmet"), "{err}");
}

/// A branch that is on no track is judged since `main`, and the commits
/// it has made since count as its own.
#[test]
fn a_branch_on_no_track_is_judged_since_main_so_commits_on_it_count() {
    let root = scratch("main");
    sh(&root, &["git", "branch", "-M", "main"]);
    sh(&root, &["git", "checkout", "-q", "-b", "pcre-preg-match"]);
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    commit(&root, "preg_match");
    let verdict = judge_since(&root, None);
    assert_eq!(verdict.base.since, "main");
    assert_eq!(
        verdict.base.commit,
        sh(&root, &["git", "rev-parse", "main"]).trim()
    );
    assert_eq!(verdict.crates, ["ext/pcre"], "{verdict:#?}");
    assert_eq!(verdict.unmet.len(), FRESH.len(), "{verdict:#?}");
    let (code, out, _) = xtask(&root, &["done"], None);
    assert_eq!(code, 1);
    assert!(out.contains("1 changed file since"), "{out}");
}

// ------------------------------------------- the base is the track

/// A scratch repository on `t-opus-5/witness`, one commit past the track
/// `t-opus-5`, which is itself one commit past `main`: the track's commit
/// changes `ext/pcre`, this branch's changes `xtask`, so which crates the
/// verdict names says which base it took. Returns the root and the track's
/// commit.
///
/// The track is `origin/t-opus-5` and no local branch, because git's ref
/// store refuses `refs/heads/t-opus-5` beside `refs/heads/t-opus-5/witness`
/// — one path cannot be a file and a directory at once — which is also why
/// an agent's worktree has the track as the remote's branch and not its own.
fn on_a_track(name: &str) -> (PathBuf, String) {
    let root = scratch(name);
    sh(&root, &["git", "branch", "-M", "main"]);
    sh(&root, &["git", "checkout", "-q", "-b", "the-track"]);
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    commit(&root, "the track's own change");
    let track = sh(&root, &["git", "rev-parse", "HEAD"]).trim().to_owned();
    sh(
        &root,
        &["git", "update-ref", "refs/remotes/origin/t-opus-5", &track],
    );
    sh(&root, &["git", "checkout", "-q", "main"]);
    sh(&root, &["git", "branch", "-q", "-D", "the-track"]);
    sh(
        &root,
        &[
            "git",
            "checkout",
            "-q",
            "-b",
            "t-opus-5/witness",
            "origin/t-opus-5",
        ],
    );
    write(&root, "xtask/src/lib.rs", "pub fn measure() {}\n");
    commit(&root, "the turn's change");
    (root, track)
}

#[test]
fn on_a_track_branch_a_bare_done_measures_since_the_track_and_names_it() {
    let (root, track) = on_a_track("track");
    let verdict = judge_since(&root, None);
    assert_eq!(verdict.base.commit, track, "{verdict:#?}");
    assert_eq!(verdict.base.since, "t-opus-5");
    assert_eq!(
        verdict.crates,
        ["xtask"],
        "the track's own change is not this turn's: {verdict:#?}"
    );
    let (code, out, err) = xtask(&root, &["done"], None);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("since t-opus-5 ("), "{out}");
}

/// The head line names where the base came from: the ref and the commit
/// it resolved to, and the commit alone when the ref given was that
/// commit, which is what pre-push passes per ref.
#[test]
fn the_head_line_names_the_ref_and_the_commit_and_a_commit_only_once() {
    let commit = "0f1e2d3c4b5a69788796a5b4c3d2e1f001234567";
    let named = Base {
        since: "t-opus-5".to_owned(),
        commit: commit.to_owned(),
    };
    assert_eq!(named.to_string(), "t-opus-5 (0f1e2d3)");
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
fn a_track_that_is_only_a_remote_tracking_branch_is_still_the_base() {
    let (root, track) = on_a_track("remote-track");
    assert!(
        sh(&root, &["git", "branch", "--list", "t-opus-5"])
            .trim()
            .is_empty(),
        "the track is the remote's branch alone"
    );
    assert_eq!(judge_since(&root, None).base.commit, track);
}

#[test]
fn on_the_track_itself_and_on_main_the_base_is_main() {
    let root = scratch("track-itself");
    sh(&root, &["git", "branch", "-M", "main"]);
    sh(&root, &["git", "checkout", "-q", "-b", "t-opus-5"]);
    assert_eq!(judge_since(&root, None).base.since, "main");
    sh(&root, &["git", "checkout", "-q", "main"]);
    assert_eq!(judge_since(&root, None).base.since, "main");
}

#[test]
fn a_detached_head_or_a_prefix_that_names_no_branch_falls_back_to_main() {
    let root = scratch("no-track");
    sh(&root, &["git", "branch", "-M", "main"]);
    sh(&root, &["git", "checkout", "-q", "-b", "t-opus-9/witness"]);
    assert_eq!(judge_since(&root, None).base.since, "main");
    sh(&root, &["git", "checkout", "-q", "--detach"]);
    assert_eq!(judge_since(&root, None).base.since, "main");
}

#[test]
fn base_overrides_the_track_inference() {
    let (root, track) = on_a_track("base-wins");
    let head = sh(&root, &["git", "rev-parse", "HEAD"]).trim().to_owned();
    let verdict = judge_since(&root, Some("HEAD"));
    assert_eq!(verdict.base.since, "HEAD");
    assert_eq!(verdict.base.commit, head);
    assert_ne!(verdict.base.commit, track);
    assert!(verdict.crates.is_empty(), "{verdict:#?}");
    let (code, out, err) = xtask(&root, &["done", "--base", &track], None);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains(&format!("since {}", &track[..7])), "{out}");
}

#[test]
fn the_stop_hook_holds_a_turn_against_the_tracks_base() {
    let (root, _) = on_a_track("hook-track");
    let (code, out, err) = xtask(
        &root,
        &["done", "--hook"],
        Some("{\"session_id\":\"s\",\"stop_hook_active\":false}"),
    );
    assert_eq!(code, 0, "{out}{err}");
    let block: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
    let reason = block["reason"].as_str().unwrap();
    assert!(reason.contains("since t-opus-5 ("), "{reason}");
    assert!(
        !reason.contains("ext/pcre"),
        "the track's own change is not this turn's: {reason}"
    );
}

#[test]
fn an_unchanged_tree_is_done() {
    let root = scratch("unchanged");
    let verdict = judge(&root);
    assert!(verdict.met(), "{verdict:#?}");
    assert!(verdict.crates.is_empty());
    let (code, out, _) = xtask(&root, &["done", "--base", "HEAD"], None);
    assert_eq!(code, 0);
    assert!(out.contains("every condition of done is met"), "{out}");
}

#[test]
fn fresh_passing_readings_for_the_touched_crate_pass_and_an_edit_after_them_is_stale() {
    let root = scratch("fresh");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let store = Store::of_workspace(&root);
    for instrument in FRESH {
        store
            .record(&[fresh(&root, instrument, "ext/pcre")])
            .unwrap();
    }
    let verdict = judge(&root);
    assert!(verdict.met(), "{verdict:#?}");

    write(
        &root,
        "ext/pcre/src/lib.rs",
        "pub fn preg_match() -> bool { true }\n",
    );
    let verdict = judge(&root);
    assert_eq!(verdict.unmet.len(), FRESH.len(), "{verdict:#?}");
    for line in &verdict.unmet {
        assert!(
            line.contains("the reading for ext/pcre is stale"),
            "{line}\n{verdict:#?}"
        );
    }
}

/// The constructor and `done` agree on the path a scope's tree is hashed
/// over: a reading written through it is fresh for its scope, and one
/// byte under that scope makes every one of them stale (harness-design.md
/// §4.5 condition 1, §13 D.18). The crate's readings go through
/// `for_crate`; the subset's, whose scope is the crate's name, through
/// `for_scope`, the two arms of the mapping a touched crate meets.
#[test]
fn a_reading_through_the_constructor_is_fresh_for_its_scope_and_stale_after_one_byte() {
    let root = scratch("one-byte");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let manifest = Manifest::load(&root).unwrap();
    let krate = manifest.crate_named("ext/pcre").unwrap();
    let store = Store::of_workspace(&root);
    for instrument in FRESH {
        let mut reading = if instrument == "phpt" {
            Reading::for_scope(instrument, "ext/pcre", &manifest, &root).unwrap()
        } else {
            Reading::for_crate(instrument, krate, &root).unwrap()
        };
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
        .filter(|l| {
            l.ends_with("the reading for ext/pcre is stale; ext/pcre changed since it was taken")
        })
        .map(|l| l.split(':').next().unwrap())
        .collect();
    assert_eq!(stale, FRESH, "{verdict:#?}");
}

#[test]
fn a_failing_or_skipped_reading_is_unmet_with_its_message() {
    let root = scratch("failing");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let store = Store::of_workspace(&root);
    for instrument in FRESH {
        let mut reading = fresh(&root, instrument, "ext/pcre");
        match instrument {
            "budget" => {
                reading.status = Status::Fail;
                reading.message = "ext/pcre: 7000 lines over the ceiling of 6000".into();
            }
            "phpt" => {
                reading.status = Status::Skip;
                reading.message = "ext/pcre: no subset run".into();
            }
            _ => {}
        }
        store.record(std::slice::from_ref(&reading)).unwrap();
    }
    let verdict = judge(&root);
    assert_eq!(
        verdict.unmet,
        [
            "budget: ext/pcre fail: ext/pcre: 7000 lines over the ceiling of 6000",
            "phpt: ext/pcre skip: ext/pcre: no subset run",
        ],
        "{verdict:#?}"
    );
}

#[test]
fn a_kernel_or_native_without_a_witness_is_unmet_from_the_witness_reading() {
    let root = scratch("missing");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let store = Store::of_workspace(&root);
    for instrument in FRESH {
        let mut reading = fresh(&root, instrument, "ext/pcre");
        if instrument == "witness" {
            reading.count("missing", 2).unwrap();
        }
        store.record(std::slice::from_ref(&reading)).unwrap();
    }
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
        let mut reading = Reading::for_scope("api-check", "heap", &manifest, root).unwrap();
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
    registry.push_str(
        "\n[[divergence]]\nid = \"phpt:php-src/ext/spl/tests/heap_top.phpt\"\nkind = \"phpt\"\nreason = \"\"\nstatus = \"open\"\nsince = \"2026-09-08\"\n",
    );
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
    let mut manifest = std::fs::read_to_string(root.join("seams.toml")).unwrap();
    manifest.push_str("\n# a person's edit\n");
    write(&root, "seams.toml", &manifest);
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
    let (code, out, _) = xtask(&root, &["done", "--base", "HEAD"], None);
    assert_eq!(code, 1);
    assert!(out.contains("notes:\n- seams.toml is in the diff"), "{out}");
}

// ---------------------------------------------------------------- the hook

#[test]
fn the_hook_blocks_with_the_list_on_the_recorded_stop_stdin_and_passes_when_active() {
    let root = scratch("hook");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let stop = std::fs::read_to_string(fixtures().join("hooks/stop.json")).unwrap();
    let (code, out, err) = xtask(&root, &["done", "--hook"], Some(&stop));
    assert_eq!(code, 0, "{err}");
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
    let (code, out, err) = xtask(&root, &["done", "--hook"], Some(&active));
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "", "the second Stop passes");

    let (code, out, _) = xtask(&root, &["done", "--hook"], Some("not json"));
    assert_eq!(code, 0);
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
    let (code, out, err) = xtask(&root, &["done", "--hook"], Some(&stop));
    assert_eq!(code, 0, "{err}");
    assert_eq!(out, "");
}

// ---------------------------------------------------------------- the reading

#[test]
fn every_verdict_is_one_reading_of_done_over_the_workspace() {
    let root = scratch("reading");
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    let (code, _, _) = xtask(&root, &["done", "--base", "HEAD"], None);
    assert_eq!(code, 1);
    let store = Store::of_workspace(&root);
    let reading = store.read("done", "workspace").unwrap().unwrap();
    assert_eq!(reading.status, Status::Fail);
    assert_eq!(reading.numbers["unmet"], 6.into());
    assert_eq!(reading.numbers["crates"], 1.into());
    assert_eq!(reading.numbers["changed"], 1.into());
    assert!(
        reading.message.starts_with("6 unmet since "),
        "{}",
        reading.message
    );
    assert_eq!(reading.tree().len(), 40);
    assert_eq!(reading.commit.len(), 7);

    let stop = std::fs::read_to_string(fixtures().join("hooks/stop.json")).unwrap();
    xtask(&root, &["done", "--hook"], Some(&stop));
    assert_eq!(
        store.log().unwrap().readings.len(),
        2,
        "the hook writes one too"
    );
}

// ---------------------------------------------------------------- the wiring

#[test]
fn the_stop_hook_runs_done_with_hook() {
    let hooks: serde_json::Value =
        serde_json::from_str(&read("harness-plugins/hooks/hooks.json")).unwrap();
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

/// CI's test job holds the function-hook module the way ledger 8 says
/// (harness-design.md §1 revision 8, §6): the declarations are written
/// by the pinned Claude Code's `/plugin-types` on the runner, since they
/// are gitignored, then `tsc -p harness-plugins` reads them and `claude
/// plugin validate --strict` reads the manifest and the module, all
/// before the crate's tests. The comments are stripped first, so a step
/// named in prose does not stand in for the step.
#[test]
fn ci_test_job_writes_the_declarations_then_typechecks_and_validates_the_module() {
    let text = read(".github/workflows/harness.yml");
    let job = &text[text.find("\n  xtask:").unwrap()..text.find("\n  per-commit:").unwrap()];
    let steps = job
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    let mut last = 0;
    for step in [
        "@anthropic-ai/claude-code@",
        "/plugin-types",
        "tsc -p harness-plugins",
        "plugin validate harness-plugins --strict",
        "run: cargo test -p xtask",
    ] {
        let at = steps
            .find(step)
            .unwrap_or_else(|| panic!("the test job lacks {step}: {steps}"));
        assert!(at > last, "{step} is out of order: {steps}");
        last = at;
    }
}

/// CI per commit (harness-design.md §7): the same instruments over the
/// workspace, then `done`; the reading store is the only thing the run
/// uploads; what it renders is the permits section.
#[test]
fn ci_per_commit_runs_the_instruments_then_done_and_uploads_only_the_store() {
    let text = read(".github/workflows/harness.yml");
    let mut last = 0;
    for step in [
        "cargo fmt --check",
        "xtask clippy",
        "xtask api-check",
        "xtask budget",
        "xtask unsafe-audit",
        "xtask vocab",
        "xtask done",
        "status --section permits",
    ] {
        let at = text
            .find(step)
            .unwrap_or_else(|| panic!("CI lacks {step}: {text}"));
        assert!(at > last, "{step} is out of order: {text}");
        last = at;
    }
    assert_eq!(
        text.matches("upload-artifact").count(),
        1,
        "one upload, the store: {text}"
    );
    let upload = text.find("upload-artifact").unwrap();
    let step = &text[upload..];
    let step = &step[..step.find("\n      - ").unwrap_or(step.len())];
    assert!(
        step.contains("path: target/harness"),
        "the upload is the store: {step}"
    );
    assert!(
        text.contains("fetch-depth: 0"),
        "done needs the merge base with the branch's track: {text}"
    );
}

/// CI per commit runs on the track branches as well as `main`, and judges
/// each since the base its Stop hook took (harness-design.md §4.5, §7). A
/// push is on its branch, so `done` reads the track off it; a pull
/// request's checkout is a detached merge commit, on no branch and so on
/// no track, so the step passes the ref the pull request targets, which
/// is that branch's track.
#[test]
fn ci_runs_the_per_commit_job_on_the_track_branches_and_judges_since_the_same_base() {
    let text = read(".github/workflows/harness.yml");
    let on = &text[text.find("on:").unwrap()..text.find("jobs:").unwrap()];
    let branches = on
        .lines()
        .find(|l| l.trim_start().starts_with("branches:"))
        .unwrap_or_else(|| panic!("no push branches: {on}"));
    assert!(branches.contains("main"), "{branches}");
    assert!(
        branches.contains("t-*"),
        "the track branches are pushed to: {branches}"
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
        step.contains("cargo xtask done\n"),
        "a push passes nothing and gets the track off the branch: {step}"
    );
}

/// #18's other half: the row is what asks, so a crate that is no
/// extension and carries one is unmet without its reading, the way
/// `ext/pcre` is.
#[test]
fn a_crate_with_a_row_and_no_reading_is_unmet_the_way_an_extension_is() {
    let root = scratch("row");
    let seams = std::fs::read_to_string(root.join("seams.toml")).unwrap();
    let with_a_row = seams.replace(
        "name = \"php_heap\"\nbudget = 20000",
        "name = \"php_heap\"\nbudget = 20000\nphpt = [\"Zend/tests\"]",
    );
    assert_ne!(with_a_row, seams, "the tracked manifest still has the row");
    std::fs::write(root.join("seams.toml"), with_a_row).unwrap();
    commit(&root, "the row");

    write(
        &root,
        "crates/php_heap/src/lib.rs",
        "pub fn allocate() {}\n",
    );
    let verdict = judge(&root);
    assert_eq!(verdict.crates, ["php_heap"], "{verdict:#?}");
    assert!(
        verdict
            .unmet
            .contains(&"phpt: no reading for php_heap".to_owned()),
        "{verdict:#?}"
    );
}

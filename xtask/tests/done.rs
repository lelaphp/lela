//! `xtask done`, the definition of done (harness-design.md §4.5, §13 D.16,
//! D.18): the six conditions over a scratch repository, each provoked by a
//! fixture and read back from the verdict; the Stop hook shapes; and the
//! wiring of the Stop hook and pre-push. The instruments whose readings
//! `done` reads have their own fixtures beside them; here a reading is
//! written by hand into the store, since what is under test is how `done`
//! reads it, not how it was measured.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use xtask::divergence::Registry;
use xtask::done::{self, FRESH, Verdict};
use xtask::git::tree_hash;
use xtask::manifest::Manifest;
use xtask::reading::{Reading, Status, Store};

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
    sh(&root, &["git", "init", "-q"]);
    sh(&root, &["git", "add", "."]);
    sh(
        &root,
        &[
            "git",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "scratch",
        ],
    );
    root
}

fn judge(root: &Path) -> Verdict {
    let manifest = Manifest::load(root).unwrap();
    let registry = Registry::load(root).unwrap();
    let store = Store::of_workspace(root);
    done::judge(root, &manifest, &registry, &store, Some("HEAD")).unwrap()
}

/// A passing reading of `instrument` over `scope`, stamped with the
/// scope's tree now.
fn fresh(root: &Path, instrument: &str, scope: &str) -> Reading {
    let mut reading = Reading::new(instrument, scope);
    reading.status = Status::Pass;
    reading.message = format!("{scope}: measured");
    reading.tree = tree_hash(root, Path::new(scope)).unwrap();
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

#[test]
fn the_harness_crate_is_asked_for_clippy_api_check_budget_and_unsafe_audit_only() {
    let asked: Vec<&str> = done::instruments_of(&krate("xtask")).collect();
    assert_eq!(asked, ["clippy", "api-check", "budget", "unsafe-audit"]);
    let asked: Vec<&str> = done::instruments_of(&krate("ext/pcre")).collect();
    assert_eq!(asked, FRESH);

    // Over a diff in xtask, the harness's own sources, witness and phpt are
    // not on the list, and the four that are pass when fresh.
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
    for instrument in done::instruments_of(&krate("xtask")) {
        store.write(&fresh(&root, instrument, "xtask")).unwrap();
    }
    assert!(judge(&root).unmet.is_empty(), "{:#?}", judge(&root));
}

fn krate(name: &str) -> xtask::manifest::Crate {
    toml::from_str(&format!("name = \"{name}\"\nbudget = 1")).unwrap()
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

#[test]
fn the_default_base_is_the_merge_base_with_main_so_commits_on_the_branch_count() {
    let root = scratch("main");
    sh(&root, &["git", "branch", "-M", "main"]);
    sh(&root, &["git", "checkout", "-q", "-b", "pcre-preg-match"]);
    write(&root, "ext/pcre/src/lib.rs", "pub fn preg_match() {}\n");
    sh(&root, &["git", "add", "."]);
    sh(
        &root,
        &[
            "git",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "preg_match",
        ],
    );
    let manifest = Manifest::load(&root).unwrap();
    let registry = Registry::load(&root).unwrap();
    let store = Store::of_workspace(&root);
    let verdict = done::judge(&root, &manifest, &registry, &store, None).unwrap();
    assert_eq!(
        verdict.base,
        sh(&root, &["git", "rev-parse", "main"]).trim()
    );
    assert_eq!(verdict.crates, ["ext/pcre"], "{verdict:#?}");
    assert_eq!(verdict.unmet.len(), FRESH.len(), "{verdict:#?}");
    let (code, out, _) = xtask(&root, &["done"], None);
    assert_eq!(code, 1);
    assert!(out.contains("1 changed file since"), "{out}");
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
        store.write(&fresh(&root, instrument, "ext/pcre")).unwrap();
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
        store.write(&reading).unwrap();
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
        store.write(&reading).unwrap();
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
    let differs = |root: &Path| {
        let mut reading = Reading::new("api-check", "heap");
        reading.status = Status::Fail;
        reading.message =
            "2 public items of php_heap have no line in crates/php_heap/heap.lock: Gc, Zval".into();
        reading.tree = tree_hash(root, Path::new("crates/php_heap")).unwrap();
        reading
    };
    store.write(&differs(&root)).unwrap();
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
    store.write(&differs(&root)).unwrap();
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
    assert_eq!(reading.tree.len(), 40);
    assert_eq!(reading.commit.len(), 7);

    let stop = std::fs::read_to_string(fixtures().join("hooks/stop.json")).unwrap();
    xtask(&root, &["done", "--hook"], Some(&stop));
    assert_eq!(store.log().unwrap().len(), 2, "the hook writes one too");
}

// ---------------------------------------------------------------- the wiring

#[test]
fn the_stop_hook_runs_done_with_hook_and_pre_push_runs_done() {
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

    let pre_push = workspace().join("scripts/hooks/pre-push");
    let text = std::fs::read_to_string(&pre_push).unwrap();
    assert!(text.starts_with("#!/"), "{text}");
    assert!(text.contains("cargo xtask done"), "{text}");
    // The push is judged against what the remote has: the remote sha git
    // hands the hook on stdin is the base, and a ref the remote lacks
    // (all zeros) falls back to done's own default.
    assert!(text.contains("--base \"$remote_sha\""), "{text}");
    assert!(
        text.contains("0000000000000000000000000000000000000000"),
        "{text}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&pre_push).unwrap().permissions().mode();
        assert!(mode & 0o111 != 0, "pre-push is executable: {mode:o}");
    }
}

/// Pre-commit (harness-design.md §6 wiring): the instruments that exist,
/// in the order the design lists them, `permit --diff` over the staged
/// diff, and `computed run` on the region files plus the local one by
/// name, with what it wrote re-staged and a hand edit refused.
#[test]
fn pre_commit_runs_the_instruments_permit_over_the_diff_and_computed_run() {
    let pre_commit = workspace().join("scripts/hooks/pre-commit");
    let whole = std::fs::read_to_string(&pre_commit).unwrap();
    assert!(whole.starts_with("#!/"), "{whole}");
    // The commands, not the comment that describes them.
    let text: String = whole
        .lines()
        .filter(|l| !l.trim_start().starts_with('#'))
        .collect::<Vec<_>>()
        .join("\n");
    let mut last = 0;
    for step in [
        "cargo fmt --check",
        "clippy --staged",
        "api-check",
        "budget",
        "unsafe-audit",
        "vocab",
        "permit --diff",
        "computed run",
    ] {
        let at = text
            .find(step)
            .unwrap_or_else(|| panic!("pre-commit lacks {step}: {text}"));
        assert!(at > last, "{step} is out of order: {text}");
        last = at;
    }
    for file in [
        "AGENTS.md",
        "ext/AGENTS.md",
        "data/INDEX.md",
        "docs/divergence.md",
        ".claude/harness.local.md",
    ] {
        assert!(text.contains(file), "computed run names {file}: {text}");
    }
    assert!(
        text.contains("git add"),
        "what computed wrote is re-staged: {text}"
    );
    assert!(
        text.contains("computed check"),
        "a hand edit is refused: {text}"
    );
    assert!(
        !text.contains("--force"),
        "a person decides --force: {text}"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&pre_commit).unwrap().permissions().mode();
        assert!(mode & 0o111 != 0, "pre-commit is executable: {mode:o}");
    }
}

/// CI per commit (harness-design.md §7): the same instruments over the
/// workspace, then `done`; the reading store is the only thing the run
/// uploads; the summary is the permits section.
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
        "done needs the merge base with main: {text}"
    );
}

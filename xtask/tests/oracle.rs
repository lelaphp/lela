//! The oracle module (harness-design.md §4.3): `run` executes a program under
//! the pinned recipe and regime and returns what it wrote, normalised; `compare`
//! returns the verdict every instrument uses and never lets one diff alone.
//!
//! The tests that spawn php need the pinned binary at the path the pin names
//! and say so on stderr when it is absent; the rest run anywhere, the tree
//! `Source::tree` answers being a fixture's.

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::divergence::Registry;
use xtask::manifest::Manifest;
use xtask::oracle::{self, Pin, Program, TREE_VARIABLE, Verdict};

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

/// The tracked pin, when the binary it names is on this machine.
fn pin() -> Option<Pin> {
    let pin = Manifest::load(workspace()).unwrap().pin;
    if pin.oracle.path.is_file() {
        Some(pin)
    } else {
        eprintln!(
            "the pinned php is not at {}; skipping",
            pin.oracle.path.display()
        );
        None
    }
}

fn registry() -> Registry {
    Registry::load(workspace()).unwrap()
}

#[test]
fn a_snippet_runs_as_witness_php_and_its_output_is_normalised() {
    let Some(pin) = pin() else { return };
    // CRLF, blanks inside (kept: the trim is of the ends, run-tests.php's),
    // an error message with the script's path and line, the scratch
    // directory itself, bytes on stderr, an exit code.
    let snippet = r#"echo "a\r\nb  \n"; trigger_error("x"); echo getcwd(), "\n"; fwrite(STDERR, "err\r\n"); exit(3);"#;
    let out = pin.oracle().run(Program::Snippet(snippet), None).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "a\nb  \n\nNotice: x in %SCRATCH%/witness.php on line 2\n%SCRATCH%"
    );
    assert_eq!(String::from_utf8_lossy(&out.stderr), "err");
    assert_eq!(out.exit, Some(3));
}

#[test]
fn a_snippet_gives_the_bytes_php_gives_by_hand_under_the_recipe() {
    let Some(pin) = pin() else { return };
    let snippet = r#"var_dump(ini_get("precision"), ini_get("date.timezone"), getenv("LANG"), getenv("HOME"), error_reporting());"#;
    let out = pin.oracle().run(Program::Snippet(snippet), None).unwrap();

    // By hand: the recipe's arguments and ini as `-d`, the environment
    // cleared and rebuilt, the snippet as witness.php in a directory of its own.
    let dir = std::env::temp_dir().join(format!("xtask-oracle-by-hand-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(&pin.recipe.script_name),
        format!("{}{snippet}", pin.recipe.preamble),
    )
    .unwrap();
    let mut cmd = Command::new(&pin.oracle.path);
    cmd.args(&pin.recipe.args);
    for ini in &pin.recipe.ini {
        cmd.arg("-d").arg(ini);
    }
    cmd.arg(&pin.recipe.script_name)
        .current_dir(&dir)
        .env_clear()
        .envs(&pin.recipe.env);
    let by_hand = cmd.output().unwrap();
    let by_hand = String::from_utf8(by_hand.stdout).unwrap();
    let trim = |c: char| pin.recipe.normalise.trim.contains(c);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        by_hand.replace("\r\n", "\n").trim_matches(trim),
        "the module adds nothing the recipe does not say"
    );
    assert!(
        by_hand.contains("string(2) \"14\"") && by_hand.contains("string(3) \"UTC\""),
        "the recipe's ini reached php: {by_hand}"
    );
    assert!(
        by_hand.contains("string(1) \"C\"") && by_hand.contains("bool(false)"),
        "the environment is the recipe's and nothing of the shell's: {by_hand}"
    );
}

#[test]
fn a_regime_adds_its_ini_after_the_recipe() {
    let Some(pin) = pin() else { return };
    // Without a regime opcache is not enabled on the CLI and reports no
    // status; `jit-off` enables it with the tier off; `jit-tracing` turns
    // the tier on.
    let snippet = r"$s = opcache_get_status(); var_dump($s === false ? null : $s['jit']['on']);";
    let none = pin.oracle().run(Program::Snippet(snippet), None).unwrap();
    assert_eq!(String::from_utf8_lossy(&none.stdout), "NULL");
    let off = pin
        .oracle()
        .run(Program::Snippet(snippet), Some("jit-off"))
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&off.stdout), "bool(false)");
    let tracing = pin
        .oracle()
        .run(Program::Snippet(snippet), Some("jit-tracing"))
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&tracing.stdout), "bool(true)");

    let err = pin
        .oracle()
        .run(Program::Snippet(snippet), Some("jit-fast"))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("jit-fast") && err.contains("jit-off"),
        "an unknown regime is named beside the known ones: {err}"
    );
}

#[test]
fn a_file_runs_in_its_own_directory() {
    let Some(pin) = pin() else { return };
    let dir = std::env::temp_dir().join(format!("xtask-oracle-file-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("case.php");
    std::fs::write(&file, "<?php\necho basename(__FILE__), ' ', getcwd();").unwrap();
    let out = pin.oracle().run(Program::File(&file), None).unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "case.php %SCRATCH%");
}

#[test]
fn a_digest_mismatch_refuses_the_run() {
    let Some(mut pin) = pin() else { return };
    let wrong = "0".repeat(32);
    pin.oracle.md5 = wrong.clone();
    let err = pin
        .oracle()
        .run(Program::Snippet("echo 1;"), None)
        .unwrap_err()
        .to_string();
    assert!(err.contains(&wrong), "the expected digest is named: {err}");
    assert!(
        err.contains(&pin.oracle.path.display().to_string()),
        "the binary is named: {err}"
    );
}

#[test]
fn a_program_past_the_timeout_is_refused() {
    let Some(mut pin) = pin() else { return };
    pin.recipe.limits.timeout_s = 1;
    let err = pin
        .oracle()
        .run(Program::Snippet("while (true) {}"), None)
        .unwrap_err()
        .to_string();
    assert!(err.contains("1 s"), "the limit is named: {err}");
}

/// The tracked pin as the file says it, its `[source].path` at `path`,
/// blind to this machine: what `parse` gives, the variable unread.
fn pin_with_path(path: &Path) -> Pin {
    let text = std::fs::read_to_string(workspace().join("oracle.toml")).unwrap();
    let mut in_source = false;
    let text: Vec<String> = text
        .lines()
        .map(|line| {
            if line.starts_with('[') {
                in_source = line.trim() == "[source]";
            }
            if in_source && line.starts_with("path = ") {
                format!("path = {:?}", path.display().to_string())
            } else {
                line.to_owned()
            }
        })
        .collect();
    Pin::parse(&text.join("\n")).unwrap()
}

/// A directory standing where the pinned php-src would.
fn fixture_tree(name: &str) -> PathBuf {
    let tree = std::env::temp_dir().join(format!("xtask-oracle-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&tree).unwrap();
    tree
}

#[test]
fn the_tree_is_what_the_variable_names_when_it_is_a_directory() {
    let named = fixture_tree("named");
    let pin = pin_with_path(Path::new("/nowhere/php-src")).on_this_machine(Some(named.clone()));
    assert_eq!(pin.source.tree().unwrap(), named);
    // Ahead of the pin's path even where that is a directory too.
    let pinned = fixture_tree("pinned");
    let pin = pin_with_path(&pinned).on_this_machine(Some(named.clone()));
    assert_eq!(pin.source.tree().unwrap(), named);
}

#[test]
fn the_tree_is_the_pins_path_when_the_variable_is_unset() {
    let pinned = fixture_tree("fallback");
    let pin = pin_with_path(&pinned);
    assert_eq!(pin.source.tree().unwrap(), pinned);
    // A variable naming no directory is not the tree either.
    let pin = pin_with_path(&pinned).on_this_machine(Some(PathBuf::from("/nowhere/php-src")));
    assert_eq!(pin.source.tree().unwrap(), pinned);
}

#[test]
fn an_absent_tree_names_the_variable_and_the_pins_path_and_not_one_of_them() {
    let pin = pin_with_path(Path::new("/nowhere/pinned/php-src"));
    let err = pin.source.tree().unwrap_err().to_string();
    assert!(err.contains(TREE_VARIABLE), "the variable is named: {err}");
    assert!(err.contains("unset"), "and its state: {err}");
    assert!(
        err.contains("/nowhere/pinned/php-src"),
        "the pin's path is named: {err}"
    );
    assert!(err.contains("php-8.5"), "and the tag the tree is at: {err}");
    // Set to no directory, the value is what is named, so a typo reads
    // as one.
    let pin = pin.on_this_machine(Some(PathBuf::from("/nowhere/named/php-src")));
    let err = pin.source.tree().unwrap_err().to_string();
    assert!(
        err.contains(&format!("{TREE_VARIABLE}=/nowhere/named/php-src")),
        "{err}"
    );
    assert!(err.contains("/nowhere/pinned/php-src"), "{err}");
}

#[test]
fn the_pin_refuses_a_key_outside_its_grammar() {
    let text = std::fs::read_to_string(workspace().join("oracle.toml")).unwrap();
    assert!(Pin::parse(&text).is_ok());
    let err = Pin::parse(&format!("{text}\n[recipe.extra]\nx = 1\n"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("extra"), "the offending key is named: {err}");
}

#[test]
fn compare_is_same_on_equal_bytes() {
    let verdict = oracle::compare(&registry(), "witness:abc", b"x\n", b"x\n");
    assert!(matches!(verdict, Verdict::Same), "{verdict:?}");
}

#[test]
fn compare_is_diverged_for_an_accepted_row() {
    let verdict = oracle::compare(
        &registry(),
        "trace:nested-entry-return",
        b"TRACE stop at 4",
        b"TRACE stop at 3",
    );
    match verdict {
        Verdict::Diverged(row) => {
            assert_eq!(row.id, "trace:nested-entry-return");
            assert!(row.reason.contains("one opline earlier"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn compare_is_differs_with_the_diff_otherwise() {
    let verdict = oracle::compare(
        &registry(),
        "witness:no-such-row",
        b"int(1)\nstring(1) \"a\"\n",
        b"int(1)\nstring(1) \"b\"\n",
    );
    match verdict {
        Verdict::Differs(diff) => {
            assert!(diff.contains("-string(1) \"a\""), "{diff}");
            assert!(diff.contains("+string(1) \"b\""), "{diff}");
            assert!(diff.contains(" int(1)"), "{diff}");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn an_open_or_fixed_row_does_not_accept_a_difference() {
    let text = r#"
[[divergence]]
id = "witness:open"
kind = "witness"
reason = ""
status = "open"
since = "2026-09-07"
[[divergence]]
id = "witness:fixed"
kind = "witness"
reason = "Was accepted; now the engine agrees."
status = "fixed"
since = "2026-09-07"
"#;
    let registry = Registry::parse(text).unwrap();
    for id in ["witness:open", "witness:fixed"] {
        let verdict = oracle::compare(&registry, id, b"a", b"b");
        assert!(matches!(verdict, Verdict::Differs(_)), "{id}: {verdict:?}");
    }
    assert!(matches!(
        oracle::compare(&registry, "witness:open", b"a", b"a"),
        Verdict::Same
    ));
}

/// The fixture comparison that must fail (ticket #5): the oracle's bytes
/// beside an engine's that differ, with no row for the id.
#[test]
fn the_fixture_comparison_fails() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/compare");
    let expected = std::fs::read(dir.join("expected.txt")).unwrap();
    let actual = std::fs::read(dir.join("actual.txt")).unwrap();
    let verdict = oracle::compare(&registry(), "witness:fixture", &expected, &actual);
    let Verdict::Differs(diff) = verdict else {
        panic!("the fixture must fail: {verdict:?}")
    };
    assert!(diff.contains("-float(0.1)"), "{diff}");
    assert!(diff.contains("+float(0.10000000000000001)"), "{diff}");
}

/// `verdict` is `compare` with the caller's match standing where byte
/// equality stands, so an expectation that is a pattern still asks the
/// registry and nothing decides a difference twice (§4.3, ledger 12).
#[test]
fn verdict_is_compare_with_the_callers_match_in_place_of_equal_bytes() {
    let registry = registry();
    for (expected, actual) in [(&b"a"[..], &b"a"[..]), (b"a", b"b")] {
        let met = expected == actual;
        assert_eq!(
            oracle::verdict(&registry, "witness:x", met, expected, actual),
            oracle::compare(&registry, "witness:x", expected, actual)
        );
    }
    // A pattern that matched bytes that are not its own is `Same`, which
    // byte equality alone would have called a difference.
    assert!(matches!(
        oracle::verdict(
            &registry,
            "phpt:php-src/x.phpt",
            true,
            b"%s on line %d",
            b"one on line 3"
        ),
        Verdict::Same
    ));
    // One that did not match asks the registry, and an accepted row
    // answers a pattern as it answers bytes.
    let missed = oracle::verdict(
        &registry,
        "trace:nested-entry-return",
        false,
        b"TRACE stop at %d",
        b"TRACE stop at 3",
    );
    match missed {
        Verdict::Diverged(row) => assert_eq!(row.id, "trace:nested-entry-return"),
        other => panic!("{other:?}"),
    }
    let unaccepted = oracle::verdict(&registry, "phpt:php-src/no-row.phpt", false, b"%d", b"x");
    match unaccepted {
        Verdict::Differs(diff) => assert!(diff.contains("-%d") && diff.contains("+x"), "{diff}"),
        other => panic!("{other:?}"),
    }
}

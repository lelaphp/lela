//! The oracle module (harness-design.md §4.3): `run` executes a program under
//! the pinned recipe and regime and returns what it wrote, normalised; `compare`
//! returns the verdict every instrument uses and never lets one diff alone.
//!
//! The tests that spawn php need the pinned binary at the path the pin names
//! and say so on stderr when it is absent; the rest run anywhere, the tree
//! `Source::tree` answers being a fixture's.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use xtask::divergence::Registry;
use xtask::manifest::Manifest;
use xtask::oracle::{
    self, PROMISE_VARIABLE, Pin, Program, Promise, Promised, Runner, TREE_VARIABLE, Verdict,
};
use xtask::scratch::Tree;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

/// The tracked pin, when the binary it names is on this machine; a
/// failure where this machine promised the binary (#38).
fn pin() -> Option<Pin> {
    let pin = Manifest::load(workspace()).unwrap().pin;
    // The path borrows the pin, so the question is asked of the unit and
    // the pin handed back whole.
    Promise::Oracle
        .or_skip(pin.oracle.file().map(drop))
        .unwrap()
        .map(|()| pin)
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

/// The load number (#317): `extension #<digits>` is `extension #<token>`
/// under the pin's `extension_number_token` row, on every occurrence, with
/// the phrase before no digit left as it stands; a pin without the row
/// compares the number. Both streams of a run go through the same
/// normalisation, so php's own words on stderr are renumbered as its
/// rendering on stdout is.
#[test]
fn the_load_number_after_extension_is_the_pins_token_in_both_streams() {
    let text = std::fs::read_to_string(workspace().join("oracle.toml")).unwrap();
    let tracked = Pin::parse(&text).unwrap();
    let rules = &tracked.recipe.normalise;
    let token = rules.extension_number_token.as_deref().unwrap();
    assert_eq!(token, "%N%", "the row as the ticket recorded it");
    let rendering = b"Extension [ <persistent> extension #10 ctype version 8.5.9 ] {\n  extension #7 x\n  extension # none\n".to_vec();
    assert_eq!(
        String::from_utf8(oracle::normalise(rules, rendering.clone())).unwrap(),
        "Extension [ <persistent> extension #%N% ctype version 8.5.9 ] {\n  extension #%N% x\n  extension # none"
    );
    let mut without = rules.clone();
    without.extension_number_token = None;
    assert_eq!(
        oracle::normalise(&without, rendering.clone()),
        rendering[..rendering.len() - 1].to_vec(),
        "without the row the number is compared"
    );
    // Through the pin's text: the row absent from the file is `None`, not
    // a refused key.
    let stripped: Vec<&str> = text
        .lines()
        .filter(|line| !line.starts_with("extension_number_token"))
        .collect();
    let stripped = stripped.join("\n");
    let stripped = Pin::parse(&stripped).unwrap();
    assert!(stripped.recipe.normalise.extension_number_token.is_none());

    let Some(pin) = pin() else { return };
    let snippet = r#"echo "extension #10 a\n"; fwrite(STDERR, "extension #42 b");"#;
    let out = pin.oracle().run(Program::Snippet(snippet), None).unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "extension #%N% a");
    assert_eq!(String::from_utf8_lossy(&out.stderr), "extension #%N% b");
}

#[test]
fn a_snippet_gives_the_bytes_php_gives_by_hand_under_the_recipe() {
    let Some(pin) = pin() else { return };
    let snippet = r#"var_dump(ini_get("precision"), ini_get("date.timezone"), getenv("LANG"), getenv("HOME"), error_reporting());"#;
    let out = pin.oracle().run(Program::Snippet(snippet), None).unwrap();

    // By hand: the recipe's arguments and ini as `-d`, the environment
    // cleared and rebuilt, the snippet as witness.php in a directory of its own.
    let dir = Tree::new("oracle-by-hand").unwrap();
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

/// The flag before the program (harness-design.md §4.3): `-l` compiles a
/// program and does not run it, which is how the compile-identity lane
/// measures a compiler over a corpus every file of which was written to
/// be run. The flag stands before the program's path and the recipe's
/// `-d` entries before that, so a lane names a mode of php without the
/// pin moving; an invocation that names none runs what it is given. php
/// names the program as it was given it, the recipe's own name in the
/// scratch directory the run happens in.
#[test]
fn a_flag_stands_before_the_program_and_the_default_names_none() {
    let Some(pin) = pin() else { return };
    let oracle = pin.oracle();
    let snippet = r#"echo "ran\n";"#;

    let lint = [String::from("-l")];
    let linted = oracle
        .run_under(&oracle::Invocation {
            flags: &lint,
            ..oracle::Invocation::new(pin.oracle.path.as_path(), Program::Snippet(snippet))
        })
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&linted.stdout),
        "No syntax errors detected in witness.php"
    );

    let ran = oracle
        .run_under(&oracle::Invocation::new(
            pin.oracle.path.as_path(),
            Program::Snippet(snippet),
        ))
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&ran.stdout), "ran");
}

#[test]
fn a_file_runs_in_its_own_directory() {
    let Some(pin) = pin() else { return };
    let dir = Tree::new("oracle-file").unwrap();
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
fn fixture_tree(name: &str) -> Tree {
    Tree::new(&format!("oracle-{name}")).unwrap()
}

#[test]
fn the_tree_is_what_the_variable_names_when_it_is_a_directory() {
    let named = fixture_tree("named");
    let pin =
        pin_with_path(Path::new("/nowhere/php-src")).on_this_machine(Some(named.to_path_buf()));
    assert_eq!(pin.source.tree().unwrap(), named.path());
    // Ahead of the pin's path even where that is a directory too.
    let pinned = fixture_tree("pinned");
    let pin = pin_with_path(&pinned).on_this_machine(Some(named.to_path_buf()));
    assert_eq!(pin.source.tree().unwrap(), named.path());
}

#[test]
fn the_tree_is_the_pins_path_when_the_variable_is_unset() {
    let pinned = fixture_tree("fallback");
    let pin = pin_with_path(&pinned);
    assert_eq!(pin.source.tree().unwrap(), pinned.path());
    // A variable naming no directory is not the tree either.
    let pin = pin_with_path(&pinned).on_this_machine(Some(PathBuf::from("/nowhere/php-src")));
    assert_eq!(pin.source.tree().unwrap(), pinned.path());
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

/// A git checkout standing where the pinned php-src would, at one commit
/// of its own, and that commit as `git rev-parse HEAD` prints it.
fn fixture_checkout(name: &str) -> (Tree, String) {
    let tree = fixture_tree(name);
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&tree)
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    git(&["init", "-q"]);
    git(&[
        "-c",
        "user.name=xtask",
        "-c",
        "user.email=xtask@example",
        "commit",
        "-q",
        "--allow-empty",
        "-m",
        "the fixture's one commit",
    ]);
    let head = git(&["rev-parse", "HEAD"]);
    (tree, head)
}

#[test]
fn a_checkout_at_the_pins_commit_is_the_tree() {
    let (tree, head) = fixture_checkout("held");
    let mut pin = pin_with_path(&tree);
    pin.source.commit = head[..8].to_owned();
    assert_eq!(pin.source.tree().unwrap(), tree.path());
    // Through the variable as through the pin's path.
    let pin =
        pin_with_path(Path::new("/nowhere/php-src")).on_this_machine(Some(tree.to_path_buf()));
    let mut pin = pin;
    pin.source.commit = head[..8].to_owned();
    assert_eq!(pin.source.tree().unwrap(), tree.path());
}

#[test]
fn a_checkout_at_another_commit_is_refused_naming_both_revisions() {
    let (tree, head) = fixture_checkout("drifted");
    // The tracked pin's commit is upstream's; the fixture's is its own.
    let pin = pin_with_path(&tree);
    let pinned = pin.source.commit.clone();
    let err = pin.source.tree().unwrap_err().to_string();
    assert!(err.contains(&head), "the tree's commit is named: {err}");
    assert!(err.contains(&pinned), "and the pin's: {err}");
    assert!(err.contains("php-8.5"), "with the tag: {err}");
    assert!(
        err.contains(&tree.display().to_string()),
        "and where the tree is: {err}"
    );
    // The variable naming it is no different: a wrong tree is refused, not
    // passed over for the pin's path.
    let pinned_dir = fixture_tree("drifted-fallback");
    let pin = pin_with_path(&pinned_dir).on_this_machine(Some(tree.to_path_buf()));
    let err = pin.source.tree().unwrap_err().to_string();
    assert!(err.contains(&head) && err.contains(TREE_VARIABLE), "{err}");
}

#[test]
fn a_tree_that_is_no_checkout_states_no_commit_and_is_taken_at_its_word() {
    // The fixture trees above have no `.git`; a tarball at the tag is the
    // same, and the pin's commit cannot be read off it.
    let tree = fixture_tree("unpacked");
    assert_eq!(pin_with_path(&tree).source.tree().unwrap(), tree.path());
}

/// What a runner that sets the variable claims: the tree is there and at
/// the pin's commit. Held whenever the variable is set, so a runner whose
/// checkout drifted from `[source].commit` fails here naming both
/// revisions, and never skips as an absent tree would (#37). A runner
/// that promised the tree and set no variable fails here too, naming
/// both (#38).
#[test]
fn a_tree_the_variable_names_is_at_the_pin() {
    if std::env::var_os(TREE_VARIABLE).is_none_or(|value| value.is_empty()) {
        assert!(
            !Promised::load().unwrap().holds(Promise::Tree),
            "{PROMISE_VARIABLE} promised the tree and {TREE_VARIABLE} is unset"
        );
        eprintln!("{TREE_VARIABLE} is unset; nothing claims a tree, skipping");
        return;
    }
    let pin = Manifest::load(workspace()).unwrap().pin;
    let tree = pin.source.tree().unwrap();
    assert!(tree.is_dir(), "{}", tree.display());
}

/// The variable's name is stated in one place that both the code and CI
/// read (#36): `oracle::TREE_VARIABLE`, which `status --section oracle`
/// renders and the workflow reads off it, setting the tree under that
/// name through `$GITHUB_ENV`. So the workflow spells neither name, in
/// an `env:` block or anywhere outside a comment, and it does run the
/// section and read both variables off its JSON; the promise travels the
/// same way, since a tree named and not promised skips in silence where
/// it is absent (#38). The comments are stripped first, so a name cited
/// in prose does not stand in for a setting; the negative control is a
/// key that looks like one of the two under another spelling, which the
/// harness would never read.
#[test]
fn ci_reads_the_variables_off_the_oracle_section_and_spells_neither() {
    let text = std::fs::read_to_string(workspace().join(".github/workflows/harness.yml")).unwrap();
    let mut env = false;
    let mut section = false;
    let mut reads = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with('#') {
            continue;
        }
        for name in [TREE_VARIABLE, PROMISE_VARIABLE] {
            assert!(
                !trimmed.contains(name),
                "line {}: {name} is spelled in the workflow; the section states it",
                n + 1
            );
        }
        if trimmed.contains("status --section oracle") {
            section = true;
        }
        for key in ["tree.variable", "promise.variable"] {
            if trimmed.contains(key) {
                reads.push(key);
            }
        }
        if trimmed == "env:" {
            env = true;
            continue;
        }
        let Some((key, _)) = trimmed.split_once(": ") else {
            env = false;
            continue;
        };
        if !env || !key.chars().all(|c| c.is_ascii_uppercase() || c == '_') {
            env = false;
            continue;
        }
        assert!(
            !key.contains("SRCDIR") && !key.contains("PROMISED"),
            "line {}: {key} is set and neither {TREE_VARIABLE} nor {PROMISE_VARIABLE} is what the harness reads",
            n + 1
        );
    }
    assert!(
        section,
        "no step runs `status --section oracle`, where the names are read:\n{text}"
    );
    assert_eq!(
        reads,
        ["tree.variable", "promise.variable"],
        "the step reads both names off the section's JSON"
    );
    assert!(
        text.contains("GITHUB_ENV"),
        "the names read are set for the steps that follow"
    );
}

/// The promise (#38): what a runner says this machine has of the pin,
/// the words the variable lists. A word outside the two is refused
/// naming them, so a misspelt promise is never an unmade one.
#[test]
fn a_promise_is_the_words_the_variable_lists_and_an_unknown_word_is_refused() {
    let none = Promised::parse("").unwrap();
    assert!(!none.holds(Promise::Tree) && !none.holds(Promise::Oracle));
    let tree = Promised::parse("tree").unwrap();
    assert!(tree.holds(Promise::Tree) && !tree.holds(Promise::Oracle));
    let both = Promised::parse(" oracle\ttree ").unwrap();
    assert!(both.holds(Promise::Tree) && both.holds(Promise::Oracle));
    let err = Promised::parse("tree php-src").unwrap_err().to_string();
    assert!(
        err.contains("php-src") && err.contains(PROMISE_VARIABLE),
        "the word and the variable are named: {err}"
    );
    assert!(
        err.contains("tree") && err.contains("oracle"),
        "the words the variable takes are named: {err}"
    );
}

/// The decision every guarded test takes (#38): a fact the pin names
/// that this machine has not is a skip, said on stderr, unless the
/// machine promised it, in which case it is a failure naming what was
/// looked for and the promise. A fact the machine has is the fact,
/// promised or not.
#[test]
fn where_the_tree_is_promised_an_absent_tree_is_a_failure_naming_what_it_looked_for() {
    let pin = pin_with_path(Path::new("/nowhere/pinned/php-src"))
        .on_this_machine(Some(PathBuf::from("/nowhere/named/php-src")));
    let absent = || pin.source.tree();

    let unpromised = Promised::parse("").unwrap();
    assert!(
        unpromised
            .or_skip(Promise::Tree, absent())
            .unwrap()
            .is_none()
    );
    let other = Promised::parse("oracle").unwrap();
    assert!(other.or_skip(Promise::Tree, absent()).unwrap().is_none());

    let promised = Promised::parse("tree").unwrap();
    let err = promised
        .or_skip(Promise::Tree, absent())
        .unwrap_err()
        .to_string();
    for looked_for in [
        &format!("{TREE_VARIABLE}=/nowhere/named/php-src"),
        "/nowhere/pinned/php-src",
        &pin.source.tag,
        PROMISE_VARIABLE,
    ] {
        assert!(err.contains(looked_for), "{looked_for} is named: {err}");
    }

    let named = fixture_tree("promised");
    let here = pin_with_path(&named);
    assert_eq!(
        promised
            .or_skip(Promise::Tree, here.source.tree())
            .unwrap()
            .unwrap(),
        named.path()
    );
}

/// The binary's half of the same promise: `Binary::file` is the path
/// when a file is there and otherwise names the path and the version,
/// which is what the promise's failure carries.
#[test]
fn an_absent_binary_names_its_path_and_version() {
    let mut pin = pin_with_path(Path::new("/nowhere"));
    pin.oracle.path = PathBuf::from("/nowhere/bin/php");
    let err = pin.oracle.file().unwrap_err().to_string();
    assert!(
        err.contains("/nowhere/bin/php") && err.contains(&pin.oracle.version),
        "{err}"
    );
    let err = Promised::parse("oracle")
        .unwrap()
        .or_skip(Promise::Oracle, pin.oracle.file())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("/nowhere/bin/php") && err.contains(PROMISE_VARIABLE),
        "{err}"
    );
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
            assert_eq!(row.id.as_str(), "trace:nested-entry-return");
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
        Verdict::Diverged(row) => assert_eq!(row.id.as_str(), "trace:nested-entry-return"),
        other => panic!("{other:?}"),
    }
    let unaccepted = oracle::verdict(&registry, "phpt:php-src/no-row.phpt", false, b"%d", b"x");
    match unaccepted {
        Verdict::Differs(diff) => assert!(diff.contains("-%d") && diff.contains("+x"), "{diff}"),
        other => panic!("{other:?}"),
    }
}

/// A stand-in for the engine's binary at `target/<profile>/php` under
/// `root`, stamped `ago` seconds back, so a test says in one line whether
/// the tree has moved past it (#475). The write time is set rather than
/// slept for: the question is which of two files is newer, and a sleep
/// would answer it by wall clock.
fn built(root: &Path, profile: &str, ago: u64) -> PathBuf {
    let path = root.join("target").join(profile).join("php");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, format!("the {profile} engine")).unwrap();
    let at = SystemTime::now() - Duration::from_secs(ago);
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(at)
        .unwrap();
    path
}

/// The engine's sources under `root`, written now: the tree a binary is
/// held to.
fn engine_sources(root: &Path) {
    let src = root.join("crates/php_engine/src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("lib.rs"), "// the kernels\n").unwrap();
}

/// A release binary built once and never again was the lane's engine for
/// every run after, however far the tree moved, since `release` is
/// preferred over `debug` (#475). It is preferred among the binaries the
/// tree has not moved past, and not over them: `cargo build -p php`
/// writes the debug one, and that is what displaces it.
#[test]
fn a_binary_the_tree_has_moved_past_is_passed_over_for_one_it_has_not() {
    let root = fixture_tree("stale-passed-over");
    let stale = built(root.path(), "release", 3600);
    engine_sources(root.path());
    let fresh = built(root.path(), "debug", 0);
    let pin = pin_with_path(root.path());
    assert_eq!(
        Runner::resolve(root.path(), &pin, None).unwrap(),
        Runner::Engine(fresh),
        "the debug binary the tree has not moved past is the lane's engine"
    );
    // And the release binary alone is the skip, not a silent engine.
    std::fs::remove_file(root.path().join("target/debug/php")).unwrap();
    let behind = Runner::resolve(root.path(), &pin, None).unwrap();
    let Runner::Stale(why) = &behind else {
        panic!("{behind:?}")
    };
    assert!(why.contains(&stale.display().to_string()), "{why}");
    assert!(why.contains("crates/php_engine/src/lib.rs"), "{why}");
    assert!(why.contains("cargo build -p php"), "{why}");
    assert_eq!(behind.binary(), None, "nothing runs under it");
    assert_eq!(behind.stamp(), None, "and no reading is stamped with it");
}

/// The engine's binary names itself on the reading that ran it, and a
/// binary the tree has not moved past is the engine as it was before
/// (#475).
#[test]
fn a_binary_the_tree_has_not_moved_past_is_the_engine_and_stamps_its_reading() {
    let root = fixture_tree("fresh-binary");
    engine_sources(root.path());
    let fresh = built(root.path(), "release", 0);
    let pin = pin_with_path(root.path());
    let runner = Runner::resolve(root.path(), &pin, None).unwrap();
    assert_eq!(runner, Runner::Engine(fresh.clone()));
    assert_eq!(runner.binary(), Some(fresh.as_path()));
    let stamp = runner.stamp().unwrap();
    let (path, digest) = stamp
        .split_once(" md5 ")
        .unwrap_or_else(|| panic!("{stamp}"));
    assert_eq!(path, fresh.display().to_string());
    assert_eq!(digest.len(), 32, "{stamp}");
    assert!(digest.bytes().all(|b| b.is_ascii_hexdigit()), "{stamp}");
}

/// A tree with no engine source in it is not one a binary can have fallen
/// behind: the harness's own tests lay a workspace down without the
/// crates, and a stand-in binary there is the lane's engine whatever its
/// write time (#475).
#[test]
fn a_tree_with_no_engine_source_leaves_every_binary_the_engine() {
    let root = fixture_tree("no-engine-source");
    let old = built(root.path(), "release", 86_400);
    let pin = pin_with_path(root.path());
    assert_eq!(
        Runner::resolve(root.path(), &pin, None).unwrap(),
        Runner::Engine(old)
    );
}

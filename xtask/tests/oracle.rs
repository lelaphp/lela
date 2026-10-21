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

use xtask::divergence::{Registry, Status};
use xtask::oracle::{
    self, PROMISE_VARIABLE, Pin, Program, Promise, Promised, Regime, Runner, SKIPPED_VARIABLE,
    Split, TREE_VARIABLE, Verdict,
};
use xtask::scratch::Tree;

mod common {
    pub mod manifest;
    pub mod registry;
    pub mod repo;
    pub mod run;
}
use common::registry::{Row, rows};
use common::repo::Repo;
use common::run::xtask;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

/// The tracked pin, when the binary it names is on this machine; a
/// failure where this machine promised the binary (#38).
fn pin() -> Option<Pin> {
    let pin = common::manifest::pin();
    // The path borrows the pin, so the question is asked of the unit and
    // the pin handed back whole.
    Promise::Oracle
        .or_skip(pin.oracle.file().map(drop))
        .unwrap()
        .map(|()| pin)
}

/// A registry of one accepted row, so these tests hold the comparison's
/// mechanics and not which rows a person keeps in `docs/divergence.toml`:
/// a row dropped there breaks nothing here (#2142).
fn registry() -> Registry {
    rows(&[Row::accepted("trace:fixture")
        .reason("A difference a person accepted, for the comparison's tests.")])
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
    assert_eq!(out.exit, xtask::oracle::Ended::Status(3));
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
    let snippet = r#"var_dump(ini_get("precision"), ini_get("date.timezone"), getenv("LANG"), getenv("PATH"), error_reporting());"#;
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

/// A file standing where a binary would that writes each argument it was
/// handed on a line of its own, and the tree that holds it.
fn argv_echo() -> (Tree, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = Tree::new("oracle-argv").unwrap();
    let path = dir.join("argv");
    std::fs::write(
        &path,
        "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\"; done\n",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (dir, path)
}

/// A regime may carry `args` (#1780, #1887): the tracked pin's
/// `[regime.extended-info]`, the row #1780 ruled, is a block of arguments
/// alone, and a regime of ini
/// alone carries none. A regime's arguments stand after the recipe's, as
/// its ini stands after the recipe's, ahead of every `-d` and of the
/// lane's flags and the program; a run under no regime, or under one of
/// ini alone, is handed nothing it was not handed before. No php is asked:
/// the binary is a file that says what it was given.
#[test]
fn a_regimes_args_stand_after_the_recipes_and_ahead_of_every_ini() {
    let text = std::fs::read_to_string(workspace().join("oracle.toml")).unwrap();
    let pin = Pin::parse(&text).unwrap();
    let extended = pin.regime("extended-info").unwrap();
    assert_eq!(extended.args, ["-e"]);
    assert!(extended.ini.is_empty(), "{extended:?}");
    assert!(pin.regime("jit-off").unwrap().args.is_empty());

    let (_dir, echo) = argv_echo();
    let oracle = pin.oracle();
    let lint = [String::from("-l")];
    let argv = |regime: Option<&str>| -> Vec<String> {
        let out = oracle
            .run_under(&oracle::Invocation {
                regimes: regime.as_slice(),
                flags: &lint,
                ..oracle::Invocation::new(&echo, Program::Snippet("echo 1;"))
            })
            .unwrap();
        String::from_utf8(out.stdout)
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect()
    };
    let recipe = pin.recipe.args.len();
    let ini = 2 * pin.recipe.ini.len();

    let plain = argv(None);
    assert_eq!(plain[..recipe], pin.recipe.args[..], "{plain:?}");
    assert_eq!(plain[recipe], "-d", "{plain:?}");
    assert_eq!(plain[recipe + ini..], ["-l", "witness.php"], "{plain:?}");
    assert!(!plain.contains(&"-e".to_owned()), "{plain:?}");

    let under = argv(Some("extended-info"));
    assert_eq!(under[..recipe], pin.recipe.args[..], "{under:?}");
    assert_eq!(under[recipe], "-e", "{under:?}");
    assert_eq!(under[recipe + 1..], plain[recipe..], "{under:?}");

    let ini_alone = argv(Some("jit-off"));
    assert_eq!(ini_alone[..recipe + ini], plain[..recipe + ini]);
    assert!(!ini_alone.contains(&"-e".to_owned()), "{ini_alone:?}");
}

/// A regime's arguments reach php itself: `-e` compiles with extended
/// info, which no ini setting reaches, so php's op-array dump under it
/// carries the `EXT_STMT` oplines the dump without it does not (#1780,
/// #1822). Regimes compose in the order named: the tracked `dump` and
/// `extended-info` rows, one after the other, are the run of a row that
/// carries both, which is how `compile-identity --regime extended-info`
/// dumps without a combined row of its own.
#[test]
fn a_regimes_args_reach_php() {
    let Some(mut pin) = pin() else { return };
    let dump = pin.regime("dump").unwrap().ini.clone();
    for (name, args) in [
        ("dump-plain", vec![]),
        ("dump-extended", vec!["-e".to_owned()]),
    ] {
        pin.regime.insert(
            name.to_owned(),
            Regime {
                ini: dump.clone(),
                args,
            },
        );
    }
    let oracle = pin.oracle();
    oracle.asserted().unwrap();
    let lint = [String::from("-l")];
    let dumped = |regimes: &[&str]| -> String {
        let out = oracle
            .run_under(&oracle::Invocation {
                regimes,
                flags: &lint,
                ..oracle::Invocation::new(
                    pin.oracle.path.as_path(),
                    Program::Snippet("$a = 1;\necho $a;"),
                )
            })
            .unwrap();
        String::from_utf8(out.stderr).unwrap()
    };
    let plain = dumped(&["dump-plain"]);
    let extended = dumped(&["dump-extended"]);
    assert!(
        plain.contains("ASSIGN") && !plain.contains("EXT_STMT"),
        "{plain}"
    );
    assert!(extended.contains("EXT_STMT"), "{extended}");
    assert_eq!(dumped(&["dump", "extended-info"]), extended);
    assert_eq!(dumped(&["dump"]), plain);
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
    let head = Repo::init(&tree).commit("the fixture's one commit");
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
    let pin = common::manifest::pin();
    let tree = pin.source.tree().unwrap();
    assert!(tree.is_dir(), "{}", tree.display());
}

/// The `.phpt` files under `dir`, `.git` left out: what `find . -name
/// '*.phpt' | wc -l` counts in a checkout.
fn phpt_files_under(dir: &Path) -> u64 {
    let mut found = 0;
    for entry in std::fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if entry.file_type().unwrap().is_dir() {
            if path.file_name().is_some_and(|name| name != ".git") {
                found += phpt_files_under(&path);
            }
        } else if path.extension().is_some_and(|e| e == "phpt") {
            found += 1;
        }
    }
    found
}

/// The tree the variable names holds every `.phpt` the pin counts,
/// `[source].phpt_files`, as `find . -name '*.phpt'` counts them at the
/// tag (#1713). A checkout narrower than the corpus fails here naming
/// both numbers, before a lane measures less in silence: the hand-written
/// list CI checked out by had left 241 tests of `sapi/cgi`, `sapi/fpm`,
/// `sapi/phpdbg` and `ext/intl/uchar` out, and their subsets read zero
/// tests. Skipped where nothing claims a tree, as the test above is.
#[test]
fn a_tree_the_variable_names_holds_every_test_the_pin_counts() {
    if std::env::var_os(TREE_VARIABLE).is_none_or(|value| value.is_empty()) {
        assert!(
            !Promised::load().unwrap().holds(Promise::Tree),
            "{PROMISE_VARIABLE} promised the tree and {TREE_VARIABLE} is unset"
        );
        eprintln!("{TREE_VARIABLE} is unset; nothing claims a tree, skipping");
        return;
    }
    let source = common::manifest::pin().source;
    let tree = source.tree().unwrap();
    assert_eq!(
        phpt_files_under(&tree),
        source.phpt_files,
        "{} holds the left-hand number of .phpt files and the pin counts the right-hand one: the checkout is not the corpus `xtask status --section corpus` names",
        tree.display()
    );
}

/// Every file CI runs, the workflows and the local actions they use
/// (`.github/actions/<name>/action.yml`), by name and text (#2253).
fn ci_files() -> Vec<(String, String)> {
    let github = workspace().join(".github");
    let mut paths: Vec<_> = std::fs::read_dir(github.join("workflows"))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .collect();
    paths.extend(
        std::fs::read_dir(github.join("actions"))
            .unwrap()
            .flatten()
            .map(|e| e.path().join("action.yml"))
            .filter(|p| p.is_file()),
    );
    paths.sort();
    paths
        .into_iter()
        .map(|p| {
            let name = p.strip_prefix(&github).unwrap().display().to_string();
            (name, std::fs::read_to_string(&p).unwrap())
        })
        .collect()
}

/// No workflow or action holds a directory of the corpus (#1713): a
/// checkout of php-src takes its sparse patterns off `status --section
/// corpus`, on stdin, so no step lists one and none can fall behind a
/// subset the manifest adds; and one action checks it out, for every job
/// that reads it (#2253). A token anchored as a pattern under a root's first
/// directory is one, `/sapi/cli/tests/` as much as `/ext/*/`; a lane's
/// `--subset Zend/tests` is the lane's composition (harness §7) and no
/// pattern. Comments are stripped, so prose that cites a pattern does not
/// count.
#[test]
fn no_workflow_holds_a_directory_of_the_corpus() {
    let out = xtask(
        workspace(),
        &["status", "--section", "corpus", "--format", "json"],
    )
    .ok();
    let corpus: serde_json::Value = serde_json::from_str(&out).unwrap();
    let firsts: Vec<String> = corpus["patterns"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            let anchored = p.as_str().unwrap().trim_start_matches('/');
            format!("/{}/", anchored.split('/').next().unwrap_or_default())
        })
        .collect();
    let mut checked_out = 0;
    for (name, text) in ci_files() {
        let mut reads_section = false;
        let mut checks_out = false;
        for (n, line) in text.lines().enumerate() {
            let line = line.trim_start();
            if line.starts_with('#') {
                continue;
            }
            reads_section |= line.contains("status --section corpus");
            for token in line.split(|c: char| c.is_whitespace() || "\"'=:,[]".contains(c)) {
                assert!(
                    !firsts.iter().any(|first| token.starts_with(first.as_str())),
                    "{name}:{}: {token} is a directory of the corpus written by hand; the patterns are `status --section corpus`'s",
                    n + 1
                );
            }
            if line.contains("sparse-checkout set") {
                checks_out = true;
                checked_out += 1;
                assert!(
                    line.contains("--stdin") && line.contains(".patterns[]"),
                    "{name}:{}: the patterns are the section's `.patterns[]`, read on stdin: {line}",
                    n + 1
                );
            }
        }
        assert!(
            !checks_out || reads_section,
            "{name} checks php-src out and never runs `status --section corpus`"
        );
    }
    assert_eq!(
        checked_out, 1,
        "the corpus action checks the corpus out, for every job that reads it"
    );
}

/// `oracle.toml` has one reader, the oracle module (#66): no workflow or
/// action parses it, and what a job needs of the pin it reads off a
/// `status` section's JSON with `jq`, the tag the corpus is cloned at and
/// the key its tree is kept under off the corpus section (#1713), the keg
/// a runner pours off the oracle section's `binary` (#2253). A line
/// outside a comment naming the file is a read of it, an action's
/// `description:` excepted, which says in prose what the action pours;
/// so is one spelling a table of the pin, `[source]`, `[oracle]` or a
/// `[recipe` one, which a parse would match on and a step that reads the
/// section has no reason to write.
#[test]
fn no_workflow_parses_the_pin_and_the_tag_and_key_are_read_off_the_corpus_section() {
    let mut reads = Vec::new();
    for (name, text) in ci_files() {
        for (n, line) in text.lines().enumerate() {
            let line = line.trim_start();
            if line.starts_with('#') || line.starts_with("description:") {
                continue;
            }
            for spelled in ["oracle.toml", "[source]", "[oracle]", "[recipe", "[variant"] {
                assert!(
                    !line.contains(spelled),
                    "{name}:{}: {spelled} is the pin read a second time; read the fact off `xtask status --section oracle` or `corpus`: {line}",
                    n + 1
                );
            }
            for key in [".tag", ".key"] {
                if line.contains(&format!("jq -er {key} ")) {
                    reads.push(key);
                }
            }
        }
    }
    for key in [".tag", ".key"] {
        assert!(
            reads.contains(&key),
            "the corpus is cloned at the section's {key}: {reads:?}"
        );
    }
}

/// The variable's name is stated in one place that both the code and CI
/// read (#36): `oracle::TREE_VARIABLE`, which `status --section oracle`
/// renders and CI reads off it, setting the tree under that name through
/// `$GITHUB_ENV`. So no workflow or action spells either name, in an
/// `env:` block or anywhere outside a comment, and CI does run the
/// section and read both variables off its JSON, once, in the corpus
/// action every job that reads the tree uses (#2253); the promise travels the
/// same way, since a tree named and not promised skips in silence where
/// it is absent (#38). The comments are stripped first, so a name cited
/// in prose does not stand in for a setting; the negative control is a
/// key that looks like one of the two under another spelling, which the
/// harness would never read.
#[test]
fn ci_reads_the_variables_off_the_oracle_section_and_spells_neither() {
    let text = ci_files()
        .into_iter()
        .map(|(_, text)| text)
        .collect::<Vec<_>>()
        .join("\n");
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

/// The promise (#38): what a runner says this machine has, the words the
/// variable lists. A word outside the three is refused naming them, so a
/// misspelt promise is never an unmade one: `mysq` promises no server
/// (#3084).
#[test]
fn a_promise_is_the_words_the_variable_lists_and_an_unknown_word_is_refused() {
    let none = Promised::parse("").unwrap();
    assert!(!none.holds(Promise::Tree) && !none.holds(Promise::Oracle));
    let tree = Promised::parse("tree").unwrap();
    assert!(tree.holds(Promise::Tree) && !tree.holds(Promise::Oracle));
    let both = Promised::parse(" oracle\ttree ").unwrap();
    assert!(both.holds(Promise::Tree) && both.holds(Promise::Oracle));
    let server = Promised::parse("tree mysql").unwrap();
    assert!(server.holds(Promise::Tree) && server.holds(Promise::Mysql));
    assert!(!server.holds(Promise::Oracle));
    let err = Promised::parse("tree php-src").unwrap_err().to_string();
    assert!(
        err.contains("php-src") && err.contains(PROMISE_VARIABLE),
        "the word and the variable are named: {err}"
    );
    assert!(
        err.ends_with("the words it takes are tree, oracle, mysql"),
        "the words the variable takes are named: {err}"
    );
    let err = Promised::parse("tree mysq").unwrap_err().to_string();
    assert!(err.contains("\"mysq\""), "{err}");
}

/// The tests of `ext/mysqli` and `ext/pdo_mysql` cannot depend on xtask, so
/// `MysqlServer::with` spells the two variables and the word itself: held
/// here to the ones [`Promised::parse`] reads and `xtask test` sets
/// (#3084, #3085).
#[test]
fn the_mysql_server_s_tests_read_the_promise_by_its_own_name_and_word() {
    let support = workspace().join("ext/mysqlnd/tests/support/mysql.rs");
    let text = std::fs::read_to_string(&support).expect("the MySQL support file");
    for spelt in [PROMISE_VARIABLE, Promise::Mysql.word(), SKIPPED_VARIABLE] {
        assert!(text.contains(&format!("\"{spelt}\"")), "{spelt}");
    }
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

/// The tracked pin with its `[fastcgi]` replaced by one of `params`, held
/// to `md5` where named and to the params' own md5 otherwise.
fn with_fastcgi(params: &str, md5: Option<&str>) -> anyhow::Result<Pin> {
    with_split(
        params,
        md5,
        [
            Some(r"^(.+?\.php)(/.*)$"),
            Some("$fastcgi_path_info"),
            Some("$document_root$fastcgi_path_info"),
        ],
    )
}

/// [`with_fastcgi`] with `split_path_info`, `path_info` and
/// `path_translated` as `split` has them, a key left out for `None`.
fn with_split(params: &str, md5: Option<&str>, split: [Option<&str>; 3]) -> anyhow::Result<Pin> {
    let text = std::fs::read_to_string(workspace().join("oracle.toml")).unwrap();
    let mut pin: toml::Table = toml::from_str(&text).unwrap();
    let digest = md5.map_or_else(|| format!("{:x}", md5::compute(params)), str::to_owned);
    let fastcgi: toml::Table = [
        ("nginx", "1.30.5"),
        ("source", "https://example.org/fastcgi_params"),
        ("md5", &digest),
        ("script_filename", "$document_root$fastcgi_script_name"),
        ("params", params),
    ]
    .into_iter()
    .map(|(key, value)| (key, Some(value)))
    .chain([
        ("split_path_info", split[0]),
        ("path_info", split[1]),
        ("path_translated", split[2]),
    ])
    .filter_map(|(key, value)| Some((key.to_owned(), value?.into())))
    .collect();
    pin.insert("fastcgi".to_owned(), fastcgi.into());
    Pin::parse(&pin.to_string())
}

/// `[fastcgi]` (#2334): nginx's `fastcgi_param` lines read as typed
/// parameters in the order nginx sends them, `SCRIPT_FILENAME` first, a
/// value's text apart from its variables, blank lines and comments
/// skipped; the copy held to its md5, and a variable, a directive or a
/// line the reader does not know refused by name. A pin without the
/// table says so when asked for the mapping.
#[test]
fn the_fastcgi_mapping_reads_nginx_s_lines_as_typed_params() {
    use oracle::{Param, Piece, Sent, Variable};
    let params = "\nfastcgi_param  QUERY_STRING  $query_string;\n\nfastcgi_param  HTTPS  $https if_not_empty;\n# PHP only\nfastcgi_param  SERVER_SOFTWARE  nginx/$nginx_version;\nfastcgi_param  REDIRECT_STATUS  200;\n";
    let pin = with_fastcgi(params, None).unwrap();
    let mapping = pin.fastcgi().unwrap();
    let param = |name: &str, value: &[Piece], sent| Param {
        name: name.to_owned(),
        value: value.to_vec(),
        sent,
    };
    let var = Piece::Variable;
    let text = |t: &str| Piece::Text(t.to_owned());
    assert_eq!(mapping.nginx, "1.30.5");
    assert_eq!(
        mapping.params,
        [
            param("PATH_INFO", &[var(Variable::PathInfo)], Sent::Always),
            param(
                "PATH_TRANSLATED",
                &[var(Variable::DocumentRoot), var(Variable::PathInfo)],
                Sent::WithPathInfo
            ),
            param(
                "SCRIPT_FILENAME",
                &[var(Variable::DocumentRoot), var(Variable::FastcgiUri)],
                Sent::Always
            ),
            param("QUERY_STRING", &[var(Variable::QueryString)], Sent::Always),
            param("HTTPS", &[var(Variable::Https)], Sent::IfNotEmpty),
            param(
                "SERVER_SOFTWARE",
                &[text("nginx/"), var(Variable::NginxVersion)],
                Sent::Always
            ),
            param("REDIRECT_STATUS", &[text("200")], Sent::Always),
        ]
    );

    let refused =
        |params: &str, md5: Option<&str>| with_fastcgi(params, md5).unwrap_err().to_string();
    let err = refused(params, Some("00000000000000000000000000000000"));
    assert!(err.contains("md5") && err.contains("verbatim"), "{err}");
    for (line, says) in [
        ("fastcgi_param  X  $not_nginx_s;\n", "$not_nginx_s"),
        ("include  fastcgi_params;\n", "fastcgi_param <name>"),
        ("fastcgi_param  X  \"a b\";\n", "fastcgi_param <name>"),
        ("fastcgi_param  X  ${https};\n", "syntax"),
        ("fastcgi_param  X  200\n", "`;`"),
        ("fastcgi_param  X  a$;\n", "names no variable"),
    ] {
        let err = refused(line, None);
        assert!(
            err.contains("line 1") && err.contains(says),
            "{line}: {err}"
        );
    }

    let err = refused("fastcgi_param  SCRIPT_FILENAME  $document_root;\n", None);
    assert!(err.contains("SCRIPT_FILENAME twice"), "{err}");

    let text = std::fs::read_to_string(workspace().join("oracle.toml")).unwrap();
    let mut bare: toml::Table = toml::from_str(&text).unwrap();
    bare.remove("fastcgi");
    let err = Pin::parse(&bare.to_string())
        .unwrap()
        .fastcgi()
        .unwrap_err();
    assert!(err.to_string().contains("[fastcgi]"), "{err}");
}

/// `[fastcgi]`'s split (#2637) and `PATH_TRANSLATED` (#2650): the
/// pattern's two captures, or the whole path with no path info where it
/// does not match; a pattern of other than two captures, half the split,
/// or `path_translated` without it refused; and a pin from before either
/// read, as an older worktree's is.
#[test]
fn the_fastcgi_split_reads_both_keys_or_neither() {
    let params = "\nfastcgi_param  QUERY_STRING  $query_string;\n";
    let pin = with_fastcgi(params, None).unwrap();
    let mapping = pin.fastcgi().unwrap();
    assert_eq!(
        mapping.split("/front.php/index.php"),
        ("/front.php", "/index.php")
    );
    assert_eq!(
        mapping.split("/a.php.jpg/b.php/c"),
        ("/a.php.jpg/b.php", "/c")
    );
    assert_eq!(mapping.split("/front.php"), ("/front.php", ""));
    for split in [
        [Some(r"^(.+?\.php)$"), Some("$fastcgi_path_info"), None],
        [Some("("), Some("$fastcgi_path_info"), None],
        [Some(r"^(.+?\.php)(/.*)$"), None, None],
        [None, Some("$fastcgi_path_info"), None],
        [None, None, Some("$document_root$fastcgi_path_info")],
    ] {
        let err = with_split(params, None, split).unwrap_err().to_string();
        assert!(err.contains("split_path_info"), "{split:?}: {err}");
    }
    // A pin from before PATH_TRANSLATED (#2650) reads, sending none.
    let split_only = [Some(r"^(.+?\.php)(/.*)$"), Some("$fastcgi_path_info"), None];
    let split_only = with_split(params, None, split_only).unwrap();
    let names = split_only
        .fastcgi()
        .unwrap()
        .params
        .iter()
        .map(|p| &p.name[..]);
    assert_eq!(
        names.take(2).collect::<Vec<_>>(),
        ["PATH_INFO", "SCRIPT_FILENAME"]
    );
    // A pin from before the split reads, splitting nothing and sending
    // no PATH_INFO, so a binary built here reads an older worktree's pin.
    let before = with_split(params, None, [None, None, None]).unwrap();
    let before = before.fastcgi().unwrap();
    assert_eq!(before.params[0].name, "SCRIPT_FILENAME");
    assert_eq!(
        before.split("/front.php/index.php"),
        ("/front.php/index.php", "")
    );
}

/// The tracked pin's mapping (#2334): nginx's stock `fastcgi_params` at
/// the release it names, its source at that release's tag,
/// `SCRIPT_FILENAME` before them as nginx's `fastcgi.conf` sets it, and
/// first the documented split's `PATH_INFO` (#2637) and RFC 3875's
/// `PATH_TRANSLATED` (#2650).
#[test]
fn the_tracked_pin_maps_http_to_fastcgi_as_nginx_s_stock_params() {
    let pin = common::manifest::pin();
    let mapping = pin.fastcgi().unwrap();
    assert_eq!(
        mapping.split_path_info.as_ref().map(Split::as_str),
        Some(r"^(.+?\.php)(/.*)$")
    );
    assert!(
        mapping
            .source
            .contains(&format!("release-{}/conf/fastcgi_params", mapping.nginx)),
        "{mapping:?}"
    );
    let names: Vec<&str> = mapping.params.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "PATH_INFO",
            "PATH_TRANSLATED",
            "SCRIPT_FILENAME",
            "QUERY_STRING",
            "REQUEST_METHOD",
            "CONTENT_TYPE",
            "CONTENT_LENGTH",
            "SCRIPT_NAME",
            "REQUEST_URI",
            "DOCUMENT_URI",
            "DOCUMENT_ROOT",
            "SERVER_PROTOCOL",
            "REQUEST_SCHEME",
            "HTTPS",
            "GATEWAY_INTERFACE",
            "SERVER_SOFTWARE",
            "REMOTE_ADDR",
            "REMOTE_PORT",
            "SERVER_ADDR",
            "SERVER_PORT",
            "SERVER_NAME",
            "REDIRECT_STATUS",
        ]
    );
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
        "trace:fixture",
        b"TRACE stop at 4",
        b"TRACE stop at 3",
    );
    match verdict {
        Verdict::Diverged(row) => {
            assert_eq!(row.id.as_str(), "trace:fixture");
            assert!(row.reason.contains("a person accepted"));
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
    let registry = rows(&[
        Row::open("witness:open"),
        Row::accepted("witness:fixed").status(Status::Fixed),
    ]);
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
    let dir = xtask::package_dir().join("fixtures/compare");
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
        "trace:fixture",
        false,
        b"TRACE stop at %d",
        b"TRACE stop at 3",
    );
    match missed {
        Verdict::Diverged(row) => assert_eq!(row.id.as_str(), "trace:fixture"),
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
/// tree has not moved past, and not over them: `cargo build -p
/// php_sapi_cli -p php_sapi_cgi` writes the debug one, and that is what
/// displaces it.
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
    assert!(
        why.contains("cargo build --profile release-da -p php_sapi_cli -p php_sapi_cgi"),
        "the repair builds the lanes' profile: {why}"
    );
    assert_eq!(behind.binary(), None, "nothing runs under it");
    assert_eq!(behind.stamp(), None, "and no reading is stamped with it");
}

/// Every correctness lane runs `release-da` (harness-design.md §7,
/// revision 82; #1994), so the lane looks for that build first: one the
/// tree has not moved past is the engine over a release binary beside it,
/// and one the tree has moved past is passed over for the release binary,
/// as #475 passes over a binary of any profile that is behind.
#[test]
fn the_lanes_build_is_the_engine_before_release_and_passed_over_when_behind() {
    let root = fixture_tree("lanes-build-first");
    engine_sources(root.path());
    let release = built(root.path(), "release", 0);
    let lanes = built(root.path(), "release-da", 0);
    let pin = pin_with_path(root.path());
    assert_eq!(
        Runner::resolve(root.path(), &pin, None).unwrap(),
        Runner::Engine(lanes),
        "the lanes' build is the engine before a release binary as fresh"
    );
    built(root.path(), "release-da", 3600);
    assert_eq!(
        Runner::resolve(root.path(), &pin, None).unwrap(),
        Runner::Engine(release),
        "a lanes' build the tree has moved past leaves the release binary the engine"
    );
}

/// Since ADR-0008 `php-cgi` is a crate's of its own, so building `php`
/// alone leaves the `php-cgi` beside it where it was, and a CGI-shaped
/// test of the lane would run an engine the tree no longer describes: the
/// pair is held to the tree, and a `php-cgi` behind it makes the fresh
/// `php` beside it a skip naming the `php-cgi`.
#[test]
fn a_php_cgi_the_tree_has_moved_past_holds_back_the_php_beside_it() {
    let root = fixture_tree("stale-cgi-beside");
    let cgi = root.path().join("target/debug/php-cgi");
    std::fs::create_dir_all(cgi.parent().unwrap()).unwrap();
    std::fs::write(&cgi, "the cgi").unwrap();
    std::fs::File::options()
        .write(true)
        .open(&cgi)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_hours(1))
        .unwrap();
    engine_sources(root.path());
    let fresh = built(root.path(), "debug", 0);
    let pin = pin_with_path(root.path());
    let behind = Runner::resolve(root.path(), &pin, None).unwrap();
    let Runner::Stale(why) = &behind else {
        panic!("{behind:?}")
    };
    assert!(why.contains(&cgi.display().to_string()), "{why}");
    std::fs::File::options()
        .write(true)
        .open(&cgi)
        .unwrap()
        .set_modified(SystemTime::now())
        .unwrap();
    assert_eq!(
        Runner::resolve(root.path(), &pin, None).unwrap(),
        Runner::Engine(fresh),
        "a php-cgi the tree has not moved past leaves the php beside it the engine"
    );
}

/// A binary cargo leaves standing is the engine, whatever else under the
/// crate directories was written since (#3109): on 2026-10-20 a commit
/// that wrote `ext/clippy.toml` alone left `target/release-da/php` fresh
/// for cargo, and the census skipped every subset, the binary "built
/// before ext/clippy.toml was last written". It is held to what its
/// dep-info names, as cargo is, and a source named there written since is
/// what puts it behind the tree.
#[test]
fn a_binary_cargo_leaves_standing_is_the_engine_and_one_it_relinks_is_not() {
    let root = fixture_tree("dep-info");
    engine_sources(root.path());
    let source = root.path().join("crates/php_engine/src/lib.rs");
    let ago = SystemTime::now() - Duration::from_hours(2);
    std::fs::File::options()
        .write(true)
        .open(&source)
        .unwrap()
        .set_modified(ago)
        .unwrap();
    let binary = built(root.path(), "release-da", 3600);
    let info = format!("{}: {}\n", binary.display(), source.display());
    std::fs::write(format!("{}.d", binary.display()), info).unwrap();
    for beside in [
        "ext/clippy.toml",
        "crates/php_jit/src/lib.rs",
        "ext/AGENTS.md",
    ] {
        let path = root.path().join(beside);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "written after the binary\n").unwrap();
    }
    let pin = pin_with_path(root.path());
    assert_eq!(
        Runner::resolve(root.path(), &pin, None).unwrap(),
        Runner::Engine(binary.clone()),
        "nothing the binary is built from moved"
    );
    std::fs::write(&source, "// the kernels, written again\n").unwrap();
    let behind = Runner::resolve(root.path(), &pin, None).unwrap();
    let Runner::Stale(why) = &behind else {
        panic!("{behind:?}")
    };
    assert!(why.contains("crates/php_engine/src/lib.rs"), "{why}");
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

/// A binary named by a path relative to where `xtask` was invoked is the
/// one its programs are handed by its absolute path: they run in the run's
/// copy of the tree, where the relative path named nothing, and every test
/// of the subset skipped for it (#1259). A path that names no file is
/// refused before anything runs, naming the path.
#[test]
fn a_named_binary_is_handed_on_by_its_absolute_path_and_one_not_there_is_refused() {
    let root = fixture_tree("named-relative");
    let binary = built(root.path(), "release", 0);
    let pin = pin_with_path(root.path());
    let here = std::env::current_dir().unwrap();
    let relative: PathBuf = here
        .components()
        .skip(1)
        .map(|_| std::path::Component::ParentDir)
        .chain(binary.components().skip(1))
        .collect();
    assert!(relative.is_relative(), "{}", relative.display());
    let runner = Runner::resolve(root.path(), &pin, Some(&relative)).unwrap();
    let absolute = std::fs::canonicalize(&binary).unwrap();
    assert_eq!(runner, Runner::Engine(absolute));

    let refused = Runner::resolve(root.path(), &pin, Some(Path::new("target/no-such-php")))
        .expect_err("a path that names no file is refused");
    assert!(
        format!("{refused:#}").contains("target/no-such-php"),
        "{refused:#}"
    );
}

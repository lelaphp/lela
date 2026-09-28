//! The judge of one test and the resolution of a subset, held from inside
//! the instrument: what a `.phpt` file comes to under the process adapter,
//! and which directories of the pinned tree a name resolves to. The
//! reading a subset ends as is held from outside, in `tests/phpt.rs`,
//! through `run`.
//!
//! The tests that spawn php run the pinned oracle as the binary under
//! test, which is the runner's own control: it measures the judge, since
//! the engine that will be measured does not exist yet. They say so on
//! stderr and return when the pinned binary is not on this machine, as
//! the ones that resolve a subset of the real corpus do when the pinned
//! tree is on neither place `Source::tree` looks, and fail there instead
//! where this machine promised the fact (#38); the rest run anywhere,
//! the subsets they resolve being a fixture tree's.

use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use super::*;
use crate::divergence::Registry;
use crate::manifest::{Crate, Manifest};
use crate::oracle::{Promise, Runner};
use crate::reading::Status;
use crate::scratch::Tree;

fn workspace() -> PathBuf {
    crate::built_root()
}

fn manifest() -> Manifest {
    Manifest::load(&workspace()).unwrap()
}

/// A manifest of these `[[crate]]` rows under the tracked pin, its tree
/// the one given, which the caller holds open for as long as it reads the
/// manifest.
fn manifest_of(rows: impl IntoIterator<Item = Crate>, tree: &Path) -> Manifest {
    let mut manifest = Manifest::of(rows);
    manifest.pin = self::manifest()
        .pin
        .on_this_machine(Some(tree.to_path_buf()));
    manifest
}

/// The tracked manifest with its tree the one given.
fn tracked_over_fixture(tree: &Path) -> Manifest {
    let mut manifest = manifest();
    manifest.pin = manifest
        .pin
        .clone()
        .on_this_machine(Some(tree.to_path_buf()));
    manifest
}

/// A tree standing where the pinned php-src would, with the directories
/// the per-commit lane's rows name and no test in them: what a subset
/// resolves to is held here, and what one runs to is held over the real
/// corpus by the tests that skip without it.
fn fixture_tree() -> Tree {
    let tree = Tree::new("phpt-tree").unwrap();
    for under in ["Zend/tests", "tests", "sapi/cli/tests", "ext/pcre/tests"] {
        fs::create_dir_all(tree.join(under)).unwrap();
    }
    tree
}

/// `Subset::resolve` over the tree the manifest's pin answers.
fn resolve(manifest: &Manifest, name: &str) -> Result<Subset> {
    Subset::resolve(manifest, &manifest.pin.source.tree()?, name)
}

/// The tracked pin's binary, when it is on this machine; a failure where
/// this machine promised the binary (#38).
fn oracle() -> Option<PathBuf> {
    let pin = manifest().pin;
    Promise::Oracle
        .or_skip(pin.oracle.file().map(Path::to_path_buf))
        .unwrap()
}

/// The tracked pin's php-src tree, when it is on this machine; a failure
/// where this machine promised the tree (#38).
fn source() -> Option<PathBuf> {
    Promise::Tree.or_skip(manifest().pin.source.tree()).unwrap()
}

/// A subset of one directory of this test's own, standing where
/// `php-src/ext/fixture/tests` would.
fn subset(name: &str, tests: &[(&str, &str)]) -> (Tree, Subset) {
    let dir = Tree::new(&format!("phpt-judge-{name}")).unwrap();
    for (file, text) in tests {
        fs::write(dir.join(file), text).unwrap();
    }
    let subset = Subset {
        scope: "ext/fixture".to_owned(),
        dirs: vec![Dir {
            under: PathBuf::from("ext/fixture/tests"),
            dir: dir.path().to_path_buf(),
        }],
        root: dir.path().to_path_buf(),
    };
    (dir, subset)
}

fn registry(text: &str) -> Registry {
    Registry::parse(text).unwrap()
}

/// Run one subset with the pinned oracle standing in for the engine.
fn run(subset: &Subset, registry: &Registry) -> Tally {
    let binary = oracle().unwrap();
    let manifest = manifest();
    let oracle = manifest.pin.oracle();
    let under = Under {
        runner: &Runner::Engine(binary),
        lane: None,
        oracle_absent: None,
        jobs: crate::tally::cores(),
    };
    run_subset(&oracle, registry, subset, under).unwrap()
}

fn only(tally: &Tally) -> (Judgement, String) {
    assert_eq!(tally.judged.len(), 1, "{tally:#?}");
    let judged = &tally.judged[0];
    (judged.judgement, judged.note.clone())
}

#[test]
fn a_lane_names_the_engine_only_directive() {
    assert_eq!(Lane::Force.ini(), "jit.mode=force");
    assert_eq!(Lane::ExitFuzz.ini(), "jit.mode=exit-fuzz");
}

#[test]
fn a_test_that_meets_its_expect_section_passes() {
    if oracle().is_none() {
        return;
    }
    let (_tree, subset) = subset(
        "pass",
        &[(
            "pass.phpt",
            "--TEST--\nEcho\n--FILE--\n<?php echo \"one\\n\", 2 + 2;\n--EXPECT--\none\n4\n",
        )],
    );
    let tally = run(&subset, &registry(""));
    assert_eq!(only(&tally).0, Judgement::Pass, "{tally:#?}");
}

#[test]
fn a_test_and_its_skipif_run_from_the_php_src_root_as_run_tests_php_runs_them() {
    if oracle().is_none() {
        return;
    }
    // The program and its SKIPIF each ask where they run (#952).
    let asks = "getcwd() === realpath(__DIR__ . '/../../..')";
    let (_root, subset) = rooted(
        "cwd",
        &format!(
            "--TEST--\ncwd\n--SKIPIF--\n<?php if (!({asks})) echo 'skip ', getcwd();\n--FILE--\n<?php var_dump({asks});\n--EXPECT--\nbool(true)\n"
        ),
    );
    let tally = run(&subset, &registry(""));
    assert_eq!(only(&tally).0, Judgement::Pass, "{tally:#?}");
}

/// A subset of one test standing at `<root>/ext/fixture/tests`, as a
/// test stands in php-src, run from `root` as the lane runs every test.
fn rooted(name: &str, text: &str) -> (Tree, Subset) {
    let root = Tree::new(&format!("phpt-judge-{name}")).unwrap();
    let dir = root.join("ext/fixture/tests");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(format!("{name}.phpt")), text).unwrap();
    let subset = Subset {
        scope: "ext/fixture".to_owned(),
        dirs: vec![Dir {
            under: PathBuf::from("ext/fixture/tests"),
            dir,
        }],
        root: root.path().to_path_buf(),
    };
    (root, subset)
}

#[test]
fn a_test_s_dir_is_its_own_directory_and_stands_in_what_it_printed() {
    if oracle().is_none() {
        return;
    }
    // The EXPECTF asks for the directory's last segment, which a token in
    // the directory's place would have taken (#1089, #906).
    let (_root, subset) = rooted(
        "dir",
        "--TEST--\n__DIR__\n--FILE--\n<?php\nvar_dump(__DIR__);\nvar_dump(__DIR__ === realpath('ext/fixture/tests'));\nvar_dump(basename(__FILE__));\n--EXPECTF--\nstring(%d) \"%s%efixture%etests\"\nbool(true)\nstring(7) \"dir.php\"\n",
    );
    let tally = run(&subset, &registry(""));
    assert_eq!(only(&tally).0, Judgement::Pass, "{tally:#?}");
    assert!(
        !subset.dirs[0].dir.join("dir.php").exists(),
        "the program is taken away after its run"
    );
}

#[test]
fn a_test_and_its_skipif_spawn_the_binary_under_test_with_its_extra_args() {
    let Some(binary) = oracle() else {
        return;
    };
    // The SKIPIF runs under the oracle and still names the binary under
    // test; the FILE spawns that binary with TEST_PHP_EXTRA_ARGS, which
    // carries the test's own --INI-- (#906).
    let (_root, subset) = rooted(
        "spawn",
        "--TEST--\nspawn\n--INI--\nprecision=3\n--SKIPIF--\n<?php if (getenv('TEST_PHP_EXTRA_ARGS') === false || getenv('TEST_PHP_CGI_EXECUTABLE') !== '') echo 'skip no environment';\n--FILE--\n<?php\n$php = getenv('TEST_PHP_EXECUTABLE_ESCAPED');\n$extra = getenv('TEST_PHP_EXTRA_ARGS');\necho shell_exec(\"$php $extra -r 'echo 1/3, ini_get(\\\"error_reporting\\\");'\"), \"\\n\";\nvar_dump(getenv('TEST_PHP_EXECUTABLE') === realpath(getenv('TEST_PHP_EXECUTABLE')));\n--EXPECT--\n0.33332767\nbool(true)\n",
    );
    let tally = run(&subset, &registry(""));
    assert_eq!(only(&tally).0, Judgement::Pass, "{tally:#?}");
    assert!(binary.is_absolute());
}

#[test]
fn the_digest_is_asserted_once_over_a_run_of_three_tests() {
    if oracle().is_none() {
        return;
    }
    // Three tests, each with a SKIPIF and a CLEAN section, so the oracle
    // runs three times beside the binary under test and the pin is asked
    // for its digest before the first program only (harness §4.3).
    let test = |n: u8| {
        format!(
            "--TEST--\nn{n}\n--SKIPIF--\n<?php echo 'ok';\n--FILE--\n<?php echo {n};\n--CLEAN--\n<?php echo 'clean';\n--EXPECT--\n{n}\n"
        )
    };
    let tests = [test(1), test(2), test(3)];
    let (_tree, subset) = subset(
        "digest",
        &[
            ("one.phpt", &tests[0]),
            ("two.phpt", &tests[1]),
            ("three.phpt", &tests[2]),
        ],
    );
    let binary = oracle().unwrap();
    let manifest = manifest();
    assert_eq!(manifest.pin.digests_asserted(), 0);
    let oracle = manifest.pin.oracle();
    let under = Under {
        runner: &Runner::Engine(binary),
        lane: None,
        oracle_absent: None,
        jobs: crate::tally::cores(),
    };
    let tally = run_subset(&oracle, &registry(""), &subset, under).unwrap();
    assert_eq!(tally.judged.len(), 3, "{tally:#?}");
    assert!(
        tally.judged.iter().all(|j| j.judgement == Judgement::Pass),
        "{tally:#?}"
    );
    assert_eq!(manifest.pin.digests_asserted(), 1);
}

#[test]
fn the_wildcards_of_an_expectf_section_are_matched_and_the_streams_are_one() {
    if oracle().is_none() {
        return;
    }
    let (_tree, subset) = subset(
        "expectf",
        &[(
            "warn.phpt",
            "--TEST--\nA warning names the file and the line\n--FILE--\n<?php\nfwrite(STDERR, \"on the second stream\\n\");\necho $nope;\n--EXPECTF--\non the second stream\n\nWarning: Undefined variable $nope in %s on line %d\n",
        )],
    );
    let tally = run(&subset, &registry(""));
    assert_eq!(only(&tally).0, Judgement::Pass, "{tally:#?}");
}

#[test]
fn a_missed_expectation_fails_with_the_difference() {
    if oracle().is_none() {
        return;
    }
    let (_tree, subset) = subset(
        "missed",
        &[(
            "missed.phpt",
            "--TEST--\nA wrong expectation\n--FILE--\n<?php echo \"four\";\n--EXPECT--\nfive\n",
        )],
    );
    let tally = run(&subset, &registry(""));
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Fail, "{tally:#?}");
    assert!(note.contains("-five") && note.contains("+four"), "{note}");
    assert_eq!(tally.against(), ["missed.phpt"]);
}

#[test]
fn a_program_past_the_recipes_limit_fails_its_test_and_the_lane_goes_on() {
    if oracle().is_none() {
        return;
    }
    // Three tests in order, the middle one a program that never ends and
    // says first which process it is. Under a limit of one second it is
    // that test's fail, naming the limit; the process is killed and
    // reaped, not left running; and the test after it is still judged
    // (#691). Before, the limit ended the whole run and took every
    // verdict with it.
    let (tree, subset) = subset("past-limit", &[]);
    let pid = tree.path().join("pid");
    let hangs = format!(
        "--TEST--\nNever ends\n--FILE--\n<?php file_put_contents({:?}, getmypid()); while (true) {{}}\n--EXPECT--\nnothing\n",
        pid.display().to_string()
    );
    let echo = |n: u8| format!("--TEST--\nn{n}\n--FILE--\n<?php echo {n};\n--EXPECT--\n{n}\n");
    fs::write(tree.join("a.phpt"), echo(1)).unwrap();
    fs::write(tree.join("b.phpt"), hangs).unwrap();
    fs::write(tree.join("c.phpt"), echo(3)).unwrap();
    let binary = oracle().unwrap();
    let mut manifest = manifest();
    manifest.pin.recipe.limits.timeout_s = 1;
    let oracle = manifest.pin.oracle();
    let under = Under {
        runner: &Runner::Engine(binary),
        lane: None,
        oracle_absent: None,
        jobs: crate::tally::cores(),
    };
    let tally = run_subset(&oracle, &registry(""), &subset, under).unwrap();
    let went: Vec<(&str, Judgement)> = tally
        .judged
        .iter()
        .map(|j| (j.name.as_str(), j.judgement))
        .collect();
    assert_eq!(
        went,
        [
            ("a.phpt", Judgement::Pass),
            ("b.phpt", Judgement::Fail),
            ("c.phpt", Judgement::Pass),
        ],
        "{tally:#?}"
    );
    let note = &tally.judged[1].note;
    assert!(note.contains("the recipe's limit of 1 s"), "{note}");
    assert_eq!(tally.against(), ["b.phpt"]);
    // The process the test ran is gone: `kill -0` finds no process by
    // that number, which it would for one still running or one killed and
    // never reaped.
    let pid = fs::read_to_string(&pid).unwrap();
    let alive = std::process::Command::new("kill")
        .args(["-0", pid.trim()])
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(!alive.success(), "process {pid} is still there");
}

#[test]
fn the_same_missed_expectation_is_diverged_under_an_accepted_row() {
    if oracle().is_none() {
        return;
    }
    let (_tree, subset) = subset(
        "accepted",
        &[(
            "missed.phpt",
            "--TEST--\nA wrong expectation\n--FILE--\n<?php echo \"four\";\n--EXPECT--\nfive\n",
        )],
    );
    let accepted = registry(
        "[[divergence]]\nid = \"phpt:php-src/ext/fixture/tests/missed.phpt\"\nkind = \"phpt\"\nreason = \"A person wrote why this one is allowed to differ.\"\nstatus = \"accepted\"\nsince = \"2026-09-08\"\n",
    );
    let tally = run(&subset, &accepted);
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Diverged, "{tally:#?}");
    assert!(note.contains("A person wrote why"), "{note}");
    assert!(tally.against().is_empty());
}

/// A program the compiler refused with its own parse error, the one
/// `Parse error` line ending in `this compiler cannot read yet`, is
/// unread and the construct is the note; the `PHP ` prefix `log_errors`
/// gives it reads the same, and so does the reader's `this compiler
/// cannot read`, the same refusal for a shape of a construct it does read
/// (#319). Anything else the program wrote is the
/// test's own: php's own parse error, a body that ran and then met the
/// refusal, a body that ran and wrote nothing, or one that wrote the words
/// itself.
#[test]
fn a_program_the_compiler_refused_is_unread_and_names_the_construct() {
    let unread = |body: &str| adapter::unread(body.as_bytes());
    assert_eq!(
        unread("\nParse error: a statement this compiler cannot read yet in /t/x.php on line 3\n"),
        Some("a statement this compiler cannot read yet".to_owned())
    );
    assert_eq!(
        unread(
            "PHP Parse error:  an expression this compiler cannot read yet in x.php on line 2\n"
        ),
        Some("an expression this compiler cannot read yet".to_owned())
    );
    assert_eq!(
        unread(
            "\nParse error: a call whose arguments this compiler cannot read in bug25745.php on line 8\n"
        ),
        Some("a call whose arguments this compiler cannot read".to_owned())
    );
    assert_eq!(
        unread("\nParse error: syntax error, unexpected token \";\" in x.php on line 2\n"),
        None
    );
    assert_eq!(
        unread(
            "ran\n\nParse error: a statement this compiler cannot read yet in x.php on line 3\n"
        ),
        None
    );
    assert_eq!(unread(""), None);
    assert_eq!(unread("this compiler cannot read yet"), None);
}

/// The fixture at `xtask/fixtures/phpt/UnreadInParentheses.phpt` (#395):
/// a program php runs and this compiler cannot read, the construct it
/// cannot read standing inside parentheses. The refusal `php_compile`
/// gives that program ends in the words [`adapter::unread`] reads, so
/// the lane counts the test a construct not read yet; while the refusal
/// named the parenthesis instead, the same program's refusal was no
/// sentence the lane knew and the test counted as a failure. The engine
/// here is the one linked into the harness, since the fixture holds one
/// program and not a subset to run.
///
/// The construct moves as the compiler grows, and the fixture moves with
/// it: `$a and $b` was the first and `b16f247` taught the compiler to
/// read it, `1 | 2` the second and `ad386d9` did the same, `print` the
/// third and `e83e41e` did the same, `eval` the fourth and #573 did the
/// same, and each time the test found no refusal at all (#483). The
/// sentence moves too: `emit`'s walk of the tree names the construct
/// inside the parentheses and not the parentheses (#595), so what the
/// lane reads here is the static property named at run time, `C::$$p`,
/// which the unread census of Zend/tests still counts. `php_compile`'s
/// own test for this refusal,
/// `an_unread_expression_inside_parentheses_names_the_expression`, names
/// an anonymous class instead; this one does not follow it there while
/// anonymous classes are being built (#777), so the two need not move
/// together.
#[test]
fn the_refusal_for_an_unread_expression_inside_parentheses_is_read_as_unread() {
    let pin = manifest().pin;
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
    let phpt = file::Phpt::load(&fixture).unwrap();
    let program = phpt.section("FILE").expect("the fixture has a --FILE--");
    let snippet = String::from_utf8_lossy(program)
        .strip_prefix("<?php\n")
        .expect("the program opens with the tag the recipe's preamble writes")
        .to_owned();
    let served = crate::oracle::linked::serve(&pin.recipe, None, &snippet).unwrap();
    assert_eq!(
        adapter::unread(&served.stdout),
        Some("a static property named at run time this compiler cannot read yet".to_owned()),
        "{}",
        String::from_utf8_lossy(&served.stdout)
    );
}

/// The one fixture the test above reads.
const FIXTURE: &str = "fixtures/phpt/UnreadInParentheses.phpt";

#[test]
fn a_skipif_that_says_skip_skips_with_its_reason() {
    if oracle().is_none() {
        return;
    }
    let (_tree, subset) = subset(
        "skipif",
        &[(
            "skipped.phpt",
            "--TEST--\nSkipped\n--SKIPIF--\n<?php echo \"skip nothing to see here\\n\";\n--FILE--\n<?php echo \"never\";\n--EXPECT--\nnever\n",
        )],
    );
    let tally = run(&subset, &registry(""));
    assert_eq!(
        only(&tally),
        (Judgement::Skip, "nothing to see here".to_owned()),
        "{tally:#?}"
    );
}

#[test]
fn an_extension_the_binary_does_not_load_skips_in_upstreams_words() {
    if oracle().is_none() {
        return;
    }
    let (_tree, subset) = subset(
        "extensions",
        &[(
            "needs.phpt",
            "--TEST--\nNeeds an extension that is not there\n--EXTENSIONS--\nnot_an_extension\n--FILE--\n<?php echo \"never\";\n--EXPECT--\nnever\n",
        )],
    );
    let tally = run(&subset, &registry(""));
    assert_eq!(
        only(&tally),
        (
            Judgement::Skip,
            "Required extension missing: not_an_extension".to_owned()
        ),
        "{tally:#?}"
    );
}

#[test]
fn a_file_that_is_not_a_test_borks_and_the_subset_fails_on_it() {
    let (_tree, subset) = subset("borked", &[("borked.phpt", "--NOTATEST--\nnothing\n")]);
    if oracle().is_none() {
        return;
    }
    let tally = run(&subset, &registry(""));
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Bork);
    assert!(note.contains("must start with --TEST--"), "{note}");
    assert_eq!(tally.against(), ["borked.phpt"]);
}

#[test]
fn a_cgi_test_is_skipped_because_this_runner_hosts_the_command_line() {
    if oracle().is_none() {
        return;
    }
    let (_tree, subset) = subset(
        "cgi",
        &[(
            "cgi.phpt",
            "--TEST--\nA CGI test\n--CGI--\n--FILE--\n<?php echo \"never\";\n--EXPECT--\nnever\n",
        )],
    );
    let tally = run(&subset, &registry(""));
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Skip);
    assert!(note.contains("CGI"), "{note}");
}

#[test]
fn the_test_s_own_ini_reaches_the_run() {
    if oracle().is_none() {
        return;
    }
    let (_tree, subset) = subset(
        "ini",
        &[(
            "ini.phpt",
            "--TEST--\nThe INI section is passed after the recipe's\n--INI--\nprecision=3\n--FILE--\n<?php echo 1/3;\n--EXPECT--\n0.333\n",
        )],
    );
    let tally = run(&subset, &registry(""));
    assert_eq!(only(&tally).0, Judgement::Pass, "{tally:#?}");
}

#[test]
fn the_subset_of_an_extension_is_its_tests_at_the_pinned_tag_and_the_scope_is_the_crate() {
    let tree = fixture_tree();
    let manifest = tracked_over_fixture(&tree);
    for name in ["pcre", "ext/pcre"] {
        let subset = resolve(&manifest, name).unwrap();
        assert_eq!(subset.scope, "ext/pcre");
        assert_eq!(subset.dirs.len(), 1, "{subset:#?}");
        assert_eq!(subset.dirs[0].under, Path::new("ext/pcre/tests"));
        assert_eq!(
            subset.dirs[0].dir,
            tree.join("ext/pcre/tests"),
            "the subset is the tree's the variable names"
        );
        assert_eq!(
            subset.id(&subset.dirs[0].dir.join("preg_match_basic.phpt")),
            "phpt:php-src/ext/pcre/tests/preg_match_basic.phpt",
            "the divergence key names the corpus, not this machine"
        );
    }
}

#[test]
fn a_corpus_directory_is_a_subset_and_an_unknown_name_names_both_forms_tried() {
    if source().is_none() {
        return;
    }
    let manifest = manifest();
    let subset = resolve(&manifest, "Zend/tests").unwrap();
    assert_eq!(subset.scope, "Zend/tests");
    assert_eq!(subset.dirs[0].under, Path::new("Zend/tests"));

    let error = resolve(&manifest, "nope").unwrap_err().to_string();
    assert!(
        error.contains("ext/nope") && error.contains("nope"),
        "{error}"
    );
}

#[test]
fn a_subset_resolves_under_the_tree_the_pin_was_handed_ahead_of_the_pins_path() {
    // The fixture is what the variable names; the pin's path, the
    // maintainer's tree or nothing, is not consulted while it is there.
    let tree = fixture_tree();
    let manifest = manifest_of([Crate::named("ext/pcre")], &tree);
    assert_eq!(manifest.pin.source.tree().unwrap(), *tree);
    let corpus = resolve(&manifest, "Zend/tests").unwrap();
    assert_eq!(corpus.scope, "Zend/tests");
    assert_eq!(corpus.dirs[0].dir, tree.join("Zend/tests"));
    let extension = resolve(&manifest, "pcre").unwrap();
    assert_eq!(extension.dirs[0].dir, tree.join("ext/pcre/tests"));
    assert_eq!(extension.absent(), Vec::<&Path>::new(), "{extension:#?}");
}

#[test]
fn a_tier_lane_and_a_control_are_readings_of_their_own() {
    let tree = fixture_tree();
    let subset = resolve(&tracked_over_fixture(&tree), "pcre").unwrap();
    let engine = Runner::Engine(PathBuf::from("target/debug/php"));
    assert_eq!(subset.scope(&engine, None), "ext/pcre");
    assert_eq!(subset.scope(&engine, Some(Lane::Force)), "ext/pcre@force");
    assert_eq!(
        subset.scope(&engine, Some(Lane::ExitFuzz)),
        "ext/pcre@exit-fuzz"
    );
    // A control of the runner keeps a reading of its own, so a green one
    // can never sit where the engine's reading belongs.
    let control = Runner::Control(PathBuf::from("php"));
    assert_eq!(subset.scope(&control, None), "ext/pcre@control");
}

#[test]
fn a_crate_resolves_to_every_subset_its_row_names() {
    let tree = fixture_tree();
    let manifest = manifest_of(
        [Crate::named("php_engine").with_phpt(&["Zend/tests", "tests"])],
        &tree,
    );
    let subset = resolve(&manifest, "php_engine").unwrap();
    assert_eq!(subset.scope, "php_engine");
    let under: Vec<&Path> = subset.dirs.iter().map(|d| d.under.as_path()).collect();
    assert_eq!(under, [Path::new("Zend/tests"), Path::new("tests")]);
    assert_eq!(
        subset.id(&subset.dirs[1].dir.join("basic/lambda.phpt")),
        "phpt:php-src/tests/basic/lambda.phpt",
        "the key names where the test stands in the corpus, whichever directory it came from"
    );
}

#[test]
fn a_crate_whose_row_names_no_subset_says_so_rather_than_looking_for_a_directory() {
    let tree = fixture_tree();
    let manifest = manifest_of([Crate::named("php_heap")], &tree);
    let error = resolve(&manifest, "php_heap").unwrap_err().to_string();
    assert!(
        error.contains("php_heap") && error.contains("no phpt subset"),
        "{error}"
    );
    assert!(
        !error.contains("php-src"),
        "the row is the answer, not the pinned tree: {error}"
    );
}

#[test]
fn a_directory_the_pinned_tree_has_not_is_kept_and_named_absent() {
    let tree = fixture_tree();
    let manifest = manifest_of(
        [Crate::named("php_engine").with_phpt(&["Zend/tests", "Zend/nope"])],
        &tree,
    );
    let subset = resolve(&manifest, "php_engine").unwrap();
    assert_eq!(subset.dirs.len(), 2, "{subset:#?}");
    assert_eq!(subset.absent(), [Path::new("Zend/nope")]);
}

#[test]
fn the_per_commit_lane_is_composed_from_the_crate_rows() {
    let tree = fixture_tree();
    let manifest = manifest_of(
        [
            Crate::named("php_engine").with_phpt(&["Zend/tests", "tests"]),
            Crate::named("php_heap"),
            Crate::named("php_sapi").with_phpt(&["sapi/cli/tests"]),
            Crate::named("ext/pcre"),
            Crate::named("ext/curl").with_feature("curl"),
        ],
        &tree,
    );
    let lane: Vec<String> = Subset::lane(&manifest, &tree)
        .into_iter()
        .map(|subset| subset.scope)
        .collect();
    // §7's per-commit row: Zend/tests and tests/ with php_engine, sapi/cli
    // with php_sapi, and the default extensions' own. A crate behind a
    // feature is not in the default build, and one with no subset owns none.
    assert_eq!(lane, ["php_engine", "php_sapi", "ext/pcre"]);
}

#[test]
fn a_pinned_php_not_on_this_machine_runs_nothing_and_asserts_no_digest() {
    // A binary is built and the pinned php is not here, the runner's
    // shape (harness D.20): the engine never spawns the pinned php, so
    // its absence is no refusal; the tests are found, none runs, the pin
    // is not asked for a digest it cannot give, and the reading is a skip
    // that says why, as the witness lane's is.
    let (_tree, mut subset) = subset(
        "oracle-absent",
        &[(
            "one.phpt",
            "--TEST--\nEcho\n--FILE--\n<?php echo 1;\n--EXPECT--\n1\n",
        )],
    );
    // A reading is stamped over its scope's tree, so the scope is a crate
    // the manifest has and the tree holds.
    subset.scope = "ext/pcre".to_owned();
    let mut manifest = manifest();
    manifest.pin.oracle.path = PathBuf::from("/nowhere/php");
    let engine = Runner::Engine(std::env::current_exe().unwrap());
    let why = format!(
        "the pinned php is not at {}",
        manifest.pin.oracle.path.display()
    );
    let oracle = manifest.pin.oracle();
    let under = Under {
        runner: &engine,
        lane: None,
        oracle_absent: Some(&why),
        jobs: crate::tally::cores(),
    };
    let tally = run_subset(&oracle, &registry(""), &subset, under).unwrap();
    assert!(tally.judged.is_empty(), "{tally:#?}");
    assert_eq!(tally.found, 1);
    assert_eq!(manifest.pin.digests_asserted(), 0);
    let source = manifest.pin.source.tree().unwrap_or_default();
    let reading = reading(&tally, &workspace(), &manifest, &source, &subset, under).unwrap();
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert!(reading.message.contains(&why), "{}", reading.message);
    assert_eq!(reading.numbers["run"], 0i64.into());
    assert_eq!(reading.numbers["engine_absent"], 0i64.into());
}

#[test]
fn every_expectf_section_of_the_pinned_pcre_subset_compiles() {
    use crate::oracle::expect::Expectation;
    let Some(tree) = source() else { return };
    let subset = Subset::resolve(&manifest(), &tree, "pcre").unwrap();
    let tests = subset.tests().unwrap();
    assert!(!tests.is_empty(), "the pinned tree has ext/pcre/tests");
    let mut seen = 0;
    for test in tests {
        let Ok(phpt) = file::Phpt::load(&test) else {
            continue;
        };
        for (section, expectation) in [
            ("EXPECTF", Expectation::Format as fn(String) -> _),
            ("EXPECTREGEX", Expectation::Pattern),
        ] {
            let Some(body) = phpt.section(section) else {
                continue;
            };
            seen += 1;
            let expectation = expectation(String::from_utf8_lossy(body).trim().to_owned());
            // The one corpus body that needs PCRE-only syntax is a
            // divergence row, not a reason to carry a backtracker (§8);
            // every other one must compile here.
            if let Err(e) = expectation.met(b"") {
                assert!(section == "EXPECTREGEX", "{}: {e:#}", test.display());
            }
        }
    }
    assert!(seen > 30, "the subset carried {seen} patterns");
}

/// The census folds the unread verdicts by the construct their refusal
/// named, the widest construct first, and a tie is broken by the
/// construct's own text so one run prints the same twice (#617). A
/// verdict of any other judgement carries a note too — a skip's reason, a
/// difference — and none of them is the compiler's refusal, so none is
/// counted here however its note reads.
#[test]
fn the_census_groups_the_unread_by_construct_widest_first() {
    let judged = |name: &str, judgement, note: &str| {
        let mut one = Judged::new(name, name, judgement, note);
        one.place = String::new();
        one
    };
    let tally = Tally {
        judged: vec![
            judged(
                "a.phpt",
                Judgement::Unread,
                "a `foreach` this compiler cannot read yet",
            ),
            judged(
                "b.phpt",
                Judgement::Unread,
                "an `eval` this compiler cannot read yet",
            ),
            judged(
                "c.phpt",
                Judgement::Unread,
                "a `foreach` this compiler cannot read yet",
            ),
            judged(
                "d.phpt",
                Judgement::Fail,
                "a `foreach` this compiler cannot read yet",
            ),
            judged("e.phpt", Judgement::Skip, "the extension is not built"),
            judged(
                "f.phpt",
                Judgement::Unread,
                "a class this compiler cannot read yet",
            ),
        ],
        found: 6,
    };
    let mut census = census::Census::default();
    census.add(&tally);

    // The fail's note reads like a refusal and is not one: four unread.
    assert_eq!(census.unread(), 4);
    let rows = census.rows();
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0].construct,
        "a `foreach` this compiler cannot read yet"
    );
    assert_eq!(
        rows[0].tests,
        vec!["a.phpt".to_owned(), "c.phpt".to_owned()]
    );
    // One test each, so the tie is the construct's own text: `a class`
    // before `an `eval``.
    assert_eq!(rows[1].construct, "a class this compiler cannot read yet");
    assert_eq!(rows[2].construct, "an `eval` this compiler cannot read yet");

    // A second subset folded in adds to the group it shares and makes a
    // group of its own, which is what the lane's census is.
    census.add(&Tally {
        judged: vec![judged(
            "g.phpt",
            Judgement::Unread,
            "an `eval` this compiler cannot read yet",
        )],
        found: 1,
    });
    let rows = census.rows();
    assert_eq!(census.unread(), 5);
    assert_eq!(rows[1].construct, "an `eval` this compiler cannot read yet");
    assert_eq!(rows[1].tests.len(), 2);
}

/// A census with nothing in it is the answer a subset the compiler read
/// through gives, and it says so rather than printing a heading over no
/// rows.
#[test]
fn a_subset_with_no_unread_test_has_an_empty_census() {
    let mut census = census::Census::default();
    census.add(&Tally {
        judged: vec![Judged::new("a.phpt", "a.phpt", Judgement::Pass, "")],
        found: 1,
    });
    assert!(census.is_empty());
    assert_eq!(census.unread(), 0);
    assert!(census.rows().is_empty());
}

/// A fixture directory under `xtask/fixtures/phpt` laid down in a scratch
/// tree standing where `php-src/ext/fixture/tests` would, so what its
/// programs write lands there and not in the fixture.
fn fixture_subset(name: &str) -> (Tree, Subset) {
    fn copy(from: &Path, to: &Path) {
        fs::create_dir_all(to).unwrap();
        for entry in fs::read_dir(from).unwrap() {
            let path = entry.unwrap().path();
            let into = to.join(path.file_name().unwrap());
            if path.is_dir() {
                copy(&path, &into);
            } else {
                fs::copy(&path, &into).unwrap();
            }
        }
    }
    let (tree, subset) = subset(name, &[]);
    copy(
        &Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures/phpt")
            .join(name),
        tree.path(),
    );
    (tree, subset)
}

/// The fixture's subset run with the pinned oracle standing in for the
/// engine, on a pool of `jobs`, under a limit of `limit_s`.
fn run_on(subset: &Subset, jobs: usize, limit_s: u64) -> Tally {
    let binary = oracle().unwrap();
    let mut manifest = manifest();
    manifest.pin.recipe.limits.timeout_s = limit_s;
    let oracle = manifest.pin.oracle();
    let under = Under {
        runner: &Runner::Engine(binary),
        lane: None,
        oracle_absent: None,
        jobs: NonZeroUsize::new(jobs).unwrap(),
    };
    run_subset(&oracle, &registry(""), subset, under).unwrap()
}

/// The span each program of the fixture wrote beside itself, in seconds,
/// by the test's path inside the fixture.
fn spans(tree: &Tree, tests: &[&str]) -> Vec<(f64, f64)> {
    tests
        .iter()
        .map(|test| {
            let text = fs::read_to_string(tree.join(test).with_extension("php.span")).unwrap();
            let (start, end) = text.split_once(' ').unwrap();
            (start.parse().unwrap(), end.parse().unwrap())
        })
        .collect()
}

fn overlap(a: (f64, f64), b: (f64, f64)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// The fixture at `xtask/fixtures/phpt/conflicts` (#938), on a pool of
/// eight, which cuts its seven tests that do not run alone into batches of
/// two neighbours: the two tests naming `shared` never run at once, the one
/// naming `all` runs beside nothing, the directory's `CONFLICTS` file holds
/// its two tests apart, and a test's own section stands in place of that
/// file. The pairs held apart stand in two batches each, so what holds them
/// is the key. The negative controls are the tests naming no key, which
/// run at once, and `own.phpt`, which runs beside the directory's second.
/// The verdicts, in order and word for word, are the ones a pool of one
/// reaches.
#[test]
fn tests_that_conflict_never_run_at_once_and_the_verdicts_are_the_serial_runs() {
    if oracle().is_none() {
        return;
    }
    let (tree, subset) = fixture_subset("conflicts");
    let tally = run_on(&subset, 8, 30);
    let tests = [
        "a-keyed/free1.phpt",
        "a-keyed/one.phpt",
        "a-keyed/two.phpt",
        "a-keyed/z-free.phpt",
        "b-alone/all.phpt",
        "c-directory/one.phpt",
        "c-directory/own.phpt",
        "c-directory/two.phpt",
    ];
    let names: Vec<&str> = tally.judged.iter().map(|j| j.name.as_str()).collect();
    assert_eq!(names, tests);
    assert!(
        tally.judged.iter().all(|j| j.judgement == Judgement::Pass),
        "{tally:#?}"
    );
    let span = spans(&tree, &tests);
    assert!(!overlap(span[1], span[2]), "shared: {span:?}");
    for n in (0..tests.len()).filter(|n| *n != 4) {
        assert!(
            !overlap(span[4], span[n]),
            "all beside {}: {span:?}",
            tests[n]
        );
    }
    assert!(!overlap(span[5], span[7]), "directory: {span:?}");
    assert!(overlap(span[0], span[3]), "the control, no key: {span:?}");
    assert!(overlap(span[6], span[7]), "the control, own: {span:?}");

    let serial = run_on(&subset, 1, 30);
    assert_eq!(serial.judged, tally.judged);
    assert_eq!(serial.found, tally.found);
    let span = spans(&tree, &tests);
    assert!(
        span.windows(2).all(|w| w[0].1 <= w[1].0),
        "one at a time: {span:?}"
    );
}

/// The fixture at `xtask/fixtures/phpt/relative` (#1091): two neighbours
/// that each make and remove one directory relative to the root every
/// program runs from, which met on `mkdir(): File exists` when the pool
/// judged them at once. On a pool of eight the three tests are cut into
/// batches of two, as run-tests.php cuts them (run-tests.php:1537-1541),
/// so the pair is judged one after the other and both pass, run after run;
/// the third, in a batch of its own, runs beside the pair's first in some
/// run, the negative control that shows the pool runs anything at once at
/// all. Some run and not every run: on a loaded machine the scheduler may
/// serve two batches one after the other, which says nothing about the
/// pool (#1153).
#[test]
fn neighbours_that_make_one_relative_path_are_one_batch_and_never_meet() {
    if oracle().is_none() {
        return;
    }
    let tests = ["a-temp.phpt", "b-temp.phpt", "c-free.phpt"];
    let (tree, subset) = fixture_subset("relative");
    let mut controls = Vec::new();
    for run in 0..20 {
        let tally = run_on(&subset, 8, 30);
        assert!(
            tally.judged.iter().all(|j| j.judgement == Judgement::Pass),
            "run {run}: {tally:#?}"
        );
        let span = spans(&tree, &tests);
        assert!(span[0].1 <= span[1].0, "run {run}, the batch: {span:?}");
        controls.push((span[0], span[2]));
    }
    assert!(
        controls.iter().any(|(a, c)| overlap(*a, *c)),
        "the control met in no run: {controls:?}"
    );
}

/// The fixture at `xtask/fixtures/phpt/past-limit` (#938): a test that
/// never ends first, and two after it that conflict with nothing. On a
/// pool of three the batches are two neighbours (run-tests.php:1537-1541,
/// #1091), so the second waits for the first, its batch's, and the third,
/// in a batch of its own, is done before the first is past its limit; on
/// a pool of one, the negative control, the third starts after it too.
/// The limit runs from the spawn and the span from the program's first
/// line, so the line between the two is drawn half a second early. The
/// verdicts are the same either way: the first fails naming the limit.
#[test]
fn a_test_past_the_limit_holds_up_only_its_batch_and_what_it_conflicts_with() {
    const LIMIT: u64 = 3;
    if oracle().is_none() {
        return;
    }
    let tests = ["a-never.phpt", "b-quick.phpt", "c-quick.phpt"];
    let (tree, subset) = fixture_subset("past-limit");
    #[allow(clippy::cast_precision_loss)]
    let past = |span: &[(f64, f64)]| span[0].0 + LIMIT as f64 - 0.5;
    let pooled = run_on(&subset, 3, LIMIT);
    let span = spans(&tree, &tests);
    assert!(
        span[1].0 >= past(&span) && span[2].1 < past(&span),
        "{span:?}"
    );

    let serial = run_on(&subset, 1, LIMIT);
    let span = spans(&tree, &tests);
    assert!(
        span[1].0 >= past(&span) && span[2].0 >= past(&span),
        "{span:?}"
    );
    assert_eq!(pooled.judged, serial.judged);
    let went: Vec<Judgement> = pooled.judged.iter().map(|j| j.judgement).collect();
    assert_eq!(went, [Judgement::Fail, Judgement::Pass, Judgement::Pass]);
}

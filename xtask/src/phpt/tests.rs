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
use crate::common::phpt_tree::fixture_tree;
use crate::common::registry::registry;
use crate::divergence::Registry;
use crate::manifest::{Crate, Manifest};
use crate::oracle::{Program, Promise, Runner};
use crate::reading::Status;
use crate::scratch::Tree;

impl<'a> Under<'a> {
    /// `runner`'s run on every core, holding its locks in `locks`, with
    /// no lane, the pinned php here and no server.
    fn of(runner: &'a Runner, locks: &'a Path) -> Under<'a> {
        Under {
            runner,
            lane: None,
            oracle_absent: None,
            jobs: crate::tally::cores(),
            served: None,
            locks,
        }
    }
}

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
        holds: Holds::Every,
    };
    (dir, subset)
}

/// Run one subset with the pinned oracle standing in for the engine.
fn run(subset: &Subset, registry: &Registry) -> Tally {
    let binary = oracle().unwrap();
    let manifest = manifest();
    let oracle = manifest.pin.oracle();
    let locks = Tree::new("phpt-locks").unwrap();
    let runner = Runner::Engine(binary);
    let under = Under::of(&runner, locks.path());
    run_subset(&oracle, registry, subset, under).unwrap().0
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

/// Under a tier's lane with no binary named, the binary under test is
/// `php` built with `php_bundle/jit` into `target/featured/jit`, where the
/// witness lane builds it (#2721), and the reading's `run by` names that
/// binary as it names any engine. Before #3053 `--jit force` resolved the
/// default build, which binds no tier, so its reading was the plain
/// lane's under another name. A variant's row is built with its feature
/// and the tier's; a named binary, the control and a run with no lane
/// build nothing.
#[test]
fn a_tier_lane_without_engine_resolves_the_build_with_the_tier() {
    let root = Path::new("/workspace");
    let lane = |engine| Ask {
        lane: Some(Lane::Force),
        engine,
        ..Ask::default()
    };
    let tier = Build::under_test(&lane(Engine::Built)).expect("--jit force builds its engine");
    assert_eq!(tier.features(), "php_bundle/jit");
    assert_eq!(tier.target_dir(root), root.join("target/featured/jit"));
    assert_eq!(tier.target_dir(root), profile::featured(root, "jit"));
    let row = Build::row("zend_test", Some(&tier));
    assert_eq!(row.features(), "php_bundle/zend_test,php_bundle/jit");
    assert_eq!(
        row.target_dir(root),
        root.join("target/featured/zend_test+jit")
    );
    let plain = Build::row("zend_test", None);
    assert_eq!(
        plain.target_dir(root),
        root.join("target/featured/zend_test")
    );
    for ask in [
        lane(Engine::Named(PathBuf::from("/usr/bin/php"))),
        lane(Engine::Control),
        Ask {
            unkept: true,
            ..lane(Engine::Built)
        },
        Ask::default(),
    ] {
        assert_eq!(Build::under_test(&ask), None, "{ask:?}");
    }
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
        holds: Holds::Every,
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
        "--TEST--\nspawn\n--INI--\nprecision=3\n--SKIPIF--\n<?php if (getenv('TEST_PHP_EXTRA_ARGS') === false || getenv('TEST_PHP_CGI_EXECUTABLE') === '') echo 'skip no environment';\n--FILE--\n<?php\n$php = getenv('TEST_PHP_EXECUTABLE_ESCAPED');\n$extra = getenv('TEST_PHP_EXTRA_ARGS');\necho shell_exec(\"$php $extra -r 'echo 1/3, ini_get(\\\"error_reporting\\\");'\"), \"\\n\";\nvar_dump(getenv('TEST_PHP_EXECUTABLE') === realpath(getenv('TEST_PHP_EXECUTABLE')));\n--EXPECT--\n0.33330719\nbool(true)\n",
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
            "--TEST--\nn{n}\n--SKIPIF--\n<?php echo 'info ok';\n--FILE--\n<?php echo {n};\n--CLEAN--\n<?php echo 'clean';\n--EXPECT--\n{n}\n"
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
    let locks = Tree::new("phpt-locks").unwrap();
    let runner = Runner::Engine(binary);
    let under = Under::of(&runner, locks.path());
    let tally = run_subset(&oracle, &registry(""), &subset, under)
        .unwrap()
        .0;
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
    // The miss keeps the lines where the two part and what they come to,
    // decoded where the verdict was made (#1185).
    assert_eq!(
        tally.judged[0].miss,
        Some(miss::Miss {
            expected: Some("five".to_owned()),
            actual: Some("four".to_owned()),
            signature: miss::Signature::Differs,
        })
    );
}

#[test]
fn a_program_past_the_recipes_limit_fails_its_test_and_the_lane_goes_on() {
    // Room for the quick tests' php to start under any load the machine
    // runs at; one second was not, at a load average of 48 to 75 (#1105).
    const LIMIT: u64 = 10;
    if oracle().is_none() {
        return;
    }
    // Three tests in order, the middle one a program that never ends and
    // says first which process it is. Under the limit it is that test's
    // fail, naming the limit; the process is killed and reaped, not left
    // running; and the test after it is still judged (#691). Before, the
    // limit ended the whole run and took every verdict with it. The
    // program sleeps rather than spins, so its wait adds no load.
    let (tree, subset) = subset("past-limit", &[]);
    let pid = tree.path().join("pid");
    let hangs = format!(
        "--TEST--\nNever ends\n--FILE--\n<?php file_put_contents({:?}, getmypid()); while (true) {{ sleep(1); }}\n--EXPECT--\nnothing\n",
        pid.display().to_string()
    );
    let echo = |n: u8| format!("--TEST--\nn{n}\n--FILE--\n<?php echo {n};\n--EXPECT--\n{n}\n");
    fs::write(tree.join("a.phpt"), echo(1)).unwrap();
    fs::write(tree.join("b.phpt"), hangs).unwrap();
    fs::write(tree.join("c.phpt"), echo(3)).unwrap();
    let binary = oracle().unwrap();
    let mut manifest = manifest();
    manifest.pin.recipe.limits.timeout_s = LIMIT;
    let oracle = manifest.pin.oracle();
    let locks = Tree::new("phpt-locks").unwrap();
    let runner = Runner::Engine(binary);
    let under = Under::of(&runner, locks.path());
    let tally = run_subset(&oracle, &registry(""), &subset, under)
        .unwrap()
        .0;
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
    assert!(
        note.contains(&format!("the recipe's limit of {LIMIT} s")),
        "{note}"
    );
    assert_eq!(tally.against(), ["b.phpt"]);
    assert_eq!(tally.judged[1].miss, Some(miss::Miss::past_limit()));
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

/// A test listed only in an accepted row's `covers` is diverged, as one
/// that is the row's own id is, and the same subset without the row fails
/// (harness §4.3, revision 62).
#[test]
fn a_missed_expectation_a_row_covers_is_diverged_and_fails_without_it() {
    if oracle().is_none() {
        return;
    }
    let (_tree, subset) = subset(
        "covered",
        &[(
            "missed.phpt",
            "--TEST--\nA wrong expectation\n--FILE--\n<?php echo \"four\";\n--EXPECT--\nfive\n",
        )],
    );
    let covering = registry(
        "[[divergence]]\nid = \"phpt:fixture-cause\"\nkind = \"phpt\"\nreason = \"One reason a person wrote for every test it covers.\"\nstatus = \"accepted\"\nsince = \"2026-09-28\"\ncovers = [\"phpt:php-src/ext/fixture/tests/missed.phpt\"]\n",
    );
    let tally = run(&subset, &covering);
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Diverged, "{tally:#?}");
    assert!(note.contains("One reason a person wrote"), "{note}");

    let tally = run(&subset, &registry(""));
    let (judgement, _) = only(&tally);
    assert_eq!(judgement, Judgement::Fail, "{tally:#?}");
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

/// The fixture at `xtask/fixtures/phpt/UnreadConstruct.phpt` (#395): a
/// program php runs and this compiler cannot read. The refusal
/// `php_compile` gives that program ends in the words [`adapter::unread`]
/// reads, so the lane counts the test a construct not read yet and not a
/// failure. The engine here is the one linked into the harness, since the
/// fixture holds one program and not a subset to run.
///
/// The construct moves as the compiler grows, and the fixture moves with
/// it: `$a and $b`, `1 | 2`, `print`, `eval` and `C::$$p` were each read in
/// turn (#483, #1214). While an expression stood unread, the fixture put it
/// inside parentheses, since `emit`'s walk names the construct inside them
/// and not the parentheses (#595); `php_compile`'s own test,
/// `an_unread_expression_inside_parentheses_names_the_expression`, holds
/// that half. With no expression left unread, the fixture was the one
/// statement the census of Zend/tests still counted, `declare(encoding)`,
/// until #1272 read it (#1352). Since then the fixture is a refusal
/// `php_compile`'s own tests hold, `declare(ticks=1): … enddeclare;`,
/// the alternative syntax php runs and no test of Zend/tests writes.
#[test]
fn a_refusal_of_the_compiler_is_read_as_unread() {
    let pin = manifest().pin;
    let fixture = crate::package_dir().join(FIXTURE);
    let phpt = file::Phpt::load(&fixture).unwrap();
    let program = phpt.section("FILE").expect("the fixture has a --FILE--");
    let snippet = String::from_utf8_lossy(program)
        .strip_prefix("<?php\n")
        .expect("the program opens with the tag the recipe's preamble writes")
        .to_owned();
    let served = crate::oracle::linked::serve(&pin.recipe, None, &snippet).unwrap();
    assert_eq!(
        adapter::unread(&served.stdout),
        Some("a `declare` in the alternative syntax this compiler cannot read yet".to_owned()),
        "{}",
        String::from_utf8_lossy(&served.stdout)
    );
}

/// The one fixture the test above reads.
const FIXTURE: &str = "fixtures/phpt/UnreadConstruct.phpt";

#[test]
fn a_skipif_is_judged_by_what_it_says_as_upstream_judges_it() {
    if oracle().is_none() {
        return;
    }
    for (skipif, judgement, why) in [
        (
            "echo \"skip nothing to see here\\n\";",
            Judgement::Skip,
            "nothing to see here",
        ),
        // A warning before the skip, as mcast_ipv6_recv.phpt's unsilenced
        // `socket_sendto` writes one: upstream borks it (#2857).
        (
            "trigger_error('w', E_USER_WARNING); echo 'skip';",
            Judgement::Bork,
            "invalid output from SKIPIF: Warning: w in ",
        ),
        // A SKIPIF a signal ended having written nothing: upstream's
        // `Termsig=` line is what it wrote (#2908).
        (
            "posix_kill(getmypid(), 9);",
            Judgement::Bork,
            "invalid output from SKIPIF: Termsig=9",
        ),
        // A `warn` with a reason runs the test, whose failure says it.
        (
            "echo 'warn slow here';",
            Judgement::Fail,
            "warn: slow here\n",
        ),
    ] {
        let test = format!(
            "--TEST--\nSkipped\n--SKIPIF--\n<?php {skipif}\n--FILE--\n<?php echo \"ran\";\n--EXPECT--\nnever\n"
        );
        let (_tree, subset) = subset("skipif", &[("skipped.phpt", &test)]);
        let tally = run(&subset, &registry(""));
        let (judged, said) = only(&tally);
        assert!(judged == judgement && said.starts_with(why), "{tally:#?}");
    }
}

/// A test whose `--XFAIL--` section is there and which passes is
/// upstream's lone `WARN` and this runner's `Warn`, which `--upstream`
/// reads alike (#2922).
#[test]
fn an_xfail_test_that_passes_is_warned_alike_by_both_runners() {
    let (Some(_), Some(source)) = (oracle(), source()) else {
        return;
    };
    let test = "--TEST--\nKnown\n--XFAIL--\nknown\n--FILE--\n<?php echo 'ok';\n--EXPECT--\nok\n";
    let (tree, subset) = subset("xpass", &[("xpass.phpt", test)]);
    let ours = only(&run(&subset, &registry(""))).0;
    fs::copy(source.join("run-tests.php"), tree.join("run-tests.php")).unwrap();
    let pin = manifest().pin;
    let said = upstream::upstream(pin.oracle().pin(), &tree, &["xpass.phpt".into()]).unwrap();
    let theirs = upstream::verdict(&said, Path::new("xpass.phpt"));
    assert_eq!((ours, theirs), (Judgement::Warn, Some("WARN")));
    assert!(upstream::alike(Some(ours), theirs));
}

/// A green reading's message names every verdict its run gave, each by
/// the key its count is kept under, and the counts it names add up to
/// the tests it says it is over (#2939).
#[test]
fn a_green_message_accounts_for_every_test_it_is_over() {
    use Judgement::{Diverged, Pass, Skip, Unread, Unsent, Warn, XFail};
    let (_tree, subset) = subset("green-words", &[]);
    let verdicts = [Pass, Pass, Skip, XFail, Warn, Diverged, Unread, Unsent];
    let tally = Tally {
        judged: verdicts.map(|j| Judged::new("id", "name", j, "")).into(),
        found: verdicts.len(),
    };
    let (outcome, source) = (Outcome::Passed, Path::new("."));
    let message = words(
        "ext/fixture",
        &tally,
        &outcome,
        &subset,
        false,
        source,
        &manifest(),
    );
    let counts = "2 passed, 1 skipped, 1 xfailed, 1 warned, 1 diverged, 1 unread, 1 unsent";
    assert_eq!(message, format!("ext/fixture: {counts} of 8 tests"));
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
    assert!(
        matches!(&tally.judged[0].miss, Some(miss::Miss { signature: miss::Signature::Bork(why), .. }) if why.contains("must start with --TEST--")),
        "{tally:#?}"
    );
}

/// The fixture at `xtask/fixtures/phpt/cgi` (#1198): a `--POST--` test
/// with a query string, a cookie and a header it wants, which passes only
/// where the runner hands it to a CGI host with its request described, so
/// a runner that skips it, or serves it the command line, fails here; and
/// its negative control, whose body is met and whose headers are not. The
/// oracle stands in for the engine, so the host is the php-cgi `[sapi]` pins.
#[test]
fn a_post_test_runs_through_the_cgi_host() {
    if oracle().is_none() {
        return;
    }
    let (_tree, subset) = fixture_subset("cgi");
    let tally = run(&subset, &registry(""));
    let went: Vec<(&str, Judgement)> = tally
        .judged
        .iter()
        .map(|j| (j.name.as_str(), j.judgement))
        .collect();
    assert_eq!(
        went,
        [
            ("headers.phpt", Judgement::Fail),
            ("post.phpt", Judgement::Pass)
        ],
        "{tally:#?}"
    );
    let note = &tally.judged[0].note;
    assert!(
        note.contains("-X-Never: 1") && note.contains("--HEADERS--"),
        "{note}"
    );
}

/// The fixture at `xtask/fixtures/phpt/fpm` (#1291): a test standing in
/// `sapi/fpm/tests` finds a php-fpm where `FPM\Tester::findExecutable`
/// looks last, which the runner put there, so a runner that puts none
/// fails here; the same test standing anywhere else finds none and skips.
/// The oracle stands in for the engine, so the php-fpm is the one `[sapi]`
/// pins.
#[test]
fn a_test_of_sapi_fpm_finds_the_process_manager() {
    if oracle().is_none() || manifest().pin.sapi.fpm.is_none() {
        return;
    }
    let tree = Tree::new("phpt-judge-fpm").unwrap();
    let tests = tree.join("sapi/fpm/tests");
    copy(&fixture("fpm"), &tests);
    let subset = Subset {
        scope: "sapi/fpm".to_owned(),
        dirs: vec![Dir {
            under: PathBuf::from("sapi/fpm/tests"),
            dir: tests.clone(),
        }],
        root: tree.path().to_path_buf(),
        holds: Holds::Every,
    };
    let tally = run(&subset, &registry(""));
    let found: Vec<(&str, Judgement)> = tally
        .judged
        .iter()
        .map(|j| (j.name.as_str(), j.judgement))
        .collect();
    assert_eq!(found, [("found.phpt", Judgement::Pass)], "{tally:#?}");
    assert!(tree.join("sapi/fpm/php-fpm").is_file());
    let (_elsewhere, subset) = fixture_subset("fpm");
    let tally = run(&subset, &registry(""));
    let skipped: Vec<(&str, Judgement)> = tally
        .judged
        .iter()
        .map(|j| (j.name.as_str(), j.judgement))
        .collect();
    assert_eq!(skipped, [("found.phpt", Judgement::Skip)], "{tally:#?}");
}

/// The fixture at `xtask/fixtures/phpt/phpdbg` (#1304): a `--PHPDBG--`
/// test whose `SKIPIF` skips it outside the debugger and whose program
/// prints the SAPI and its arguments, which passes only where the runner
/// hands it to a phpdbg with its commands on stdin and its `--ARGS--`
/// after `--`, and runs its `SKIPIF` through a phpdbg too, so a control
/// that skips it where the pin names a phpdbg fails here. The oracle
/// stands in for the engine, so the phpdbg is the one `[sapi]` pins.
#[test]
fn a_phpdbg_test_runs_through_the_debugger() {
    if oracle().is_none() || manifest().pin.sapi.phpdbg.is_none() {
        return;
    }
    let (_tree, subset) = fixture_subset("phpdbg");
    let tally = run(&subset, &registry(""));
    let (judgement, note) = only(&tally);
    assert_eq!(judgement, Judgement::Pass, "{note}");
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
        assert_eq!(subset.dirs[0].under, Path::new("ext/pcre"));
        assert_eq!(
            subset.dirs[0].dir,
            tree.join("ext/pcre"),
            "the subset is the tree's the variable names"
        );
        assert_eq!(
            subset.id(&subset.dirs[0].dir.join("tests/preg_match_basic.phpt")),
            "phpt:php-src/ext/pcre/tests/preg_match_basic.phpt",
            "the divergence key names the corpus, not this machine"
        );
    }
}

/// The nightly lane's corpus holds what the rows derive: an extension's
/// subset is `ext/<name>` whole, a nested test directory in it, and an
/// extension with no `.phpt` is no subset of it (#1590, #1598).
#[test]
fn the_corpus_holds_each_extension_whole_as_a_row_derives_it() {
    let tree = fixture_tree();
    for (under, file) in [
        ("Zend/tests", "a.phpt"),
        ("ext/pcre/tests", "b.phpt"),
        ("ext/nested/tests", "c.phpt"),
        ("ext/nested/uchar/tests", "d.phpt"),
        ("ext/hollow/src", "hollow.c"),
        ("sapi/cli/tests", "e.phpt"),
    ] {
        fs::create_dir_all(tree.join(under)).unwrap();
        fs::write(tree.join(under).join(file), "").unwrap();
    }
    let corpus = Subset::corpus(&tree);
    let scopes: Vec<&str> = corpus.iter().map(|s| s.scope.as_str()).collect();
    assert_eq!(
        scopes,
        ["Zend/tests", "ext/nested", "ext/pcre", "sapi/cli/tests"],
        "{corpus:#?}"
    );
    let nested = &corpus[1];
    let manifest = manifest_of([Crate::named("ext/nested")], &tree);
    assert_eq!(
        *nested,
        resolve(&manifest, "ext/nested").unwrap(),
        "the nightly lane's subset is the one the row derives"
    );
    let names: Vec<String> = nested
        .tests()
        .unwrap()
        .iter()
        .map(|test| nested.name(test))
        .collect();
    assert_eq!(names, ["tests/c.phpt", "uchar/tests/d.phpt"]);
}

/// The patterns CI checks the corpus out by (#1713) take every subset the
/// manifest's rows hold, `sapi/cgi/tests`, `sapi/fpm/tests`,
/// `sapi/phpdbg/tests` and the nested `ext/intl/uchar/tests` among them,
/// and every subset of the nightly lane's corpus, as git itself reads
/// them: a fixture repository with one test in each is checked out sparse
/// by the patterns, as the workflows check out php-src, and the negative
/// control is a test outside every root, which stays out.
#[test]
fn the_sparse_patterns_check_out_every_subset_the_manifest_and_the_corpus_hold() {
    use crate::git::git;
    let patterns: Vec<String> = ROOTS.iter().map(|root| root.pattern()).collect();
    assert_eq!(
        patterns,
        ["/Zend/tests/", "/tests/", "/ext/*/", "/sapi/*/tests/"],
        "every .phpt under ext/<name> is the extension's, as Mitchell ruled on #1713"
    );

    let mut held: Vec<PathBuf> = manifest().crates.iter().flat_map(Crate::subsets).collect();
    for host in ["sapi/cgi/tests", "sapi/fpm/tests", "sapi/phpdbg/tests"] {
        assert!(
            held.contains(&PathBuf::from(host)),
            "the tracked rows hold {host}: {held:?}"
        );
    }
    held.extend(["Zend/tests", "tests/lang", "ext/intl/uchar/tests"].map(PathBuf::from));
    let outside = [
        "main/streams/outside.phpt",
        "Zend/outside.phpt",
        "sapi/cli/outside.phpt",
    ];

    let origin = Tree::new("phpt-sparse-origin").unwrap();
    git(&origin, &["init", "-q", "-b", "main"], None).unwrap();
    for under in &held {
        fs::create_dir_all(origin.join(under)).unwrap();
        fs::write(origin.join(under).join("held.phpt"), "").unwrap();
    }
    for file in outside {
        let file = origin.join(file);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, "").unwrap();
    }
    git(&origin, &["add", "-A"], None).unwrap();
    git(
        &origin,
        &[
            "-c",
            "user.name=x",
            "-c",
            "user.email=x@x",
            "commit",
            "-q",
            "-m",
            "corpus",
        ],
        None,
    )
    .unwrap();

    let clone = Tree::new("phpt-sparse-clone").unwrap();
    let from = origin.path().to_string_lossy().into_owned();
    git(
        &clone,
        &["clone", "-q", "--no-checkout", &from, "php-src"],
        None,
    )
    .unwrap();
    let checkout = clone.join("php-src");
    let stdin = patterns.join("\n") + "\n";
    git(
        &checkout,
        &["sparse-checkout", "set", "--no-cone", "--stdin"],
        Some(&stdin),
    )
    .unwrap();
    git(&checkout, &["checkout", "-q"], None).unwrap();

    for under in &held {
        assert!(
            checkout.join(under).join("held.phpt").is_file(),
            "{} is not checked out by {patterns:?}",
            under.display()
        );
    }
    for file in outside {
        assert!(
            !checkout.join(file).exists(),
            "{file} is outside every root and was checked out"
        );
    }
    let scopes = |tree: &Path| -> Vec<String> {
        Subset::corpus(tree)
            .into_iter()
            .map(|subset| subset.scope)
            .collect()
    };
    assert_eq!(
        scopes(&checkout),
        scopes(&origin),
        "the checkout holds every subset of the corpus the whole tree does"
    );
}

/// Every `.phpt` of the pinned tree is in exactly one subset of the
/// nightly lane's corpus, `ext/intl/uchar/tests` among them, so the lane
/// holds the whole corpus and nothing twice (#1598).
#[test]
fn every_test_of_the_pinned_tree_is_in_one_subset_of_the_corpus() {
    let Some(tree) = source() else { return };
    let mut all = Vec::new();
    crate::tally::files_under(&tree, &mut all).unwrap();
    all.retain(|path| path.extension().is_some_and(|e| e == "phpt"));
    all.sort();
    let mut held: Vec<PathBuf> = Subset::corpus(&tree)
        .iter()
        .flat_map(|subset| subset.tests().unwrap())
        .collect();
    held.sort();
    assert_eq!(
        held.len(),
        all.len(),
        "the corpus holds a test twice or misses one"
    );
    assert_eq!(held, all);
    assert!(
        held.contains(&tree.join("ext/intl/uchar/tests/bug70453.phpt")),
        "ext/intl/uchar/tests is held"
    );
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
    assert_eq!(extension.dirs[0].dir, tree.join("ext/pcre"));
    assert_eq!(extension.absent(), Vec::<&Path>::new(), "{extension:#?}");
}

#[test]
fn a_tier_lane_and_a_control_are_readings_of_their_own() {
    let tree = fixture_tree();
    let subset = resolve(&tracked_over_fixture(&tree), "pcre").unwrap();
    let engine = Runner::Engine(PathBuf::from("target/debug/php"));
    let scope = |runner: &Runner, lane| subset.scope(runner, lane).to_string();
    assert_eq!(scope(&engine, None), "ext/pcre");
    assert_eq!(scope(&engine, Some(Lane::Force)), "ext/pcre@force");
    assert_eq!(scope(&engine, Some(Lane::ExitFuzz)), "ext/pcre@exit-fuzz");
    // A control of the runner keeps a reading of its own, so a green one
    // can never sit where the engine's reading belongs.
    let control = Runner::Control(PathBuf::from("php"));
    assert_eq!(scope(&control, None), "ext/pcre@control");
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

/// An extension's row derives its subset, `ext/<name>`, and where the
/// pinned tree has no such directory, or no `.phpt` under it, the
/// extension has no tests of its own, as `ext/lexbor` has none
/// (#1598): its reading, asked for by name and in
/// the lane alike, is a skip naming the directory, not the standing fail
/// a row that names a directory the tree has not earns. A subset named on
/// the command line that is no crate and no directory is still refused,
/// so a typo is not a skip (#1231).
#[test]
fn an_extension_with_no_test_directory_reads_as_a_skip_and_a_name_that_is_nothing_fails() {
    let tree = fixture_tree();
    // `ext/hollow` stands in the tree with no `.phpt` under it, as
    // `ext/lexbor` does in php-src; `ext/testless` is not there at all.
    fs::create_dir_all(tree.join("ext/hollow/src")).unwrap();
    fs::write(tree.join("ext/hollow/src/hollow.c"), "").unwrap();
    let manifest = manifest_of(
        [Crate::named("ext/testless"), Crate::named("ext/hollow")],
        &tree,
    );
    let registry = registry("");
    for (subset, has) in [
        (Some("ext/testless"), "has no such directory"),
        (Some("ext/hollow"), "holds no .phpt under it"),
        (None, ""),
    ] {
        let ask = Ask {
            subset: subset.map(str::to_owned),
            jobs: NonZeroUsize::new(1),
            ..Ask::default()
        };
        let readings = super::run(&workspace(), &manifest, &registry, &ask).unwrap();
        assert_eq!(
            readings.len(),
            if subset.is_some() { 1 } else { 2 },
            "{readings:#?}"
        );
        for reading in &readings {
            assert!(
                ["ext/testless", "ext/hollow"].contains(&reading.scope.as_str()),
                "{reading:#?}"
            );
            assert_eq!(reading.status, Status::Skip, "{reading:#?}");
            assert!(
                reading
                    .message
                    .contains(&format!("derives {}", reading.scope))
                    && reading.message.contains(has)
                    && reading.message.contains("no tests of its own"),
                "{}",
                reading.message
            );
        }
    }
    let ask = Ask {
        subset: Some("ext/nosuch".to_owned()),
        ..Ask::default()
    };
    let refused = super::run(&workspace(), &manifest, &registry, &ask).unwrap_err();
    assert!(format!("{refused:#}").contains("ext/nosuch"), "{refused:#}");
}

#[test]
fn the_per_commit_lane_is_composed_from_the_crate_rows() {
    let tree = fixture_tree();
    let manifest = manifest_of(
        [
            Crate::named("php_engine").with_phpt(&["Zend/tests", "tests"]),
            Crate::named("php_heap"),
            Crate::named("sapi/cli").with_phpt(&["sapi/cli/tests"]),
            Crate::named("ext/pcre"),
            Crate::named("ext/openssl").with_feature("openssl"),
            Crate::named("ext/curl").with_feature("curl"),
            Crate::named("ext/zend_test")
                .with_feature("zend_test")
                .with_oracle("zend-test"),
        ],
        &tree,
    );
    let lane: Vec<String> = Subset::lane(&manifest, &Carried::turning_on(&["openssl"]), &tree)
        .into_iter()
        .map(|subset| subset.scope)
        .collect();
    // §7's per-commit row: Zend/tests and tests/ with php_engine, sapi/cli
    // with its host, and the extensions the binary carries: the default
    // set's and the one behind a feature it turns on (revision 63). One
    // behind a feature it does not turn on is the nightly lane's unless
    // its row names an oracle variant, which the lane runs on the engine
    // built with the feature (#2029), and a crate with no subset owns none.
    assert_eq!(
        lane,
        [
            "php_engine",
            "sapi/cli",
            "ext/pcre",
            "ext/openssl",
            "ext/zend_test"
        ]
    );
}

/// The lane over the tracked manifest and the binary's own declaration:
/// every row whose `feature` the binary turns on and which owns a subset
/// is in it, so a lane that drops one, by a filter that reads the
/// manifest's `feature` alone or by a second list of the binary's
/// features gone stale, is this test red (revision 63, #1252).
#[test]
fn the_lane_runs_every_feature_row_the_binary_turns_on() {
    let tree = Tree::new("phpt-lane-carried").unwrap();
    let manifest = tracked_over_fixture(&tree);
    let carried = Carried::binary(&workspace()).unwrap();
    let carried_rows: Vec<&Crate> = manifest
        .crates
        .iter()
        .filter(|krate| krate.feature.is_some() && carried.carries(krate))
        .filter(|krate| !krate.subsets().is_empty())
        .collect();
    assert!(
        !carried_rows.is_empty(),
        "the binary turns on no feature row"
    );
    for krate in &carried_rows {
        for under in krate.subsets() {
            fs::create_dir_all(tree.join(under)).unwrap();
        }
    }
    let lane: Vec<String> = Subset::lane(&manifest, &carried, &tree)
        .into_iter()
        .map(|subset| subset.scope)
        .collect();
    for krate in carried_rows {
        assert!(
            lane.contains(&krate.name),
            "the lane drops {}, whose feature the binary turns on: {lane:?}",
            krate.name
        );
    }
    let left_out = manifest
        .crates
        .iter()
        .filter(|krate| !carried.carries(krate) && krate.oracle.is_none())
        .find(|krate| lane.contains(&krate.name));
    assert_eq!(left_out, None, "{lane:?}");
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
    let locks = Tree::new("phpt-locks").unwrap();
    let under = Under {
        oracle_absent: Some(&why),
        ..Under::of(&engine, locks.path())
    };
    let tally = run_subset(&oracle, &registry(""), &subset, under)
        .unwrap()
        .0;
    assert!(tally.judged.is_empty(), "{tally:#?}");
    assert_eq!(tally.found, 1);
    assert_eq!(manifest.pin.digests_asserted(), 0);
    let source = manifest.pin.source.tree().unwrap_or_default();
    let reading = reading(
        &tally,
        &crate::key::Snapshot::disk(&workspace()).unwrap(),
        &manifest,
        &source,
        &subset,
        under,
    )
    .unwrap();
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
            ("EXPECTF", Expectation::Format as fn(Vec<u8>) -> _),
            ("EXPECTREGEX", Expectation::Pattern),
        ] {
            let Some(body) = phpt.section(section) else {
                continue;
            };
            seen += 1;
            let expectation = expectation(body.trim_ascii().to_vec());
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

/// A verdict as the adapter reaches it, with the miss it keeps.
fn missed(name: &str, judgement: Judgement, miss: Option<miss::Miss>) -> Judged {
    let judged = Judged::new(
        &format!("phpt:php-src/Zend/tests/{name}"),
        name,
        judgement,
        "",
    );
    match miss {
        Some(miss) => judged.keeping(miss),
        None => judged,
    }
}

/// The census folds the misses by the signature each kept, the widest
/// first, and a tie is broken by the signature's own order so one run
/// prints the same twice (#617, #1185). The unread census is the groups
/// whose signature is the compiler's refusal; a fail whose line reads
/// like one is not one, and a skip keeps nothing.
#[test]
fn the_census_groups_every_miss_by_signature_and_the_unread_by_construct() {
    let foreach = || {
        Some(miss::Miss::unread(
            "a `foreach` this compiler cannot read yet",
        ))
    };
    let eval = || {
        Some(miss::Miss::unread(
            "an `eval` this compiler cannot read yet",
        ))
    };
    let tally = Tally {
        judged: vec![
            missed("a.phpt", Judgement::Unread, foreach()),
            missed("b.phpt", Judgement::Unread, eval()),
            missed("c.phpt", Judgement::Unread, foreach()),
            missed(
                "d.phpt",
                Judgement::Fail,
                Some(miss::Miss::bork(
                    "a `foreach` this compiler cannot read yet",
                )),
            ),
            missed("e.phpt", Judgement::Skip, None),
            missed("f.phpt", Judgement::Fail, Some(miss::Miss::past_limit())),
            missed("g.phpt", Judgement::Fail, Some(miss::Miss::past_limit())),
            missed("h.phpt", Judgement::Fail, Some(miss::Miss::past_limit())),
        ],
        found: 8,
    };
    let mut census = census::Census::default();
    census.add(census::misses(&tally));
    census.accounts_for(census::verdicts(&tally)).unwrap();

    assert_eq!(census.missed(), 7);
    assert_eq!(census.unread(), 3);
    let rows = census.rows();
    let widths: Vec<(String, usize)> = rows
        .iter()
        .map(|row| (row.signature.to_string(), row.tests.len()))
        .collect();
    assert_eq!(
        widths,
        [
            ("ran past the recipe's limit".to_owned(), 3),
            ("a `foreach` this compiler cannot read yet".to_owned(), 2),
            ("an `eval` this compiler cannot read yet".to_owned(), 1),
            (
                "bork: a `foreach` this compiler cannot read yet".to_owned(),
                1
            ),
        ]
    );
    // Each test by its path in the corpus, so a lane's groups tell two
    // subsets' `001.phpt` apart.
    assert_eq!(rows[1].tests, ["Zend/tests/a.phpt", "Zend/tests/c.phpt"]);
    let unread: Vec<String> = census
        .unread_rows()
        .iter()
        .map(|row| row.signature.to_string())
        .collect();
    assert_eq!(
        unread,
        [
            "a `foreach` this compiler cannot read yet",
            "an `eval` this compiler cannot read yet"
        ]
    );

    // A second subset folded in adds to the group it shares, which is
    // what the lane's census is.
    census.add(census::misses(&Tally {
        judged: vec![missed("i.phpt", Judgement::Unread, eval())],
        found: 1,
    }));
    assert_eq!(census.unread(), 4);
    assert_eq!(census.unread_rows()[1].tests.len(), 2);
}

/// A census with nothing in it is the answer a subset that met every
/// expectation gives, and it says so rather than printing a heading over
/// no rows.
#[test]
fn a_subset_that_missed_nothing_has_an_empty_census() {
    let tally = Tally {
        judged: vec![Judged::new("a.phpt", "a.phpt", Judgement::Pass, "")],
        found: 1,
    };
    let mut census = census::Census::default();
    census.add(census::misses(&tally));
    census.accounts_for(census::verdicts(&tally)).unwrap();
    assert_eq!(census.missed(), 0);
    assert_eq!(census.unread(), 0);
    assert!(census.rows().is_empty());
}

/// The run at `xtask/fixtures/phpt/census/run.json`, as `xtask phpt`
/// keeps one.
fn census_fixture() -> against::Run {
    let file = crate::package_dir().join("fixtures/phpt/census/run.json");
    serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap()
}

/// The kept run's verdicts as [`census::Census::accounts_for`] reads them.
fn kept(run: &against::Run) -> impl Iterator<Item = (&str, Judgement, Option<&miss::Miss>)> {
    run.verdicts
        .iter()
        .map(|(id, judgement)| (id.as_str(), *judgement, run.misses.get(id)))
}

/// The census of the fixture run groups each of its twelve misses under
/// its signature, the widest first, and nothing the run met, skipped or
/// expected to fail (#1185).
#[test]
fn the_census_of_a_kept_run_groups_every_miss_it_kept() {
    let run = census_fixture();
    let mut census = census::Census::default();
    census.add(run.misses.iter().map(|(id, miss)| (id.as_str(), miss)));
    census.accounts_for(kept(&run)).unwrap();
    assert_eq!(census.missed(), 12);
    let rows: Vec<(String, Vec<String>)> = census
        .rows()
        .into_iter()
        .map(|row| (row.signature.to_string(), row.tests))
        .collect();
    let row = |signature: &str, tests: &[&str]| {
        (
            signature.to_owned(),
            tests.iter().map(|t| format!("Zend/tests/{t}")).collect(),
        )
    };
    assert_eq!(
        rows,
        [
            row(
                "undefined function array_any",
                &["function_a.phpt", "function_b.phpt", "function_c.phpt"]
            ),
            row(
                "a `declare(encoding)` this compiler cannot read yet",
                &["unread_a.phpt", "unread_b.phpt"]
            ),
            row(
                "Warning: Undefined variable $x in _ on line _",
                &["warning_a.phpt", "warning_b.phpt"]
            ),
            row("Class \"Foo\" not found", &["class.phpt"]),
            row("output differs", &["wrong.phpt"]),
            row(
                "php only: Fatal error: Cannot redeclare function f() (previously declared in _:_) in _ on line _",
                &["php_only.phpt"]
            ),
            row("ran past the recipe's limit", &["hang.phpt"]),
            row(
                "bork: missing section --EXPECT--, --EXPECTF-- or --EXPECTREGEX--",
                &["bork.phpt"]
            ),
        ]
    );
}

/// The negative control: a census that drops a miss from its groups
/// fails, and so does one built from a run that dropped a miss, naming
/// the test either way.
#[test]
fn a_census_that_drops_a_miss_fails() {
    let run = census_fixture();
    let mut census = census::Census::default();
    census.add(
        run.misses
            .iter()
            .filter(|(id, _)| !id.ends_with("/warning_b.phpt"))
            .map(|(id, miss)| (id.as_str(), miss)),
    );
    let why = census.accounts_for(kept(&run)).unwrap_err();
    assert!(
        why.starts_with("Zend/tests/warning_b.phpt missed with"),
        "{why}"
    );

    let mut dropped = run.clone();
    dropped.misses.remove("phpt:php-src/Zend/tests/hang.phpt");
    let mut census = census::Census::default();
    census.add(dropped.misses.iter().map(|(id, miss)| (id.as_str(), miss)));
    let why = census.accounts_for(kept(&dropped)).unwrap_err();
    assert_eq!(
        why,
        "Zend/tests/hang.phpt is a fail and kept no miss, so no group of the census holds it"
    );
}

/// A fixture directory under `xtask/fixtures/phpt` laid down in a scratch
/// tree standing where `php-src/ext/fixture/tests` would, so what its
/// programs write lands there and not in the fixture.
fn fixture_subset(name: &str) -> (Tree, Subset) {
    let (tree, subset) = subset(name, &[]);
    copy(&fixture(name), tree.path());
    (tree, subset)
}

/// A fixture directory under `xtask/fixtures/phpt`.
fn fixture(name: &str) -> PathBuf {
    crate::package_dir().join("fixtures/phpt").join(name)
}

/// Every file under `from` copied to the same place under `to`.
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

/// The redirect fixture laid down as a tree standing where the pinned
/// php-src would, and the subset of one directory of it.
fn redirect_subset(tree: &Tree, under: &str) -> Subset {
    copy(&fixture("redirect"), tree.path());
    Subset {
        scope: "ext/driver".to_owned(),
        dirs: vec![Dir::at(tree.path(), PathBuf::from(under))],
        root: tree.path().to_path_buf(),
        holds: Holds::Every,
    }
}

/// `--unkept`'s reading of a redirect (#1830): a target stands for the
/// redirecting test only where it kept a verdict through it for every test
/// of the set as the tree holds it now. A test added to the set is one the
/// target predates, as one added to the subset is.
#[test]
fn a_redirect_stands_only_where_every_test_of_its_set_was_kept_through_it() {
    let tree = Tree::new("redirect-kept").unwrap();
    let subset = redirect_subset(&tree, "ext/driver/tests");
    let by = subset.id(&tree.path().join("ext/driver/tests/common.phpt"));
    let through = |name: &str| {
        format!("phpt:php-src/ext/shared/tests/{name} via php-src/ext/driver/tests/common.phpt")
    };
    let both = [through("dsn.phpt"), through("second.phpt")];
    assert!(redirect::set_kept(&subset, &by, both.iter()));
    assert!(
        !redirect::set_kept(&subset, &by, both[..1].iter()),
        "second.phpt was not kept"
    );
    assert!(!redirect::set_kept(&subset, &by, std::iter::empty()));
    let other = subset.id(&tree.path().join("ext/driver/tests/own.phpt"));
    assert!(
        !redirect::set_kept(&subset, &other, both.iter()),
        "kept through another test"
    );
    fs::write(
        tree.path().join("ext/shared/tests/third.phpt"),
        "--TEST--\nthird\n",
    )
    .unwrap();
    assert!(
        !redirect::set_kept(&subset, &by, both.iter()),
        "the target predates third.phpt"
    );
}

/// A `--REDIRECTTEST--` runs the set its section names in its place: each
/// test under the environment the section returns and `REDIR_TEST_DIR`,
/// its program written in the redirecting test's directory, needing the
/// redirecting test's extensions beside its own, and its verdict naming
/// both tests; the redirecting test is no verdict of its own. The set's
/// own directory run as a subset has no driver and skips, and a redirect
/// whose extension is not loaded or whose section names no directory runs
/// no set and is a skip or a bork in upstream's words (#814).
#[test]
fn a_redirecttest_runs_its_set_under_the_environment_its_section_returns() {
    if oracle().is_none() {
        return;
    }
    let verdicts = |under: &str| {
        let tree = Tree::new("phpt-redirect").unwrap();
        let tally = run(&redirect_subset(&tree, under), &registry(""));
        tally
            .judged
            .iter()
            .map(|j| (j.id.clone(), j.name.clone(), j.judgement, j.note.clone()))
            .collect::<Vec<_>>()
    };
    let driver = verdicts("ext/driver/tests");
    assert_eq!(
        driver
            .iter()
            .map(|(id, name, judgement, _)| (id.as_str(), name.as_str(), *judgement))
            .collect::<Vec<_>>(),
        [
            (
                "phpt:php-src/ext/shared/tests/dsn.phpt via php-src/ext/driver/tests/common.phpt",
                "ext/shared/tests/dsn.phpt via common.phpt",
                Judgement::Pass
            ),
            (
                "phpt:php-src/ext/shared/tests/second.phpt via php-src/ext/driver/tests/common.phpt",
                "ext/shared/tests/second.phpt via common.phpt",
                Judgement::Pass
            ),
            (
                "phpt:php-src/ext/driver/tests/own.phpt",
                "own.phpt",
                Judgement::Pass
            ),
        ],
        "{driver:#?}"
    );
    let shared = verdicts("ext/shared/tests");
    assert_eq!(shared.len(), 2, "{shared:#?}");
    assert!(
        shared
            .iter()
            .all(|(_, _, judgement, note)| (*judgement, note.as_str())
                == (Judgement::Skip, "no driver")),
        "{shared:#?}"
    );
    let refused = verdicts("ext/refused/tests");
    assert_eq!(
        refused
            .iter()
            .map(|(_, name, judgement, note)| (name.as_str(), *judgement, note.as_str()))
            .collect::<Vec<_>>(),
        [
            (
                "no-extension.phpt",
                Judgement::Skip,
                "Required extension missing: not_an_extension"
            ),
            (
                "no-set.phpt",
                Judgement::Bork,
                "Redirect info must contain exactly one TEST string to be used as redirect directory."
            ),
        ],
        "{refused:#?}"
    );
}

/// A reading counts the corpus's files its subset holds beside the tests
/// its run judged: the driver's directory holds two files, and its run
/// judges the set of two its `common.phpt` names and its own test, so a
/// count of the lane does not hold the set again beside the set's own
/// subset (#1589).
#[test]
fn a_reading_counts_the_files_its_subset_holds_beside_the_tests_a_redirect_ran() {
    let Some(binary) = oracle() else { return };
    let tree = Tree::new("phpt-redirect-files").unwrap();
    let mut subset = redirect_subset(&tree, "ext/driver/tests");
    let tally = run(&subset, &registry(""));
    // A reading is stamped over its scope's tree, so the scope is a crate
    // the manifest has and the tree holds.
    subset.scope = "ext/pcre".to_owned();
    let manifest = manifest();
    let locks = Tree::new("phpt-locks").unwrap();
    let runner = Runner::Engine(binary);
    let under = Under::of(&runner, locks.path());
    let source = manifest.pin.source.tree().unwrap_or_default();
    let reading = reading(
        &tally,
        &crate::key::Snapshot::disk(&workspace()).unwrap(),
        &manifest,
        &source,
        &subset,
        under,
    )
    .unwrap();
    assert_eq!(
        (&reading.numbers["tests"], &reading.numbers["files"]),
        (&3i64.into(), &2i64.into()),
        "{reading:#?}"
    );
}

/// The fixture's subset run with the pinned oracle standing in for the
/// engine, on a pool of `jobs`, under a limit of `limit_s`.
fn run_on(subset: &Subset, jobs: usize, limit_s: u64) -> Tally {
    let binary = oracle().unwrap();
    let mut manifest = manifest();
    manifest.pin.recipe.limits.timeout_s = limit_s;
    let oracle = manifest.pin.oracle();
    let locks = Tree::new("phpt-locks").unwrap();
    let runner = Runner::Engine(binary);
    let under = Under {
        jobs: NonZeroUsize::new(jobs).unwrap(),
        ..Under::of(&runner, locks.path())
    };
    run_subset(&oracle, &registry(""), subset, under).unwrap().0
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

/// The `.here` files a fixture's control pair leaves, each waiting a
/// bounded time for the other's, removed before a run, so the pair meets
/// on this run's files and not on a run's before it (#1105).
fn clear_here(tree: &Tree, tests: &[&str]) {
    for test in tests {
        let here = tree.join(test).with_extension("php.here");
        if here.exists() {
            fs::remove_file(&here).unwrap();
        }
    }
}

/// The fixture at `xtask/fixtures/phpt/conflicts` (#938), on a pool of
/// eight, which cuts its seven tests that do not run alone into batches of
/// two neighbours: the two tests naming `shared` never run at once, the one
/// naming `all` runs beside nothing, the directory's `CONFLICTS` file holds
/// its two tests apart, and a test's own section stands in place of that
/// file. The pairs held apart stand in two batches each, so what holds them
/// is the key. The negative controls are the tests naming no key, which
/// run at once, and `own.phpt`, which runs beside the directory's second;
/// each pair waits, bounded, for the other's `.here` file, so they meet
/// however long the load holds one up and not by the clock (#1105). The
/// verdicts, in order and word for word, are the ones a pool of one
/// reaches, which finds the pooled run's `.here` files and waits on none.
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
/// the third, in a batch of its own, runs beside the pair's first, the
/// negative control that shows the pool runs anything at once at all.
/// The control is held on every run: the first and the third each wait,
/// bounded, for the other's `.here` file, so they meet however long the
/// load holds one up, where a race against the clock lost under load
/// (#1152, #1105).
#[test]
fn neighbours_that_make_one_relative_path_are_one_batch_and_never_meet() {
    if oracle().is_none() {
        return;
    }
    let tests = ["a-temp.phpt", "b-temp.phpt", "c-free.phpt"];
    let (tree, subset) = fixture_subset("relative");
    for run in 0..20 {
        clear_here(&tree, &tests);
        let tally = run_on(&subset, 8, 30);
        assert!(
            tally.judged.iter().all(|j| j.judgement == Judgement::Pass),
            "run {run}: {tally:#?}"
        );
        let span = spans(&tree, &tests);
        assert!(span[0].1 <= span[1].0, "run {run}, the batch: {span:?}");
        assert!(
            overlap(span[0], span[2]),
            "run {run}, the control: {span:?}"
        );
    }
}

/// The fixture at `xtask/fixtures/phpt/past-limit` (#938): a test that
/// never ends first, and two after it that conflict with nothing. On a
/// pool of three the batches are two neighbours (run-tests.php:1537-1541,
/// #1091), so the second waits for the first, its batch's, and the third,
/// in a batch of its own, is done before the first is past its limit; on
/// a pool of one, the negative control, the third starts after it too.
/// The verdicts are the same either way: the first fails naming the limit.
///
/// The limit runs from the spawn and the spans from each program's first
/// line, so a php's start-up stands on both sides of the line between
/// "done before the first is past its limit" and "after it", and it took
/// more than 1.7 s at a load average of 50 to 108 (#2293). The line is
/// drawn at half a limit of ten seconds past the first's first line, so
/// the start-ups of two programs may differ by five seconds, and the
/// third's 300 ms run may start that late, before load crosses it
/// (#2315); a limit of three and a line half a second early left 0.5 s.
#[test]
fn a_test_past_the_limit_holds_up_only_its_batch_and_what_it_conflicts_with() {
    const LIMIT: u64 = 10;
    if oracle().is_none() {
        return;
    }
    let tests = ["a-never.phpt", "b-quick.phpt", "c-quick.phpt"];
    let (tree, subset) = fixture_subset("past-limit");
    #[allow(clippy::cast_precision_loss)]
    let past = |span: &[(f64, f64)]| span[0].0 + LIMIT as f64 / 2.0;
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

/// Whether a process of this pid stands, as `kill -0` asks, waiting out
/// the moment a killed one takes to be reaped by whoever adopted it.
fn stands(pid: i32) -> bool {
    let pid = nix::unistd::Pid::from_raw(pid);
    let since = std::time::Instant::now();
    while nix::sys::signal::kill(pid, None).is_ok() {
        if since.elapsed() > std::time::Duration::from_secs(5) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    false
}

/// A test whose program leaves a child sleeping and exits, and one whose
/// program leaves one and passes the limit (#1211): each child is in its
/// program's process group, which the runner kills whole, so neither
/// outlives its run. Each program writes the child's pid beside itself,
/// which is how the test learns it; the child ran, which is the control.
/// A program that exits pays none of its limit, so the first has thirty
/// seconds of room; the second's limit is ten, past which its `sleep(60)`
/// always runs and before which its php has started and written the pid
/// under any load (#2315), where two seconds raced a start-up that took
/// more than 1.7 s at a load average of 50 to 108 (#2293).
#[test]
fn a_programs_children_go_with_it_whether_it_exits_or_passes_the_limit() {
    if oracle().is_none() {
        return;
    }
    let leaves = |then: &str| {
        format!(
            "--TEST--\nleaves a child\n--FILE--\n<?php\n\
             $pid = (int) shell_exec('sleep 120 > /dev/null 2>&1 & echo $!');\n\
             file_put_contents(__DIR__ . '/child.pid', $pid);\n\
             var_dump(posix_kill($pid, 0));\n{then}\n--EXPECT--\nbool(true)\n"
        )
    };
    for (name, then, limit, went) in [
        ("exits", "", 30, Judgement::Pass),
        ("past-limit", "sleep(60);", 10, Judgement::Fail),
    ] {
        let (root, subset) = rooted(name, &leaves(then));
        let tally = run_on(&subset, 1, limit);
        assert_eq!(only(&tally).0, went, "{name}: {tally:#?}");
        let pid: i32 = fs::read_to_string(root.join("ext/fixture/tests/child.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(pid > 0, "{name}: the child never started");
        assert!(!stands(pid), "{name}: the child {pid} outlived its run");
    }
}

/// A process a program started that left its group by `setsid()` and
/// names the program's files, as php-fpm's master names the configuration
/// `FPM\Tester` wrote beside the test: once the program passes the limit
/// it goes with it, and the test's note names it, rather than standing
/// on the test's port until the subset ends (#2777).
#[test]
fn what_a_program_past_the_limit_left_outside_its_group_goes_with_it() {
    if oracle().is_none() {
        return;
    }
    let leaves = r#"--TEST--
leaves a session
--FILE--
<?php
$conf = __DIR__ . '/' . basename(__FILE__, 'php') . 'ini';
$pid = __DIR__ . '/left.pid';
shell_exec("perl -MPOSIX -e 'POSIX::setsid(); open my \$f, q(>), \$ARGV[1]; print \$f \$\$; close \$f; sleep 120' $conf $pid > /dev/null 2>&1 &");
sleep(60);
--EXPECT--
nothing
"#;
    let (root, subset) = rooted("left-session", leaves);
    let (went, note) = only(&run_on(&subset, 1, 10));
    assert_eq!(went, Judgement::Fail, "{note}");
    let pid = fs::read_to_string(root.join("ext/fixture/tests/left.pid")).unwrap();
    let pid: i32 = pid.trim().parse().unwrap();
    assert!(
        note.ends_with(&format!("outside its group: {pid}")),
        "{note}"
    );
    assert!(!stands(pid), "the session {pid} outlived its run");
}

/// A child that shares its program's stdout and writes there after the
/// program exits is read, as upstream reads a test's stdout to its end
/// (#1491): gh20582.phpt's `proc_open`ed `php -r` lost its `var_dump` to
/// the group kill 13 times in 200 at twelve jobs, the oracle's as often as
/// the engine's. One that still holds the pipe at the limit leaves what
/// came by then and goes with its group, so it neither holds the run nor
/// outlives it. The first half's limit is one the 300 ms child meets with
/// room under any load, since a run read to its end pays none of it; two
/// seconds was not, at a load average of 80 to 108 (#2293).
#[test]
fn a_child_sharing_stdout_is_read_to_the_end_and_killed_at_the_limit() {
    if oracle().is_none() {
        return;
    }
    let shares = |child: &str| {
        format!(
            "--TEST--\nshares stdout\n--FILE--\n<?php\n\
             $p = proc_open([PHP_BINARY, '-n', '-r', '{child}'], [1 => STDOUT, 2 => STDOUT], $pipes);\n\
             file_put_contents(__DIR__ . '/child.pid', proc_get_status($p)['pid']);\n\
             echo \"parent\\n\";\n--EXPECT--\nparent\nchild\n"
        )
    };
    let (_root, subset) = rooted(
        "writes-after",
        &shares(r#"usleep(300000); echo "child\n";"#),
    );
    let tally = run_on(&subset, 1, 30);
    assert_eq!(only(&tally).0, Judgement::Pass, "{tally:#?}");

    let (root, subset) = rooted("holds-past-limit", &shares("sleep(120);"));
    let since = std::time::Instant::now();
    let tally = run_on(&subset, 1, 2);
    assert!(
        since.elapsed() < std::time::Duration::from_secs(30),
        "{:?}",
        since.elapsed()
    );
    let (went, note) = only(&tally);
    assert_eq!(went, Judgement::Fail, "{note}");
    let pid: i32 = fs::read_to_string(root.join("ext/fixture/tests/child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(pid > 0, "the child never started");
    assert!(!stands(pid), "the child {pid} outlived its run");
}

/// The fixture at `xtask/fixtures/phpt/mysql` (#1366): a subset of a `MySQL`
/// suite whose server does not answer is refused, by the probe that asks
/// the server and by the runner handed it with no server named, and never
/// read as the skips its tests would make; where the pinned recipe's server
/// answers, the fixture passes under it.
#[test]
fn a_mysql_subset_with_no_server_is_refused_and_not_skipped() {
    if oracle().is_none() {
        return;
    }
    let tree = Tree::new("phpt-judge-mysql").unwrap();
    let tests = tree.join("ext/pdo_mysql/tests");
    copy(&fixture("mysql"), &tests);
    let subset = Subset {
        scope: "ext/pdo_mysql".to_owned(),
        dirs: vec![Dir {
            under: PathBuf::from("ext/pdo_mysql/tests"),
            dir: tests,
        }],
        root: tree.path().to_path_buf(),
        holds: Holds::Every,
    };
    assert!(server::needs(&subset));
    let mut down = manifest();
    down.pin.recipe.env.insert(
        "PDO_MYSQL_TEST_DSN".to_owned(),
        "mysql:host=127.0.0.1;port=1;dbname=test".to_owned(),
    );
    let refused = server::version(&down.pin.oracle(), &subset.scope).unwrap_err();
    assert!(
        refused.to_string().contains("refused, not skipped"),
        "{refused:#}"
    );
    let binary = oracle().unwrap();
    let runner = Runner::Engine(binary);
    let locks = Tree::new("phpt-locks").unwrap();
    let under = Under::of(&runner, locks.path());
    let unserved = run_subset(&down.pin.oracle(), &registry(""), &subset, under);
    assert!(unserved.is_err(), "{unserved:#?}");
    let manifest = manifest();
    let oracle = manifest.pin.oracle();
    let Ok(version) = server::version(&oracle, &subset.scope) else {
        return;
    };
    let under = Under {
        served: Some(&version),
        ..under
    };
    let tally = run_subset(&oracle, &registry(""), &subset, under)
        .unwrap()
        .0;
    assert_eq!(only(&tally).0, Judgement::Pass, "{tally:#?}");
}

/// A lane whose second subset is refused for a `MySQL` server that does
/// not answer still writes the first subset's reading, and the refusal is
/// the second's own failing reading with its cause, so the lane's exit
/// says it without erasing the rest (#1612). Before, the refusal was the
/// lane's error and no reading of the lane was written.
#[test]
fn a_lane_whose_second_subset_is_refused_still_writes_the_first_subsets_reading() {
    if oracle().is_none() {
        return;
    }
    let tree = fixture_tree();
    // `ext/hollow` is first and holds no `.phpt`, so its reading is a skip
    // and nothing runs; `ext/pdo_mysql` is second and needs the server.
    fs::create_dir_all(tree.join("ext/hollow/src")).unwrap();
    copy(&fixture("mysql"), &tree.join("ext/pdo_mysql/tests"));
    let mut manifest = manifest_of(
        [Crate::named("ext/hollow"), Crate::named("ext/pdo_mysql")],
        &tree,
    );
    manifest.pin.recipe.env.insert(
        "PDO_MYSQL_TEST_DSN".to_owned(),
        "mysql:host=127.0.0.1;port=1;dbname=test".to_owned(),
    );
    // An engine no source is newer than, so the run copies the tree and
    // asks the server; it never runs a test, the refusal comes first. The
    // workspace is a scratch one with no engine source in it, so this
    // binary is fresh by construction and never held to the real tree,
    // whose sources a merge writes after the test binary is built (#2557).
    let root = Tree::new("phpt-refused-lane").unwrap();
    crate::permit::scratch_workspace(&workspace(), &root).unwrap();
    let ask = Ask {
        engine: super::Engine::Named(std::env::current_exe().unwrap()),
        jobs: NonZeroUsize::new(1),
        ..Ask::default()
    };
    let readings = super::run(&root, &manifest, &registry(""), &ask).unwrap();
    let scopes: Vec<&str> = readings.iter().map(|r| r.scope.as_str()).collect();
    assert_eq!(scopes, ["ext/hollow", "ext/pdo_mysql"], "{readings:#?}");
    assert_eq!(readings[0].status, Status::Skip, "{readings:#?}");
    let refused = &readings[1];
    assert_eq!(refused.status, Status::Fail, "{refused:#?}");
    assert!(
        refused.message.contains("refused, not skipped"),
        "{}",
        refused.message
    );
    assert_eq!(refused.numbers["refused"], 1i64.into());
    let kept = Tree::new("phpt-lane-store").unwrap();
    let store = crate::reading::Store::at(kept.path());
    let outcome = store.record(&readings).unwrap();
    assert!(
        outcome.exit().is_err(),
        "the lane's exit says nothing was refused"
    );
    for scope in scopes {
        assert!(
            store.read(INSTRUMENT, scope).unwrap().is_some(),
            "{scope} is not written"
        );
    }
}

/// A run's database is named in the DSN as in the two variables php's
/// suites read (#1546), and a database of the runs' is dropped only once
/// the process its name carries is gone; one of another name never.
#[test]
fn a_runs_database_is_named_to_its_programs_and_a_dead_runs_is_dropped() {
    assert_eq!(
        server::with_database("mysql:host=127.0.0.1;dbname=test", "xtask_phpt_7_0"),
        "mysql:host=127.0.0.1;dbname=xtask_phpt_7_0"
    );
    assert_eq!(
        server::with_database("mysql:host=127.0.0.1;port=3306", "x"),
        "mysql:host=127.0.0.1;port=3306;dbname=x"
    );
    let listed = "xtask_phpt_100_0\nxtask_phpt_200_3\nxtask_phpt_x_1\ntest\nxtask_phpt_300_0b\n";
    assert_eq!(server::left(listed, |pid| pid == 200), ["xtask_phpt_100_0"]);
}

/// Each run of a `MySQL` subset runs in a database of its own on the
/// server (#1546): made before the subset, the one its programs connect
/// to through `connect.inc`'s and `inc/config.inc`'s variables, and gone
/// after; a database a run that is gone left is dropped by the next. The
/// control: the run's own pin, with no database laid over it, names
/// `test`, which every run would share.
#[test]
fn a_mysql_subset_runs_in_a_database_of_its_own() {
    if oracle().is_none() {
        return;
    }
    let manifest = manifest();
    let oracle = manifest.pin.oracle();
    if server::version(&oracle, "ext/mysqli").is_err() {
        eprintln!("skipped: the MySQL server the recipe names does not answer");
        return;
    }
    let ask = "$dsn = getenv('PDO_MYSQL_TEST_DSN');
        $db = new PDO($dsn, getenv('PDO_MYSQL_TEST_USER') ?: 'root', getenv('PDO_MYSQL_TEST_PASS') ?: '');
        echo getenv('MYSQL_TEST_DB') ?: 'test', ' ', $db->query('SELECT DATABASE()')->fetchColumn(), ' ',
            implode(',', $db->query(\"SHOW DATABASES LIKE 'xtask\\\\_phpt\\\\_%'\")->fetchAll(PDO::FETCH_COLUMN));";
    let said = |oracle: &crate::oracle::Oracle<'_>| {
        let seen = oracle.run(Program::Snippet(ask), None).unwrap();
        String::from_utf8_lossy(&seen.stdout).trim().to_owned()
    };
    // A run that is gone: a process that has exited, whose number its
    // database carries.
    let mut gone = std::process::Command::new("true").spawn().unwrap();
    let dead = gone.id();
    gone.wait().unwrap();
    let left = format!("xtask_phpt_{dead}_0");
    let made = oracle
        .run(
            Program::Snippet(&format!(
                "$db = new PDO(getenv('PDO_MYSQL_TEST_DSN'), 'root', ''); $db->exec('CREATE DATABASE IF NOT EXISTS `{left}`');"
            )),
            None,
        )
        .unwrap();
    assert_eq!(made.exit, crate::oracle::Ended::Status(0), "{made:?}");
    let name = {
        let database = server::Database::made(&oracle, "ext/mysqli").unwrap();
        let mut pin = manifest.pin.clone();
        database.lay_over(&mut pin);
        let beside = oracle.beside(&pin);
        let seen = said(&beside);
        let mut words = seen.split(' ');
        assert_eq!(words.next(), Some(database.name.as_str()), "{seen}");
        assert_eq!(words.next(), Some(database.name.as_str()), "{seen}");
        let listed: Vec<&str> = words.next().unwrap_or("").split(',').collect();
        assert!(listed.contains(&database.name.as_str()), "{seen}");
        assert!(!listed.contains(&left.as_str()), "{left} stands: {seen}");
        let control = said(&oracle);
        assert!(control.starts_with("test test "), "{control}");
        database.name.clone()
    };
    let after = said(&oracle);
    assert!(!after.contains(&name), "{name} was not dropped: {after}");
}

/// An `--EXPECTREGEX--` is matched as run-tests.php matches it, by the
/// pinned php's `preg_match` (#1543): `ext/zip/tests/bug76524.phpt`'s
/// negative lookahead meets what has no leak line after `ok` and misses
/// what has one, a back-reference is PCRE's, a pattern PCRE does not
/// compile is an error naming the section, and a test carrying the
/// lookahead passes. The control: the RE2 engine takes no look-around.
#[test]
fn an_expectregex_is_matched_as_pcre_matches_it() {
    use crate::oracle::expect::Expectation;
    if oracle().is_none() {
        return;
    }
    let manifest = manifest();
    let oracle = manifest.pin.oracle();
    let leak = Expectation::Pattern("ok((?!memory leaks detected).)*".into());
    assert!(leak.met(b"ok").is_err(), "the RE2 engine took a lookahead");
    assert!(leak.met_under(&oracle, b"ok").unwrap());
    assert!(
        leak.met_under(&oracle, b"ok\nwarning: libzip says so")
            .unwrap()
    );
    assert!(
        !leak
            .met_under(
                &oracle,
                b"ok\n[Fri] Script:  '-'\n=== Total 1 memory leaks detected ==="
            )
            .unwrap()
    );
    let anchored = Expectation::Pattern("o.".into());
    assert!(!anchored.met_under(&oracle, b"xok").unwrap(), "unanchored");
    assert!(anchored.met_under(&oracle, b"o\n").unwrap(), "no s flag");
    let backref = Expectation::Pattern(r"(a)\1".into());
    assert!(backref.met_under(&oracle, b"aa").unwrap());
    let broken = Expectation::Pattern("(".into());
    let error = broken.met_under(&oracle, b"(").unwrap_err().to_string();
    assert!(error.contains("EXPECTREGEX"), "{error}");
    let exact = Expectation::Exact(b"ok".to_vec());
    assert!(exact.met_under(&oracle, b"ok").unwrap());
    let (_root, subset) = rooted(
        "lookahead",
        "--TEST--\nlookahead\n--FILE--\n<?php echo 'ok';\n--EXPECTREGEX--\nok((?!memory leaks detected).)*\n",
    );
    let tally = run_on(&subset, 1, 10);
    assert_eq!(only(&tally).0, Judgement::Pass, "{tally:#?}");
}

/// The divergence key of the fixture subset's test `name`.
fn fixture_id(name: &str) -> String {
    format!("phpt:php-src/ext/fixture/tests/{name}")
}

/// A target kept over the fixture subset: the oracle's verdict on each
/// test named.
fn fixture_target(verdicts: &[(&str, &str)]) -> Target {
    let verdicts: serde_json::Map<String, serde_json::Value> = verdicts
        .iter()
        .map(|(name, j)| (fixture_id(name), serde_json::Value::from(*j)))
        .collect();
    Target::of(
        serde_json::from_value(serde_json::json!({
            "commit": "e01b1f6",
            "corpus": "the fixture's corpus",
            "engine": "php md5 0",
            "verdicts": verdicts,
        }))
        .unwrap(),
    )
}

/// The fixture subset's reading of a linked run that judged each test
/// named as given, held to `target` under `registry`.
fn held_to_fixture(judged: &[(&str, Judgement)], target: &Target, registry: &Registry) -> Reading {
    let tally = Tally {
        judged: judged
            .iter()
            .map(|(name, judgement)| Judged::new(&fixture_id(name), name, *judgement, ""))
            .collect(),
        found: judged.len(),
    };
    let mut reading = Reading::new(INSTRUMENT, "ext/fixture");
    held_to(
        &mut reading,
        (&tally, "ext/fixture"),
        (Some(target), false, None),
        registry,
        (&Runner::Linked, &manifest().pin),
    )
    .unwrap();
    reading
}

/// One count of a reading, where it has one.
fn number(reading: &Reading, key: &str) -> Option<crate::reading::Number> {
    reading.numbers.get(key).copied()
}

/// A target kept over another corpus than the one the subset holds now
/// (#1644), as #1598 grew `ext/intl` by `ext/intl/uchar/tests` under the
/// same pin: the tests it holds no verdict for are counted as `predated`
/// and not `outside`, met-of-target stays over the target's own tests, and
/// the reading says how many and the command that keeps the target again.
/// The negative control is the same run held to a target that has a
/// verdict for every test: nothing predated, and the words say nothing of
/// it.
#[test]
fn a_test_the_kept_target_has_no_verdict_for_is_predated_and_not_outside() {
    use crate::reading::Number;
    let judged = [
        ("met.phpt", Judgement::Pass),
        ("oracle-miss.phpt", Judgement::Fail),
        ("uchar/new.phpt", Judgement::Pass),
        ("uchar/newer.phpt", Judgement::Fail),
    ];
    let registry = Registry::parse("").unwrap();

    let older = fixture_target(&[("met.phpt", "pass"), ("oracle-miss.phpt", "fail")]);
    let reading = held_to_fixture(&judged, &older, &registry);
    assert_eq!(
        number(&reading, "target"),
        Some(Number::Int(1)),
        "{reading:#?}"
    );
    assert_eq!(
        number(&reading, "met"),
        Some(Number::Int(1)),
        "{reading:#?}"
    );
    assert_eq!(
        number(&reading, "outside"),
        Some(Number::Int(1)),
        "{reading:#?}"
    );
    assert_eq!(
        number(&reading, "predated"),
        Some(Number::Int(2)),
        "{reading:#?}"
    );
    let keeps = "1 of 1 target tests met (ext/fixture/tests/met.phpt), 1 outside (missed by the oracle: ext/fixture/tests/oracle-miss.phpt); 2 tests the target predates, kept over another corpus; `xtask phpt --subset ext/fixture --control` keeps it again";
    assert!(reading.message.contains(keeps), "{}", reading.message);

    let current = fixture_target(&[
        ("met.phpt", "pass"),
        ("oracle-miss.phpt", "fail"),
        ("uchar/new.phpt", "pass"),
        ("uchar/newer.phpt", "skip"),
    ]);
    let reading = held_to_fixture(&judged, &current, &registry);
    assert_eq!(
        number(&reading, "target"),
        Some(Number::Int(2)),
        "{reading:#?}"
    );
    assert_eq!(
        number(&reading, "outside"),
        Some(Number::Int(2)),
        "{reading:#?}"
    );
    assert_eq!(number(&reading, "predated"), None, "{reading:#?}");
    assert!(!reading.message.contains("predates"), "{}", reading.message);
}

/// A run whose target misses include a test the engine skipped counts it
/// as `missed_skipped` (#1834), so the forecast reads how many of the
/// tests still to meet are skips off the reading and not an estimate: of
/// the target, `skipped.phpt` skipped is a miss and a skipped one,
/// `failed.phpt` is a miss and not, and `row.phpt` skipped under an
/// accepted row is met and neither; `oracle-skip.phpt`, which the oracle
/// skips too, is outside and never counted. The negative control is the
/// same run with nothing of the target skipped: the count is written, as 0.
#[test]
fn a_run_counts_the_targets_misses_the_engine_skipped() {
    use crate::reading::Number;
    let target = fixture_target(&[
        ("met.phpt", "pass"),
        ("skipped.phpt", "pass"),
        ("failed.phpt", "pass"),
        ("row.phpt", "pass"),
        ("oracle-skip.phpt", "skip"),
    ]);
    let registry = Registry::parse(&format!(
        "[[divergence]]\nid = \"{}\"\nkind = \"phpt\"\nreason = \"a person's reason\"\nstatus = \"accepted\"\nsince = \"2026-10-04\"\n",
        fixture_id("row.phpt")
    ))
    .unwrap();
    let held = |skipped: Judgement| {
        let judged = [
            ("met.phpt", Judgement::Pass),
            ("skipped.phpt", skipped),
            ("failed.phpt", Judgement::Fail),
            ("row.phpt", Judgement::Skip),
            ("oracle-skip.phpt", Judgement::Skip),
        ];
        held_to_fixture(&judged, &target, &registry)
    };

    let reading = held(Judgement::Skip);
    assert_eq!(
        number(&reading, "target"),
        Some(Number::Int(4)),
        "{reading:#?}"
    );
    assert_eq!(
        number(&reading, "met"),
        Some(Number::Int(2)),
        "{reading:#?}"
    );
    assert_eq!(
        number(&reading, "outside"),
        Some(Number::Int(1)),
        "{reading:#?}"
    );
    assert_eq!(
        number(&reading, "missed_skipped"),
        Some(Number::Int(1)),
        "{reading:#?}"
    );

    let reading = held(Judgement::Pass);
    assert_eq!(
        number(&reading, "missed_skipped"),
        Some(Number::Int(0)),
        "{reading:#?}"
    );
}

/// A subset the lane could not run is refused, a fail whose message is
/// the cause; one a full volume kept from running is not refused but not
/// measured, a skip naming the volume, so the session reads the disk and
/// not a red subset (#644).
#[test]
fn a_subset_a_full_volume_kept_from_running_is_a_skip_and_not_a_refusal() {
    let snapshot = crate::key::Snapshot::disk(&workspace()).unwrap();
    let manifest = manifest();
    let tree = Tree::new("phpt-full").unwrap();
    let full = crate::scratch::made(
        std::io::Error::from_raw_os_error(nix::errno::Errno::ENOSPC as i32),
        &tree.join("oracle.lock-taking"),
        "opening the scratch lock",
    )
    .context("running ext/ctype/tests/ctype_alnum_basic.phpt");
    let reading = super::refused(&snapshot, &manifest, "ext/ctype", &full).unwrap();
    assert_eq!(reading.status, Status::Skip, "{}", reading.message);
    let volume = crate::scratch::Full::of(&full)
        .unwrap()
        .volume
        .display()
        .to_string();
    assert!(reading.message.contains(&volume), "{}", reading.message);
    assert!(!reading.numbers.contains_key("refused"), "{reading:?}");

    let other = anyhow::anyhow!("the server did not answer");
    let reading = super::refused(&snapshot, &manifest, "ext/ctype", &other).unwrap();
    assert_eq!(reading.status, Status::Fail);
    assert_eq!(reading.message, "the server did not answer");
}

/// The dashboard's oracle job, the one CI job that runs the phpt lane on
/// `main`, uploads the runs it keeps beside its store (#2362): one upload
/// is the store's log, `oracle`, which `publish` folds, and one is
/// [`against::RUNS`] under [`against::UPLOAD`], the name the fetch line
/// downloads, which `publish`'s download of the stores does not take. The
/// job is the lines from its key to the next job's, each upload the
/// lines from its `uses` to the next step.
#[test]
fn the_oracle_job_uploads_the_kept_runs_beside_its_store() {
    let text = fs::read_to_string(workspace().join(".github/workflows/dashboard.yml")).unwrap();
    let job = |name: &str| -> String {
        let key = |line: &str| {
            line.strip_prefix("  ").is_some_and(|rest| {
                rest.ends_with(':') && rest.starts_with(|c: char| c.is_ascii_lowercase())
            })
        };
        let job: Vec<&str> = text
            .lines()
            .skip_while(|line| *line != format!("  {name}:"))
            .enumerate()
            .take_while(|(n, line)| *n == 0 || !key(line))
            .map(|(_, line)| line)
            .collect();
        assert!(!job.is_empty(), "dashboard.yml has no {name} job");
        job.join("\n") + "\n"
    };
    let oracle = &job("oracle");
    let uploads: Vec<&str> = oracle
        .match_indices("- uses: actions/upload-artifact")
        .map(|(at, _)| {
            let step = &oracle[at..];
            &step[..step[1..]
                .find("\n      - ")
                .map_or(step.len(), |end| end + 1)]
        })
        .collect();
    let named = |name: &str| {
        uploads
            .iter()
            .copied()
            .find(|step| step.contains(&format!("name: {name}\n")))
            .unwrap_or_else(|| panic!("the oracle job uploads no {name}: {oracle}"))
    };
    assert!(
        named("oracle").contains("path: target/harness/log/\n"),
        "{}",
        named("oracle")
    );
    let runs = named(against::UPLOAD);
    assert!(
        runs.contains(&format!("path: {}/\n", against::RUNS)),
        "the upload is the kept runs: {runs}"
    );
    let publish = job("publish");
    let stores = publish
        .lines()
        .find(|line| line.trim_start().starts_with("pattern:"))
        .expect("publish downloads the stores by a pattern");
    assert!(
        stores.contains("oracle") && !stores.contains(against::UPLOAD),
        "publish folds the store and not the runs: {stores}"
    );
}

//! The lane over a scripted shell: a plan's steps answered from a script,
//! with no network and no process, and a runner's log read and matched.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use super::{Came, Held, Outcome, Package, Php, Program, Service, Shell, Step, Verdict, cases};

/// A shell that answers from a script: each tree or file's digest, each
/// log's text, and the exit of each side's suite; every other program
/// exits 0. It keeps every step it was asked to take.
#[derive(Default)]
struct Scripted {
    digests: BTreeMap<PathBuf, String>,
    logs: BTreeMap<Php, String>,
    taken: Vec<Step>,
}

/// Where a run of [`package`] works.
const WORK: &str = "/work";

/// The pinned digests of the package's source and lock.
const SOURCE: &str = "sha256:5e";
const LOCK: &str = "sha256:10";

impl Scripted {
    /// A shell whose source and lock are the pinned ones, the oracle's and
    /// the engine's logs `oracle` and `engine`, none where `None`.
    fn new(oracle: Option<&str>, engine: Option<&str>) -> Scripted {
        let mut shell = Scripted::default();
        shell
            .digests
            .insert(Path::new(WORK).join("source"), SOURCE.to_owned());
        shell
            .digests
            .insert(PathBuf::from("/list/greet.lock"), LOCK.to_owned());
        for (php, log) in [(Php::Oracle, oracle), (Php::Engine, engine)] {
            if let Some(log) = log {
                shell.logs.insert(php, log.to_owned());
            }
        }
        shell
    }

    /// The side a log path belongs to.
    fn side(path: &Path) -> Php {
        if path.ends_with("oracle.xml") {
            Php::Oracle
        } else {
            Php::Engine
        }
    }

    /// The commands taken, as `program args...`.
    fn commands(&self) -> Vec<String> {
        (self.taken.iter())
            .filter_map(|step| match step {
                Step::Run(command) => {
                    Some(format!("{:?} {}", command.program, command.args.join(" ")))
                }
                _ => None,
            })
            .collect()
    }
}

impl Shell for Scripted {
    fn run(&mut self, step: &Step) -> Came {
        self.taken.push(step.clone());
        match step {
            Step::Run(_) | Step::Copy { .. } => Came::Exited(Some(0), String::new()),
            Step::Digest(path) => Came::Digest(self.digests.get(path).cloned().unwrap_or_default()),
            Step::Read(path) => Came::Text(self.logs.get(&Scripted::side(path)).cloned()),
        }
    }
}

/// The services' addresses every test's package is given.
fn services() -> &'static [(String, String)] {
    static SERVICES: std::sync::OnceLock<Vec<(String, String)>> = std::sync::OnceLock::new();
    SERVICES.get_or_init(|| {
        [("MYSQL_HOST", "127.0.0.1"), ("REDIS_PORT", "6379")]
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .to_vec()
    })
}

/// The package every test runs, needing `needs`.
fn package(needs: &[String]) -> Package<'_> {
    Package {
        repository: "https://github.com/fixture/greet",
        commit: "0123456789abcdef0123456789abcdef01234567",
        digest: SOURCE,
        lock: Path::new("/list/greet.lock"),
        lock_digest: LOCK,
        runner: "vendor/bin/phpunit",
        args: &[],
        needs,
        env: services(),
    }
}

/// An XML log of `cases`, each `(class::name, child)`, the child the
/// element a failure, error or skip writes inside the case, empty for a pass.
fn log(cases: &[(&str, &str)]) -> String {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuites>\n<testsuite name=\"all\">\n",
    );
    for (id, child) in cases {
        let (class, name) = id.split_once("::").unwrap();
        if child.is_empty() {
            writeln!(xml, "<testcase name=\"{name}\" class=\"{class}\" classname=\"{class}\" assertions=\"1\"/>").unwrap();
        } else {
            writeln!(
                xml,
                "<testcase name=\"{name}\" class=\"{class}\">{child}</testcase>"
            )
            .unwrap();
        }
    }
    xml + "</testsuite>\n</testsuites>\n"
}

fn judged(verdict: Verdict) -> super::Matched {
    match verdict {
        Verdict::Judged(matched) => matched,
        other => panic!("not judged: {other:?}"),
    }
}

/// The tests the oracle passes are the found set: one the oracle fails,
/// errors or skips is outside, counted and never met, whatever Lela does.
#[test]
fn a_test_the_oracle_does_not_pass_is_outside_and_never_met() {
    let theirs = log(&[
        ("GreetTest::testHello", ""),
        ("GreetTest::testBroken", "<failure type=\"x\">no</failure>"),
        ("GreetTest::testThrows", "<error type=\"E\">boom</error>"),
        ("GreetTest::testLater", "<skipped/>"),
    ]);
    let ours = log(&[
        ("GreetTest::testHello", ""),
        ("GreetTest::testBroken", ""),
        ("GreetTest::testThrows", ""),
        ("GreetTest::testLater", ""),
    ]);
    let mut shell = Scripted::new(Some(&theirs), Some(&ours));
    let matched = judged(super::run(&package(&[]), Path::new(WORK), &mut shell));
    assert_eq!(matched.outside, 3, "{matched:?}");
    let found: Vec<(&str, Held)> = (matched.found.iter())
        .map(|t| (t.id.as_str(), t.held))
        .collect();
    assert_eq!(found, [("GreetTest::testHello", Held::Met)]);
}

/// A test the oracle passes and Lela fails is missed, and one Lela's run
/// does not report, as when a crash ends its suite, is unreported.
#[test]
fn a_test_lela_fails_is_missed_and_one_it_does_not_report_is_unreported() {
    let theirs = log(&[
        ("GreetTest::testHello", ""),
        ("GreetTest::testWorld", ""),
        ("GreetTest::testGone", ""),
    ]);
    let ours = log(&[
        ("GreetTest::testHello", ""),
        (
            "GreetTest::testWorld",
            "<failure type=\"F\">Failed asserting that 1 is 2.\nmore</failure>",
        ),
    ]);
    let mut shell = Scripted::new(Some(&theirs), Some(&ours));
    let matched = judged(super::run(&package(&[]), Path::new(WORK), &mut shell));
    let held: Vec<Held> = matched.found.iter().map(|t| t.held).collect();
    assert_eq!(held, [Held::Unreported, Held::Met, Held::Missed]);
    let world = &matched.found[2];
    assert_eq!(world.id, "GreetTest::testWorld");
    assert!(
        world.note.contains("Failed asserting that 1 is 2."),
        "{world:?}"
    );
    assert_eq!(matched.outside, 0);
}

/// Lela's run that wrote no log at all reports none of the found tests,
/// so every one of them is unreported and none is unfound.
#[test]
fn a_run_of_lela_that_wrote_no_log_leaves_every_found_test_unreported() {
    let theirs = log(&[("A::testOne", ""), ("A::testTwo", "")]);
    let mut shell = Scripted::new(Some(&theirs), None);
    let matched = judged(super::run(&package(&[]), Path::new(WORK), &mut shell));
    assert!(
        matched.found.iter().all(|t| t.held == Held::Unreported),
        "{matched:?}"
    );
    assert_eq!(matched.found.len(), 2);
}

/// A source whose digest is not the pin's is refused before anything of
/// it is installed or run, naming both digests.
#[test]
fn a_source_whose_digest_differs_is_refused_before_anything_runs() {
    let mut shell = Scripted::new(Some(&log(&[("A::testOne", "")])), None);
    shell
        .digests
        .insert(Path::new(WORK).join("source"), "sha256:ff".to_owned());
    let verdict = super::run(&package(&[]), Path::new(WORK), &mut shell);
    let Verdict::Unjudged(why) = verdict else {
        panic!("{verdict:?}")
    };
    assert!(why.contains("sha256:ff") && why.contains(SOURCE), "{why}");
    assert!(why.contains("refused"), "{why}");
    let commands = shell.commands();
    assert!(
        commands.iter().all(|c| c.starts_with("Git ")),
        "nothing past the fetch ran: {commands:?}"
    );
}

/// So is a lock whose digest is not the pin's: nothing is installed from it.
#[test]
fn a_lock_whose_digest_differs_is_refused_before_it_is_installed() {
    let mut shell = Scripted::new(Some(&log(&[("A::testOne", "")])), None);
    shell
        .digests
        .insert(PathBuf::from("/list/greet.lock"), "sha256:ee".to_owned());
    let verdict = super::run(&package(&[]), Path::new(WORK), &mut shell);
    assert!(
        matches!(&verdict, Verdict::Unjudged(why) if why.contains("sha256:ee") && why.contains("lock")),
        "{verdict:?}"
    );
    assert!(!shell.taken.iter().any(|s| matches!(s, Step::Copy { .. })));
    assert!(!shell.commands().iter().any(|c| c.starts_with("Composer")));
}

/// A suite that reports no test under the oracle did not run there: the
/// package is left out, saying so, and Lela's run is not taken.
#[test]
fn a_suite_that_does_not_run_under_the_oracle_is_left_out() {
    for theirs in [None, Some(log(&[]))] {
        let mut shell = Scripted::new(theirs.as_deref(), None);
        let verdict = super::run(&package(&[]), Path::new(WORK), &mut shell);
        assert!(
            matches!(&verdict, Verdict::LeftOut(why) if why.contains("did not run under the oracle")),
            "{verdict:?}"
        );
        assert!(
            !shell
                .commands()
                .iter()
                .any(|c| c.starts_with("Php(Engine)"))
        );
    }
}

/// A package the list marks as needing what the lane does not provide is
/// left out naming it, and nothing of it is fetched; one needing a
/// service the lane provides runs.
#[test]
fn a_package_needing_a_service_the_lane_does_not_provide_is_left_out() {
    let needs = ["memcached".to_owned()];
    let mut shell = Scripted::new(None, None);
    let verdict = super::run(&package(&needs), Path::new(WORK), &mut shell);
    assert!(
        matches!(&verdict, Verdict::LeftOut(why) if why.contains("memcached")),
        "{verdict:?}"
    );
    assert!(shell.taken.is_empty(), "{:?}", shell.taken);
    let needs = ["mysql".to_owned(), "redis".to_owned()];
    let mut shell = Scripted::new(
        Some(&log(&[("A::testOne", "")])),
        Some(&log(&[("A::testOne", "")])),
    );
    let verdict = super::run(&package(&needs), Path::new(WORK), &mut shell);
    assert!(matches!(verdict, Verdict::Judged(_)), "{verdict:?}");
}

/// The plan: fetched at the pinned commit, held to its digest, the lock
/// held and laid in before the install, then the suite once under each
/// php with its log, each side's after the other's.
#[test]
fn the_plan_fetches_at_the_commit_and_runs_the_suite_once_under_each_php() {
    let ours = log(&[("A::testOne", "")]);
    let mut shell = Scripted::new(Some(&ours), Some(&ours));
    judged(super::run(&package(&[]), Path::new(WORK), &mut shell));
    let commit = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(
        shell.commands(),
        [
            "Git init -q source".to_owned(),
            format!("Git fetch -q --depth 1 https://github.com/fixture/greet {commit}"),
            format!("Git -c advice.detachedHead=false checkout -q {commit}"),
            "Composer install --no-interaction --no-progress".to_owned(),
            "Php(Oracle) vendor/bin/phpunit --log-junit /work/oracle.xml".to_owned(),
            "Php(Engine) vendor/bin/phpunit --log-junit /work/engine.xml".to_owned(),
        ]
    );
    let copied = shell.taken.iter().find_map(|s| match s {
        Step::Copy { from, to } => Some((from.clone(), to.clone())),
        _ => None,
    });
    assert_eq!(
        copied,
        Some((
            PathBuf::from("/list/greet.lock"),
            PathBuf::from("/work/source/composer.lock")
        ))
    );
}

/// Every suite's environment carries the services' addresses the package
/// is given, under the oracle, under Lela and at the install, whatever the
/// package needs.
#[test]
fn each_suite_s_environment_carries_the_services_addresses() {
    let ours = log(&[("A::testOne", "")]);
    let mut shell = Scripted::new(Some(&ours), Some(&ours));
    judged(super::run(&package(&[]), Path::new(WORK), &mut shell));
    let suites: Vec<_> = (shell.taken.iter())
        .filter_map(|s| match s {
            Step::Run(c) if c.program != Program::Git => Some(c),
            _ => None,
        })
        .collect();
    assert_eq!(suites.len(), 3);
    for command in suites {
        assert_eq!(command.env, services(), "{command:?}");
    }
    assert_eq!(
        Service::ALL.map(Service::name),
        ["mysql", "postgres", "redis"]
    );
}

/// A test reported twice is met only where it passed every time, on
/// either side, as a data provider's cases sharing one name are.
#[test]
fn a_test_reported_twice_is_met_only_where_every_report_passed() {
    let theirs = log(&[
        ("A::testOne", ""),
        ("A::testOne", ""),
        ("A::testTwo", ""),
        ("A::testTwo", "<skipped/>"),
    ]);
    let ours = log(&[
        ("A::testOne", ""),
        ("A::testOne", "<error type=\"E\">x</error>"),
    ]);
    let mut shell = Scripted::new(Some(&theirs), Some(&ours));
    let matched = judged(super::run(&package(&[]), Path::new(WORK), &mut shell));
    assert_eq!(matched.outside, 1);
    let held: Vec<(&str, Held)> = matched
        .found
        .iter()
        .map(|t| (t.id.as_str(), t.held))
        .collect();
    assert_eq!(held, [("A::testOne", Held::Missed)]);
}

/// `phpunit`'s log as `phpunit` 11 writes it: nested suites, a data set's name
/// with its entities decoded, a failure's text, a skip, and a file cut
/// short by a crash, whose case left open is not reported.
#[test]
fn a_phpunit_log_reads_as_its_cases() {
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<testsuites>
  <testsuite name="Unit" tests="4">
    <testsuite name="Acme\GreetTest" file="/w/tests/GreetTest.php">
      <testcase name="testHello" file="/w/tests/GreetTest.php" line="9" class="Acme\GreetTest" classname="Acme.GreetTest" assertions="1" time="0.01"/>
      <testcase name="testQuote with data set &quot;a &amp; b&quot;" class="Acme\GreetTest" classname="Acme.GreetTest">
        <failure type="PHPUnit\Framework\ExpectationFailedException">Acme\GreetTest::testQuote
Failed asserting that &apos;x&apos; is &lt;y&gt;.</failure>
        <system-out><![CDATA[<error> not an element]]></system-out>
      </testcase>
      <testcase name="testLater" class="Acme\GreetTest"><skipped/></testcase>
      <!-- <testcase name="commented"/> -->
      <testcase name="testCut" class="Acme\GreetTest">
        <system-out>half"#;
    let read = cases(xml);
    let got: Vec<(&str, Outcome)> = read.iter().map(|c| (c.id.as_str(), c.outcome)).collect();
    assert_eq!(
        got,
        [
            ("Acme\\GreetTest::testHello", Outcome::Passed),
            (
                "Acme\\GreetTest::testQuote with data set \"a & b\"",
                Outcome::Failed
            ),
            ("Acme\\GreetTest::testLater", Outcome::Skipped),
        ]
    );
    assert!(
        read[1]
            .message
            .contains("Failed asserting that 'x' is <y>."),
        "{:?}",
        read[1]
    );
}

/// A case with no class is named by its file, and one with neither by its
/// name alone.
#[test]
fn a_case_with_no_class_is_named_by_its_file() {
    let xml = r#"<testcase name="it greets" file="tests/Greet.php"/><testcase name="bare"/>"#;
    let ids: Vec<String> = cases(xml).into_iter().map(|c| c.id).collect();
    assert_eq!(ids, ["tests/Greet.php::it greets", "bare"]);
}

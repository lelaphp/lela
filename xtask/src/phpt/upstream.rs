//! `xtask phpt --upstream`: php-src's own `run-tests.php` beside this
//! runner, over named tests of one subset, so a miss of the runner's
//! control can be told apart from a miss of php's own runner (#1973, as
//! #1929 ruled it). Each runner runs the named tests in a copy of the
//! pinned tree of its own, under the pinned oracle and the recipe's
//! environment, the subset's own database on the server laid over it as a
//! lane's run lays it (the `server` module) and the tests' conflict keys
//! held among every run on the machine (the `across` module);
//! `run-tests.php` is handed the recipe's `-n` for every test, its
//! `$ini_overwrites` being the recipe's ini already. What it writes is
//! printed as it wrote it, then each test's two verdicts on one line; a
//! test the two judge apart is the exit, and nothing is recorded.
//!
//! **Named files only.** A directory, a glob or no file at all is refused,
//! so the corpus is never run this way: the whole of it under php's runner
//! is the nightly lane's (harness §8), and `lanes.nightly` keeps denying
//! `run-tests.php` called any other way.

use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use super::{Judgement, Own, Subset, Under, across, copy_of, file, is_phpt, run_tests};
use crate::divergence::Registry;
use crate::manifest::Manifest;
use crate::oracle::{Oracles, Pin, Runner};
use crate::tally;

/// What `run-tests.php` is asked for beside the tests: the recipe's `-n`
/// passed to each test, no questions, no colour or progress on the lines a
/// verdict is read from, and the difference of each test it fails.
const FLAGS: [&str; 5] = ["-n", "-q", "--no-color", "--no-progress", "--show-diff"];

/// Run the tests `files` names of the subset `name` through both runners
/// and say how each judged them; refused where a test is judged apart.
pub(crate) fn compare(
    manifest: &Manifest,
    registry: &Registry,
    name: &str,
    files: &[PathBuf],
    jobs: Option<std::num::NonZeroUsize>,
) -> Result<()> {
    let source = manifest.pin.source.tree()?;
    let subset = Subset::resolve(manifest, &source, name)?;
    let tests = named(&source, &subset, files)?;
    let variants = manifest.variants();
    let oracles = Oracles::new(&manifest.pin, &variants);
    let oracle = oracles.of(manifest.variant_of(&subset.scope))?;
    oracle.asserted()?;
    let own = Own::of(oracle, &subset, &subset.scope, true)?;
    let beside = own.pin.as_ref().map(|pin| oracle.beside(pin));
    let oracle = beside.as_ref().unwrap_or(oracle);
    let jobs = jobs.unwrap_or_else(tally::cores);
    let locks = across::machine();
    let ours = {
        let copy = copy_of(&source, jobs)?;
        let runner = Runner::Control(oracle.pin().oracle.file()?.to_path_buf());
        let under = Under {
            runner: &runner,
            lane: None,
            oracle_absent: None,
            jobs,
            served: own.served.as_deref(),
            locks: &locks,
        };
        let within = tests.iter().map(|test| copy.join(test)).collect();
        run_tests(oracle, registry, &subset.rooted(&copy), within, under)?.0
    };
    // Upstream's runner holds what the tests hold among every run on the
    // machine, as this one holds it test by test (the `across` module).
    let mut dirs = std::collections::BTreeMap::new();
    let keys: Vec<String> = (tests.iter())
        .flat_map(|test| file::conflicts(&source.join(test), &mut dirs))
        .collect();
    let said = {
        let _held = across::hold(&locks, &keys)?;
        upstream(oracle.pin(), &copy_of(&source, jobs)?, &tests)?
    };
    let mut apart = 0;
    for test in &tests {
        let id = subset.id(&source.join(test));
        let ours = ours.judged.iter().find(|judged| judged.id == id);
        let ours = ours.map(|judged| judged.judgement);
        let theirs = verdict(&said, test);
        let alike = ours.and_then(word).is_some_and(|ours| Some(ours) == theirs);
        apart += usize::from(!alike);
        println!(
            "phpt: --upstream: {}: xtask phpt {}, run-tests.php {}; {}",
            test.display(),
            ours.map_or("gave no verdict", tally::Judgement::as_str),
            theirs.unwrap_or("gave no verdict"),
            if alike { "alike" } else { "apart" }
        );
    }
    if apart > 0 {
        bail!(
            "{apart} of {} named test{} judged apart by xtask phpt and run-tests.php under the pinned oracle",
            tests.len(),
            crate::reading::plural(tests.len())
        );
    }
    Ok(())
}

/// The tests `files` names, each relative to the pinned tree at `source`
/// and a test of `subset`: a path inside the tree or one of it; a glob, a
/// directory, a file that is no `.phpt` of the tree, one outside the
/// subset, or no file at all is refused.
pub(super) fn named(source: &Path, subset: &Subset, files: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if files.is_empty() {
        bail!("--upstream names no test: name each .phpt file it runs");
    }
    files
        .iter()
        .map(|file| {
            let shown = file.display();
            let under = file.strip_prefix(source).unwrap_or(file);
            if file.to_string_lossy().contains(['*', '?', '[']) {
                bail!("--upstream {shown}: a glob; name each .phpt file it runs");
            }
            if under.components().any(|c| !matches!(c, Component::Normal(_))) {
                bail!("--upstream {shown}: not a path inside the pinned tree at {}", source.display());
            }
            let path = source.join(under);
            if path.is_dir() {
                bail!("--upstream {shown}: a directory; name each .phpt file it runs, as the corpus under php's runner is the nightly lane's");
            }
            if !(is_phpt(&path) && path.is_file()) {
                bail!("--upstream {shown}: no .phpt file of the pinned tree at {}", source.display());
            }
            if subset.found_in(&path).is_none() {
                bail!("--upstream {shown}: not a test of the subset {}", subset.scope);
            }
            Ok(under.to_path_buf())
        })
        .collect()
}

/// `run-tests.php` of the copy `tree` over `tests`, under the oracle of
/// `pin` and the recipe's environment, what it wrote printed and returned.
fn upstream(pin: &Pin, tree: &Path, tests: &[PathBuf]) -> Result<String> {
    let php = pin.oracle.file()?;
    let mut runner = Command::new(php);
    pin.recipe
        .environ(&mut runner)?
        .current_dir(tree)
        .args(&pin.recipe.args)
        .arg("run-tests.php")
        .args(FLAGS)
        .arg("-p")
        .arg(php)
        .args(tests)
        .stdin(Stdio::null());
    let out = runner
        .output()
        .with_context(|| format!("running run-tests.php in {}", tree.display()))?;
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    println!(
        "phpt: --upstream: run-tests.php in {}:\n{said}",
        tree.display()
    );
    Ok(said)
}

/// The verdict `run-tests.php` gives `test`, as `show_result` writes it
/// with no colour: the verdict's word, the test's title and its path in
/// brackets (run-tests.php:3258-3260). A `BORK` of a `SKIPIF` writes what
/// the section wrote as the title (run-tests.php:2179), so the word may
/// stand lines above the path; the lines between hold no other test's.
pub(super) fn verdict<'a>(said: &'a str, test: &Path) -> Option<&'a str> {
    let shown = test.display();
    let placed =
        |line: &str| line.contains(&format!("[{shown}]")) || line.contains(&format!("/{shown}]"));
    let lines: Vec<&str> = said.lines().collect();
    let end = lines.iter().position(|line| placed(line))?;
    let ends =
        |line: &&str| line.contains(".phpt]") || line.starts_with("===") || line.starts_with("---");
    let record = lines[..end].iter().rev().take_while(|line| !ends(line));
    std::iter::once(&lines[end]).chain(record).find_map(|line| {
        let (word, _) = line.split_once(' ')?;
        let result = !word.is_empty() && word.bytes().all(|b| b.is_ascii_uppercase() || b == b'&');
        result.then_some(word)
    })
}

/// What `run-tests.php` calls a verdict of this runner's: a difference a
/// registry row accepts is still its `FAIL`, and a test the compiler did
/// not read has none, since only the engine's compiler refuses one, nor
/// one the served lane left out, since only that lane sends HTTP.
fn word(judgement: Judgement) -> Option<&'static str> {
    match judgement {
        Judgement::Pass => Some("PASS"),
        Judgement::Fail | Judgement::Diverged => Some("FAIL"),
        Judgement::Skip => Some("SKIP"),
        Judgement::Bork => Some("BORK"),
        Judgement::XFail => Some("XFAIL"),
        Judgement::Unread | Judgement::Unsent => None,
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::phpt::Dir;
    use crate::scratch::Tree;

    #[test]
    fn upstream_takes_named_phpt_files_of_the_subset_and_refuses_a_directory_a_glob_or_none() {
        let tree = Tree::new("phpt-upstream").unwrap();
        for dir in ["ext/a/tests", "ext/b/tests"] {
            fs::create_dir_all(tree.join(dir)).unwrap();
        }
        for file in [
            "ext/a/tests/x.phpt",
            "ext/a/tests/x.inc",
            "ext/b/tests/y.phpt",
        ] {
            fs::write(tree.join(file), "").unwrap();
        }
        let subset = Subset {
            scope: "ext/a".to_owned(),
            dirs: vec![Dir {
                under: PathBuf::from("ext/a"),
                dir: tree.join("ext/a"),
            }],
            root: tree.to_path_buf(),
            holds: crate::phpt::Holds::Every,
        };
        let named = |files: &[&str]| {
            let files: Vec<PathBuf> = files.iter().map(PathBuf::from).collect();
            named(&tree, &subset, &files).map_err(|e| e.to_string())
        };
        let x = vec![PathBuf::from("ext/a/tests/x.phpt")];
        assert_eq!(named(&["ext/a/tests/x.phpt"]), Ok(x.clone()));
        let absolute = tree.join("ext/a/tests/x.phpt");
        assert_eq!(named(&[absolute.to_str().unwrap()]), Ok(x));
        for (files, refused) in [
            (&[][..], "names no test"),
            (&["ext/a/tests"][..], "a directory"),
            (&["ext/a/tests/*.phpt"][..], "a glob"),
            (
                &["ext/a/tests/x.phpt", "ext/a/tests/[x].phpt"][..],
                "a glob",
            ),
            (&["ext/a/tests/x.inc"][..], "no .phpt file"),
            (&["ext/a/tests/z.phpt"][..], "no .phpt file"),
            (&["ext/a/../b/tests/y.phpt"][..], "not a path inside"),
            (
                &["ext/b/tests/y.phpt"][..],
                "not a test of the subset ext/a",
            ),
        ] {
            let said = named(files).unwrap_err();
            assert!(said.contains(refused), "{files:?}: {said}");
        }
    }

    #[test]
    fn a_verdict_is_read_off_the_line_that_brackets_the_test() {
        let test = Path::new("ext/a/tests/x.phpt");
        let line = "FAIL MySQL [x] - DSN [ext/a/tests/x.phpt] ";
        assert_eq!(verdict(line, test), Some("FAIL"));
        let skipped = "SKIP t [/tmp/tree/ext/a/tests/x.phpt] reason: no server";
        assert_eq!(verdict(skipped, test), Some("SKIP"));
        assert_eq!(
            verdict("LEAK&FAIL t [ext/a/tests/x.phpt] ", test),
            Some("LEAK&FAIL")
        );
        assert_eq!(verdict("FAIL t [ext/a/tests/xx.phpt] ", test), None);
        assert_eq!(verdict("Tests failed: 1 [ext/a/tests/x.phpt]", test), None);
        // A SKIPIF that warned before its skip, borked under its words.
        let borked = "Running selected tests.\nBORK Warning: socket_sendto(): Unable to write to socket [65]: No route to host in /t/x.php on line 24\nskip unable to send multicast packet. [ext/a/tests/x.phpt] reason: invalid output from SKIPIF\n";
        assert_eq!(verdict(borked, test), Some("BORK"));
        let another = "PASS y [ext/a/tests/y.phpt] \nwhat came after\n[ext/a/tests/x.phpt]";
        assert_eq!(verdict(another, test), None);
        assert_eq!(word(Judgement::Diverged), Some("FAIL"));
    }
}

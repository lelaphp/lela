//! `--REDIRECTTEST--`: a test of one subset whose section names another
//! directory's tests and the environment they run in, as each PDO driver's
//! `common.phpt` runs `ext/pdo/tests` under its own DSN (#814). Read as
//! run-tests.php reads it (run-tests.php:2202-2238): the section is PHP
//! that returns `ENV` and `TESTS`, evaluated where `__DIR__` is the root of
//! the tree the subset stands in, as it is run-tests.php's own; each test of
//! the set runs in that environment over the runner's, with
//! `REDIR_TEST_DIR` naming the set's directory, and its programs are
//! written in the redirecting test's directory, not its own
//! (run-tests.php:1933-1937). A redirected test also needs the redirecting
//! test's `--EXTENSIONS--` (run-tests.php:2029-2031), and a set runs one test
//! after another, as upstream's worker runs it inside the redirecting test.
//!
//! **Where the set is counted.** In the redirecting test's subset, in the
//! redirecting test's place: run-tests.php over `ext/pdo_sqlite` counts
//! the set among that run's tests and not `common.phpt` itself, and over
//! `ext/pdo` runs the same files without a driver, where they skip. The set
//! measures the driver, so it is the driver's crate's reading, and a
//! verdict names both tests, the one run and the one that redirected it.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;

use super::adapter::Written;
use super::file::Phpt;
use super::{CORPUS, Dir, Judged, Subset};
use crate::oracle::{Invocation, Oracle, Program};
use crate::tally;

/// What names the redirecting test after the test it ran, in a verdict's
/// id and its name.
const VIA: &str = " via ";

/// Whether `keys`, the verdicts a target kept, hold every test of the set
/// the redirecting test keyed `by` ran, as the set stands in `subset`'s
/// tree now (#1830): the tests kept through it, each keyed `<test> via
/// <by>` ([`Redirect::id`]), name the set's directory as the deepest one
/// they share, and each `.phpt` there ([`set_files`]) is one of them. A set
/// whose tests all stand in one directory of a deeper one is read as that
/// directory; a redirect nothing was kept through holds nothing.
pub(crate) fn set_kept<'k>(
    subset: &Subset,
    by: &str,
    keys: impl Iterator<Item = &'k String>,
) -> bool {
    let corpus = format!("phpt:{CORPUS}/");
    let Some(by) = by.strip_prefix(&corpus) else {
        return false;
    };
    let via = format!("{VIA}{CORPUS}/{by}");
    let kept: BTreeSet<PathBuf> = keys
        .filter_map(|key| key.strip_suffix(&via)?.strip_prefix(&corpus))
        .map(PathBuf::from)
        .collect();
    let Some(mut dir) = kept
        .first()
        .and_then(|first| first.parent())
        .map(Path::to_path_buf)
    else {
        return false;
    };
    while !kept.iter().all(|test| test.starts_with(&dir)) {
        if !dir.pop() {
            return false;
        }
    }
    set_files(&subset.root.join(&dir)).is_ok_and(|files| {
        files.iter().all(|file| {
            file.strip_prefix(&subset.root)
                .is_ok_and(|test| kept.contains(test))
        })
    })
}

/// Every `.phpt` under `dir`, a redirect's set, as upstream lists it: not
/// a hidden file, sorted.
fn set_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut listed = Vec::new();
    tally::files_under(dir, &mut listed)
        .with_context(|| format!("listing the redirected set at {}", dir.display()))?;
    listed.retain(|file| {
        file.extension().is_some_and(|e| e == "phpt")
            && !file
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with('.'))
    });
    listed.sort();
    Ok(listed)
}

/// Where the section is evaluated: beside run-tests.php at the root of the
/// tree, where upstream evaluates it, so `__DIR__` is that root.
const PROGRAM: &str = "run-tests.redirect.php";

/// One redirect, read from its test's section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Redirect {
    /// The redirecting test, where the subset holds it.
    pub by: PathBuf,
    /// The set's directory: in the pinned tree, and on this machine.
    pub set: Dir,
    /// What every test of the set finds in its environment over the
    /// runner's: the section's `ENV`, and `REDIR_TEST_DIR`, the set's
    /// directory with a separator after it (run-tests.php:2228-2229).
    pub env: Vec<(String, OsString)>,
    /// The redirecting test's `--EXTENSIONS--`, which each test of the set
    /// needs beside its own.
    pub extensions: Vec<String>,
}

/// One test a subset's run judges: a `.phpt` file, and the redirect it
/// runs under where a `--REDIRECTTEST--` of the subset named it.
#[derive(Debug, Clone)]
pub(crate) struct Test {
    pub file: PathBuf,
    pub via: Option<Arc<Redirect>>,
}

/// One item of a subset's run: a test to judge, or a `--REDIRECTTEST--`
/// that runs no set and the verdict it was given where the set was listed,
/// a skip or a bork in upstream's words.
#[derive(Debug, Clone)]
pub(crate) enum Item {
    Test(Test),
    Reached(Judged),
}

impl Test {
    /// A file of the subset, run as itself.
    pub(crate) fn own(file: PathBuf) -> Test {
        Test { file, via: None }
    }

    /// Where a file of the test's run is written: its program at `x.php`,
    /// a section run beside it at `x.skip.php` or `x.clean.php`, in the
    /// test's own directory, or the redirecting test's for a test of a
    /// set.
    pub(crate) fn beside(&self, kind: Option<&str>) -> PathBuf {
        let home = match &self.via {
            Some(via) => via.by.parent().unwrap_or(&via.by),
            None => self.file.parent().unwrap_or(&self.file),
        };
        let named = home.join(self.file.file_name().unwrap_or_default());
        named.with_extension(kind.map_or_else(|| "php".to_owned(), |kind| format!("{kind}.php")))
    }

    /// The divergence key: the test's own, and for a test of a set the
    /// set's test and the redirecting one, each where it stands in the
    /// corpus (`phpt:php-src/ext/pdo/tests/pdo_001.phpt via
    /// php-src/ext/pdo_sqlite/tests/common.phpt`).
    pub(crate) fn id(&self, subset: &Subset) -> String {
        match &self.via {
            None => subset.id(&self.file),
            Some(via) => format!(
                "phpt:{CORPUS}/{}{VIA}{CORPUS}/{}",
                via.set.under.join(via.set.inside(&self.file)).display(),
                subset.under(&via.by).display()
            ),
        }
    }

    /// The name inside the subset: the test's own, and for a test of a set
    /// the set's test in the corpus and the redirecting test by its name.
    pub(crate) fn name(&self, subset: &Subset) -> String {
        match &self.via {
            None => subset.name(&self.file),
            Some(via) => format!(
                "{}{VIA}{}",
                via.set.under.join(via.set.inside(&self.file)).display(),
                subset.name(&via.by)
            ),
        }
    }
}

/// What the section returned, as the program below renders it.
#[derive(Deserialize)]
struct Returned {
    env: BTreeMap<String, String>,
    tests: Option<String>,
}

impl Redirect {
    /// The redirect the section of `phpt`, the test at `by`, names: the
    /// section evaluated under the oracle from the root of the tree the
    /// subset stands in, and the directory it names listed. Why it names
    /// none is the test's bork, in upstream's words where upstream has
    /// them: no `TESTS`, or one that is no directory of that tree.
    pub(crate) fn read(
        oracle: &Oracle<'_>,
        subset: &Subset,
        by: &Path,
        phpt: &Phpt,
    ) -> Result<Result<(Redirect, Vec<PathBuf>), String>> {
        let section = phpt.text("REDIRECTTEST").unwrap_or_default();
        let program = subset.root.join(PROGRAM);
        let body = format!(
            "<?php\n$redirect = (function () {{\n{section}\n}})();\n\
             echo json_encode(['env' => (object) array_map('strval', (array) ($redirect['ENV'] ?? [])), \
             'tests' => is_array($redirect) ? ($redirect['TESTS'] ?? null) : null]);\n"
        );
        let written = Written::new(&program, body.as_bytes())?;
        let pinned = &oracle.pin().oracle.path;
        let out = oracle
            .run_under(&Invocation {
                cwd: Some(&subset.root),
                ..Invocation::new(pinned, Program::Beside(&program))
            })
            .with_context(|| format!("evaluating the --REDIRECTTEST-- of {}", by.display()))?;
        drop(written);
        let said = String::from_utf8_lossy(&out.stdout);
        let returned: Returned = match serde_json::from_str(said.trim()) {
            Ok(returned) => returned,
            Err(_) => {
                return Ok(Err(format!(
                    "the --REDIRECTTEST-- section returned no redirect: {}",
                    said.trim()
                )));
            }
        };
        let Some(tests) = returned.tests.filter(|tests| !tests.is_empty()) else {
            return Ok(Err(
                "Redirect info must contain exactly one TEST string to be used as redirect directory."
                    .to_owned(),
            ));
        };
        let root = fs::canonicalize(&subset.root)
            .with_context(|| format!("resolving {}", subset.root.display()))?;
        let dir = match fs::canonicalize(&tests) {
            Ok(dir) if dir.is_dir() => dir,
            _ => {
                return Ok(Err(format!(
                    "the --REDIRECTTEST-- names {tests}, which is no directory"
                )));
            }
        };
        let Ok(under) = dir.strip_prefix(&root).map(Path::to_path_buf) else {
            return Ok(Err(format!(
                "the --REDIRECTTEST-- names {tests}, which is outside the tree the subset stands in, {}",
                root.display()
            )));
        };
        let mut env: Vec<(String, OsString)> = returned
            .env
            .into_iter()
            .map(|(name, value)| (name, value.into()))
            .collect();
        let mut named = dir.clone().into_os_string();
        named.push("/");
        env.push(("REDIR_TEST_DIR".to_owned(), named));
        let extensions = phpt.extensions();
        let listed = set_files(&dir)?;
        let redirect = Redirect {
            by: by.to_path_buf(),
            set: Dir { under, path: dir },
            env,
            extensions,
        };
        Ok(Ok((redirect, listed)))
    }
}

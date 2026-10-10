//! The production server's two lanes beside its witnesses (#2337), each an
//! instrument over `sapi/makala`'s `makala` as `profile::build` builds it,
//! in the lanes' profile, whose debug assertions arm the request leak
//! sensor:
//!
//! - `fpm-compare` ([`compare`], #2349): each request of the corpus served
//!   by makala over HTTP and by the pinned php-fpm under the pin's
//!   `[fastcgi]` mapping, the two responses held to each other. It is the
//!   oracle for what PHP code sees under makala, and runs on request where
//!   the pinned php-fpm is.
//! - `soak` ([`soak`], #2350): many clients sending the corpus to one
//!   makala for a fixed time, each response held to the same request
//!   served alone, with the leak sensor's findings and the growth of the
//!   process's resident memory counted. Its length, its clients and its
//!   lane are #2336's ruling: ten minutes, 64 clients, nightly on
//!   `dax-build`, and a 30-second smoke in makala's own tests.
//!
//! Both are served one corpus, the HTTP-served subset: the tests of the
//! phpt lane's served subset (`phpt::http`, #2348) that one file serves
//! whole, each its `--FILE--`
//! and the request its sections describe as run-tests.php's CGI
//! translation of them ([`cgi::request`]) reads back as HTTP, under its
//! `--INI--`. A test that runs only after its `--SKIPIF--`, reads a file
//! beside it in place of `--FILE--`, cleans up after itself, redirects to
//! another test, sends a compressed body or takes arguments is left out,
//! since none of that crosses one request to one file. The soak adds the
//! bench workloads committed under `docs/perf/workloads/` (measurement
//! protocol §11), each a `GET` of its own.

pub(crate) mod bench;
pub(crate) mod compare;
pub(crate) mod soak;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::cli::Command;
use crate::manifest::Manifest;
use crate::oracle::hosts::MAKALA;
use crate::oracle::profile;
use crate::phpt::environment::Environment;
use crate::phpt::file::Phpt;
use crate::phpt::{adapter, cgi, http};
use crate::reading::Reading;
use crate::witness::makala::Http;

/// The crate both instruments' readings are scoped to.
pub(crate) const SCOPE: &str = "sapi/makala";

/// The sections whose test the corpus leaves out, the reasons above.
const LEFT_OUT: [&str; 7] = [
    "SKIPIF",
    "FILE_EXTERNAL",
    "REDIRECTTEST",
    "CLEAN",
    "GZIP_POST",
    "DEFLATE_POST",
    "ARGS",
];

/// Where the bench workloads stand, relative to the workspace root.
const WORKLOADS: &str = "docs/perf/workloads";

/// One request of the corpus: what names it, the whole file it runs, the
/// request as a client sends it and the ini entries it is served under.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) id: String,
    pub(crate) program: Vec<u8>,
    pub(crate) http: Http,
    pub(crate) ini: Vec<String>,
}

/// The reading of the instrument `ask` names: `fpm-compare` or `soak`.
pub(crate) fn run(root: &Path, manifest: &Manifest, ask: Command) -> Result<Reading> {
    match ask {
        Command::FpmCompare { tests } => compare::run(root, manifest, &tests),
        Command::Soak {
            nightly,
            seconds,
            clients,
        } => soak::run(root, manifest, soak::seconds(nightly, seconds), clients),
        _ => anyhow::bail!("{ask:?} is neither fpm-compare nor soak"),
    }
}

/// The tests of the pinned tree at `tree` one file serves whole, in the
/// order of their paths, and how many CGI-shaped tests it left out: the
/// served subset's tests (`phpt::http`), which says which tests are
/// CGI-shaped for every lane of the production server.
pub(crate) fn subset(tree: &Path) -> Result<(Vec<Entry>, usize)> {
    let mut files = http::subset(tree).tests()?;
    files.sort();
    let (mut entries, mut left) = (Vec::new(), 0);
    for path in files {
        let Ok(phpt) = Phpt::load(&path) else {
            continue;
        };
        let file = path.strip_prefix(tree).unwrap_or(&path);
        match entry(&phpt, &path, file) {
            Some(entry) => entries.push(entry),
            None => left += 1,
        }
    }
    Ok((entries, left))
}

/// The entry a CGI-shaped test is, or none where it is left out.
fn entry(phpt: &Phpt, path: &Path, file: &Path) -> Option<Entry> {
    let program = phpt.section("FILE")?;
    if LEFT_OUT.iter().any(|section| phpt.has(section)) {
        return None;
    }
    let mut env = Environment::from(Vec::new());
    env.reset();
    env.extend(crate::phpt::env_of(phpt, path));
    let request = cgi::request(phpt, path, &env).ok()?;
    Some(Entry {
        id: file.display().to_string(),
        program: program.to_vec(),
        http: Http::described(&request),
        ini: adapter::ini_of(phpt, path).ok()?,
    })
}

/// The bench workloads of the workspace at `root`, each a `GET` with no
/// ini of its own, in the order of their names.
pub(crate) fn workloads(root: &Path) -> Result<Vec<Entry>> {
    let dir = root.join(WORKLOADS);
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("reading {}", dir.display()))?
        .filter_map(|found| found.ok().map(|found| found.path()))
        .filter(|path| path.extension().is_some_and(|e| e == "php"))
        .collect();
    paths.sort();
    paths
        .into_iter()
        .map(|path| {
            Ok(Entry {
                id: format!(
                    "{WORKLOADS}/{}",
                    path.file_name().unwrap_or_default().to_string_lossy()
                ),
                program: std::fs::read(&path)
                    .with_context(|| format!("reading {}", path.display()))?,
                http: Http::get(),
                ini: Vec::new(),
            })
        })
        .collect()
}

/// `response` with each temporary copy of an uploaded file named alike:
/// php names one `php` and random characters in `upload_tmp_dir`
/// (`php_open_temporary_fd`), a name no two requests share, so a response
/// that prints one is held to another with the characters as one token.
pub(crate) fn uploads_named_alike(response: &[u8]) -> Vec<u8> {
    const NAME: &[u8] = b"/php";
    let (mut out, mut at) = (Vec::with_capacity(response.len()), 0);
    while at < response.len() {
        let random = (response[at..].starts_with(NAME))
            .then(|| response[at + NAME.len()..].iter())
            .map_or(0, |rest| {
                rest.take_while(|b| b.is_ascii_alphanumeric()).count()
            });
        if random >= 6 {
            out.extend_from_slice(b"/php%UPLOAD%");
            at += NAME.len() + random;
        } else {
            out.push(response[at]);
            at += 1;
        }
    }
    out
}

/// makala's binary, built now in the lanes' profile.
pub(crate) fn makala(root: &Path) -> Result<PathBuf> {
    profile::build(root, &MAKALA.package(), MAKALA.binary, None, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scratch::Tree;

    /// A CGI-shaped test one file serves whole is an entry with its
    /// `--INI--`, a multi-line cookie folded onto one field; one with a
    /// `--SKIPIF--` is left out, and one that is not CGI-shaped is no
    /// part of the subset.
    #[test]
    fn the_subset_is_the_cgi_shaped_tests_one_file_serves_whole() {
        let tree = Tree::new("makala-subset").unwrap();
        let test = |sections: &str| {
            format!("--TEST--\nt\n{sections}--FILE--\n<?php echo 1;\n--EXPECT--\n1\n")
        };
        std::fs::create_dir_all(tree.path().join("tests")).unwrap();
        tree.write(
            "tests/a.phpt",
            test("--INI--\nprecision=4\n--COOKIE--\na=1;\nb=2\n"),
        )
        .unwrap();
        tree.write("tests/b.phpt", test("--SKIPIF--\n<?php\n--GET--\nx=1\n"))
            .unwrap();
        tree.write("tests/c.phpt", test("")).unwrap();
        let (entries, left) = subset(tree.path()).unwrap();
        assert_eq!(left, 1);
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["tests/a.phpt"]);
        assert_eq!(entries[0].ini, ["precision=4"]);
        assert_eq!(entries[0].program, b"<?php echo 1;\n");
        assert_eq!(entries[0].http.field("Cookie"), b"a=1; b=2");
    }

    /// An uploaded file's temporary name reads as one token, and a path
    /// that only begins alike keeps its name.
    #[test]
    fn an_uploaded_file_s_temporary_name_reads_as_one_token() {
        let named = uploads_named_alike(b"\"/tmp/phpAb12Cd9\" /php/x /phpinfo");
        assert_eq!(named, b"\"/tmp/php%UPLOAD%\" /php/x /phpinfo");
    }
}

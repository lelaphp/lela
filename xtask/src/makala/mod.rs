//! The production server's lanes beside its witnesses (#2337), each an
//! instrument over `sapi/makala`'s `makala` as `profile::build` builds it,
//! in the lanes' profile, whose debug assertions arm the request leak
//! sensor:
//!
//! - `fpm-compare` ([`compare`], #2349): each request of the corpus served
//!   by makala over HTTP and by the pinned php-fpm under the pin's
//!   `[fastcgi]` mapping, the two responses held to each other. It is the
//!   oracle for what PHP code sees under makala, and runs on request where
//!   the pinned php-fpm is.
//!
//! It is served the HTTP-served subset: the CGI-shaped tests
//! of the pinned php-src that one file serves whole, each its `--FILE--`
//! and the request its sections describe as run-tests.php's CGI
//! translation of them ([`cgi::request`]) reads back as HTTP, under its
//! `--INI--`. A test that runs only after its `--SKIPIF--`, reads a file
//! beside it in place of `--FILE--`, cleans up after itself, redirects to
//! another test, sends a compressed body or takes arguments is left out,
//! since none of that crosses one request to one file.

pub(crate) mod compare;

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::cli::Command;
use crate::manifest::Manifest;
use crate::oracle::hosts::MAKALA;
use crate::oracle::{Source, profile};
use crate::phpt::environment::Environment;
use crate::phpt::file::Phpt;
use crate::phpt::{adapter, cgi};
use crate::reading::Reading;
use crate::witness::makala::Http;

/// The crate the lanes' readings are scoped to.
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

/// One request of the corpus: what names it, the whole file it runs, the
/// request as a client sends it and the ini entries it is served under.
#[derive(Debug, Clone)]
pub(crate) struct Entry {
    pub(crate) id: String,
    pub(crate) program: Vec<u8>,
    pub(crate) http: Http,
    pub(crate) ini: Vec<String>,
}

/// The reading of the instrument `ask` names: `fpm-compare`.
pub(crate) fn run(root: &Path, manifest: &Manifest, ask: Command) -> Result<Reading> {
    match ask {
        Command::FpmCompare { tests } => compare::run(root, manifest, &tests),
        _ => anyhow::bail!("{ask:?} is not fpm-compare"),
    }
}

/// The HTTP-served subset of the pinned tree at `tree`, in the order of
/// its paths, and how many CGI-shaped tests it left out.
pub(crate) fn subset(tree: &Path) -> Result<(Vec<Entry>, usize)> {
    let mut files = Source::files(tree)?;
    files.retain(|file| file.extension().is_some_and(|e| e == "phpt"));
    files.sort();
    let (mut entries, mut left) = (Vec::new(), 0);
    for file in files {
        let path = tree.join(&file);
        let Ok(phpt) = Phpt::load(&path) else {
            continue;
        };
        if !phpt.is_cgi() {
            continue;
        }
        match entry(&phpt, &path, &file) {
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
        tree.write(
            "a.phpt",
            test("--INI--\nprecision=4\n--COOKIE--\na=1;\nb=2\n"),
        )
        .unwrap();
        tree.write("b.phpt", test("--SKIPIF--\n<?php\n--GET--\nx=1\n"))
            .unwrap();
        tree.write("c.phpt", test("")).unwrap();
        let (entries, left) = subset(tree.path()).unwrap();
        assert_eq!(left, 1);
        let ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, ["a.phpt"]);
        assert_eq!(entries[0].ini, ["precision=4"]);
        assert_eq!(entries[0].program, b"<?php echo 1;\n");
        let shown = format!("{:?}", entries[0].http);
        assert!(shown.contains("\"a=1; b=2\""), "{shown}");
    }

    /// An uploaded file's temporary name reads as one token, and a path
    /// that only begins alike keeps its name.
    #[test]
    fn an_uploaded_file_s_temporary_name_reads_as_one_token() {
        let named = uploads_named_alike(b"\"/tmp/phpAb12Cd9\" /php/x /phpinfo");
        assert_eq!(named, b"\"/tmp/php%UPLOAD%\" /php/x /phpinfo");
    }
}

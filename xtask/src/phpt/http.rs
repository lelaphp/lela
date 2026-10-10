//! The CGI-shaped tests served to the production server over HTTP
//! (#2348): `sapi/makala`'s own subset, the one adapter beside the CGI
//! translation.
//!
//! **Which tests.** Every CGI-shaped test ([`Phpt::is_cgi`], upstream's
//! `isCGI`) of the corpus's subsets ([`super::ROOTS`]), one subset scoped
//! by the host's crate. Its row keeps no `phpt` key: each test is already
//! counted under the subset whose directory it stands in, and #2334 ruled
//! this lane reported per host and never added to the parity total twice,
//! so the tests are chosen here and never by naming their directories on
//! the row. The corpus `fpm-compare` and `soak` serve is the tests of this
//! subset one file serves whole (`crate::makala::subset`).
//!
//! **What serves them.** The runner's control runs them as it runs every
//! CGI-shaped test, through the php-cgi `[sapi]` pins, so the target
//! makala is held to is php-cgi's verdicts on the same tests. The engine's
//! run hands each test's request, the CGI translation's ([`cgi::request`])
//! read back as HTTP ([`Http::described`]), to a makala of its own serving
//! the test's directory, the one built beside the binary under test, and
//! reads the response back as php-cgi writes one: a `Status:` line where
//! the code is not 200, the header lines, a blank line and the body. The
//! rest of the judge is the CGI one, but for a header's name, which is held
//! without regard to its case, since makala writes every name lowercase by
//! design (#2570). What CGI has and HTTP cannot carry, the words of
//! `--ARGS--` and an environment variable no field stands for, is not sent.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, anyhow};

use super::cgi;
use super::environment::Environment;
use super::file::Phpt;
use super::{Holds, Subset, is_phpt};
use crate::oracle::hosts::MAKALA;
use crate::oracle::{PastLimit, Recipe};
use crate::scratch::Tree;
use crate::tally;
use crate::witness::makala::{Http, Server, split_head};

/// The subset's scope: the host's crate.
pub(crate) const SCOPE: &str = MAKALA.krate;

/// The served subset of the pinned tree at `source`: the directories of
/// the corpus that hold a CGI-shaped test, each test of them that is one.
pub(crate) fn subset(source: &Path) -> Subset {
    let mut dirs = Vec::new();
    for corpus in Subset::corpus(source) {
        dirs.extend(
            (corpus.dirs.into_iter()).filter(|place| tally::any_file_under(&place.dir, is_served)),
        );
    }
    Subset {
        scope: SCOPE.to_owned(),
        dirs,
        root: source.to_path_buf(),
        holds: Holds::Served,
    }
}

/// Whether the file at `path` is a test this subset serves: a CGI-shaped
/// `.phpt`.
pub(super) fn is_served(path: &Path) -> bool {
    is_phpt(path) && Phpt::load(path).is_ok_and(|phpt| phpt.is_cgi())
}

/// makala beside `binary`, the binary under test, where it was built.
pub(super) fn host(binary: &Path) -> Result<PathBuf> {
    let host = binary.with_file_name(MAKALA.binary);
    if host.is_file() {
        return Ok(host);
    }
    Err(anyhow!(
        "{SCOPE}: no makala stands beside {}, so no test of the subset can be served: a run \
         of the engine's own build builds it there, and beside a binary named for the run \
         `cargo build -p {}` in the binary's profile does",
        binary.display(),
        MAKALA.package()
    ))
}

/// `request`, the one the test of `program` describes, served by a makala
/// of its own at `host` over the program's directory, under the recipe's
/// ini and `ini` after it and the test's environment `env`: the response
/// as php-cgi writes one ([`as_cgi`]), a [`PastLimit`] where no response
/// came within the recipe's limit.
pub(super) fn served(
    recipe: &Recipe,
    host: &Path,
    program: &Path,
    request: &cgi::Request,
    ini: &[String],
    env: &Environment,
) -> Result<Vec<u8>> {
    let dir = program.parent().context("a program with no directory")?;
    let script = (program.file_name().and_then(|name| name.to_str()))
        .context("a program with no name of text")?;
    let logs = Tree::new("phpt-http")?;
    let log = logs.path().join("errors");
    let server = Server::start(recipe, host, dir, ini, Some(1), &log, env.entries())?;
    let limit_s = recipe.limits.timeout_s;
    let timeout = Duration::from_secs(limit_s);
    match server.ask(&Http::described(request), script, timeout) {
        Ok((answer, _)) => Ok(as_cgi(&answer)),
        Err(e) if timed_out(&e) => Err(PastLimit {
            binary: host.to_path_buf(),
            script: program.to_path_buf(),
            limit_s,
        }
        .into()),
        Err(e) => Err(e),
    }
}

/// Whether a read or a write gave up at its timeout.
fn timed_out(e: &anyhow::Error) -> bool {
    e.chain()
        .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
        .any(|io| {
            matches!(
                io.kind(),
                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
            )
        })
}

/// makala's HTTP response as php-cgi writes one: the status line a
/// `Status:` line where its code is not 200, then the header lines but the
/// server's own `date` and `connection`, a blank line and the body; a
/// response with no head is its bytes.
pub(super) fn as_cgi(answer: &[u8]) -> Vec<u8> {
    let (head, body) = split_head(answer);
    let mut lines = head.lines();
    let Some(status) = lines.next().filter(|line| line.starts_with("HTTP/")) else {
        return answer.to_vec();
    };
    let status = status.split_once(' ').map_or("", |(_, status)| status);
    let ours = |line: &&str| line.starts_with("date:") || line.starts_with("connection:");
    let set = lines.filter(|line| !ours(line));
    let status = (!status.starts_with("200")).then(|| format!("Status: {status}"));
    let mut out = Vec::with_capacity(answer.len());
    for line in status.as_deref().into_iter().chain(set) {
        out.extend_from_slice(line.as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A response of another code carries it as php-cgi's `Status:` line,
    /// the server's own lines are left out, and a 200 carries none.
    #[test]
    fn a_response_reads_as_php_cgi_writes_one() {
        let shown = as_cgi(
            b"HTTP/1.0 404 Not Found\r\nx-powered-by: PHP\r\ndate: x\r\nconnection: close\r\n\r\nbody\r\n\r\nmore",
        );
        assert_eq!(
            String::from_utf8_lossy(&shown),
            "Status: 404 Not Found\r\nx-powered-by: PHP\r\n\r\nbody\r\n\r\nmore"
        );
        let ok = as_cgi(b"HTTP/1.0 200 OK\r\ncontent-type: text/html\r\n\r\n1");
        assert_eq!(
            String::from_utf8_lossy(&ok),
            "content-type: text/html\r\n\r\n1"
        );
        assert_eq!(as_cgi(b"garbled"), b"garbled");
    }

    /// The subset holds the CGI-shaped tests of the corpus and their
    /// directories alone, so a directory with none is no part of it.
    #[test]
    fn the_subset_is_every_cgi_shaped_test_of_the_corpus() {
        let tree = Tree::new("phpt-http-subset").unwrap();
        let test = |sections: &str| format!("--TEST--\nt\n{sections}--FILE--\n<?php\n--EXPECT--\n");
        for (file, text) in [
            ("ext/a/tests/get.phpt", test("--GET--\nx=1\n")),
            ("ext/a/tests/plain.phpt", test("")),
            ("ext/b/tests/plain.phpt", test("")),
            ("tests/basic/post.phpt", test("--POST--\na=1\n")),
            (
                "ext/c/tests/headers.phpt",
                test("--EXPECTHEADERS--\nStatus: 404\n"),
            ),
        ] {
            std::fs::create_dir_all(tree.path().join(file).parent().unwrap()).unwrap();
            tree.write(file, text).unwrap();
        }
        let subset = subset(tree.path());
        assert_eq!(subset.scope, SCOPE);
        let unders: Vec<String> = (subset.dirs.iter())
            .map(|dir| dir.under.display().to_string())
            .collect();
        assert_eq!(unders, ["tests", "ext/a", "ext/c"]);
        let names: Vec<String> = (subset.tests().unwrap().iter())
            .map(|test| subset.name(test))
            .collect();
        assert_eq!(
            names,
            [
                "tests/basic/post.phpt",
                "ext/a/tests/get.phpt",
                "ext/c/tests/headers.phpt"
            ]
        );
    }
}

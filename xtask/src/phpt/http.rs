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
//! design (#2570). The judge holds `--EXPECTHEADERS--` as run-tests.php
//! does, each wanted name by the last line of that name, so it reads no
//! order across names and makala's grouping of a name's lines (#2640)
//! cannot miss it; within a name it reads which line comes last, which
//! the grouping keeps. The served lane needs no fold of its own. A difference an accepted
//! `makala:` row names is accepted, as `fpm-compare` reads it
//! (`crate::makala::held`, #2644). What CGI has and HTTP cannot carry is
//! not sent: the words of `--ARGS--`, and an environment variable no field
//! stands for, whose test is left out of the lane, outside the target
//! ([`unsent`], #2639).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, anyhow};

use super::cgi;
use super::environment::Environment;
use super::file::Phpt;
use super::target::named;
use super::{Holds, Judgement, Subset, Tally, is_phpt};
use crate::divergence::Registry;
use crate::makala::{Consulted, Held, held};
use crate::oracle::hosts::MAKALA;
use crate::oracle::{PastLimit, Recipe, Runner, Verdict};
use crate::reading::Reading;
use crate::scratch::Tree;
use crate::tally;
use crate::witness::makala::{Http, Server, placed, split_head};

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

/// The variables of a CGI request HTTP carries: those the request line
/// stands for and the body's two fields; every `HTTP_` one is a field of
/// its own.
const CARRIED: [&str; 7] = [
    "REQUEST_METHOD",
    "REQUEST_URI",
    "QUERY_STRING",
    "SCRIPT_NAME",
    "PATH_INFO",
    "CONTENT_TYPE",
    "CONTENT_LENGTH",
];

/// Why the served lane leaves the test at `path` out, where it does: its
/// `--ENV--` sets a variable no HTTP field carries, which a request
/// cannot hand the server, so no verdict on it would be makala's (#2639).
/// Counted outside the target and named, never a row.
pub(super) fn unsent(phpt: &Phpt, path: &Path) -> Option<String> {
    let carried = |name: &str| name.starts_with("HTTP_") || CARRIED.contains(&name);
    let unsent: Vec<String> = (super::env_of(phpt, path).into_iter())
        .map(|(name, _)| name)
        .filter(|name| !carried(name))
        .collect();
    (!unsent.is_empty()).then(|| {
        format!(
            "its --ENV-- sets {}, which no HTTP field carries",
            unsent.join(", ")
        )
    })
}

/// A served test's verdict, `compared` as the registry read it under the
/// test's `phpt:` key `id`: a difference no row accepted there is accepted
/// where an accepted `makala:` row names the test, as `fpm-compare` reads
/// it (#2644), and stands otherwise.
pub(super) fn accepted(registry: &Registry, id: &str, compared: Verdict) -> Verdict {
    match (&compared, held(registry, named(id), false)) {
        (Verdict::Differs(_), Held::Accepted(row)) => Verdict::Diverged(row.clone()),
        _ => compared,
    }
}

/// The `makala:` rows that accepted a difference in the engine's run of
/// the served subset, counted on `reading` and each test named under its
/// row, as `fpm-compare` counts and names them (#2644); nothing for
/// another subset or the runner's control. The lane
/// names no row stale: a row records what php-fpm shows, and a test held
/// to its own expectation may not show it, as bug60761.phpt, which
/// prints the length of `phpinfo()`'s page, does not.
pub(super) fn consulted(
    reading: &mut Reading,
    (subset, runner): (&Subset, &Runner),
    tally: &Tally,
    registry: &Registry,
) -> Result<()> {
    if subset.holds != Holds::Served || matches!(runner, Runner::Control(_)) {
        return Ok(());
    }
    let mut consulted = Consulted::default();
    // A difference a `phpt:` row accepted is that row's, never makala's.
    let diverged = (tally.judged.iter()).filter(|judged| {
        judged.judgement == Judgement::Diverged && registry.accepted(&judged.id).is_none()
    });
    for judged in diverged {
        consulted.hold(registry, named(&judged.id), false);
    }
    if let Some(words) = consulted.counted(reading)? {
        super::told(reading, runner, &words);
    }
    Ok(())
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
    let (dir, script) = placed(program)?;
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
            stopped: Vec::new(),
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
    use crate::oracle::Normalise;
    use crate::phpt::Judged;
    use crate::reading::Number;

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

    /// The negative control of #2644, over the fixture `fpm-compare` is
    /// held to: a difference on a test an accepted `makala:` row names is
    /// accepted under the row and counted apart, one on a test no row
    /// names stands and fails, and a test the row covers that met its
    /// expectation is no row stale.
    #[test]
    fn the_served_lane_reads_makala_rows_as_fpm_compare_does() {
        let registry = crate::makala::fixture();
        let id = |test: &str| format!("phpt:php-src/ext/filter/tests/{test}");
        let differs = || Verdict::Differs("-a\n+b\n".to_owned());
        let held = accepted(&registry, &id("004.phpt"), differs());
        assert!(
            matches!(&held, Verdict::Diverged(row) if row.id.as_str() == "makala:ext/filter/tests/004.phpt"),
            "{held:?}"
        );
        let standing = accepted(&registry, &id("011.phpt"), differs());
        assert!(
            matches!(standing, Verdict::Differs(_)),
            "no row names 011.phpt: {standing:?}"
        );
        assert_eq!(
            accepted(&registry, &id("007.phpt"), Verdict::Same),
            Verdict::Same
        );
        let judged = |test: &str, judgement| Judged::new(&id(test), test, judgement, "");
        let tally = Tally {
            judged: vec![
                judged("004.phpt", Judgement::Diverged),
                judged("007.phpt", Judgement::Pass),
                judged("011.phpt", Judgement::Fail),
            ],
            found: 3,
        };
        let mut reading = Reading::new(super::super::INSTRUMENT, SCOPE);
        let served = subset(Path::new("php-src"));
        consulted(&mut reading, (&served, &Runner::Linked), &tally, &registry).unwrap();
        let words = &reading.message;
        assert!(
            words.contains("1 differs as a makala: row accepts: makala:ext/filter/tests/004.phpt"),
            "{words}"
        );
        assert!(!words.contains("stale"), "{words}");
        assert_eq!(
            reading.numbers.get("accepted").copied(),
            Some(Number::count(1))
        );
    }

    /// A served test whose `--ENV--` sets what no HTTP field carries is
    /// left out by the rule and not by its name: `FOO` is no field, while
    /// a field's `HTTP_` variable and those the request line stands for
    /// are carried.
    #[test]
    fn a_test_whose_env_no_http_field_carries_is_left_out() {
        let tree = Tree::new("phpt-http-unsent").unwrap();
        let test = |env: &str| {
            format!("--TEST--\nt\n--CGI--\n--ENV--\n{env}\n--FILE--\n<?php\n--EXPECT--\n")
        };
        for (file, env, unsent) in [
            (
                "a.phpt",
                "FOO=BAR",
                Some("its --ENV-- sets FOO, which no HTTP field carries"),
            ),
            ("b.phpt", "HTTP_ACCEPT_ENCODING=gzip", None),
            (
                "c.phpt",
                "SCRIPT_NAME=/c.php\nREQUEST_URI=/c.php/x\nPATH_INFO=/x",
                None,
            ),
        ] {
            tree.write(file, test(env)).unwrap();
            let path = tree.path().join(file);
            let phpt = Phpt::load(&path).unwrap();
            assert_eq!(super::unsent(&phpt, &path).as_deref(), unsent, "{file}");
        }
    }

    /// The served lane holds `--EXPECTHEADERS--` by name, the last line of
    /// a name standing, as run-tests.php does, so the order across names
    /// is never read (#2640): makala's grouped `Vary` lines meet
    /// bug51269.phpt's expectation, and the same two lines swapped, whose
    /// last line is then another, miss it.
    #[test]
    fn the_served_lane_folds_header_order_across_names_and_holds_it_within_one() {
        let wanted = "Vary: Cookie\nContent-Encoding: gzip\nVary: Accept-Encoding";
        let missed = |vary: [&str; 2]| {
            let head = format!(
                "HTTP/1.1 200 OK\r\nvary: {}\r\nvary: {}\r\ncontent-encoding: gzip\r\n\r\nx",
                vary[0], vary[1]
            );
            let (headers, _) = cgi::parted(&as_cgi(head.as_bytes()), &Normalise::default());
            cgi::headers_missed(wanted, (&headers, cgi::Names::Folded))
        };
        assert_eq!(missed(["Cookie", "Accept-Encoding"]), None);
        assert!(missed(["Accept-Encoding", "Cookie"]).is_some());
    }
}

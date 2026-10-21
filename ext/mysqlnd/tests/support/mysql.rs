//! The `MySQL` server of `ci/mysql/compose.yaml` as the tests of the
//! extensions that connect through mysqlnd meet it: `ext/mysqli`'s and
//! `ext/pdo_mysql`'s, each test file including this one with `#[path]`.
//! It lives with mysqlnd, below both, so a change here stales the
//! readings of both crates, whose keys cover mysqlnd's tree.
//!
//! A test hands its body to [`MysqlServer::with`]. Where nothing
//! answers on the server's address the body is never called: the test says
//! so on stderr and passes, unless the machine promised the server,
//! `mysql` among the words of xtask's `XTASK_PROMISED`, and then it
//! fails naming the address (#3084). A skip is written where xtask's
//! `XTASK_SKIPPED` names a file, so `xtask test` counts it (#3085).

#![allow(
    dead_code,
    reason = "each test binary uses its own part of what is shared"
)]

use std::fs::OpenOptions;
use std::io::Write as _;
use std::net::{SocketAddr, TcpStream};

/// Where `ci/mysql/compose.yaml` publishes the server, the address every
/// test's PHP connects to.
const AT: &str = "127.0.0.1:3306";

/// xtask's `oracle::PROMISE_VARIABLE`, `Promise::Mysql`'s word and
/// `oracle::SKIPPED_VARIABLE`, spelt here since a test of an extension
/// cannot depend on xtask; xtask's
/// `the_mysql_server_s_tests_read_the_promise_by_its_own_name_and_word`
/// holds the three spellings to its own.
const PROMISE_VARIABLE: &str = "XTASK_PROMISED";
const PROMISE: &str = "mysql";
const SKIPPED_VARIABLE: &str = "XTASK_SKIPPED";

/// The server, once it answered on [`AT`].
pub struct MysqlServer {
    at: SocketAddr,
}

impl MysqlServer {
    /// `test` called with the server where it answers. Where it does not,
    /// `test` is never called: a skip said on stderr, or a failure where the
    /// machine promised the server.
    pub fn with(test: impl FnOnce(&MysqlServer)) {
        match TcpStream::connect(AT).and_then(|probe| probe.peer_addr()) {
            Ok(at) => test(&MysqlServer { at }),
            Err(absent) => skip(&absent),
        }
    }

    /// The server's address.
    pub fn at(&self) -> SocketAddr {
        self.at
    }
}

/// The skip of a test the server did not answer, or its failure where
/// [`PROMISE_VARIABLE`] lists [`PROMISE`], as xtask's `Promise::or_skip`
/// says either, and the skip's line, `<test>\t<word>`, appended to the
/// file [`SKIPPED_VARIABLE`] names, as it writes one: the test is the
/// name libtest gives the thread it runs on, and a thread the test
/// started, which has none, is named by its id.
fn skip(absent: &std::io::Error) {
    let promised = std::env::var(PROMISE_VARIABLE)
        .is_ok_and(|words| words.split_whitespace().any(|word| word == PROMISE));
    let said = format!("no MySQL server on {AT}: {absent}");
    assert!(
        !promised,
        "{said}; {PROMISE_VARIABLE} promised the {PROMISE}"
    );
    eprintln!(
        "{said}; skipping: `docker compose -f ci/mysql/compose.yaml up -d --wait` starts one"
    );
    let Some(file) = std::env::var_os(SKIPPED_VARIABLE) else {
        return;
    };
    let thread = std::thread::current();
    let test = thread
        .name()
        .map_or_else(|| format!("{:?}", thread.id()), str::to_owned);
    OpenOptions::new()
        .create(true)
        .append(true)
        .open(file)
        .and_then(|mut out| out.write_all(format!("{test}\t{PROMISE}\n").as_bytes()))
        .expect("the skip is written to the file XTASK_SKIPPED names");
}

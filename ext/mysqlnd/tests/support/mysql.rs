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
//!
//! A test that holds what a client sends the server connects the client
//! through a [`MysqlRelay`] and reads the bytes back as [`Packets`].

#![allow(
    dead_code,
    reason = "each test binary uses its own part of what is shared"
)]

use std::fs::OpenOptions;
use std::io::{Read as _, Write as _};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

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
}

/// A relay of the test's own between one client and the server: it hands
/// the server's bytes back as they come and keeps every byte the client
/// sends, which it hands on once the client closes its side.
pub struct MysqlRelay {
    at: SocketAddr,
    sent: Receiver<Packets>,
}

impl MysqlRelay {
    /// A relay to `server` on a free local address, for one client.
    #[allow(
        clippy::disallowed_methods,
        reason = "a test starts its own threads (seams.toml rule fork.threads)"
    )]
    pub fn start(server: &MysqlServer) -> MysqlRelay {
        let listener = TcpListener::bind("127.0.0.1:0").expect("the relay binds a free address");
        let at = listener.local_addr().expect("the relay's address");
        let upstream = server.at;
        let (sent, received) = mpsc::channel();
        std::thread::spawn(move || {
            let (mut client, _) = listener.accept().expect("the client connects to the relay");
            let mut upstream =
                TcpStream::connect(upstream).expect("the relay connects to the server");
            let mut from = upstream.try_clone().expect("the server's side, to read");
            let mut to = client.try_clone().expect("the client's side, to write");
            std::thread::spawn(move || std::io::copy(&mut from, &mut to));
            let mut seen = Vec::new();
            let mut chunk = [0; 4096];
            while let Ok(n @ 1..) = client.read(&mut chunk) {
                seen.extend_from_slice(&chunk[..n]);
                upstream.write_all(&chunk[..n]).ok();
            }
            upstream.shutdown(Shutdown::Write).ok();
            sent.send(Packets(seen)).ok();
        });
        MysqlRelay { at, sent: received }
    }

    /// Where the client connects.
    pub fn at(&self) -> SocketAddr {
        self.at
    }

    /// What the client sent the server, once it has closed its side,
    /// waiting at most `within`; `None` while it holds its side open.
    pub fn closed_within(&self, within: Duration) -> Option<Packets> {
        match self.sent.recv_timeout(within) {
            Ok(packets) => Some(packets),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => {
                panic!("the relay ended without handing on what the client sent")
            }
        }
    }
}

/// The bytes a client sent the server, the protocol's packets end to end:
/// each a three-byte little-endian length, a sequence number and that
/// many bytes of payload.
pub struct Packets(Vec<u8>);

impl Packets {
    /// The last packet, whole, or what was sent of it where the bytes end
    /// inside it.
    pub fn last(&self) -> &[u8] {
        let mut last = &[][..];
        let mut rest = &self.0[..];
        while let [a, b, c, _, ..] = *rest {
            let len = usize::from(a) | usize::from(b) << 8 | usize::from(c) << 16;
            last = &rest[..(4 + len).min(rest.len())];
            rest = &rest[last.len()..];
        }
        last
    }

    /// That the last packet is a `COM_QUIT`: one byte of payload, the
    /// command `0x01`, at sequence number 0.
    pub fn assert_quit(&self) {
        assert_eq!(
            self.last(),
            [1, 0, 0, 0, 1],
            "the last packet the server was sent is no COM_QUIT"
        );
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

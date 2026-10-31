//! TLS over a real socket against the binary (#2343): a PHP request served
//! over TLS 1.3 and 1.2 and TLS 1.1 refused, each host's certificate
//! through SNI, and a TLS listener with no certificate refused at
//! startup. A TLS handshake on the clear listener is held in `framing.rs`.
//!
//! The certificates are fixtures under `tests/tls/`, made once with the
//! `openssl` command, a test CA and a leaf for each of `a.test` and
//! `b.test` signed by it, P-256 and valid until 2126: checked in rather
//! than made as the test starts, since no crate in `Cargo.lock` makes a certificate
//! and a test that runs `openssl` holds the machine's build of it too.
//! Their keys guard nothing but these tests.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

mod common;

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use common::{Ending, Request, Server, eventually, manifest_dir, read_until_closed};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{ClientConfig, ClientConnection, ProtocolVersion, RootCertStore, StreamOwned};

/// Long enough for any answer on a loaded machine.
const WAIT: Duration = Duration::from_secs(20);

/// A fixture under `tests/tls/`.
fn fixture(name: &str) -> PathBuf {
    manifest_dir().join("tests/tls").join(name)
}

/// `--certificate` for `host`'s fixtures, under its name where `named`.
fn certificate(host: &str, named: bool) -> String {
    let files = format!(
        "{},{}",
        fixture(&format!("{host}.pem")).display(),
        fixture(&format!("{host}.key")).display()
    );
    if named {
        format!("{host}={files}")
    } else {
        files
    }
}

/// The document root: `scheme.php` says how `$_SERVER` names the
/// request's scheme.
fn tls_root() -> &'static PathBuf {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        common::root(
            "tls",
            &[(
                "scheme.php",
                "<?php echo $_SERVER['REQUEST_SCHEME'], ' ', $_SERVER['HTTPS'] ?? '(no HTTPS)', \"\\n\";",
            )],
        )
    })
}

/// The server, clear and under TLS, with `certificates`, and `args`.
fn server(certificates: &[String], args: &[&str]) -> (Server, SocketAddr) {
    let mut line = vec!["--threads", "2", "--listen-tls", "127.0.0.1:0"];
    for certificate in certificates {
        line.extend(["--certificate", certificate]);
    }
    line.extend(args);
    let server = Server::with_args(tls_root(), &line);
    let tls = server
        .tls
        .unwrap_or_else(|| panic!("the started line names no https:// address"));
    (server, tls)
}

/// A client of `version` alone that trusts the test CA and offers ALPN
/// `http/1.1`.
fn client(version: &'static rustls::SupportedProtocolVersion) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    let ca = CertificateDer::from_pem_file(fixture("ca.pem")).unwrap_or_else(|e| panic!("{e}"));
    roots.add(ca).unwrap_or_else(|e| panic!("{e}"));
    let mut config = ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_protocol_versions(&[version])
    .unwrap_or_else(|e| panic!("{e}"))
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

/// A TLS connection to `addr` as `host`, its handshake made, or why it
/// failed.
fn connect(
    addr: SocketAddr,
    host: &str,
    config: Arc<ClientConfig>,
) -> Result<StreamOwned<ClientConnection, TcpStream>, String> {
    let name = ServerName::try_from(host.to_owned()).unwrap_or_else(|e| panic!("{e}"));
    let connection = ClientConnection::new(config, name).map_err(|e| e.to_string())?;
    let socket = TcpStream::connect(addr).unwrap_or_else(|e| panic!("{e}"));
    socket
        .set_read_timeout(Some(WAIT))
        .unwrap_or_else(|e| panic!("{e}"));
    let mut stream = StreamOwned::new(connection, socket);
    while stream.conn.is_handshaking() {
        stream
            .conn
            .complete_io(&mut stream.sock)
            .map_err(|e| e.to_string())?;
    }
    Ok(stream)
}

/// What `scheme.php` answers on a TLS connection, read to the server's
/// `close_notify`.
fn scheme_over(stream: &mut StreamOwned<ClientConnection, TcpStream>) -> String {
    stream
        .write_all(&Request::get("/scheme.php").host("a.test").close().bytes())
        .unwrap_or_else(|e| panic!("{e}"));
    let mut read = Vec::new();
    stream
        .read_to_end(&mut read)
        .unwrap_or_else(|e| panic!("reading the response: {e}"));
    let response = common::parse_response(&read);
    assert!(response.status.ends_with(" 200 OK"), "{}", response.status);
    String::from_utf8_lossy(&response.body).into_owned()
}

/// A `ClientHello` of TLS 1.1, as a client that offers nothing newer sends
/// it: a legacy version of 3.2 and no `supported_versions` extension,
/// with a suite, a group and a signature scheme rustls takes, so nothing
/// but its version refuses it.
fn tls_1_1_hello() -> Vec<u8> {
    let mut body = vec![0x03, 0x02];
    body.extend([0x5a; 32]);
    body.push(0);
    // TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256; null compression.
    body.extend([0x00, 0x02, 0xc0, 0x2b, 0x01, 0x00]);
    // signature_algorithms: ecdsa_secp256r1_sha256; supported_groups:
    // secp256r1.
    body.extend([0x00, 0x10]);
    body.extend([0x00, 0x0d, 0x00, 0x04, 0x00, 0x02, 0x04, 0x03]);
    body.extend([0x00, 0x0a, 0x00, 0x04, 0x00, 0x02, 0x00, 0x17]);
    let length = u8::try_from(body.len()).unwrap_or_else(|e| panic!("{e}"));
    let mut handshake = vec![0x01, 0x00, 0x00, length];
    handshake.extend(body);
    let length = u8::try_from(handshake.len()).unwrap_or_else(|e| panic!("{e}"));
    let mut record = vec![0x16, 0x03, 0x01, 0x00, length];
    record.extend(handshake);
    record
}

/// A PHP request is served over TLS 1.3 and over TLS 1.2, ALPN agreeing
/// on `http/1.1` and `$_SERVER` saying `https` and `HTTPS=on`, as nginx's
/// `fastcgi_params` pass them; a client of TLS 1.1 is refused with a
/// `protocol_version` alert and no `ServerHello`.
#[test]
fn tls_1_3_and_1_2_serve_a_php_request_and_tls_1_1_is_refused() {
    let (server, tls) = server(&[certificate("a.test", false)], &[]);
    for (version, wanted) in [
        (&rustls::version::TLS13, ProtocolVersion::TLSv1_3),
        (&rustls::version::TLS12, ProtocolVersion::TLSv1_2),
    ] {
        let mut stream =
            connect(tls, "a.test", client(version)).unwrap_or_else(|e| panic!("{wanted:?}: {e}"));
        assert_eq!(stream.conn.protocol_version(), Some(wanted));
        assert_eq!(stream.conn.alpn_protocol(), Some(&b"http/1.1"[..]));
        assert_eq!(scheme_over(&mut stream), "https on\n", "{wanted:?}");
    }
    let mut old = TcpStream::connect(tls).unwrap_or_else(|e| panic!("{e}"));
    old.write_all(&tls_1_1_hello())
        .unwrap_or_else(|e| panic!("{e}"));
    let refused = read_until_closed(&mut old, WAIT);
    // An alert record, fatal, `protocol_version(70)`; never a handshake.
    assert_eq!(
        refused.bytes.get(..1).zip(refused.bytes.get(5..7)),
        Some((&[0x15][..], &[2, 70][..])),
        "{:02x?}",
        refused.bytes
    );
    assert_ne!(refused.ending, Ending::Open);
    // The clear port serves on beside it, saying `http` and no `HTTPS`.
    let clear = server.exchange(&Request::get("/scheme.php").close().bytes(), WAIT);
    assert_eq!(clear.response().body, b"http (no HTTPS)\n");
}

/// Two hosts each get their own certificate through SNI, each verified
/// against the test CA under the name asked for; a handshake that names
/// no host, with no default certificate, fails.
#[test]
fn two_hosts_each_get_their_own_certificate_through_sni() {
    let (_server, tls) = server(
        &[certificate("a.test", true), certificate("b.test", true)],
        &[],
    );
    for host in ["a.test", "b.test"] {
        let mut stream = connect(tls, host, client(&rustls::version::TLS13))
            .unwrap_or_else(|e| panic!("{host}: {e}"));
        let served = stream
            .conn
            .peer_certificates()
            .and_then(<[_]>::first)
            .unwrap_or_else(|| panic!("{host}: no certificate"))
            .clone();
        let own = CertificateDer::from_pem_file(fixture(&format!("{host}.pem")))
            .unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(served, own, "{host}");
        assert_eq!(scheme_over(&mut stream), "https on\n", "{host}");
    }
    // An address names no host, so the client sends no SNI.
    let unnamed = connect(tls, "127.0.0.1", client(&rustls::version::TLS13));
    assert!(unnamed.is_err(), "a handshake with no host was served");
}

/// TLS asked for with no certificate refuses to start, before it listens,
/// as does a certificate whose key is another's.
#[test]
fn tls_without_a_certificate_refuses_to_start() {
    let mismatched = format!(
        "{},{}",
        fixture("a.test.pem").display(),
        fixture("b.test.key").display()
    );
    for (certificates, refusal) in [
        (
            &[][..],
            "makala: --listen-tls without a certificate: TLS needs at least one \
             --certificate [<host>=]<chain.pem>,<key.pem>",
        ),
        (
            &["--certificate", &mismatched][..],
            "makala: --certificate ",
        ),
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_makala"))
            .args(["-n", "--listen-tls", "127.0.0.1:0", "--root"])
            .arg(tls_root())
            .args(certificates)
            .stdin(Stdio::null())
            .output()
            .unwrap_or_else(|e| panic!("running makala: {e}"));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{stderr}");
        assert!(stderr.starts_with(refusal), "{stderr}");
        assert!(!stderr.contains("listening on"), "{stderr}");
    }
}

/// A client that connects to the TLS listener and never begins its handshake
/// is closed at the handshake timeout, and the error log says so.
#[test]
fn a_handshake_past_the_handshake_timeout_is_closed_and_logged() {
    let log = tls_root().join("handshake.log");
    std::fs::remove_file(&log).ok();
    let log_arg = log.to_string_lossy().into_owned();
    let (_server, tls) = server(
        &[certificate("a.test", false)],
        &[
            "--limit",
            "handshake-timeout=300ms",
            "--error-log",
            &log_arg,
        ],
    );
    let started = Instant::now();
    let mut silent = TcpStream::connect(tls).unwrap_or_else(|e| panic!("{e}"));
    let closed = read_until_closed(&mut silent, WAIT);
    assert_eq!(closed.bytes, b"");
    assert_eq!(closed.ending, Ending::Closed);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
    let read = || std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        eventually(WAIT, || read().contains(
            "the TLS handshake failed: it passed the handshake timeout"
        )),
        "{}",
        read()
    );
}

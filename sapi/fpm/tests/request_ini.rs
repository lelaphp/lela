//! An ini value a request carries in `PHP_VALUE` or `PHP_ADMIN_VALUE` is
//! laid over the pool's for that request and is gone when it ends; the
//! pool's own `php_value` stays for the child's life (design §4.8, as
//! ruled 2026-11-11).
//!
//! On a pool of one static child whose `php_value[precision]` is 10,
//! request A carries `precision=7` and runs a program that reads the
//! entry, `ini_set`s it to 9 and reads it again; requests B and C carry
//! neither parameter and read it. Within A each parameter keeps php's
//! meaning, A's bytes being the pinned php-fpm 8.5.12's (md5
//! f7b17a549250f398e137eef2bd917f1c, measured 2026-11-11): `ini_set`
//! changes a `PHP_VALUE` and answers the old value, and answers `false`
//! to a `PHP_ADMIN_VALUE`, which `fpm_php_zend_ini_alter_master` makes
//! `PHP_INI_SYSTEM`. B and C read the pool's 10 here; the pinned php-fpm
//! keeps the carried value on the child and B and C read 7 there (php's
//! bug #53611), the divergence
//! `trace:fpm-request-ini-ends-with-request`.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use php_proto::fcgi::{self, Header, Kind};

/// A master on a directory of its own, `[www]` one static child listening
/// on `fpm.sock` with `php_value[precision] = 10`: dropped, it is sent
/// `SIGTERM`, waited for and its directory removed.
struct Master {
    dir: PathBuf,
    child: Child,
}

impl Master {
    /// The master, and the programs `a.php`, which reads `precision`,
    /// sets it to 9 and reads it again, and `b.php`, which reads it.
    fn start(name: &str) -> Master {
        let dir = std::env::temp_dir().join(format!(
            "sapi-fpm-request-ini-{name}-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("the test's directory is made");
        for (file, program) in [
            (
                "a.php",
                r#"<?php echo ini_get('precision'), ' '; var_dump(ini_set('precision', '9')); echo ini_get('precision'), ' ', 1/3, "\n";"#,
            ),
            (
                "b.php",
                r#"<?php echo ini_get('precision'), ' ', 1/3, "\n";"#,
            ),
        ] {
            std::fs::write(dir.join(file), program).expect("the program is written");
        }
        let at = dir.display();
        let path = dir.join("php-fpm.conf");
        std::fs::write(
            &path,
            format!(
                "[global]\nerror_log = {at}/fpm.log\n[www]\nlisten = {at}/fpm.sock\npm = static\npm.max_children = 1\nphp_value[precision] = 10\n"
            ),
        )
        .expect("the configuration is written");
        let child = Command::new(env!("CARGO_BIN_EXE_php-fpm"))
            .args(["-R", "-n", "-F", "-y"])
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("php-fpm starts");
        Master { dir, child }
    }

    /// What request 1 for the program `name`, with `more` after its
    /// parameters, writes on `FCGI_STDOUT` after the head, and on
    /// `FCGI_STDERR`.
    fn get(&self, name: &str, more: &[(&str, &str)]) -> (String, String) {
        let socket = self.dir.join("fpm.sock");
        let started = Instant::now();
        let mut stream = loop {
            match UnixStream::connect(&socket) {
                Ok(stream) => break stream,
                Err(refused) => {
                    assert!(
                        started.elapsed() < Duration::from_secs(10),
                        "nothing listens at {} after ten seconds: {refused}",
                        socket.display()
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };
        let script = self.dir.join(name).display().to_string();
        let mut params = Vec::new();
        for (key, value) in [
            ("SCRIPT_FILENAME", script.as_str()),
            ("REQUEST_METHOD", "GET"),
        ]
        .into_iter()
        .chain(more.iter().copied())
        {
            fcgi::put_pair(&mut params, key.as_bytes(), value.as_bytes());
        }
        let mut out = record(Kind::BeginRequest, &[0, 1, 0, 0, 0, 0, 0, 0]);
        out.extend(record(Kind::Params, &params));
        out.extend(record(Kind::Params, &[]));
        out.extend(record(Kind::Stdin, &[]));
        stream.write_all(&out).expect("the request is written");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("the reply's reads are timed");
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        loop {
            let mut bytes = [0u8; 8];
            if stream.read_exact(&mut bytes).is_err() {
                break;
            }
            let header = Header::decode(bytes);
            let len = usize::from(header.content_length);
            let mut content = vec![0u8; len + usize::from(header.padding_length)];
            stream
                .read_exact(&mut content)
                .expect("the record's content follows its header");
            content.truncate(len);
            match header.kind {
                Kind::Stdout => stdout.extend_from_slice(&content),
                Kind::Stderr => stderr.extend_from_slice(&content),
                Kind::EndRequest => break,
                _ => {}
            }
        }
        let stdout = String::from_utf8_lossy(&stdout);
        let body = stdout.split_once("\r\n\r\n").map_or(&*stdout, |(_, b)| b);
        (
            body.to_owned(),
            String::from_utf8_lossy(&stderr).into_owned(),
        )
    }
}

impl Drop for Master {
    fn drop(&mut self) {
        if let Ok(pid) = i32::try_from(self.child.id()) {
            nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGTERM,
            )
            .ok();
        }
        self.child.wait().ok();
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// One `FastCGI` record of request 1, padded to a multiple of eight bytes
/// as php's `fcgi_make_header` pads it.
fn record(kind: Kind, content: &[u8]) -> Vec<u8> {
    let header = Header::make(kind, 1, content.len());
    let mut out = header.encode().to_vec();
    out.extend_from_slice(content);
    out.resize(out.len() + usize::from(header.padding_length), 0);
    out
}

/// Requests A, B and C on one child, A carrying `precision=7` under
/// `parameter`, and what each wrote.
fn a_then_b_and_c(parameter: &str) -> [(String, String); 3] {
    let master = Master::start(&parameter.to_ascii_lowercase());
    [
        master.get("a.php", &[(parameter, "precision=7")]),
        master.get("b.php", &[]),
        master.get("b.php", &[]),
    ]
}

/// What B and C write: the pool's precision.
fn pools() -> (String, String) {
    (String::from("10 0.3333333333\n"), String::new())
}

#[test]
fn a_php_value_ini_set_changes_ends_with_its_request() {
    let [a, b, c] = a_then_b_and_c("PHP_VALUE");
    assert_eq!(
        a,
        (
            String::from("7 string(1) \"7\"\n9 0.333333333\n"),
            String::new()
        )
    );
    assert_eq!(b, pools());
    assert_eq!(c, pools());
}

#[test]
fn a_php_admin_value_ini_set_cannot_change_ends_with_its_request() {
    let [a, b, c] = a_then_b_and_c("PHP_ADMIN_VALUE");
    assert_eq!(
        a,
        (String::from("7 bool(false)\n7 0.3333333\n"), String::new())
    );
    assert_eq!(b, pools());
    assert_eq!(c, pools());
}

//! The shared pool `pm.status_listen` makes serves its socket under
//! `security.limit_extensions` as every pool does (#4352), each request
//! held to the bytes the pinned php-fpm 8.5.12 (md5
//! f7b17a549250f398e137eef2bd917f1c) wrote on 2026-11-11 for the same
//! configuration, the directory written `{dir}`.
//!
//! php allocates the shared pool before the default pass reaches it
//! (`fpm_worker_pool_shared_status_alloc`, `Z/sapi/fpm/fpm/fpm_conf.c:733`)
//! and copies no `security.limit_extensions` into it, so the pass gives it
//! php's default, `.php .phar` (`fpm_conf.c:1147`), whatever its pool
//! names. A program the status path does not name is served on the status
//! socket when its name ends in `.php`, and refused with a 403 and
//! `fpm_php_limit_extensions`'s notice on `FCGI_STDERR` otherwise
//! (`Z/sapi/fpm/fpm/fpm_main.c:1866`), a pool whose list is `.inc`
//! changing neither.

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

/// A master on a directory of its own, `[www]` listening on `fpm.sock`
/// and its status pool on `status.sock`: dropped, it is sent `SIGTERM`,
/// waited for and its directory removed.
struct Master {
    dir: PathBuf,
    child: Child,
}

impl Master {
    /// The master of `[www]` with `more` after its directives, and the
    /// programs `a.php`, `a.txt` and `a.inc`, each saying which it is.
    fn start(name: &str, more: &str) -> Master {
        let dir = std::env::temp_dir().join(format!(
            "sapi-fpm-status-listen-{name}-{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("the test's directory is made");
        for ext in ["php", "txt", "inc"] {
            std::fs::write(
                dir.join(format!("a.{ext}")),
                format!("<?php echo 'ran {ext}';"),
            )
            .expect("the script is written");
        }
        let at = dir.display();
        let path = dir.join("php-fpm.conf");
        std::fs::write(
            &path,
            format!(
                "[global]\nerror_log = {at}/fpm.log\n[www]\nlisten = {at}/fpm.sock\npm = static\npm.max_children = 1\npm.status_listen = {at}/status.sock\npm.status_path = /status\n{more}"
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

    /// What request 1 for the program `name` writes on `FCGI_STDOUT` and
    /// on `FCGI_STDERR` through `socket`, the directory written `{dir}`.
    fn get(&self, socket: &str, name: &str) -> (String, String) {
        let socket = self.dir.join(socket);
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
        let uri = format!("/{name}");
        let mut params = Vec::new();
        for (key, value) in [
            ("SCRIPT_FILENAME", script.as_str()),
            ("REQUEST_METHOD", "GET"),
            ("SCRIPT_NAME", uri.as_str()),
            ("REQUEST_URI", uri.as_str()),
            ("QUERY_STRING", ""),
        ] {
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
        let (mut body, mut stderr) = (Vec::new(), Vec::new());
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
                Kind::Stdout => body.extend_from_slice(&content),
                Kind::Stderr => stderr.extend_from_slice(&content),
                Kind::EndRequest => break,
                _ => {}
            }
        }
        let shown = |bytes: &[u8]| {
            String::from_utf8_lossy(bytes).replace(&self.dir.display().to_string(), "{dir}")
        };
        (shown(&body), shown(&stderr))
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

/// What the pinned php-fpm wrote for a program it ran.
fn ran(ext: &str) -> (String, String) {
    (
        format!(
            "X-Powered-By: PHP/8.5.12\r\nContent-type: text/html; charset=UTF-8\r\n\r\nran {ext}"
        ),
        String::new(),
    )
}

/// What the pinned php-fpm wrote for a program `security.limit_extensions`
/// refused.
fn refused(name: &str) -> (String, String) {
    (
        String::from(
            "Status: 403 Forbidden\r\nX-Powered-By: PHP/8.5.12\r\nContent-type: text/html; charset=UTF-8\r\n\r\nAccess denied.\n",
        ),
        format!(
            "Access to the script '{{dir}}/{name}' has been denied (see security.limit_extensions)"
        ),
    )
}

#[test]
fn the_status_socket_refuses_a_script_whose_extension_the_default_does_not_list() {
    let master = Master::start("default", "");
    assert_eq!(master.get("status.sock", "a.php"), ran("php"));
    assert_eq!(master.get("status.sock", "a.txt"), refused("a.txt"));
    assert_eq!(master.get("status.sock", "a.inc"), refused("a.inc"));
}

#[test]
fn the_status_socket_keeps_php_s_default_where_its_pool_names_another_list() {
    let master = Master::start("pool", "security.limit_extensions = .inc\n");
    assert_eq!(master.get("status.sock", "a.php"), ran("php"));
    assert_eq!(master.get("status.sock", "a.inc"), refused("a.inc"));
    assert_eq!(master.get("fpm.sock", "a.inc"), ran("inc"));
    assert_eq!(master.get("fpm.sock", "a.php"), refused("a.php"));
}

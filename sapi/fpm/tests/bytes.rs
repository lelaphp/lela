//! Words that are not UTF-8 in php-fpm's log, held here where
//! `lines.toml`, whose files are TOML text, cannot lay them (#4401), each
//! to the bytes the pinned php-fpm 8.5.12 (md5
//! f7b17a549250f398e137eef2bd917f1c) wrote on 2026-11-10 for the same
//! configuration, the time and the pids masked and the directory named
//! `{dir}`. php's `zlog` writes a `%s` word as its bytes
//! (`Z/sapi/fpm/fpm/zlog.c:220`), so a `\xff` stays one byte where a
//! decoding would write U+FFFD's three. Each line is compared byte for
//! byte, a byte past ASCII written `\xNN` only to say what differed.
//!
//! - a pool named `w\xffw` whose `listen.allowed_clients` names a word
//!   that is no address, `\xff\xff`: the child says so through
//!   `fpm_fcgi_log` (`Z/main/fastcgi.c:853`), and the master writes what
//!   the child said into its stderr under the pool's name. The list is
//!   read for an inet socket alone, so the pool listens on a loopback TCP
//!   address, which nothing is sent to.
//! - `-tt` over a configuration whose include pattern, pool name, `env`
//!   and `php_value` entries are not UTF-8: the warning for the pattern
//!   nothing matches and the dump's lines for each.
//! - `-t` over a pool entry whose name is not UTF-8: the reader's line
//!   for the unknown entry.
//! - `-h` under an `argv[0]` that is not UTF-8: the usage's first line,
//!   which names the program (#4455).
//! - a pool's `env[A] = $B\xff`, the master started with `B\xff` set to
//!   `val`: php hands the bytes after `$` to `getenv`
//!   (`Z/sapi/fpm/fpm/fpm_env.c:172`), so a request's `getenv("A")` is
//!   the variable's value, and the request's body is held to the pinned
//!   php-fpm's, `string(3) "val"`, written on 2026-11-11 (#4453).

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

use std::net::{Ipv4Addr, TcpListener};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// A directory of the test's own.
fn tree(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sapi-fpm-bytes-{name}-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).expect("the test's directory is made");
    dir
}

/// `conf` written to `dir/php-fpm.conf`, each `{dir}` in it the
/// directory's path; the file's path.
fn conf(dir: &Path, conf: &[u8]) -> PathBuf {
    let path = dir.join("php-fpm.conf");
    let at = dir.as_os_str().as_encoded_bytes();
    std::fs::write(&path, replace(conf, b"{dir}", at)).expect("the configuration is written");
    path
}

/// `bytes` with each `from` in it `to`.
fn replace(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut rest = bytes;
    while !rest.is_empty() {
        if rest.starts_with(from) {
            out.extend_from_slice(to);
            rest = &rest[from.len()..];
        } else {
            out.push(rest[0]);
            rest = &rest[1..];
        }
    }
    out
}

/// `bytes` as text, each byte past ASCII written `\xNN` and a `\` as
/// `\\`, so no two lines differing show alike.
fn shown(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b == b'\\' {
                String::from("\\\\")
            } else if b.is_ascii() {
                char::from(b).to_string()
            } else {
                format!("\\x{b:02x}")
            }
        })
        .collect()
}

/// The lines of `log` that hold a byte past ASCII, each with its
/// `[time] ` head taken off, a `child <pid>` written `child N` and the
/// directory written `{dir}`, as text ([`shown`]).
fn masked(log: &[u8], dir: &Path) -> Vec<String> {
    let log = replace(log, dir.as_os_str().as_encoded_bytes(), b"{dir}");
    log.split(|b| *b == b'\n')
        .filter(|line| !line.is_ascii())
        .map(|line| {
            let line = match line.iter().position(|b| *b == b']') {
                Some(at) if line.first() == Some(&b'[') => &line[at + 2..],
                _ => line,
            };
            let mut out = Vec::new();
            let mut rest = line;
            while let Some(at) = rest.windows(6).position(|w| w == b"child ") {
                out.extend_from_slice(&rest[..at + 6]);
                rest = &rest[at + 6..];
                let digits = rest.iter().take_while(|b| b.is_ascii_digit()).count();
                if digits > 0 {
                    out.push(b'N');
                }
                rest = &rest[digits..];
            }
            out.extend_from_slice(rest);
            shown(&out)
        })
        .collect()
}

#[test]
fn a_word_of_listen_allowed_clients_is_logged_with_its_own_bytes() {
    let dir = tree("allowed");
    let port = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("a port is found")
        .local_addr()
        .expect("the port is read")
        .port();
    let path = conf(
        &dir,
        &[
            &b"[global]\nerror_log = {dir}/fpm.log\n[w\xffw]\nlisten = 127.0.0.1:"[..],
            port.to_string().as_bytes(),
            b"\nlisten.allowed_clients = 127.0.0.1,bogus,\xff\xff,::1\npm = static\npm.max_children = 1\ncatch_workers_output = yes\n",
        ]
        .concat(),
    );
    let mut master = Command::new(env!("CARGO_BIN_EXE_php-fpm"))
        .args(["-R", "-n", "-F", "-y"])
        .arg(&path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("php-fpm starts");
    let log = dir.join("fpm.log");
    let said = |log: &[u8]| {
        log.windows(24)
            .filter(|w| w == b"listen.allowed_clients\"\n")
            .count()
    };
    let started = Instant::now();
    while said(&std::fs::read(&log).unwrap_or_default()) < 2
        && started.elapsed() < Duration::from_secs(30)
    {
        std::thread::sleep(Duration::from_millis(20));
    }
    let pid = nix::unistd::Pid::from_raw(i32::try_from(master.id()).expect("a pid"));
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).ok();
    master.wait().ok();
    let written = std::fs::read(&log).unwrap_or_default();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(
        masked(&written, &dir),
        [
            r#"WARNING: [pool w\xffw] child N said into stderr: "ERROR: Wrong IP address 'bogus' in listen.allowed_clients""#,
            r#"WARNING: [pool w\xffw] child N said into stderr: "ERROR: Wrong IP address '\xff\xff' in listen.allowed_clients""#,
        ]
    );
}

#[test]
fn a_configuration_dump_writes_each_word_with_its_own_bytes() {
    let dir = tree("dump");
    let path = conf(
        &dir,
        b"[global]\nerror_log = {dir}/fpm.log\ninclude = {dir}/no\xff*.conf\n[w\xffw]\nlisten = {dir}/fpm.sock\npm = static\npm.max_children = 1\nenv[k\xff] = v\xff\nphp_value[x\xff] = \xff\n",
    );
    let out = Command::new(env!("CARGO_BIN_EXE_php-fpm"))
        .args(["-R", "-n", "-tt", "-y"])
        .arg(&path)
        .output()
        .expect("php-fpm -tt runs");
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(
        masked(&out.stderr, &dir),
        [
            r"WARNING: Nothing matches the include pattern '{dir}/no\xff*.conf' from {dir}/php-fpm.conf at line 3.",
            r"NOTICE: [w\xffw]",
            "NOTICE: \tenv[k\\xff] = v\\xff",
            "NOTICE: \tphp_value[x\\xff] = \\xff",
        ]
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn an_unknown_entry_is_named_with_its_own_bytes() {
    let dir = tree("unknown");
    let path = conf(
        &dir,
        b"[global]\nerror_log = {dir}/fpm.log\n[www]\nlisten = {dir}/fpm.sock\npm = static\npm.max_children = 1\nx\xffx = 1\n",
    );
    let out = Command::new(env!("CARGO_BIN_EXE_php-fpm"))
        .args(["-R", "-n", "-t", "-y"])
        .arg(&path)
        .output()
        .expect("php-fpm -t runs");
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(
        masked(&out.stderr, &dir),
        [r"ERROR: [{dir}/php-fpm.conf:7] unknown entry 'x\xffx'"]
    );
    assert_eq!(out.status.code(), Some(78));
}

/// What request 1, for `script`, writes on `FCGI_STDOUT` through the pool
/// listening at `socket`, tried for ten seconds; empty where nothing
/// listens or answers.
fn served(socket: &Path, script: &Path) -> Vec<u8> {
    use php_proto::fcgi::{self, Header, Kind};
    use std::io::{Read, Write};
    let record = |kind: Kind, content: &[u8]| {
        let header = Header::make(kind, 1, content.len());
        let mut out = header.encode().to_vec();
        out.extend_from_slice(content);
        out.resize(out.len() + usize::from(header.padding_length), 0);
        out
    };
    let started = Instant::now();
    let mut stream = loop {
        match std::os::unix::net::UnixStream::connect(socket) {
            Ok(stream) => break stream,
            Err(_) if started.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(_) => return Vec::new(),
        }
    };
    let mut params = Vec::new();
    let script = script.as_os_str().as_encoded_bytes();
    fcgi::put_pair(&mut params, b"SCRIPT_FILENAME", script);
    fcgi::put_pair(&mut params, b"REQUEST_METHOD", b"GET");
    let mut out = record(Kind::BeginRequest, &[0, 1, 0, 0, 0, 0, 0, 0]);
    out.extend(record(Kind::Params, &params));
    out.extend(record(Kind::Params, &[]));
    out.extend(record(Kind::Stdin, &[]));
    if stream.write_all(&out).is_err()
        || stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .is_err()
    {
        return Vec::new();
    }
    let mut body = Vec::new();
    loop {
        let mut bytes = [0u8; 8];
        if stream.read_exact(&mut bytes).is_err() {
            return body;
        }
        let header = Header::decode(bytes);
        let len = usize::from(header.content_length);
        let mut content = vec![0u8; len + usize::from(header.padding_length)];
        if stream.read_exact(&mut content).is_err() {
            return body;
        }
        match header.kind {
            Kind::Stdout => body.extend_from_slice(&content[..len]),
            Kind::EndRequest => return body,
            _ => {}
        }
    }
}

#[test]
fn an_env_value_naming_a_variable_finds_it_by_its_bytes() {
    use std::os::unix::ffi::OsStrExt;
    let dir = tree("env");
    let path = conf(
        &dir,
        b"[global]\nerror_log = {dir}/fpm.log\n[www]\nlisten = {dir}/fpm.sock\npm = static\npm.max_children = 1\nenv[A] = $B\xff\n",
    );
    let script = dir.join("index.php");
    std::fs::write(&script, "<?php var_dump(getenv(\"A\"));").expect("the script is written");
    let mut master = Command::new(env!("CARGO_BIN_EXE_php-fpm"))
        .args(["-R", "-n", "-F", "-y"])
        .arg(&path)
        .env(std::ffi::OsStr::from_bytes(b"B\xff"), "val")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("php-fpm starts");
    let response = served(&dir.join("fpm.sock"), &script);
    let pid = nix::unistd::Pid::from_raw(i32::try_from(master.id()).expect("a pid"));
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM).ok();
    master.wait().ok();
    std::fs::remove_dir_all(&dir).ok();
    let body = match response.windows(4).position(|w| w == b"\r\n\r\n") {
        Some(at) => &response[at + 4..],
        None => &response[..],
    };
    assert_eq!(
        shown(body),
        shown(b"string(3) \"val\"\n"),
        "{}",
        shown(&response)
    );
}

/// `-h` names the program with what follows `argv[0]`'s last `/`, as its
/// bytes, and `php` where `argv[0]` holds no `/` (`php_cgi_usage`,
/// `Z/sapi/fpm/fpm/fpm_main.c:832-840`): each first line the bytes the
/// pinned php-fpm 8.5.12 (md5 f7b17a549250f398e137eef2bd917f1c) wrote on
/// 2026-11-11 under the same `argv[0]`.
#[test]
fn the_usage_names_the_program_with_argv0s_own_bytes() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::process::CommandExt;
    let first = |argv0: &[u8]| {
        let out = Command::new(env!("CARGO_BIN_EXE_php-fpm"))
            .arg0(OsStr::from_bytes(argv0))
            .args(["-n", "-h"])
            .output()
            .expect("php-fpm -h runs");
        assert_eq!(out.status.code(), Some(0));
        let line = out.stdout.split(|b| *b == b'\n').next().unwrap_or_default();
        shown(line)
    };
    let options = "[-n] [-e] [-h] [-i] [-m] [-v] [-t] [-p <prefix>] [-g <pid>] [-c <file>] [-d foo[=bar]] [-y <file>] [-D] [-F [-O]]";
    assert_eq!(first(b"/x/f\xffm"), format!(r"Usage: f\xffm {options}"));
    assert_eq!(first(b"f\xffm"), format!("Usage: php {options}"));
}

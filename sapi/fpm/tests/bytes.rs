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

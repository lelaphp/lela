//! Words that are not UTF-8 on php-cgi's stderr, held here where
//! `lines.toml`, whose files are TOML text, cannot lay them (#4450), each
//! to the bytes the pinned php-cgi 8.5.12 (md5
//! f4f783bf54d2bfa24a558693f7c12188) wrote on 2026-11-11 for the same
//! command line. php writes a `%s` word as its bytes, so a `\xe9` stays
//! one byte where a decoding would write U+FFFD's three.
//!
//! - `env -i php-cgi -n -b $'caf\xe9':29701`: the host no resolver
//!   answers is said through `fcgi_log` (`Z/main/fastcgi.c:693`), and
//!   the refused bind with the whole `-b` word through
//!   `fprintf(stderr, "…%s\n", bindpath)` (`Z/sapi/cgi/cgi_main.c:1945`).
//!   Nothing is bound, so the test takes no socket.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;

/// What the pinned php-cgi wrote on stderr, whole, for a `-b` host of
/// `caf\xe9`; it wrote nothing on stdout and ended with 255.
const PINNED_HOST: &[u8] = b"Cannot resolve host name 'caf\xe9'!\nCouldn't create FastCGI listen socket on port caf\xe9:29701\n";

/// `bytes` as text, each byte past ASCII written `\xNN` and a `\` as
/// `\\`, so no two outputs differing show alike.
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

#[test]
fn a_bind_host_that_is_not_utf8_is_said_with_its_own_bytes() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_php-cgi"))
        .arg("-n")
        .arg("-b")
        .arg(OsStr::from_bytes(b"caf\xe9:29701"))
        .env_clear()
        .output()
        .unwrap();
    assert_eq!(
        (shown(&out.stdout), shown(&out.stderr)),
        (String::new(), shown(PINNED_HOST))
    );
    assert_eq!(out.status.code(), Some(255));
}

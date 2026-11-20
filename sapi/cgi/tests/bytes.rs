//! Words that are not UTF-8 on php-cgi's stderr or stdout, held here where
//! `lines.toml`, whose files are TOML text, cannot lay them, each
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
//! - `env -i php-cgi -n -h` with an `argv[0]` of `/x/p\xe9`, on stdout:
//!   `php_cgi_usage` (`Z/sapi/cgi/cgi_main.c:1015`) names the program by
//!   what follows `argv[0]`'s last `/` through `printf`'s `%s`.
//! - `env -i php-cgi -n $'-\xe9'`, on stderr: `php_opt_error` names the
//!   switch it did not find as the one byte it was given, through
//!   `fprintf(stderr, "…option not found %c\n", …)` (`Z/main/getopt.c:39`).

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::process::CommandExt as _;

/// What the pinned php-cgi wrote on stderr, whole, for a `-b` host of
/// `caf\xe9`; it wrote nothing on stdout and ended with 255.
const PINNED_HOST: &[u8] = b"Cannot resolve host name 'caf\xe9'!\nCouldn't create FastCGI listen socket on port caf\xe9:29701\n";

/// What the pinned php-cgi wrote on stdout, whole, for `-h` under an
/// `argv[0]` of `/x/p\xe9`; it wrote nothing on stderr and ended with 0.
const PINNED_USAGE: &[u8] = b"Usage: p\xe9 [-q] [-h] [-s] [-v] [-i] [-f <file>]\n\
    \x20      p\xe9 <file> [args...]\n\
    \x20 -a               Run interactively\n\
    \x20 -b <address:port>|<port> Bind Path for external FASTCGI Server mode\n\
    \x20 -C               Do not chdir to the script's directory\n\
    \x20 -c <path>|<file> Look for php.ini file in this directory\n\
    \x20 -n               No php.ini file will be used\n\
    \x20 -d foo[=bar]     Define INI entry foo with value 'bar'\n\
    \x20 -e               Generate extended information for debugger/profiler\n\
    \x20 -f <file>        Parse <file>.  Implies `-q'\n\
    \x20 -h               This help\n\
    \x20 -i               PHP information\n\
    \x20 -l               Syntax check only (lint)\n\
    \x20 -m               Show compiled in modules\n\
    \x20 -q               Quiet-mode.  Suppress HTTP Header output.\n\
    \x20 -s               Display colour syntax highlighted source.\n\
    \x20 -v               Version number\n\
    \x20 -w               Display source with stripped comments and whitespace.\n\
    \x20 -T <count>       Measure execution time of script repeated <count> times.\n";

/// What the pinned php-cgi wrote on stderr, whole, for a switch of `\xe9`;
/// its usage followed on stdout and it ended with 1.
const PINNED_NOT_FOUND: &[u8] = b"Error in argument 2, char 2: option not found \xe9\n";

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
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_lela-cgi"))
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

#[test]
fn the_usage_names_a_program_that_is_not_utf8_with_its_own_bytes() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_lela-cgi"))
        .arg0(OsStr::from_bytes(b"/x/p\xe9"))
        .arg("-n")
        .arg("-h")
        .env_clear()
        .output()
        .unwrap();
    assert_eq!(
        (shown(&out.stdout), shown(&out.stderr)),
        (shown(PINNED_USAGE), String::new())
    );
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn a_switch_that_is_not_utf8_is_named_with_its_own_byte() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_lela-cgi"))
        .arg("-n")
        .arg(OsStr::from_bytes(b"-\xe9"))
        .env_clear()
        .output()
        .unwrap();
    assert_eq!(shown(&out.stderr), shown(PINNED_NOT_FOUND));
    assert_eq!(out.status.code(), Some(1));
}

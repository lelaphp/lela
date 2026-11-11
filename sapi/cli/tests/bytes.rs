//! Words that are not UTF-8 on php's stderr, held here where `lines.toml`,
//! whose files are TOML text, cannot lay them, each to the bytes the pinned
//! php 8.5.12 (md5 18e167fbb0e7a514e962a4e6b3d85804) wrote on 2026-11-11
//! for the same command line. php writes a `%c` byte as it stands, so a
//! `\xe9` stays one byte where a decoding would write U+FFFD's three.
//!
//! - `env -i php -n $'-\xe9'`, on stderr: `php_opt_error` names the switch
//!   it did not find as the one byte it was given, through
//!   `fprintf(stderr, "…option not found %c\n", …)` (`Z/main/getopt.c:39`).

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;

/// What the pinned php wrote on stderr, whole, for a switch of `\xe9`; its
/// usage followed on stdout and it ended with 1.
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
fn a_switch_that_is_not_utf8_is_named_with_its_own_byte() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_php"))
        .arg("-n")
        .arg(OsStr::from_bytes(b"-\xe9"))
        .env_clear()
        .output()
        .unwrap();
    assert_eq!(shown(&out.stderr), shown(PINNED_NOT_FOUND));
    assert_eq!(out.status.code(), Some(1));
}

//! `php_getopt` (`Z/main/getopt.c`), which php's CLI, CGI, FPM and
//! debugging hosts read their command lines with, each over its own
//! `OPTIONS` table: the engine's one walk, which `getopt()` reads a
//! program's `argv` with too, and what the hosts share around it,
//! a table's rows written as php writes them and `php_opt_error`'s line.

use std::ffi::OsString;

pub use php_engine::getopt::{Getopt, Need, Opt, OptError, Stopped, Taken};

/// One row of a host's `OPTIONS` as php writes it: the code `php_getopt`
/// answers, a letter or php's number for a long switch, whether it takes a
/// value (`1`), may (`2`) or does not (`0`), and its long name.
#[must_use]
pub const fn opt(code: u8, need: u8, name: &'static str) -> Opt<'static> {
    Opt {
        code,
        need: match need {
            0 => Need::Nothing,
            1 => Need::Required,
            _ => Need::Optional,
        },
        name: Some(name.as_bytes()),
    }
}

/// `php_opt_error`'s line on stderr (`Z/main/getopt.c:28-50`), the
/// argument counted from the program's name and the character from
/// one; the host's usage follows it on stdout. `argv` is the line after
/// the program's name, as a host's walk reads it from 0. The switch is
/// named by its byte as php's `%c` writes it, whether or not it is UTF-8.
#[must_use]
pub fn getopt_line(argv: &[Vec<u8>], argument: usize, at: usize, error: OptError) -> Vec<u8> {
    let letter = argv.get(argument).and_then(|word| word.get(at));
    let (reason, letter): (&[u8], _) = match error {
        OptError::Colon => (b": in flags", None),
        OptError::NotFound => (b"option not found ", letter),
        OptError::NoArgument => (b"no argument for option ", letter),
    };
    let mut line = format!("Error in argument {}, char {}: ", argument + 1, at + 1).into_bytes();
    line.extend_from_slice(reason);
    line.extend(letter);
    line.push(b'\n');
    line
}

/// Bytes of the command line as a word the host hands on.
#[must_use]
pub fn os(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

//! `php_getopt` (`Z/main/getopt.c`), which php's CLI, CGI, FPM and
//! debugging hosts read their command lines with, each over its own
//! `OPTIONS` table: the engine's one walk, which `getopt()` reads a
//! program's `argv` with too (#889), and what the hosts share around it,
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
/// the program's name, as a host's walk reads it from 0.
#[must_use]
pub fn getopt_line(argv: &[Vec<u8>], argument: usize, at: usize, error: OptError) -> String {
    let letter = argv
        .get(argument)
        .and_then(|word| word.get(at))
        .map_or(String::new(), |&b| {
            String::from_utf8_lossy(&[b]).into_owned()
        });
    let reason = match error {
        OptError::Colon => String::from(": in flags"),
        OptError::NotFound => format!("option not found {letter}"),
        OptError::NoArgument => format!("no argument for option {letter}"),
    };
    format!(
        "Error in argument {}, char {}: {reason}\n",
        argument + 1,
        at + 1
    )
}

/// Bytes of the command line as a word the host hands on.
#[must_use]
pub fn os(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

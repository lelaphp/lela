//! A field a request supplied, as a line of a log holds it (#4355): the
//! one escape every log the engine and its hosts keep runs such a field
//! through, the file of a diagnostic's log form here, php-fpm's access
//! log and slow log and makala's refusals through `php_sapi`.
//!
//! A line of a log is one entry, so a field that could end the line
//! could write a second entry the request made up. Each `\r` is written
//! as the two bytes `\r` and each `\n` as `\n`, and every other byte as
//! php writes it: a line that holds neither is php's own bytes, and a
//! parser of php's log formats reads every other line as before. The
//! price is that a field holding the two bytes `\` `n` reads as one that
//! held a line feed; what stays certain is that each line is one entry.
//!
//! What php itself writes into an entry, a message over several lines or
//! the stack trace after an uncaught throwable, is not a field and is
//! written as php writes it.

use std::borrow::Cow;

/// `field` as a log line holds it: each `\r` the two bytes `\r`, each
/// `\n` the two bytes `\n`, every other byte its own, and `field` itself,
/// borrowed, where it holds neither.
#[must_use]
pub fn log_field(field: &[u8]) -> Cow<'_, [u8]> {
    if !field.iter().any(|&b| b == b'\r' || b == b'\n') {
        return Cow::Borrowed(field);
    }
    let mut out = Vec::with_capacity(field.len() + 8);
    for &b in field {
        match b {
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\n' => out.extend_from_slice(b"\\n"),
            _ => out.push(b),
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::log_field;

    #[test]
    fn a_field_with_no_line_break_is_its_own_bytes_borrowed() {
        let field = b"/a.php?x=1\\n\xff\t\"";
        assert!(matches!(log_field(field), std::borrow::Cow::Borrowed(f) if f == field));
    }

    #[test]
    fn each_carriage_return_and_line_feed_is_written_as_two_bytes() {
        assert_eq!(
            &*log_field(b"a\r\nFORGED\n\rb\\"),
            &b"a\\r\\nFORGED\\n\\rb\\"[..]
        );
    }
}

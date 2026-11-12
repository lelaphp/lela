//! A value a request supplied, as a line of a log holds it (#4355): each
//! `\r` is written as the two bytes `\r` and each `\n` as `\n`, and every
//! other byte as its own.
//!
//! A line of a log is one entry, so a value that could end the line could
//! write a second entry the request made up. A value that holds neither
//! byte is written unchanged. The price is that a value holding the two
//! bytes `\` `n` reads as one that held a line feed.
//!
//! The values that go through the function: the file of a diagnostic's
//! log form, here; and through `php_sapi`, the fields of php-fpm's access
//! log, the request in its lines naming a slow request, the program and
//! the frames' files in its slow log, the path in its two lines refusing
//! a program (by its extension, or one that did not open) and in makala's
//! copies of them, and the file in the built-in server's line for a
//! request that ended in a fatal (#4559).
//!
//! The text that does not: a diagnostic's message and its stack trace,
//! the message of an uncaught throwable among them, which are written as
//! they stand, a request's value they quote included (#4592); the program
//! in php-fpm's lines naming a slow request, each byte of it outside
//! printable ASCII written as `.`; and what a host writes from a value
//! that cannot hold either byte, a method from a fixed table or a request
//! line's raw target.

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

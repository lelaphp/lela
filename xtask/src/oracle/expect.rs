//! What a test expects, and how it is matched: upstream's three forms and
//! its own translation of them at the pinned tag. `--EXPECT--` is compared
//! byte for byte after the recipe's normalisation; `--EXPECTF--` becomes a
//! regular expression by `expectf_to_regex` (run-tests.php:2856-2904) and
//! `--EXPECTREGEX--` is one already, and both are matched anchored under
//! the `s` flag (run-tests.php:2584-2600).
//!
//! All three are bytes, as run-tests.php holds them: its `preg_match` has
//! no `u` modifier, so PCRE reads the pattern and what ran a byte at a
//! time, and `.`, `%c` and `\xF2` are each one byte. This module matches
//! so by reading every byte as the character of the same value
//! ([`bytewise`]) on both sides, which the engine here, matching code
//! point by code point with ASCII classes, then matches as PCRE does; a
//! test whose expectation is not UTF-8 is held to its bytes (#1937).
//!
//! The engine behind an `--EXPECTF--` has RE2 semantics (design §8), since
//! `expectf_to_regex` writes nothing past them but `%f`'s lookahead, which
//! is written here as the same language. An `--EXPECTREGEX--` is the
//! test's own PCRE, look-around and all (`ok((?!memory leaks
//! detected).)*` in `ext/zip/tests/bug76524.phpt`), so a lane that has the
//! oracle matches it as run-tests.php does, by the pinned php's
//! `preg_match` ([`Expectation::met_under`], design §8, revision 73,
//! #1543); the RE2 engine reads it only where no oracle runs.

use std::borrow::Cow;
use std::fmt::Write as _;

use anyhow::{Context, Result, bail};
use regex_lite::Regex;

use super::{Invocation, Oracle, Program};
use crate::scratch::Tree;

/// What the pinned php runs to match a pattern as run-tests.php does
/// (run-tests.php:2592-2598), the pattern and what ran each read from
/// the file its argument names: exit 0 met, 1 not met, and 2 with what
/// PCRE said where the pattern is none it compiles.
const PREG_MATCH: &str = r#"$wanted_re = preg_replace('/\r\n/', "\n", file_get_contents($argv[1]));
$met = @preg_match('/^' . $wanted_re . '$/s', file_get_contents($argv[2]));
if ($met === false) {
    echo error_get_last()['message'] ?? preg_last_error_msg();
    exit(2);
}
exit($met === 1 ? 0 : 1);
"#;

/// The one expectation a test carries, the section's bytes as the
/// program's stdout is bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expectation {
    /// `--EXPECT--`: the bytes, exactly.
    Exact(Vec<u8>),
    /// `--EXPECTF--`: the bytes with php's wildcards in them.
    Format(Vec<u8>),
    /// `--EXPECTREGEX--`: a pattern the test wrote itself.
    Pattern(Vec<u8>),
}

impl Expectation {
    /// The section name this expectation came from.
    pub fn section(&self) -> &'static str {
        match self {
            Expectation::Exact(_) => "EXPECT",
            Expectation::Format(_) => "EXPECTF",
            Expectation::Pattern(_) => "EXPECTREGEX",
        }
    }

    /// The expectation as the file wrote it, which is what a difference is
    /// shown against.
    pub fn text(&self) -> &[u8] {
        match self {
            Expectation::Exact(body) | Expectation::Format(body) | Expectation::Pattern(body) => {
                body
            }
        }
    }

    /// Whether `actual`, already normalised, meets the expectation.
    pub fn met(&self, actual: &[u8]) -> Result<bool> {
        let Some(pattern) = self.pattern(self.text()) else {
            return Ok(actual == self.text());
        };
        let anchored = format!("(?s)^{}$", pcre_literals(&pattern));
        let regex = Regex::new(&anchored).with_context(|| {
            format!(
                "the {} section is not a pattern this engine takes: {pattern}",
                self.section()
            )
        })?;
        Ok(regex.is_match(&bytewise(actual)))
    }

    /// `text`, a section or a line of one, as the pattern this engine
    /// matches, read [`bytewise`]; none for an `--EXPECT--`.
    fn pattern(&self, text: &[u8]) -> Option<String> {
        match self {
            Expectation::Exact(_) => None,
            Expectation::Format(_) => Some(expectf_to_regex(&bytewise(text))),
            Expectation::Pattern(_) => Some(bytewise(text)),
        }
    }
}

/// Bytes as the characters of the same values, U+0000 to U+00FF, which is
/// how PCRE without the `u` modifier reads a pattern and its subject: a
/// character of either is one byte.
fn bytewise(bytes: &[u8]) -> String {
    bytes.iter().copied().map(char::from).collect()
}

/// `text` line by line, as `str::lines` splits: at each `\n`, a `\r`
/// before it dropped, and no empty line after the last.
pub(crate) fn lines(text: &[u8]) -> impl Iterator<Item = &[u8]> {
    text.split_inclusive(|&b| b == b'\n').map(|line| {
        let line = line.strip_suffix(b"\n").unwrap_or(line);
        line.strip_suffix(b"\r").unwrap_or(line)
    })
}

impl Expectation {
    /// Whether `actual`, already normalised, meets the expectation, an
    /// `--EXPECTREGEX--` matched by `oracle`'s pinned php as run-tests.php
    /// matches it, with `preg_match` (#1543); the other two as
    /// [`Expectation::met`] matches them.
    pub(crate) fn met_under(&self, oracle: &Oracle<'_>, actual: &[u8]) -> Result<bool> {
        let Expectation::Pattern(body) = self else {
            return self.met(actual);
        };
        oracle.asserted()?;
        let files = Tree::new("expectregex")?;
        let (pattern, output) = (
            files.write("pattern", body)?,
            files.write("output", actual)?,
        );
        let args = [
            pattern.to_string_lossy().into_owned(),
            output.to_string_lossy().into_owned(),
        ];
        let seen = oracle.run_under(&Invocation {
            args: &args,
            ..Invocation::new(&oracle.pin().oracle.path, Program::Snippet(PREG_MATCH))
        })?;
        match seen.exit {
            super::Ended::Status(0) => Ok(true),
            super::Ended::Status(1) => Ok(false),
            _ => bail!(
                "the EXPECTREGEX section is not a pattern PCRE takes: {}: {}",
                String::from_utf8_lossy(&seen.stdout).trim(),
                String::from_utf8_lossy(body)
            ),
        }
    }
}

/// One line of an expectation, held to one line of what ran: where a
/// test's first difference is found (phpt's miss, #1185). An
/// `--EXPECT--` line is its bytes; a line of either pattern form is its
/// own pattern, anchored at the line's ends, and its text where it is no
/// pattern this engine takes alone. Upstream matches the whole section at
/// once, and a line is not always a pattern by itself, so this says where
/// two runs part and never whether a test passes.
#[derive(Debug)]
pub(crate) struct Line {
    bytes: Vec<u8>,
    pattern: Option<Regex>,
    spans: bool,
}

impl Line {
    /// The line as the section wrote it, as words to show.
    pub(crate) fn text(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }

    /// Whether `actual`, one line of what ran, is what this line asks for.
    pub(crate) fn meets(&self, actual: &[u8]) -> bool {
        match &self.pattern {
            Some(pattern) => pattern.is_match(&bytewise(actual)),
            None => self.bytes == actual,
        }
    }

    /// Whether the line may stand for more lines than one, as `%a`, `%A`
    /// and a `%r` section of an `--EXPECTF--`, and `.*` and `.+` of an
    /// `--EXPECTREGEX--` under the `s` flag, may.
    pub(crate) fn spans(&self) -> bool {
        self.spans
    }
}

impl Expectation {
    /// The expectation line by line, each held to one line of what ran.
    pub(crate) fn lines(&self) -> Vec<Line> {
        lines(self.text())
            .map(|line| {
                let has = |words: &[&[u8]]| {
                    words
                        .iter()
                        .any(|w| line.windows(w.len()).any(|at| at == *w))
                };
                let (source, spans) = match self {
                    Expectation::Exact(_) => (None, false),
                    Expectation::Format(_) => (
                        has(&[b"%"]).then(|| self.pattern(line)).flatten(),
                        has(&[b"%a", b"%A", b"%r"]),
                    ),
                    Expectation::Pattern(_) => (self.pattern(line), has(&[b".*", b".+"])),
                };
                let pattern = source.and_then(|source| {
                    Regex::new(&format!("^(?:{})$", pcre_literals(&source))).ok()
                });
                Line {
                    bytes: line.to_vec(),
                    pattern,
                    spans,
                }
            })
            .collect()
    }
}

/// php's `expectf_to_regex` (run-tests.php:2856-2904): everything outside a
/// `%r`-delimited section is quoted, each such section is inserted as a
/// group, and the twelve wildcards are then substituted.
pub fn expectf_to_regex(wanted: &str) -> String {
    let wanted = wanted.replace("\r\n", "\n");
    let bytes = wanted.as_bytes();
    let mut quoted = String::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let (start, end) = match find(bytes, b"%r", offset) {
            // An unbalanced tag is ignored, as it is upstream.
            Some(start) => match find(bytes, b"%r", start + 2) {
                Some(end) => (start, end),
                None => (bytes.len(), bytes.len()),
            },
            None => (bytes.len(), bytes.len()),
        };
        quoted.push_str(&quoted_for_the_engine(&wanted[offset..start]));
        if end > start {
            quoted.push('(');
            quoted.push_str(&wanted[start + 2..end]);
            quoted.push(')');
        }
        offset = end + 2;
    }
    substitute(&quoted)
}

/// `text` as `--EXPECTF--` text that matches it and nothing else, where
/// the grammar has no escape of its own (run-tests.php has none): each `%`
/// that would open a wildcard or a `%r` section is written `%r[%]%r`, a
/// section whose one-character class is the `%` alone, so `100%|%s` is
/// `100%|%r[%]%rs`, which `%s` read as a wildcard would widen.
pub(crate) fn literal(text: &str) -> Cow<'_, str> {
    let opens = |rest: &str| {
        WILDCARDS
            .iter()
            .map(|(w, _)| *w)
            .chain(["%r"])
            .any(|w| rest.starts_with(w))
    };
    if !text.match_indices('%').any(|(at, _)| opens(&text[at..])) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 8);
    for (at, c) in text.char_indices() {
        match c {
            '%' if opens(&text[at..]) => out.push_str("%r[%]%r"),
            c => out.push(c),
        }
    }
    Cow::Owned(out)
}

/// The wildcards, in upstream's `strtr` table. `%e` is the directory
/// separator quoted, which on this engine's hosts is `/`.
const WILDCARDS: [(&str, &str); 12] = [
    ("%e", r"\/"),
    ("%s", r"[^\r\n]+"),
    ("%S", r"[^\r\n]*"),
    ("%a", r".+?"),
    ("%A", r".*?"),
    ("%w", r"\s*"),
    ("%i", r"[+-]?\d+"),
    ("%d", r"\d+"),
    ("%x", r"[0-9a-fA-F]+"),
    // php writes `[+-]?(?:\d+|(?=\.\d))(?:\.\d+)?(?:[Ee][+-]?\d+)?`, whose
    // lookahead is how a backtracker says "a dot must be followed by a
    // digit". The two accept the same strings: a sign, then digits with an
    // optional fraction or a fraction alone, then an optional exponent.
    ("%f", r"[+-]?(?:\d+(?:\.\d+)?|\.\d+)(?:[Ee][+-]?\d+)?"),
    ("%c", r"."),
    // php writes `\x00`; a NUL byte in the pattern is the same character
    // class of one, and needs no escape this engine must know.
    ("%0", "\0"),
];

/// `strtr` over the twelve pairs: one left-to-right pass in which a
/// replacement is never itself replaced.
fn substitute(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        if let Some((from, to)) = WILDCARDS.iter().find(|(from, _)| rest.starts_with(from)) {
            out.push_str(to);
            rest = &rest[from.len()..];
        } else {
            let width = rest
                .char_indices()
                .nth(1)
                .map_or(rest.len(), |(index, _)| index);
            out.push_str(&rest[..width]);
            rest = &rest[width..];
        }
    }
    out
}

/// php's quoting, made safe for an engine with RE2 semantics: `\<` and
/// `\>` are how php writes those two characters, and Rust's regular
/// expressions read them as the start-of-word and end-of-word assertions,
/// which match no character at all, so `[1]=>` would never match itself.
/// Written as one-character classes they are the same language. Every
/// other escape php writes means the same in both.
fn quoted_for_the_engine(text: &str) -> String {
    preg_quote(text).replace(r"\<", "[<]").replace(r"\>", "[>]")
}

/// php's `preg_quote($text, '/')`: the twenty-one characters its switch
/// names (ext/pcre/php_pcre.c:2803-2830 at the pinned tag), the delimiter,
/// and NUL as `\000`.
fn preg_quote(text: &str) -> String {
    const SPECIAL: [char; 22] = [
        '.', '\\', '+', '*', '?', '[', '^', ']', '$', '(', ')', '{', '}', '=', '!', '>', '<', '|',
        ':', '-', '#', '/',
    ];
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\0' {
            out.push_str(r"\000");
        } else if SPECIAL.contains(&c) {
            out.push('\\');
            out.push(c);
        } else {
            out.push(c);
        }
    }
    out
}

/// Two spellings PCRE reads as a literal and an RE2 engine refuses,
/// written as that engine reads the same literal: the octal escape, `\0`
/// and up to two octal digits after it, as `\x{..}` (`%r\0%r` in an
/// `--EXPECTF--` and a `\0` in an `--EXPECTREGEX--` class mean NUL to
/// php, and `preg_quote` writes NUL as `\000`), and a `{` that opens no
/// `{n}`, `{n,}` or `{n,m}` quantifier, as `\{` (`array\(2\) {`).
/// Every other escape is kept as written, an escaped backslash with it,
/// so `\\0` stays a backslash and a zero.
fn pcre_literals(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    let mut chars = pattern.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some((_, '0')) => {
                    let mut value = 0u32;
                    for _ in 0..2 {
                        match chars.peek().and_then(|(_, d)| d.to_digit(8)) {
                            Some(digit) => {
                                value = value * 8 + digit;
                                chars.next();
                            }
                            None => break,
                        }
                    }
                    write!(out, "\\x{{{value:x}}}").ok();
                }
                Some((_, next)) => {
                    out.push('\\');
                    out.push(next);
                }
                None => out.push('\\'),
            },
            '{' if !opens_quantifier(&pattern[at..]) => out.push_str("\\{"),
            _ => out.push(c),
        }
    }
    out
}

/// Whether `text`, which starts at a `{`, is a quantifier PCRE reads:
/// digits, then `}`, `,}` or `,` and digits and `}`.
fn opens_quantifier(text: &str) -> bool {
    let body = &text[1..];
    let Some(close) = body.find('}') else {
        return false;
    };
    let (low, high) = match body[..close].split_once(',') {
        Some((low, high)) => (low, Some(high)),
        None => (&body[..close], None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    digits(low) && high.is_none_or(|high| high.is_empty() || digits(high))
}

/// The offset of `needle` in `haystack` at or after `from`.
fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= haystack.len() {
        return None;
    }
    haystack[from..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|at| at + from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_exact_expectation_is_the_bytes() {
        let expectation = Expectation::Exact(b"int(1)".to_vec());
        assert!(expectation.met(b"int(1)").unwrap());
        assert!(!expectation.met(b"int(2)").unwrap());
        // Bytes that are no encoding at all are compared as they stand.
        let latin = Expectation::Exact(vec![0xe1, 0xe9]);
        assert!(latin.met(&[0xe1, 0xe9]).unwrap());
        assert!(!latin.met(&[0xe1, 0xea]).unwrap());
    }

    #[test]
    fn a_pattern_is_matched_over_bytes_as_pcre_without_the_u_modifier_matches() {
        // Windows-1251 bytes are not passed by the replacement characters
        // a lossy reading of either side would make of them.
        let cyrillic = Expectation::Format(b"\xF2\xE5".to_vec());
        assert!(!cyrillic.met(b"\xEF\xBF\xBD\xEF\xBF\xBD").unwrap());
        assert!(cyrillic.met(b"\xF2\xE5").unwrap());
        // `%c` is one byte, so a two-byte character is two of them.
        let two = Expectation::Format(b"%c%c".to_vec());
        assert!(two.met("\u{e9}".as_bytes()).unwrap());
        assert!(
            !Expectation::Format(b"%c".to_vec())
                .met("\u{e9}".as_bytes())
                .unwrap()
        );
        // A pattern's `\xF2` is the byte, and a UTF-8 literal its bytes.
        assert!(
            Expectation::Pattern(br"\xF2.".to_vec())
                .met(b"\xF2\xE5")
                .unwrap()
        );
        assert!(
            Expectation::Pattern("\u{e9}%".into())
                .met("\u{e9}%".as_bytes())
                .unwrap()
        );
        // Lines split as `str::lines` splits them.
        let split = |text: &'static [u8]| lines(text).collect::<Vec<_>>();
        assert_eq!(split(b"a\r\n\nb\n"), [&b"a"[..], b"", b"b"]);
        assert_eq!(split(b"\n"), [&b""[..]]);
        assert!(split(b"").is_empty());
    }

    #[test]
    fn the_wildcards_are_php_s() {
        assert_eq!(expectf_to_regex("%s"), r"[^\r\n]+");
        assert_eq!(expectf_to_regex("%d"), r"\d+");
        assert_eq!(expectf_to_regex("a.b"), r"a\.b");
        assert_eq!(expectf_to_regex("%e"), r"\/");
        assert_eq!(
            expectf_to_regex("in %s on line %d"),
            r"in [^\r\n]+ on line \d+"
        );
    }

    /// A literal reads as the text it was written from and nothing wider:
    /// `%s` would meet `AB` where the oracle printed `%s`.
    #[test]
    fn a_literal_meets_its_text_alone() {
        let printed = "100%|%s|%d|%r|%%|50% off";
        let written = super::literal(printed);
        assert_eq!(written, "100%|%r[%]%rs|%r[%]%rd|%r[%]%rr|%%|50% off");
        let met = |actual: &str| {
            Expectation::Format(written.as_bytes().to_vec())
                .met(actual.as_bytes())
                .unwrap()
        };
        assert!(met(printed));
        assert!(!met("100%|AB|7|%r|%%|50% off"));
        assert_eq!(super::literal("no wildcard"), "no wildcard");
    }

    #[test]
    fn a_percent_r_section_is_a_group_and_is_not_quoted() {
        assert_eq!(expectf_to_regex("a%r(b|c)%rd"), "a((b|c))d");
        // Unbalanced: upstream quotes the rest, the tag with it.
        assert_eq!(expectf_to_regex("a%rb"), "a%rb");
    }

    #[test]
    fn every_wildcard_compiles_and_matches_what_php_says_it_does() {
        let cases = [
            ("%s", "one line", true),
            ("%s", "", false),
            ("%S", "", true),
            ("%a", "two\nlines", true),
            ("%A", "", true),
            ("%w", "   ", true),
            ("%i", "-12", true),
            ("%d", "12", true),
            ("%d", "-12", false),
            ("%x", "beef01", true),
            ("%f", "-1.5e3", true),
            ("%c", "z", true),
            ("%0", "\0", true),
            ("%e", "/", true),
        ];
        for (wanted, actual, met) in cases {
            let expectation = Expectation::Format(wanted.into());
            assert_eq!(
                expectation.met(actual.as_bytes()).unwrap(),
                met,
                "{wanted} against {actual:?}"
            );
        }
    }

    #[test]
    fn a_format_expectation_is_anchored_and_dots_span_lines() {
        let expectation = Expectation::Format("Fatal error: %s in %s on line %d".into());
        assert!(
            expectation
                .met(b"Fatal error: nope in /tmp/x.php on line 3")
                .unwrap()
        );
        assert!(
            !expectation
                .met(b"before\nFatal error: nope in /tmp/x.php on line 3")
                .unwrap()
        );
        assert!(Expectation::Format("a%Ab".into()).met(b"a\n\nb").unwrap());
    }

    #[test]
    fn a_pattern_expectation_is_the_test_s_own() {
        let expectation = Expectation::Pattern("int\\([0-9]+\\)".into());
        assert!(expectation.met(b"int(12)").unwrap());
        assert!(!expectation.met(b"int(x)").unwrap());
    }

    #[test]
    fn a_pattern_this_engine_cannot_take_is_an_error_naming_the_section() {
        let error = Expectation::Pattern(r"(a)\1".into())
            .met(b"aa")
            .unwrap_err()
            .to_string();
        assert!(error.contains("EXPECTREGEX"), "{error}");
    }

    #[test]
    fn the_two_escapes_an_re2_engine_reads_as_assertions_are_written_as_classes() {
        assert_eq!(quoted_for_the_engine("[1]=>"), r"\[1\]\=[>]");
        assert_eq!(quoted_for_the_engine("<a>"), "[<]a[>]");
        let arrow = Expectation::Format("  [1]=>\n  string(3) \"abc\"".into());
        assert!(arrow.met(b"  [1]=>\n  string(3) \"abc\"").unwrap());
    }

    #[test]
    fn pcre_s_literals_are_written_as_this_engine_reads_them() {
        assert_eq!(pcre_literals(r"a\0b\000c\012"), r"a\x{0}b\x{0}c\x{a}");
        assert_eq!(pcre_literals(r"\\0 \d"), r"\\0 \d");
        assert_eq!(pcre_literals(r"array\(2\) {"), r"array\(2\) \{");
        assert_eq!(pcre_literals(r"a{2}b{1,}c{1,3}\{"), r"a{2}b{1,}c{1,3}\{");
        assert!(
            Expectation::Pattern("array\\(2\\) {\n}".into())
                .met(b"array(2) {\n}")
                .unwrap()
        );
        let format = Expectation::Format("string(9) \"%r\\0%rA%r\\0%rb\"".into());
        assert!(format.met(b"string(9) \"\0A\0b\"").unwrap());
        let pattern = Expectation::Pattern(r#"string\([0-9]*\) "[#\0]*""#.into());
        assert!(pattern.met(b"string(1) \"\0\"").unwrap());
        assert!(Expectation::Format("a\0b".into()).met(b"a\0b").unwrap());
    }

    #[test]
    fn preg_quote_is_php_s_switch() {
        assert_eq!(
            preg_quote(r".\+*?[^]$(){}=!><|:-#/"),
            r"\.\\\+\*\?\[\^\]\$\(\)\{\}\=\!\>\<\|\:\-\#\/"
        );
        assert_eq!(preg_quote("a b,c"), "a b,c");
        assert_eq!(preg_quote("%s"), "%s");
        assert_eq!(preg_quote("\0"), r"\000");
    }
}

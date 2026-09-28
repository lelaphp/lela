//! What a test expects, and how it is matched: upstream's three forms and
//! its own translation of them at the pinned tag. `--EXPECT--` is compared
//! byte for byte after the recipe's normalisation; `--EXPECTF--` becomes a
//! regular expression by `expectf_to_regex` (run-tests.php:2856-2904) and
//! `--EXPECTREGEX--` is one already, and both are matched anchored under
//! the `s` flag (run-tests.php:2584-2600).
//!
//! The engine behind the two patterns has RE2 semantics, as the design
//! says (design §8): the one corpus body that needs PCRE-only syntax is a
//! row in the divergence registry, not a reason to carry a backtracker.

use std::fmt::Write as _;

use anyhow::{Context, Result};
use regex_lite::Regex;

/// The one expectation a test carries. `--EXPECT--` is bytes, as the
/// program's stdout is; the two pattern forms are words, and a byte of
/// that stdout that is no character reaches the match as the replacement
/// character.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expectation {
    /// `--EXPECT--`: the bytes, exactly.
    Exact(Vec<u8>),
    /// `--EXPECTF--`: the bytes with php's wildcards in them.
    Format(String),
    /// `--EXPECTREGEX--`: a pattern the test wrote itself.
    Pattern(String),
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
            Expectation::Exact(body) => body,
            Expectation::Format(body) | Expectation::Pattern(body) => body.as_bytes(),
        }
    }

    /// Whether `actual`, already normalised, meets the expectation.
    pub fn met(&self, actual: &[u8]) -> Result<bool> {
        let pattern = match self {
            Expectation::Exact(body) => return Ok(actual == body),
            Expectation::Format(body) => expectf_to_regex(body),
            Expectation::Pattern(body) => body.clone(),
        };
        let anchored = format!("(?s)^{}$", pcre_literals(&pattern));
        let regex = Regex::new(&anchored).with_context(|| {
            format!(
                "the {} section is not a pattern this engine takes: {pattern}",
                self.section()
            )
        })?;
        Ok(regex.is_match(&String::from_utf8_lossy(actual)))
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
    text: String,
    pattern: Option<Regex>,
    spans: bool,
}

impl Line {
    /// The line as the section wrote it.
    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    /// Whether `actual`, one line of what ran, is what this line asks for.
    pub(crate) fn meets(&self, actual: &str) -> bool {
        match &self.pattern {
            Some(pattern) => pattern.is_match(actual),
            None => self.text == actual,
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
        let text = String::from_utf8_lossy(self.text());
        text.lines()
            .map(|line| {
                let (source, spans) = match self {
                    Expectation::Exact(_) => (None, false),
                    Expectation::Format(_) => (
                        line.contains('%').then(|| expectf_to_regex(line)),
                        ["%a", "%A", "%r"].iter().any(|w| line.contains(w)),
                    ),
                    Expectation::Pattern(_) => (
                        Some(line.to_owned()),
                        line.contains(".*") || line.contains(".+"),
                    ),
                };
                let pattern = source.and_then(|source| {
                    Regex::new(&format!("^(?:{})$", pcre_literals(&source))).ok()
                });
                Line {
                    text: line.to_owned(),
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
                    let _ = write!(out, "\\x{{{value:x}}}");
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
            let expectation = Expectation::Format(wanted.to_owned());
            assert_eq!(
                expectation.met(actual.as_bytes()).unwrap(),
                met,
                "{wanted} against {actual:?}"
            );
        }
    }

    #[test]
    fn a_format_expectation_is_anchored_and_dots_span_lines() {
        let expectation = Expectation::Format("Fatal error: %s in %s on line %d".to_owned());
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
        assert!(
            Expectation::Format("a%Ab".to_owned())
                .met(b"a\n\nb")
                .unwrap()
        );
    }

    #[test]
    fn a_pattern_expectation_is_the_test_s_own() {
        let expectation = Expectation::Pattern("int\\([0-9]+\\)".to_owned());
        assert!(expectation.met(b"int(12)").unwrap());
        assert!(!expectation.met(b"int(x)").unwrap());
    }

    #[test]
    fn a_pattern_this_engine_cannot_take_is_an_error_naming_the_section() {
        let error = Expectation::Pattern(r"(a)\1".to_owned())
            .met(b"aa")
            .unwrap_err()
            .to_string();
        assert!(error.contains("EXPECTREGEX"), "{error}");
    }

    #[test]
    fn the_two_escapes_an_re2_engine_reads_as_assertions_are_written_as_classes() {
        assert_eq!(quoted_for_the_engine("[1]=>"), r"\[1\]\=[>]");
        assert_eq!(quoted_for_the_engine("<a>"), "[<]a[>]");
        let arrow = Expectation::Format("  [1]=>\n  string(3) \"abc\"".to_owned());
        assert!(arrow.met(b"  [1]=>\n  string(3) \"abc\"").unwrap());
    }

    #[test]
    fn pcre_s_literals_are_written_as_this_engine_reads_them() {
        assert_eq!(pcre_literals(r"a\0b\000c\012"), r"a\x{0}b\x{0}c\x{a}");
        assert_eq!(pcre_literals(r"\\0 \d"), r"\\0 \d");
        assert_eq!(pcre_literals(r"array\(2\) {"), r"array\(2\) \{");
        assert_eq!(pcre_literals(r"a{2}b{1,}c{1,3}\{"), r"a{2}b{1,}c{1,3}\{");
        assert!(
            Expectation::Pattern("array\\(2\\) {\n}".to_owned())
                .met(b"array(2) {\n}")
                .unwrap()
        );
        let format = Expectation::Format("string(9) \"%r\\0%rA%r\\0%rb\"".to_owned());
        assert!(format.met(b"string(9) \"\0A\0b\"").unwrap());
        let pattern = Expectation::Pattern(r#"string\([0-9]*\) "[#\0]*""#.to_owned());
        assert!(pattern.met(b"string(1) \"\0\"").unwrap());
        assert!(Expectation::Format("a\0b".to_owned()).met(b"a\0b").unwrap());
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

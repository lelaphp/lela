//! Text, span, line and level assertions for every diagnostic the
//! scanner raises on its own: an unterminated comment at EOF, a legacy
//! octal number with a bad digit, a malformed `\u{…}` escape, an octal
//! escape past `\377`, heredoc indentation, the `(real)` cast and a
//! non-canonical cast spelling. What php's scanner hands its parser as a
//! token, a stray byte, an unterminated string or the identifier after a
//! malformed number, raises nothing here: the token is the parser's to
//! name, and the parser's tests and the recovery fixtures hold that
//! text.

use super::*;
use crate::lexer::{LexOptions, lex};
use crate::source::{Diagnostic, Level};

fn lex_diags(input: &[u8]) -> Vec<Diagnostic> {
    let lexed = lex(F, input, &LexOptions::default());
    pairs(&lexed, input);
    lexed.diagnostics
}

/// One diagnostic whose text starts as given, at (start, len).
#[track_caller]
fn assert_one(ds: &[Diagnostic], text_starts: &str, start: u32, len: u32) {
    assert_eq!(ds.len(), 1, "expected one diagnostic, got {ds:?}");
    assert!(
        ds[0].message.starts_with(text_starts),
        "expected {text_starts:?}, got {:?}",
        ds[0].message
    );
    assert_eq!((ds[0].span.start, ds[0].span.len), (start, len));
}

const INVALID_ESCAPE: &str = "Invalid UTF-8 codepoint escape sequence";
const MIXED: &str = "Invalid indentation - tabs and spaces cannot be mixed";

fn under_indented(width: usize) -> String {
    format!("Invalid body indentation level (expecting an indentation level of at least {width})")
}

#[test]
fn stray_bytes_are_one_error_token_per_span_and_no_diagnostic() {
    // Zend's `{ANY_CHAR}` hands each back as `T_BAD_CHARACTER`; the
    // parser names the first byte of the span.
    check_bytes(
        b"<?php \x00\x01\x02;",
        &[
            (OPEN_TAG, b"<?php "),
            (ERROR, b"\x00\x01\x02"),
            (SEMICOLON, b";"),
        ],
    );
    assert_eq!(diags(b"<?php \x00\x01\x02;"), vec![]);
    assert_eq!(diags(b"<?php \x00;\x7F;"), vec![]);
}

#[test]
fn unterminated_strings_are_the_parsers() {
    // php returns the rest of the file as `T_ENCAPSED_AND_WHITESPACE`
    // for an unclosed `'`, the quote included, and its parser says
    // `unexpected string content "abc"`; an unclosed `"`, backtick,
    // heredoc or nowdoc reaches its parser as an unclosed run, and the
    // parser's `unexpected end of file` carries the clause.
    check("<?php 'abc", &[(OPEN_TAG, "<?php "), (STRING_TEXT, "'abc")]);
    check(
        "<?php b'abc",
        &[(OPEN_TAG, "<?php "), (STRING_TEXT, "b'abc")],
    );
    for src in [
        &b"<?php 'abc"[..],
        b"<?php \"abc",
        b"<?php b\"abc",
        b"<?php `cmd",
        b"<?php <<<EOT\nbody",
        b"<?php <<<EOT\n",
        b"<?php <<<'EOT'\nbody\nEOT2",
        b"<?php \"$a[",
    ] {
        assert_eq!(diags(src), vec![], "{src:?}");
    }
}

#[test]
fn removed_and_deprecated_casts() {
    // php's scanner throws on `(real)` as it scans it, on the token's
    // line; `(unset)` is the parser's, since php's compiler refuses it.
    let ds = lex_diags(b"<?php\n( Real )$x; (unset)$y;");
    assert_eq!(ds.len(), 1);
    assert_eq!(
        ds[0].message,
        "The (real) cast has been removed, use (float) instead"
    );
    assert_eq!((ds[0].span.start, ds[0].span.len, ds[0].line), (6, 8, 2));
    assert_eq!(ds[0].level, Level::Parse);
    // The four spellings 8.5 deprecates, each naming its canonical one.
    let ds = lex_diags(b"<?php (integer)$a; (double)$b; (binary)$c; (boolean)$d; (int)$e;");
    let texts: Vec<&str> = ds.iter().map(|d| d.message.as_str()).collect();
    assert_eq!(
        texts,
        [
            "Non-canonical cast (integer) is deprecated, use the (int) cast instead",
            "Non-canonical cast (double) is deprecated, use the (float) cast instead",
            "Non-canonical cast (binary) is deprecated, use the (string) cast instead",
            "Non-canonical cast (boolean) is deprecated, use the (bool) cast instead",
        ]
    );
    assert!(ds.iter().all(|d| d.level == Level::Deprecated));
}

#[test]
fn octal_escape_overflow_warns_per_escape_on_its_line() {
    // Three octal digits past `\377` are one escape and one warning,
    // reported on the escape's own line; `\4000` is `\400` and a `0`;
    // `\377` and two-digit escapes are silent, as is a single-quoted
    // string.
    let ds = lex_diags(b"<?php \"\\777\n\\4000\\377\\40\"; '\\777'; `\\400`;");
    let seen: Vec<(&str, u32, Level)> = ds
        .iter()
        .map(|d| (d.message.as_str(), d.line, d.level))
        .collect();
    assert_eq!(
        seen,
        [
            (
                "Octal escape sequence overflow \\777 is greater than \\377",
                1,
                Level::CompileWarning
            ),
            (
                "Octal escape sequence overflow \\400 is greater than \\377",
                2,
                Level::CompileWarning
            ),
            (
                "Octal escape sequence overflow \\400 is greater than \\377",
                2,
                Level::CompileWarning
            ),
        ]
    );
    // A heredoc body is held to the same rule.
    let ds = lex_diags(b"<?php <<<E\n\n\\777\nE;");
    assert_eq!(ds.len(), 1);
    assert_eq!(ds[0].line, 3);
}

#[test]
fn unterminated_block_comment() {
    // php's text names the line the comment opened on, and so does the
    // line it reports, whatever line the input ends on.
    assert_one(
        &lex_diags(b"<?php /* x"),
        "Unterminated comment starting line 1",
        10,
        0,
    );
    let ds = lex_diags(b"<?php\n\n/* x\n\n");
    assert_one(&ds, "Unterminated comment starting line 3", 13, 0);
    assert_eq!(ds[0].line, 3);
}

#[test]
fn legacy_octal_bad_digit() {
    // The span pinpoints the first bad digit; the token is still one INT.
    assert_eq!(
        diags(b"<?php 08;"),
        vec![("Invalid numeric literal".into(), 7, 1)]
    );
    assert_eq!(
        diags(b"<?php 0779;"),
        vec![("Invalid numeric literal".into(), 9, 1)]
    );
    // A float with a leading zero is not octal.
    assert_eq!(diags(b"<?php 08.5; 09e2;"), vec![]);
}

#[test]
fn malformed_unicode_escape() {
    // Empty braces: the span covers `\u{}`.
    assert_eq!(
        diags(b"<?php \"\\u{}\";"),
        vec![(INVALID_ESCAPE.into(), 7, 4)]
    );
    // Non-hex content: the span covers `\u{` (the failure point).
    assert_eq!(
        diags(b"<?php \"\\u{zz}\";"),
        vec![(INVALID_ESCAPE.into(), 7, 3)]
    );
    // Out of range: the span covers the whole escape, and php adds a tail.
    assert_eq!(
        diags(b"<?php \"\\u{110000}\";"),
        vec![(format!("{INVALID_ESCAPE}: Codepoint too large"), 7, 10)]
    );
    // Well-formed: silent. `\u` without `{` is literal text: silent.
    assert_eq!(diags(b"<?php \"\\u{10FFFF}\\u x\";"), vec![]);
    // Enough hex digits to saturate u32: the accumulator must not
    // overflow past the saturation point. The unterminated variant also
    // exercises the no-closer span path, and php judges the shape before
    // the size (`zend_scan_escape_string` walks to the `}` first), so it
    // is malformed and not too large (probed 8.5.9), as is a run of hex
    // digits that a byte other than `}` ends.
    assert_eq!(
        diags(b"<?php \"\\u{FFFFFFFFFF}\";"),
        vec![(format!("{INVALID_ESCAPE}: Codepoint too large"), 7, 14)]
    );
    assert_eq!(
        diags(b"<?php \"\\u{1F60aF60aF60a\";"),
        vec![(INVALID_ESCAPE.into(), 7, 16)]
    );
    assert_eq!(
        diags(b"<?php \"\\u{FFFFFFFFzz}\";"),
        vec![(INVALID_ESCAPE.into(), 7, 11)]
    );
    // A heredoc is held to the same rule.
    assert_eq!(
        diags(b"<?php <<<E\n\\u{}\nE;"),
        vec![(INVALID_ESCAPE.into(), 11, 4)]
    );
}

#[test]
fn underindented_body_line() {
    // The closing marker indented 2, the body line indented 1: the span is
    // the line's (short) whitespace prefix.
    assert_eq!(
        diags(b"<?php <<<E\n a\n  E;"),
        vec![(under_indented(2), 11, 1)]
    );
    // Content at column zero: a zero-length prefix span at the line start.
    assert_eq!(
        diags(b"<?php <<<E\nx\n  E;"),
        vec![(under_indented(2), 11, 0)]
    );
    // One diagnostic per offending line.
    assert_eq!(
        diags(b"<?php <<<E\nx\ny\n  E;"),
        vec![(under_indented(2), 11, 0), (under_indented(2), 13, 0)]
    );
    // A whitespace-only short line is exempt (php-src's rule).
    assert_eq!(diags(b"<?php <<<E\n \n  E;"), vec![]);
}

#[test]
fn body_mixes_tabs_and_spaces() {
    // The closing marker uses spaces; the body line's tab is the wrong
    // class, and the mix fires before the under-indentation rule, as
    // Zend's per-character walk does.
    assert_eq!(diags(b"<?php <<<E\n\tx\n  E;"), vec![(MIXED.into(), 11, 1)]);
    // Equal width, wrong class.
    assert_eq!(
        diags(b"<?php <<<E\n  x\n\t\tE;"),
        vec![(MIXED.into(), 11, 2)]
    );
}

#[test]
fn closing_marker_mixes_tabs_and_spaces() {
    // ` \t` on the closing line itself: the span is the marker's
    // indentation, and the line php reports is the one after the
    // opener, wherever the marker stands (probed 8.5.9).
    assert_eq!(
        diags(b"<?php <<<E\n \tx\n \tE;"),
        vec![(MIXED.into(), 15, 2)]
    );
    let ds = lex_diags(b"<?php <<<E\n a\n\n \tE;");
    assert_eq!(ds.len(), 1);
    assert_eq!(ds[0].line, 2);
}

#[test]
fn under_indentation_not_fooled_by_nested_heredoc_closers() {
    // A nested heredoc (same label!) inside `${…}` interpolation must not
    // donate its closer's indentation to the outer heredoc: php-src
    // flexible-heredoc-complex-test2/4.
    let src = b"<?php\nvar_dump(<<<D\n  x\n  ${<<<D\n    y\n    D}\n  }\n  c\n  D);\n";
    let found = diags(src);
    assert!(
        found
            .iter()
            .all(|(text, _, _)| !text.starts_with("Invalid body indentation")),
        "no under-indentation false positives: {found:?}"
    );
}

//! Text and span assertions for every lexical diagnostic: an unexpected
//! byte, an unterminated string, heredoc or comment at EOF, a malformed
//! number, a malformed `\u{…}` escape, and heredoc indentation.

use super::*;
use crate::lexer::{LexOptions, lex};
use crate::source::Diagnostic;

fn lex_diags(input: &[u8]) -> Vec<Diagnostic> {
    let lexed = lex(F, input, &LexOptions::default());
    let _ = pairs(&lexed, input);
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

const UNTERMINATED_STRING: &str = "string runs to end of file without a closing delimiter";
const UNTERMINATED_HEREDOC: &str = "syntax error, unexpected end of file, expecting variable \
                                    or heredoc end or \"${\" or \"{$\"";
const INVALID_ESCAPE: &str = "Invalid UTF-8 codepoint escape sequence";
const MIXED: &str = "Invalid indentation - tabs and spaces cannot be mixed";

fn unexpected_char(byte: u8) -> String {
    format!("syntax error, unexpected character 0x{byte:02X}")
}

fn unexpected_ident(id: &str) -> String {
    format!("syntax error, unexpected identifier \"{id}\"")
}

fn under_indented(width: usize) -> String {
    format!("Invalid body indentation level (expecting an indentation level of at least {width})")
}

#[test]
fn unexpected_bytes_one_per_span() {
    assert_eq!(
        diags(b"<?php \x00\x01\x02;"),
        vec![(unexpected_char(0x00), 6, 3)]
    );
    // Two separate spans: two diagnostics.
    assert_eq!(
        diags(b"<?php \x00;\x7F;"),
        vec![(unexpected_char(0x00), 6, 1), (unexpected_char(0x7F), 8, 1)]
    );
}

#[test]
fn unterminated_single_quoted() {
    assert_one(&lex_diags(b"<?php 'abc"), UNTERMINATED_STRING, 10, 0);
}

#[test]
fn unterminated_double_quoted() {
    assert_one(&lex_diags(b"<?php \"abc"), UNTERMINATED_STRING, 10, 0);
}

#[test]
fn unterminated_b_prefixed() {
    assert_one(&lex_diags(b"<?php b\"abc"), UNTERMINATED_STRING, 11, 0);
}

#[test]
fn unterminated_backquote() {
    assert_one(&lex_diags(b"<?php `cmd"), UNTERMINATED_STRING, 10, 0);
}

#[test]
fn unterminated_heredoc() {
    assert_one(
        &lex_diags(b"<?php <<<EOT\nbody"),
        UNTERMINATED_HEREDOC,
        17,
        0,
    );
    // A body of zero bytes is php's bare `unexpected end of file`.
    let ds = lex_diags(b"<?php <<<EOT\n");
    assert_eq!(ds.len(), 1);
    assert_eq!(ds[0].message, "syntax error, unexpected end of file");
}

#[test]
fn unterminated_nowdoc() {
    // `EOT2` goes on with a label character, so it does not terminate.
    assert_one(
        &lex_diags(b"<?php <<<'EOT'\nbody\nEOT2"),
        UNTERMINATED_HEREDOC,
        24,
        0,
    );
}

#[test]
fn unterminated_block_comment() {
    // php's text names the line the comment opened on.
    assert_one(
        &lex_diags(b"<?php /* x"),
        "Unterminated comment starting line 1",
        10,
        0,
    );
    assert_one(
        &lex_diags(b"<?php\n\n/* x"),
        "Unterminated comment starting line 3",
        11,
        0,
    );
}

#[test]
fn bare_radix_prefixes() {
    assert_eq!(diags(b"<?php 0x;"), vec![(unexpected_ident("x"), 6, 2)]);
    assert_eq!(diags(b"<?php 0b;"), vec![(unexpected_ident("b"), 6, 2)]);
    assert_eq!(diags(b"<?php 0o;"), vec![(unexpected_ident("o"), 6, 2)]);
    // `0x_` consumes the underscore into the token span.
    assert_eq!(diags(b"<?php 0x_;"), vec![(unexpected_ident("x_"), 6, 3)]);
}

#[test]
fn underscore_placement() {
    // The span pinpoints the offending underscore byte; the text is the
    // identifier php's parser reports, which begins there.
    assert_eq!(diags(b"<?php 1_;"), vec![(unexpected_ident("_"), 7, 1)]);
    assert_eq!(diags(b"<?php 1__2;"), vec![(unexpected_ident("__2"), 7, 1)]);
    assert_eq!(diags(b"<?php 1_.5;"), vec![(unexpected_ident("_"), 7, 1)]);
    assert_eq!(diags(b"<?php 1._5;"), vec![(unexpected_ident("_5"), 8, 1)]);
    assert_eq!(diags(b"<?php 0x1_;"), vec![(unexpected_ident("_"), 9, 1)]);
    assert_eq!(
        diags(b"<?php 0x_FF;"),
        vec![(unexpected_ident("x_FF"), 8, 1)]
    );
    // One diagnostic per token, even with several violations.
    assert_eq!(
        diags(b"<?php 1__2__3;"),
        vec![(unexpected_ident("__2__3"), 7, 1)]
    );
    // Legal placements: silent.
    assert_eq!(diags(b"<?php 1_2 + 0x1_2 + 1_2.3_4e5_6;"), vec![]);
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
    // exercises the no-closer span path.
    assert_eq!(
        diags(b"<?php \"\\u{FFFFFFFFFF}\";"),
        vec![(format!("{INVALID_ESCAPE}: Codepoint too large"), 7, 14)]
    );
    assert_eq!(
        diags(b"<?php \"\\u{1F60aF60aF60a\";"),
        vec![(format!("{INVALID_ESCAPE}: Codepoint too large"), 7, 16)]
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
    // ` \t` on the closing line itself.
    assert_eq!(
        diags(b"<?php <<<E\n \tx\n \tE;"),
        vec![(MIXED.into(), 15, 2)]
    );
}

#[test]
fn unterminated_string_then_eof_inside_interpolation_states() {
    // EOF while still inside `$a[` (VarOffset above DoubleQuoted): the
    // string diagnostic still fires for the enclosing string state.
    assert_one(&lex_diags(b"<?php \"$a["), UNTERMINATED_STRING, 10, 0);
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

//! The lexer's tests, one group per lexical construct. Every assertion goes
//! through [`check`] and [`check_bytes`], which hold the coverage invariant:
//! the tokens are contiguous and their lengths sum to the input length.

mod diagnostics;
mod halt;
mod heredoc;
mod numbers;
mod php_names;
mod scripting;
mod strings;
mod tags;

pub(crate) use crate::SyntaxKind;
pub(crate) use crate::SyntaxKind::*;

use crate::source::FileId;

use super::{LexOptions, Lexed, Position, lex};

pub(crate) const F: FileId = FileId(0);

pub(crate) fn no_short_tags() -> LexOptions {
    LexOptions {
        short_open_tag: false,
        ..LexOptions::default()
    }
}

/// Splits the tokens back into (kind, text) pairs, asserting coverage.
pub(crate) fn pairs<'a>(lexed: &Lexed, input: &'a [u8]) -> Vec<(SyntaxKind, &'a [u8])> {
    let total: usize = lexed.tokens.iter().map(|t| t.len as usize).sum();
    assert_eq!(
        total,
        input.len(),
        "coverage: token lengths must sum to the input length"
    );
    let mut off = 0usize;
    lexed
        .tokens
        .iter()
        .map(|t| {
            assert_eq!(
                t.offset as usize, off,
                "token offset is the sum of the lengths before it"
            );
            let text = &input[off..off + t.len as usize];
            off += t.len as usize;
            (t.kind, text)
        })
        .collect()
}

#[track_caller]
pub(crate) fn check_opts(input: &str, options: &LexOptions, expected: &[(SyntaxKind, &str)]) {
    let lexed = lex(F, input.as_bytes(), options);
    let actual: Vec<(SyntaxKind, &str)> = pairs(&lexed, input.as_bytes())
        .into_iter()
        .map(|(k, t)| (k, std::str::from_utf8(t).unwrap_or("<not utf-8>")))
        .collect();
    let expected: Vec<(SyntaxKind, &str)> = expected.to_vec();
    assert_eq!(actual, expected, "token mismatch for {input:?}");
}

/// The standard assertion: the exact (kind, text) sequence under default options.
#[track_caller]
pub(crate) fn check(input: &str, expected: &[(SyntaxKind, &str)]) {
    check_opts(input, &LexOptions::default(), expected);
}

/// [`check`] over `body` after `<?php `, the tag's token taken as read,
/// for the fixtures that do not care about the tag.
#[track_caller]
pub(crate) fn check_php(body: &str, expected: &[(SyntaxKind, &str)]) {
    let input = format!("<?php {body}");
    let full = [&[(OPEN_TAG, "<?php ")], expected].concat();
    check(&input, &full);
}

/// Byte-input variant for non-UTF-8 fixtures (BOM, NUL, binary trailers).
#[track_caller]
pub(crate) fn check_bytes(input: &[u8], expected: &[(SyntaxKind, &[u8])]) {
    let lexed = lex(F, input, &LexOptions::default());
    let actual = pairs(&lexed, input);
    assert_eq!(actual, expected, "token mismatch for {input:?}");
}

/// Diagnostics as (message, span start, span len), in emission order.
/// Coverage is asserted here too, so a diagnostics-only test keeps the
/// invariant honest.
pub(crate) fn diags(input: &[u8]) -> Vec<(String, u32, u32)> {
    let lexed = lex(F, input, &LexOptions::default());
    pairs(&lexed, input);
    lexed
        .diagnostics
        .iter()
        .map(|d| (d.message.clone(), d.span.start, d.span.len))
        .collect()
}

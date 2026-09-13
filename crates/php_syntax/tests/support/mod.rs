//! The fixture harness of the parser's snapshot suite (design §4.9): every
//! fixture goes through [`parse_fixture`], which parses the source through
//! the real entry and renders the arena's dump, then every promise the
//! grammar broke as a `REQUIRED-VIOLATION:` line, then the diagnostics one
//! per line as `error@start..end message`. For every fixture, on every
//! recovery path, it also holds the token stream lossless: the token texts
//! concatenate to the source.
//!
//! Each fixture file is regenerated once against the dump, the whitespace
//! and comment lines of the inherited snapshots gone since the tree holds
//! significant tokens only; a mis-parse a snapshot pins is named beside
//! the fixture.

use std::fmt::Write as _;

/// Parse `src` and render the dump, the broken promises and the
/// diagnostics.
pub fn parse_fixture(src: &str) -> String {
    let parsed = php_syntax::parse(src.as_bytes());

    // Lossless on every path, every recovery path included.
    let mut text = Vec::new();
    for token in &parsed.tokens {
        text.extend_from_slice(
            &parsed.source[token.offset as usize..(token.offset + token.len) as usize],
        );
    }
    assert_eq!(
        text,
        src.as_bytes(),
        "the token stream is lossless for fixture:\n{src}"
    );

    let mut out = parsed.dump();
    // A required child the grammar did not supply is a parser defect;
    // rendered into the snapshot so a person reads it rather than a
    // panic.
    for missing in &parsed.missing {
        let _ = writeln!(
            out,
            "REQUIRED-VIOLATION: {} on {}..{}",
            missing.field,
            missing.range.start,
            missing.range.end()
        );
    }
    for d in &parsed.diagnostics {
        let _ = writeln!(
            out,
            "error@{}..{} {}",
            d.span.start,
            d.span.end(),
            d.message
        );
    }
    out
}

/// The snapshot assertion: `expect` holds the [`parse_fixture`] rendering.
// expect-test's `Expect` is a by-value token by convention; a reference
// would put `&expect![[…]]` at every fixture.
#[allow(clippy::needless_pass_by_value)]
pub fn check(src: &str, expect: expect_test::Expect) {
    expect.assert_eq(&parse_fixture(src));
}

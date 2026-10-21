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

// Each test binary includes this module and reads the part it needs.
#![allow(dead_code)]

use std::fmt::Write as _;

/// Parse `src` through the real entry and hold the token stream
/// lossless on the way: the token texts concatenate to the source, on
/// every recovery path included.
pub fn parse_lossless(src: impl AsRef<[u8]>) -> php_syntax::Parsed<'static> {
    let src = src.as_ref();
    // The parse is the fixture's for the rest of the test: its arena,
    // and the source in it, are kept.
    let arena: &'static php_syntax::Arena = Box::leak(Box::new(php_syntax::Arena::new()));
    let source: &'static [u8] = Box::leak(src.into());
    let parsed = php_syntax::parse(arena, source, &php_syntax::Options::default());
    let text: Vec<u8> = parsed
        .tokens
        .iter()
        .flat_map(|token| {
            &parsed.source[token.offset as usize..(token.offset + token.len) as usize]
        })
        .copied()
        .collect();
    assert_eq!(
        text,
        src,
        "the token stream is lossless for fixture:\n{}",
        String::from_utf8_lossy(src)
    );
    parsed
}

/// [`parse_lossless`] over a source that parses clean: no diagnostic
/// and no required child the grammar did not supply.
pub fn parse_clean(src: &str) -> php_syntax::Parsed<'static> {
    let parsed = parse_lossless(src);
    assert!(
        parsed.diagnostics.is_empty() && parsed.missing.is_empty(),
        "{}\n{:?}\n{:?}",
        parsed.dump(),
        parsed.diagnostics,
        parsed.missing
    );
    parsed
}

/// Parse `src` and render the dump, the broken promises and the
/// diagnostics.
pub fn parse_fixture(src: &str) -> String {
    let parsed = parse_lossless(src);
    let mut out = parsed.dump();
    // A required child the grammar did not supply is a parser defect;
    // rendered into the snapshot so a person reads it rather than a
    // panic.
    for missing in &parsed.missing {
        writeln!(
            out,
            "REQUIRED-VIOLATION: {} on {}..{}",
            missing.field,
            missing.range.start,
            missing.range.end()
        )
        .ok();
    }
    for d in &parsed.diagnostics {
        writeln!(
            out,
            "error@{}..{} {}",
            d.span.start,
            d.span.end(),
            d.message
        )
        .ok();
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

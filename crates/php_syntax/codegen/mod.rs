//! The generator behind `build.rs` (design §4.9, ADR-0003): `ungram` reads
//! `php.ungram` into a [`ungram::Grammar`], and `kind` emits `SyntaxKind`
//! from it. Neither is part of the library: `build.rs` includes this body
//! by path to write `OUT_DIR/syntax_kind.rs`, and `tests/schema.rs`
//! includes it the same way to hold the schema and the emitter to their
//! tests, since nothing exercises a build's own tests.

pub mod kind;
pub mod ungram;

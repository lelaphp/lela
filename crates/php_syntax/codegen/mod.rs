//! The generator behind `build.rs` (design §4.9, ADR-0003): `ungram` reads
//! `php.ungram` into a [`ungram::Grammar`], `kind` emits `SyntaxKind` from
//! it, and `tree` emits the typed nodes and the display table. None is
//! part of the library: `build.rs` includes this body by path to write
//! `OUT_DIR/syntax_kind.rs`, `tree.rs` and `display.rs`, and
//! `tests/schema.rs` includes it the same way to hold the schema and the
//! emitters to their tests, since nothing exercises a build's own tests.

pub mod kind;
pub mod tree;
pub mod ungram;

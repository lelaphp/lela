//! The syntax crate (design §4.1, §4.9; ADR-0003): the lexer, the lossless
//! token stream, the typed tree in a safe arena and the parser with php's
//! error text. What stands today is the schema, `php.ungram` beside this
//! crate, and [`SyntaxKind`], the kind of every token and node, which
//! `build.rs` emits from it; and the stub that gives `php_compile` its edge
//! and its input type: [`parse`] keeps the source and produces no tokens,
//! no tree and no diagnostics, and the four merged Mahout crates land
//! behind this signature as work of their own. `Options` joins `parse`
//! with them.
#![forbid(unsafe_code)]

mod kind {
    //! The generated enum (`build.rs`).
    include!(concat!(env!("OUT_DIR"), "/syntax_kind.rs"));
}

pub use kind::SyntaxKind;

/// What one parse produced (design §4.1): the retained source, and, once
/// the copy from Mahout lands, the token stream, the line table, the tree and the
/// diagnostics in php's phase order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parsed {
    /// The source as given; tokens are offsets into it.
    pub source: Box<[u8]>,
}

/// Parse one source file. The stub retains the source and nothing else.
#[must_use]
pub fn parse(source: &[u8]) -> Parsed {
    Parsed {
        source: source.into(),
    }
}

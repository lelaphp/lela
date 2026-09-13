//! The syntax crate (design §4.1, §4.9; ADR-0003): the lexer, the lossless
//! token stream, the typed tree in a safe arena and the parser with php's
//! error text. What stands today is the schema, `php.ungram` beside this
//! crate, and [`SyntaxKind`], the kind of every token and node, which
//! `build.rs` emits from it, with the typed nodes of [`tree`], one struct
//! per node kind with named child fields, emitted from the same schema;
//! the lexer, [`lex`], whose state stack mirrors
//! Zend's scanner and whose tokens concatenate to the input; the source
//! model under it, [`Span`], [`LineIndex`] and [`Diagnostic`]; php's names
//! for the lexer's kinds, [`operator_name`] and the four tables beside it,
//! which `token_get_all` spells; the arena, [`tree::Tree`], the builder
//! that fills it from the parser's events, [`tree::build`], and the dump
//! that prints it, [`Parsed::dump`]; and the entry that gives `php_compile`
//! its edge and its input type: [`parse`] keeps the source and lexes it,
//! and the tree it hands back is empty until the parser lands behind this
//! signature (#202 to #206). `Options` joins `parse` with them.
#![forbid(unsafe_code)]

mod kind {
    //! The generated enum and its display names (`build.rs`).
    include!(concat!(env!("OUT_DIR"), "/syntax_kind.rs"));
    include!(concat!(env!("OUT_DIR"), "/display.rs"));
}
mod lexer;
mod line_index;
mod php_names;
mod source;
pub mod tree;

pub use kind::SyntaxKind;
pub use lexer::{LexOptions, Lexed, Token, lex};
pub use line_index::{LineCol, LineIndex};
pub use php_names::{amp_name, cast_name, int_overflows, magic_const_name, operator_name};
pub use source::{Diagnostic, FileId, MAX_FILE_LEN, Span, TextRange, load_bytes};
pub use tree::Tree;

/// What one parse produced (design §4.1): the retained source, the
/// lossless token stream over it, and the tree; the line table and the
/// diagnostics in php's phase order join them with the parse entry (#206).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parsed {
    /// The source as given; tokens are offsets into it.
    pub source: Box<[u8]>,
    /// Every token of the source in order, whitespace and comments
    /// included: the texts concatenate to `source`.
    pub tokens: Box<[Token]>,
    /// The tree over the significant tokens; empty until the parser lands.
    pub tree: Tree,
}

impl Parsed {
    /// The dump of the tree, `KIND@start..end` per node and
    /// `KIND@start..end "text"` per token ([`tree::dump`]).
    #[must_use]
    pub fn dump(&self) -> String {
        tree::dump(&self.tree, &self.tokens, &self.source)
    }
}

/// Parse one source file. The entry lexes the source and retains both; the
/// tree is empty until the parser lands (#202 to #206).
#[must_use]
pub fn parse(source: &[u8]) -> Parsed {
    let lexed = lex(FileId(0), source, &LexOptions::default());
    Parsed {
        source: source.into(),
        tokens: lexed.tokens.into_boxed_slice(),
        tree: Tree::default(),
    }
}

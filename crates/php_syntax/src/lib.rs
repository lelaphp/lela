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
//! that prints it, [`Parsed::dump`]; the parser, `parser::Parser`, the
//! cursor over the significant tokens and the markers the grammar writes
//! its nodes through; and the entry that gives `php_compile` its edge and
//! its input type: [`parse`] keeps the source, lexes it and hands the
//! stream through the parser to the builder, and the tree it hands back
//! is one root over every token until the grammar lands behind this
//! signature (#203 to #206). `Options` joins `parse` with them.
#![forbid(unsafe_code)]

mod kind {
    //! The generated enum and its display names (`build.rs`).
    include!(concat!(env!("OUT_DIR"), "/syntax_kind.rs"));
    include!(concat!(env!("OUT_DIR"), "/display.rs"));
}
mod lexer;
mod line_index;
// The grammar (#203 to #205) is the reader of most of the parser's
// surface; what `parse` alone does not call is not dead.
#[allow(dead_code)]
mod parser;
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
    /// The tree over the significant tokens: one root over every token
    /// until the grammar lands.
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

/// Parse one source file. The entry lexes the source, hands the stream
/// through the parser to the builder and retains all three. The root is
/// opened and closed here until the grammar lands (#203 to #205), so the
/// builder gives it every token as a flat child; the diagnostics join
/// `Parsed` with #206.
#[must_use]
pub fn parse(source: &[u8]) -> Parsed {
    let file = FileId(0);
    let lexed = lex(file, source, &LexOptions::default());
    let input = parser::Input::new(source, &lexed.tokens);
    let mut p = parser::Parser::new(&input, file);
    let root = p.start();
    root.complete(&mut p, SyntaxKind::SOURCE_FILE);
    let built = tree::build(&lexed.tokens, p.finish());
    Parsed {
        source: source.into(),
        tokens: lexed.tokens.into_boxed_slice(),
        tree: built.tree,
    }
}

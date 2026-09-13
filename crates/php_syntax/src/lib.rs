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
//! its nodes through; the grammar, `grammar`, the Pratt core and the
//! statement, declaration and type rules with their hand-placed error
//! sites; and the entry that gives `php_compile` its edge and its input
//! type: [`parse`] keeps the source, lexes it and hands the stream through
//! the grammar to the builder. The diagnostics it hands back are in the
//! inherited wording where php's was not beside the site, and their order
//! and shape are settled with the parse entry (#206). `Options` joins
//! `parse` with them.
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

mod kind {
    //! The generated enum and its display names (`build.rs`).
    include!(concat!(env!("OUT_DIR"), "/syntax_kind.rs"));
    include!(concat!(env!("OUT_DIR"), "/display.rs"));
}
mod grammar;
mod lexer;
mod line_index;
mod parser;
mod php_names;
mod source;
pub mod tree;

pub use kind::SyntaxKind;
pub use lexer::{LexOptions, Lexed, Token, lex};
pub use line_index::{LineCol, LineIndex};
pub use php_names::{amp_name, cast_name, int_overflows, magic_const_name, operator_name};
pub use source::{Diagnostic, FileId, MAX_FILE_LEN, Span, TextRange, load_bytes};
pub use tree::{Missing, Tree};

/// What one parse produced (design §4.1): the retained source, the
/// lossless token stream over it, the tree, the diagnostics and the
/// promises the grammar broke; the line table, and the diagnostics in
/// php's phase order, join them with the parse entry (#206).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parsed {
    /// The source as given; tokens are offsets into it.
    pub source: Box<[u8]>,
    /// Every token of the source in order, whitespace and comments
    /// included: the texts concatenate to `source`.
    pub tokens: Box<[Token]>,
    /// The tree over the significant tokens.
    pub tree: Tree,
    /// The lexer's diagnostics in emission order, then the parser's in
    /// offset order, each once: a site that is reached twice by two
    /// recovery layers raises one diagnostic. Which of them php reports,
    /// and in what order, is #206's.
    pub diagnostics: Vec<Diagnostic>,
    /// Every required child (schema convention A2) the grammar did not
    /// supply, in finish order: a grammar defect, which the fixture
    /// harness prints beside the dump and a reader of the tree treats as
    /// an internal error.
    pub missing: Vec<Missing>,
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
/// through the grammar to the builder and retains all three, with the
/// diagnostics and the broken promises the build reported.
#[must_use]
pub fn parse(source: &[u8]) -> Parsed {
    let file = FileId(0);
    let lexed = lex(file, source, &LexOptions::default());
    let input = parser::Input::new(source, &lexed.tokens);
    let mut p = parser::Parser::new(&input, file);
    grammar::source_file(&mut p);
    let built = tree::build(&lexed.tokens, p.finish());
    // One defect, one diagnostic: two recovery layers that reach one site
    // emit the same span and text, and the pair is one. The sort is
    // stable, so at one offset the first emitted stays first.
    let mut parsed = built.diagnostics;
    parsed.sort_by_key(|d| d.span.start);
    parsed.dedup_by(|a, b| a.span == b.span && a.message == b.message);
    let mut diagnostics = lexed.diagnostics;
    diagnostics.extend(parsed);
    Parsed {
        source: source.into(),
        tokens: lexed.tokens.into_boxed_slice(),
        tree: built.tree,
        diagnostics,
        missing: built.missing,
    }
}

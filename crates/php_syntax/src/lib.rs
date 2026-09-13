//! The syntax crate (design §4.1, §4.9; ADR-0003): the lexer, the lossless
//! token stream, the typed tree in a safe arena and the parser with php's
//! error text. What stands today is the schema, `php.ungram` beside this
//! crate, and [`SyntaxKind`], the kind of every token and node, which
//! `build.rs` emits from it; the lexer, [`lex`], whose state stack mirrors
//! Zend's scanner and whose tokens concatenate to the input; the source
//! model under it, [`Span`], [`LineIndex`] and [`Diagnostic`]; php's names
//! for the lexer's kinds, [`operator_name`] and the four tables beside it,
//! which `token_get_all` spells; and the stub that gives `php_compile` its
//! edge and its input type: [`parse`] keeps the source and produces no
//! tokens, no tree and no diagnostics, and the rest of the copy from Mahout
//! lands behind this signature as work of its own. `Options` joins `parse`
//! with them.
#![forbid(unsafe_code)]

mod kind {
    //! The generated enum (`build.rs`).
    include!(concat!(env!("OUT_DIR"), "/syntax_kind.rs"));
}
mod lexer;
mod line_index;
mod php_names;
mod source;

pub use kind::SyntaxKind;
pub use lexer::{LexOptions, Lexed, Token, lex};
pub use line_index::{LineCol, LineIndex};
pub use php_names::{amp_name, cast_name, int_overflows, magic_const_name, operator_name};
pub use source::{Diagnostic, FileId, MAX_FILE_LEN, Span, TextRange, load_bytes};

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

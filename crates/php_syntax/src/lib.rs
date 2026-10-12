//! The syntax crate (design §4.1, §4.9; ADR-0003): the lexer, the lossless
//! token stream, the typed tree in a safe arena and the parser with php's
//! error text. The schema is `php.ungram` beside this crate, and
//! [`SyntaxKind`], the kind of every token and node, is what `build.rs`
//! emits from it, with the typed nodes of [`tree`], one struct per node
//! kind with named child fields, emitted from the same schema; the lexer,
//! [`lex`], whose state stack mirrors Zend's scanner and whose tokens
//! concatenate to the input; the source model under it, [`Span`],
//! [`LineIndex`] and [`Diagnostic`]; php's names for the lexer's kinds,
//! [`operator_name`] and the four tables beside it, which `token_get_all`
//! spells; the arena, [`tree::Tree`], the builder that fills it from the
//! parser's events, [`tree::build`], and the dump that prints it,
//! [`Parsed::dump`]; the parser, `parser::Parser`, the cursor over the
//! significant tokens and the markers the grammar writes its nodes
//! through; the grammar, `grammar`, the Pratt core and the statement,
//! declaration and type rules with their hand-placed error sites, each
//! worded as php words it; and the entry that gives `php_compile` its
//! edge and its input type: [`parse`] keeps the source, lexes it and
//! hands the stream through the grammar to the builder, and hands back
//! the diagnostics in php's phase order.
//!
//! **What a diagnostic is** (design §6.1): php's text, at php's line,
//! under php's level, and no code number. The engine has one voice, and
//! it is php's: `syntax error, unexpected token ";", expecting ")"` is
//! bison's sentence for the state php's parser was in, pinned site by
//! site against the oracle; `Unclosed '{' on line 3` and `Unterminated
//! comment starting line 3` are its scanner's. What php reports first is
//! what [`Parsed::diagnostics`] holds first.
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
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod kind {
    //! The generated enum and its display names (`build.rs`).
    include!(concat!(env!("OUT_DIR"), "/syntax_kind.rs"));
    include!(concat!(env!("OUT_DIR"), "/display.rs"));
}
mod grammar;
mod highlight;
mod lexer;
mod line_index;
mod output;
mod parser;
mod php_names;
mod scanner;
mod source;
pub mod tree;
mod value;

pub use highlight::{Colours, highlight, html_puts, strip};
pub use kind::SyntaxKind;
pub use lexer::{LexOptions, Lexed, Position, Token, lex};
pub use line_index::{LineCol, LineIndex};
pub use output::{Filtered, filter_tokens, token_text};
pub use php_names::{amp_name, cast_name, int_overflows, magic_const_name, operator_name};
pub use scanner::{ScannedToken, scanned};
pub use source::{Diagnostic, FileId, Level, MAX_FILE_LEN, Span, TextRange, load_bytes};
pub use tree::{Missing, Tree};
pub use value::{Holder, TokenValue};

/// What the parser is told beside the source (design §4.1).
#[derive(Debug, Clone, Default)]
pub struct Options {
    /// The file the spans point into; its name is the caller's.
    pub file: FileId,
    /// The lexer's own: `short_open_tag`.
    pub lex: LexOptions,
}

/// What one parse produced (design §4.1): the retained source, the
/// lossless token stream over it, its line table, the tree, the
/// diagnostics in php's phase order and the promises the grammar broke.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parsed {
    /// The source as given; tokens are offsets into it.
    pub source: Box<[u8]>,
    /// Every token of the source in order, whitespace and comments
    /// included: the texts concatenate to `source`. Kept as the lexer
    /// grew it: a parse lives as long as its compile, and shrinking the
    /// stream to its length would copy it.
    pub tokens: Vec<Token>,
    /// The line table over `source`, which every diagnostic's line was
    /// read from.
    pub lines: LineIndex,
    /// The tree over the significant tokens.
    pub tree: Tree,
    /// The diagnostics in the order php would raise them, so the first
    /// is the one php reports and the rest are what recovery found
    /// after it. php's scanner runs a token ahead of its parser and
    /// raises as it scans, its parser raises at the token it did not
    /// want, and its compiler raises after the whole file parsed: so the
    /// parse-phase diagnostics stand in offset order, the scanner's
    /// before the parser's at one token, and the compile-phase ones
    /// ([`Level::CompileError`]) after every one of them. A site two
    /// recovery layers reach raises one diagnostic.
    pub diagnostics: Vec<Diagnostic>,
    /// Every required child (schema convention A2) the grammar did not
    /// supply, in finish order: a grammar defect, which the fixture
    /// harness prints beside the dump and a reader of the tree treats as
    /// an internal error.
    pub missing: Vec<Missing>,
    /// What php's token filter made of the tokens it converts, empty
    /// until [`filter_tokens`] passes one.
    pub filtered: Filtered,
}

impl Parsed {
    /// The dump of the tree, `KIND@start..end` per node and
    /// `KIND@start..end "text"` per token ([`tree::dump`]).
    #[must_use]
    pub fn dump(&self) -> String {
        tree::dump(&self.tree, &self.tokens, &self.source)
    }
}

/// Parse one source file. The entry indexes the lines, lexes the source,
/// hands the stream through the grammar to the builder and retains all
/// of it, with the diagnostics merged into php's phase order
/// ([`Parsed::diagnostics`]) and the broken promises the build reported.
#[must_use]
pub fn parse(source: &[u8], options: &Options) -> Parsed {
    let file = options.file;
    let lines = LineIndex::new(source);
    let lexed = lexer::lex_with_lines(file, source, &options.lex, &lines);
    let input = parser::Input::new(source, &lexed.tokens);
    let mut p = parser::Parser::new(&input, file, &lines);
    grammar::source_file(&mut p);
    let built = tree::build(&lexed.tokens, &lines, p.finish());
    let diagnostics = in_php_order(&lexed.tokens, lexed.diagnostics, built.diagnostics);
    Parsed {
        source: source.into(),
        tokens: lexed.tokens,
        lines,
        tree: built.tree,
        diagnostics,
        missing: built.missing,
        filtered: Filtered::default(),
    }
}

/// The scanner's and the parser's diagnostics as one list in the order
/// php raises them. php's scanner raises while it produces a token, its
/// parser at the token it did not want, and its compiler once the file
/// has parsed: so a scanner diagnostic is placed at the start of the
/// token it lies in (an escape's warning precedes the parser's word on
/// the string), the scanner's before the parser's at one token, and
/// every compile-phase diagnostic after every parse-phase one. Two
/// recovery layers that reach one site emit the same span and text, and
/// the pair is one; the sort is stable, so which was emitted first
/// stays first.
///
/// php's scanner throws on `(real)` while its parser still holds the
/// `1` it will not want it after, so the scanner's sentence is the one
/// php reports, and the first here:
///
/// ```php
/// echo 1 (real)$x;
/// ```
/// ```expect exit=255
/// Parse error: The (real) cast has been removed, use (float) instead in %s on line 2
/// ```
fn in_php_order(
    tokens: &[Token],
    lexed: Vec<Diagnostic>,
    parsed: Vec<Diagnostic>,
) -> Vec<Diagnostic> {
    let token_start = |offset: u32| -> u32 {
        let next = tokens.partition_point(|token| token.offset <= offset);
        let Some(i) = next.checked_sub(1) else {
            return offset;
        };
        // A `"…"` with no interpolation is one token to php, so a
        // diagnostic in its text stands at its opening mark.
        let plain_string = tokens[i].kind == SyntaxKind::STRING_TEXT
            && i > 0
            && tokens[i - 1].kind == SyntaxKind::DOUBLE_QUOTE
            && tokens.get(i + 1).map(|t| t.kind) == Some(SyntaxKind::DOUBLE_QUOTE);
        if plain_string {
            tokens[i - 1].offset
        } else {
            tokens[i].offset
        }
    };
    let mut all: Vec<(u32, Diagnostic)> = lexed
        .into_iter()
        .map(|d| (token_start(d.span.start), d))
        .chain(parsed.into_iter().map(|d| (d.span.start, d)))
        .collect();
    all.sort_by_key(|(at, d)| (d.level == Level::CompileError, *at));
    all.dedup_by(|(_, a), (_, b)| a.span == b.span && a.message == b.message);
    all.into_iter().map(|(_, d)| d).collect()
}

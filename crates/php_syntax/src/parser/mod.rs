//! The parser (design §4.9, ADR-0003): a cursor over the significant
//! tokens, markers that open and close nodes as [`Event`]s for the
//! builder, the forward-progress fuel, the nesting cap and the open-delimiter stack
//! from which php's bracket-balance text is spelled. The grammar (#203 to
//! #205) is written against this surface and nothing else sees it.
//!
//! **What is php's.** The text of a diagnostic is php's: the unexpected
//! token is named as `zend_yytnamerr` names it
//! (`Zend/zend_language_parser.y` at 8.5.9), a bracket imbalance is
//! `Unclosed '(' on line 3 does not match ']'` as `zend_language_scanner.l`
//! spells it, and the nesting cap answers `memory exhausted`, which is
//! what bison's stack answers in php (`Zend/tests/bug64660.phpt`). Where a
//! grammar position has a fixed `, expecting …` clause in php's tables,
//! the site pins it through [`Parser::unexpected_expecting`] or
//! [`Parser::unexpected_here_expecting`]; the parser records no expected
//! set of its own, since nothing would print it. A statement position's clause is the
//! enclosing list's, [`Parser::stmt_clause`], which the list loops set:
//! `expecting end of file` at the top level, `"elseif" or "else" or
//! "endif"` inside an alternative-syntax `if`, none inside a block. The
//! line is php's too, [`Parser::line_here`]: the scanner's line after the
//! unexpected token, so a token spanning lines reports its last.
//!
//! **What is bounded.** Grammar recursion is capped at [`MAX_DEPTH`]
//! levels through [`Parser::enter_depth`]; php's bison stack refuses on
//! the same axis near 10,000 levels of `(`, so a source between the two
//! parses under php and is refused here, a divergence to propose
//! when #206 pins the text. Lookahead without consuming spends fuel, and
//! a loop that looks without consuming trips [`Parser::is_stuck`], which
//! the statement and expression loops answer by consuming into an `ERROR`
//! node. The cap keeps the parser's own stack bounded only on a thread
//! that can absorb `MAX_DEPTH` levels before the cap refuses, which is
//! [`MIN_THREAD_STACK`].

mod input;

use crate::SyntaxKind;
use crate::line_index::LineIndex;
use crate::source::{Diagnostic, FileId, Span};
use crate::tree::{Event, TOMBSTONE};

pub use input::Input;

/// The grammar's view of the parse.
pub struct Parser<'t> {
    input: &'t Input<'t>,
    pos: usize,
    file: FileId,
    /// The line table, for the line a diagnostic reports and php's `on
    /// line N` inside the bracket-balance text.
    lines: &'t LineIndex,
    events: Vec<Event>,
    /// Forward-progress fuel: spent on every lookahead, refilled on every
    /// consumed token. Exhaustion trips [`Parser::is_stuck`]. A legitimate
    /// lookahead burst (an attribute scan) exceeds any small constant, so
    /// the hard failure is the burst cap below.
    fuel: u32,
    /// Lookaheads since the last consumed token. A burst past `burst_cap`
    /// cannot come from looking over the input, only from a loop that
    /// looks without consuming: a debug build asserts, a release build
    /// relies on `is_stuck`.
    burst: u32,
    burst_cap: u32,
    /// Grammar recursion depth (statements, expressions, types), bounded
    /// by [`MAX_DEPTH`]. It bounds the parser's own stack and nothing
    /// else: the Pratt core wraps a left-nested chain iteratively, so
    /// `1+1+…+1` is one level here and `n` levels of tree.
    depth: u32,
    /// The cap is reported once per file, at the first refusal.
    depth_reported: bool,
    /// The open delimiters, pushed and popped by every consumed token:
    /// where php's `Unclosed '('` and `Unmatched ')'` read their opener.
    delims: Vec<(SyntaxKind, Span)>,
    /// The one-shot grants the next primary consumes.
    pub(crate) grants: Grants,
    /// php's `, expecting …` clause for a token that cannot start a
    /// statement where one is due, which is the enclosing list's: bison
    /// prints the list's own continuations where they are few enough.
    /// The list loops set it and restore it (probed 8.5.9: `<?php =` is
    /// `expecting end of file`, `if (1): =` is `expecting "elseif" or
    /// "else" or "endif"`, `{ = }` has no clause).
    pub(crate) stmt_clause: Option<&'static str>,
    /// The highest start offset of any diagnostic raised so far. php's
    /// wording at a statement that has already gone wrong is the
    /// unexpected-token one (`const return` is `unexpected token "return",
    /// expecting identifier`, never a missing `;`), so a zero-width repair
    /// anchored after the previous token yields to an error already
    /// raised at or after its offset.
    errors_high_water: Option<u32>,
}

const FUEL: u32 = 256;

/// One-shot grants the grammar gives the next primary and the prefix
/// parse takes at its top, so only the immediately next primary
/// inherits one; a descent through another primary first revokes it.
#[derive(Clone, Copy, Default)]
pub(crate) struct Grants {
    /// `list(...)` parsed here needs no `=` after it: a `foreach`
    /// target, an array or list element (Zend's `array_pair` contexts).
    pub(crate) list_target: bool,
    /// A `(void)` cast may stand here, which php allows at a statement's
    /// start and in a `for` header's expression lists and nowhere else
    /// (`zend_language_parser.y`: `T_VOID_CAST expr ';'`,
    /// `non_empty_for_exprs`). Elsewhere `(void)` is the unexpected token.
    pub(crate) void: bool,
    /// The primary is a `class_name_reference` (`new`'s operand, the right
    /// operand of `instanceof`), where a bare `static` stands on its own
    /// (`new static`, `$x instanceof static`).
    pub(crate) class_ref: bool,
}

/// The grammar nesting cap: a bound on the parser's recursion, and on
/// nothing else. Well clear of real code (Mahout's 193k-file corpus
/// measured its deepest file far under it) and a few binary orders under
/// php's bison stack, which refuses near 10,000 levels; the band between
/// is the difference the crate docs name. The cap closes the
/// stack-overflow class only on a thread that can absorb this many levels
/// of native recursion before the cap refuses, which is what
/// [`MIN_THREAD_STACK`] sizes.
pub const MAX_DEPTH: u32 = 512;

/// The native-stack budget of any thread that parses: 16 MiB, the
/// measured debug-build worst case of [`MAX_DEPTH`] levels of grammar
/// recursion (about 8 MiB, Mahout, 2026-06-12) with twice the headroom.
/// A virtual reservation; pages commit on use. Rust sizes a spawned
/// thread at 2 MiB and the main thread at what the linker gave it, so a
/// host that parses on a thread it did not size this way is the one
/// thread the cap does not cover.
// Read by the nesting test and by a host that sizes a parsing thread.
#[allow(dead_code)]
pub const MIN_THREAD_STACK: usize = 16 * 1024 * 1024;

/// An open node, to be completed or abandoned.
#[must_use = "a marker is completed or abandoned"]
pub struct Marker {
    event_index: u32,
    settled: bool,
}

impl Drop for Marker {
    fn drop(&mut self) {
        // A marker that is neither completed nor abandoned is a grammar
        // defect; a debug build says so, and not while unwinding from an
        // earlier one, which would abort and hide the first message.
        debug_assert!(
            self.settled || std::thread::panicking(),
            "marker dropped without complete() or abandon()"
        );
    }
}

/// A finished node, re-openable for wrapping by [`CompletedMarker::precede`].
#[derive(Clone, Copy)]
pub struct CompletedMarker {
    start_event: u32,
    /// The node's kind, which the Pratt core reads off its left operand.
    pub kind: SyntaxKind,
}

impl<'t> Parser<'t> {
    /// A parser at the start of `input`, `lines` being the table over the
    /// same source.
    pub fn new(input: &'t Input<'t>, file: FileId, lines: &'t LineIndex) -> Parser<'t> {
        let burst_cap = u32::try_from(input.len())
            .unwrap_or(u32::MAX)
            .saturating_mul(4)
            .saturating_add(10_000);
        Parser {
            input,
            pos: 0,
            file,
            lines,
            // A token's event and about a node's two per token: room
            // for what a parse of this many tokens emits, so the stream
            // seldom grows.
            events: Vec::with_capacity(input.len().saturating_mul(3).saturating_add(4)),
            fuel: FUEL,
            burst: 0,
            burst_cap,
            depth: 0,
            depth_reported: false,
            delims: Vec::new(),
            grants: Grants::default(),
            stmt_clause: None,
            errors_high_water: None,
        }
    }

    /// The file being parsed, for a diagnostic's span.
    // The surface of record §3.5: kept whole; what the grammar does not
    // call today, #206 reads or the next grammar site does.
    #[allow(dead_code)]
    pub fn file(&self) -> FileId {
        self.file
    }

    /// The current token's kind; `EOF` at the end.
    pub fn current(&mut self) -> SyntaxKind {
        self.spend_fuel();
        self.input.kind(self.pos)
    }

    /// The kind `n` tokens ahead.
    pub fn nth(&mut self, n: usize) -> SyntaxKind {
        self.spend_fuel();
        self.input.kind(self.pos + n)
    }

    /// The current token's text.
    pub fn current_text(&self) -> &'t [u8] {
        self.input.text(self.pos)
    }

    /// The current token's span.
    pub fn current_span(&self) -> Span {
        self.input.range(self.pos).in_file(self.file)
    }

    /// The span of the token `n` ahead.
    pub fn nth_span(&self, n: usize) -> Span {
        self.input.range(self.pos + n).in_file(self.file)
    }

    /// The span of the previous significant token, for a diagnostic
    /// raised after consuming it.
    pub fn prev_span(&self) -> Span {
        let prev = self.pos.saturating_sub(1);
        self.input.range(prev).in_file(self.file)
    }

    /// Is the current token `kind`?
    pub fn at(&self, kind: SyntaxKind) -> bool {
        self.input.kind(self.pos) == kind
    }

    /// Is the current token one of `kinds`?
    #[allow(dead_code)]
    pub fn at_any(&self, kinds: &[SyntaxKind]) -> bool {
        kinds.contains(&self.input.kind(self.pos))
    }

    /// Was there a newline in the whitespace or comments before the
    /// current token?
    pub fn newline_before_current(&self) -> bool {
        self.input.newline_before(self.pos)
    }

    /// The number of significant tokens left.
    #[allow(dead_code)]
    pub fn remaining(&self) -> usize {
        self.input.len().saturating_sub(self.pos)
    }

    /// At the end of the input?
    pub fn at_eof(&self) -> bool {
        self.pos >= self.input.len()
    }

    /// Opens a node.
    pub fn start(&mut self) -> Marker {
        let index = u32::try_from(self.events.len()).unwrap_or(u32::MAX);
        self.events.push(Event::Start {
            kind: TOMBSTONE,
            forward_parent: None,
        });
        Marker {
            event_index: index,
            settled: false,
        }
    }

    /// Consumes the current token, which is `kind`, into the tree.
    pub fn bump(&mut self, kind: SyntaxKind) {
        debug_assert_eq!(self.input.kind(self.pos), kind, "bump kind mismatch");
        self.do_bump(kind);
    }

    /// Consumes the current token under another kind: a contextual
    /// keyword consumed as a name becomes `IDENT`.
    pub fn bump_remap(&mut self, remap: SyntaxKind) {
        self.do_bump(remap);
    }

    /// Consumes the current token, which is `kind`, without it entering
    /// or leaving the bracket nesting: the `[` and `]` of a string's
    /// `$a[…]`, which php's `ST_VAR_OFFSET` scanner state returns without
    /// `enter_nesting`, so an unclosed one is never `Unclosed '['`.
    pub fn bump_unnested(&mut self, kind: SyntaxKind) {
        debug_assert_eq!(self.input.kind(self.pos), kind, "bump kind mismatch");
        debug_assert!(!self.at_eof(), "bump at EOF");
        self.events.push(Event::Token { remap: kind });
        self.pos += 1;
        self.fuel = FUEL;
        self.burst = 0;
    }

    /// Consumes whatever the current token is; nothing at the end.
    pub fn bump_any(&mut self) {
        let kind = self.input.kind(self.pos);
        if kind == SyntaxKind::EOF {
            return;
        }
        self.do_bump(kind);
    }

    fn do_bump(&mut self, remap: SyntaxKind) {
        debug_assert!(!self.at_eof(), "bump at EOF");
        if Bracket::from_opener(remap).is_some() {
            self.delims.push((remap, self.current_span()));
        } else if let Some(closed) = Bracket::from_closer(remap) {
            // The nearest matching opener; recovery may interleave.
            if let Some(at) = self
                .delims
                .iter()
                .rposition(|(k, _)| Bracket::from_opener(*k) == Some(closed))
            {
                self.delims.truncate(at);
            }
        }
        self.events.push(Event::Token { remap });
        self.pos += 1;
        self.fuel = FUEL;
        self.burst = 0;
    }

    /// At `kind`, consume it and answer true; otherwise record the
    /// expectation and answer false, with no diagnostic: the caller
    /// decides.
    pub fn eat(&mut self, kind: SyntaxKind) -> bool {
        if self.at(kind) {
            self.bump(kind);
            true
        } else {
            false
        }
    }

    /// php's `backup_doc_comment` at this position of the grammar: the
    /// innermost open node takes the pending doc comment. The grammar
    /// places it before the last token php's parser had read when it
    /// reduces the rule, which `tree::builder`'s docs derive position by
    /// position.
    pub fn doc_comment(&mut self) {
        self.events.push(Event::DocComment);
    }

    /// The `attribute` rule's `CG(doc_comment) = $2`: after the group's
    /// `]`, what it took at its `#[` is pending again.
    pub fn doc_comment_restore(&mut self) {
        self.events.push(Event::DocCommentRestore);
    }

    /// Raises a diagnostic.
    pub fn error(&mut self, diagnostic: Diagnostic) {
        let start = diagnostic.span.start;
        self.errors_high_water = Some(self.errors_high_water.map_or(start, |hw| hw.max(start)));
        self.events.push(Event::Error(Box::new(diagnostic)));
    }

    /// Has a diagnostic been raised at or after byte offset `pos`? What
    /// the zero-width repairs ask before they anchor one (see
    /// `errors_high_water`).
    pub fn has_error_at_or_after(&self, pos: u32) -> bool {
        self.errors_high_water.is_some_and(|hw| hw >= pos)
    }

    /// The line php reports for a diagnostic at the current token: the
    /// scanner's line once the token is scanned, so a token spanning
    /// lines reports its last, and the end of the input reports the last
    /// line. The one case apart is php's: an unclosed single-quoted string
    /// is handed back without its newlines counted, so it reports the
    /// line it opened on.
    ///
    /// ```php
    /// echo 1 'a
    /// b';
    /// ```
    /// ```expect exit=255
    /// Parse error: syntax error, unexpected single-quoted string "a", expecting "," or ";" in %s on line 3
    /// ```
    pub fn line_here(&self) -> u32 {
        let range = self.input.range(self.pos);
        let text = self.input.text(self.pos);
        let unclosed_single = self.input.kind(self.pos) == SyntaxKind::STRING_TEXT
            && self.pos + 1 >= self.input.len()
            && matches!(text, [b'\'', ..] | [b'b' | b'B', b'\'', ..]);
        if unclosed_single {
            return self.lines.line_number(range.start);
        }
        // A `"` opening a string with no interpolation is one token to
        // php, closing quote included (see `unexpected_name`).
        if self.plain_double_quoted() {
            return self.lines.line_number(self.input.range(self.pos + 2).end());
        }
        self.lines.line_number(range.end())
    }

    /// Whether the current token opens a `"…"` with no interpolation:
    /// the mark, one text and the mark, or the two marks.
    fn plain_double_quoted(&self) -> bool {
        self.input.kind(self.pos) == SyntaxKind::DOUBLE_QUOTE
            && ((self.input.kind(self.pos + 1) == SyntaxKind::STRING_TEXT
                && self.input.kind(self.pos + 2) == SyntaxKind::DOUBLE_QUOTE)
                || self.input.kind(self.pos + 1) == SyntaxKind::DOUBLE_QUOTE)
    }

    /// Do the current token and the three after it spell `private(set)`,
    /// `protected(set)` or `public(set)` as php's scanner fuses them into
    /// one `T_PRIVATE_SET`, `T_PROTECTED_SET` or `T_PUBLIC_SET` (8.4): the
    /// visibility keyword, `(`, `set` and `)`, byte-adjacent, since Zend
    /// tolerates no interior whitespace (probed 8.5.9: `private (set)`
    /// reports `unexpected token "private"`). The lexer keeps the four
    /// tokens; the modifier list glues them and the unexpected-token
    /// clause names them as the one token php saw.
    ///
    /// ```php
    /// private(set) int $x;
    /// ```
    /// ```expect exit=255
    /// Parse error: syntax error, unexpected token "private(set)", expecting end of file in %s on line 2
    /// ```
    pub fn at_fused_set_visibility(&self) -> bool {
        matches!(
            self.input.kind(self.pos),
            SyntaxKind::PRIVATE_KW | SyntaxKind::PROTECTED_KW | SyntaxKind::PUBLIC_KW
        ) && self.input.kind(self.pos + 1) == SyntaxKind::L_PAREN
            && self.input.kind(self.pos + 2) == SyntaxKind::SET_KW
            && self.input.kind(self.pos + 3) == SyntaxKind::R_PAREN
            && self.current_span().end() == self.nth_span(1).start
            && self.nth_span(1).end() == self.nth_span(2).start
            && self.nth_span(2).end() == self.nth_span(3).start
    }

    /// The span of what php sees as the unexpected token: the current
    /// token, or the four a fused `private(set)` covers.
    fn unexpected_span(&self) -> Span {
        if self.at_fused_set_visibility() {
            let first = self.current_span();
            return Span::new(self.file, first.start, self.nth_span(3).end());
        }
        self.current_span()
    }

    /// The current token as php names it in `syntax error, unexpected …`
    /// ([`php_token_name`]), with the one case one token cannot settle: a
    /// `"` that opens a string with no interpolation is one
    /// `T_CONSTANT_ENCAPSED_STRING` in php, `double-quoted string "abc"`,
    /// where the lexer here split it into the mark, its text and the
    /// closing mark.
    fn unexpected_name(&self) -> String {
        let kind = self.input.kind(self.pos);
        if self.at_fused_set_visibility() {
            let visibility = String::from_utf8_lossy(self.current_text()).to_ascii_lowercase();
            return format!("token \"{visibility}(set)\"");
        }
        if kind == SyntaxKind::DOUBLE_QUOTE {
            let (next, after) = (self.input.kind(self.pos + 1), self.input.kind(self.pos + 2));
            if next == SyntaxKind::DOUBLE_QUOTE {
                return "double-quoted string \"\"".to_owned();
            }
            if next == SyntaxKind::STRING_TEXT && after == SyntaxKind::DOUBLE_QUOTE {
                let mut quoted = b"\"".to_vec();
                quoted.extend_from_slice(self.input.text(self.pos + 1));
                return php_token_name(SyntaxKind::SINGLE_QUOTED, &quoted);
            }
        }
        php_token_name(kind, self.current_text())
    }

    /// php's `syntax error, unexpected …` for the current token, or its
    /// bracket-balance text where that applies: `Unclosed '('` at the end
    /// of the input inside an open delimiter, `Unmatched ')'` and
    /// `Unclosed '(' … does not match ']'` at a closer that closes
    /// nothing or the wrong thing. php prints the balance text alone,
    /// and it is what `eval()`'s `ParseError` carries.
    ///
    /// ```php
    /// echo 1 +;
    /// ```
    /// ```expect exit=255
    /// Parse error: syntax error, unexpected token ";" in %s on line 2
    /// ```
    pub fn unexpected_here(&mut self) -> Diagnostic {
        let span = self.unexpected_span();
        let line = self.line_here();
        if self.at_eof() {
            if let Some(m) = self.unclosed_eof_message() {
                return Diagnostic::new(span, line, m);
            }
            return Diagnostic::new(span, line, "syntax error, unexpected end of file");
        }
        if let Some(m) = self.bracket_closer_message() {
            return Diagnostic::new(span, line, m);
        }
        Diagnostic::new(
            span,
            line,
            format!("syntax error, unexpected {}", self.unexpected_name()),
        )
    }

    /// Builds and raises the [`unexpected_here`](Self::unexpected_here)
    /// diagnostic in one step: what the grammar reaches for wherever the
    /// current token is plainly out of place.
    pub fn error_unexpected(&mut self) {
        let d = self.unexpected_here();
        self.error(d);
    }

    /// [`unexpected_here`](Self::unexpected_here) with php's `, expecting …`
    /// clause pinned on by the site: the LALR expected set bison's tables
    /// print at that position, which a hand-written parser cannot rebuild
    /// from its own recorded set. For a position whose clause is a fixed,
    /// position-specific list: a group `use` item list, the start of a
    /// namespace declaration's name (php-src `ns_trailing_comma_error_*`,
    /// `ns_096`, `namespace_name_namespace_start`). The current token is
    /// not `EOF`: a site reaches for this with a concrete token in hand.
    ///
    /// ```php
    /// foo(1 2);
    /// ```
    /// ```expect exit=255
    /// Parse error: syntax error, unexpected integer "2", expecting ")" in %s on line 2
    /// ```
    pub fn unexpected_expecting(&mut self, expecting: &str) -> Diagnostic {
        let span = self.unexpected_span();
        let line = self.line_here();
        Diagnostic::new(
            span,
            line,
            format!(
                "syntax error, unexpected {}, expecting {expecting}",
                self.unexpected_name()
            ),
        )
    }

    /// [`unexpected_expecting`](Self::unexpected_expecting) at a position
    /// whose current token may legitimately be a closer or the end of the
    /// input: a declaration body, a parameter or argument list's closer.
    /// php prints its balance text alone at a bracket imbalance, so those
    /// defer to the balance-aware base; a plain end of file still carries
    /// the clause (`unexpected end of file, expecting "{"`, probed 8.5.7).
    ///
    /// ```php
    /// echo 1
    /// ```
    /// ```expect exit=255
    /// Parse error: syntax error, unexpected end of file, expecting "," or ";" in %s on line 2
    /// ```
    pub fn unexpected_here_expecting(&mut self, expecting: &str) -> Diagnostic {
        if self.at_eof() {
            if self.unclosed_eof_message().is_some() {
                return self.unexpected_here();
            }
            let span = self.current_span();
            let line = self.line_here();
            return Diagnostic::new(
                span,
                line,
                format!("syntax error, unexpected end of file, expecting {expecting}"),
            );
        }
        if self.bracket_closer_message().is_some() {
            return self.unexpected_here();
        }
        self.unexpected_expecting(expecting)
    }

    /// [`unexpected_here_expecting`](Self::unexpected_here_expecting) with
    /// the clause `Some`, [`unexpected_here`](Self::unexpected_here) with
    /// it `None`: for a site whose clause the caller decides.
    pub fn unexpected_here_maybe_expecting(&mut self, expecting: Option<&str>) -> Diagnostic {
        match expecting {
            Some(expecting) => self.unexpected_here_expecting(expecting),
            None => self.unexpected_here(),
        }
    }

    /// The diagnostic for a token that cannot start a statement where one
    /// is due: [`unexpected_here`](Self::unexpected_here) with the
    /// enclosing list's clause, [`Parser::stmt_clause`]. At the end of
    /// the input the clause is gone: a statement php required and did
    /// not get is its bare `unexpected end of file` (`declare(ticks=1)`,
    /// probed 8.5.9).
    ///
    /// ```php
    /// =
    /// ```
    /// ```expect exit=255
    /// Parse error: syntax error, unexpected token "=", expecting end of file in %s on line 2
    /// ```
    pub fn unexpected_at_stmt(&mut self) -> Diagnostic {
        let clause = if self.at_eof() {
            None
        } else {
            self.stmt_clause
        };
        self.unexpected_here_maybe_expecting(clause)
    }

    /// The 1-based line of a byte offset, for php's `on line N` in the
    /// bracket-balance text.
    fn line_of(&self, offset: u32) -> u32 {
        self.lines.line_number(offset)
    }

    /// php's bracket-balance text when the current token is a closer that
    /// closes nothing (`Unmatched ')'`) or not the innermost open
    /// delimiter (`Unclosed '(' [on line N ]does not match ']'`). `None`
    /// when the current token is not a closer, or closes the innermost
    /// opener: then the site's own flow reads it. The opener is read off
    /// the same stack the secondaries trust.
    ///
    /// ```php
    /// foo(];
    /// ```
    /// ```expect exit=255
    /// Parse error: Unclosed '(' does not match ']' in %s on line 2
    /// ```
    ///
    /// ```php
    /// }
    /// ```
    /// ```expect exit=255
    /// Parse error: Unmatched '}' in %s on line 2
    /// ```
    pub fn bracket_closer_message(&self) -> Option<String> {
        let cur = self.input.kind(self.pos);
        let cur_bracket = Bracket::from_closer(cur)?;
        let closer = cur_bracket.close_glyph();
        match self.delims.last() {
            None => Some(format!("Unmatched '{closer}'")),
            Some(&(open_kind, open_span)) => {
                let open = Bracket::from_opener(open_kind)?;
                if open == cur_bracket {
                    // A genuine match: not a balance error at this site.
                    return None;
                }
                let open_c = open.open_glyph();
                let open_line = self.line_of(open_span.start);
                let cur_line = self.line_of(self.current_span().start);
                Some(if open_line == cur_line {
                    format!("Unclosed '{open_c}' does not match '{closer}'")
                } else {
                    format!("Unclosed '{open_c}' on line {open_line} does not match '{closer}'")
                })
            }
        }
    }

    /// php's bracket-balance text at the end of the input inside an open
    /// delimiter (`Unclosed '{' [on line N]`); `None` when nothing is
    /// open, where a plain `unexpected end of file` stands
    /// (`eval('abc')`).
    ///
    /// ```php
    /// foo(
    /// ```
    /// ```expect exit=255
    /// Parse error: Unclosed '(' in %s on line 2
    /// ```
    pub fn unclosed_eof_message(&self) -> Option<String> {
        let &(open_kind, open_span) = self.delims.last()?;
        let open_c = Bracket::from_opener(open_kind)?.open_glyph();
        let open_line = self.line_of(open_span.start);
        let eof_line = self.line_of(u32::try_from(self.input.source().len()).unwrap_or(u32::MAX));
        Some(if open_line == eof_line {
            format!("Unclosed '{open_c}'")
        } else {
            format!("Unclosed '{open_c}' on line {open_line}")
        })
    }

    /// Has the lookahead fuel gone without progress? A loop that sees this
    /// consumes into an `ERROR` node rather than looking again.
    pub fn is_stuck(&self) -> bool {
        self.fuel == 0
    }

    /// Enters one level of grammar recursion; `false` at [`MAX_DEPTH`],
    /// where the caller consumes its construct iteratively (Mahout's
    /// `grammar::absorb_balanced`). The refusal is raised once per file,
    /// as php's `memory exhausted`.
    pub fn enter_depth(&mut self) -> bool {
        if self.depth >= MAX_DEPTH {
            if !self.depth_reported {
                self.depth_reported = true;
                let span = self.current_span();
                let line = self.line_here();
                self.error(Diagnostic::new(span, line, "memory exhausted"));
            }
            return false;
        }
        self.depth += 1;
        true
    }

    /// Leaves one level of grammar recursion.
    pub fn exit_depth(&mut self) {
        debug_assert!(self.depth > 0, "exit_depth without enter_depth");
        self.depth = self.depth.saturating_sub(1);
    }

    fn spend_fuel(&mut self) {
        self.fuel = self.fuel.saturating_sub(1);
        self.burst += 1;
        debug_assert!(
            self.burst <= self.burst_cap,
            "parser spinning: {} lookaheads without consuming at token {}",
            self.burst,
            self.pos
        );
    }

    /// Ends the parse: the event stream, for the builder.
    pub fn finish(self) -> Vec<Event> {
        self.events
    }
}

impl Marker {
    /// Completes the node as `kind`.
    pub fn complete(mut self, p: &mut Parser<'_>, kind: SyntaxKind) -> CompletedMarker {
        self.settled = true;
        match p.events.get_mut(self.event_index as usize) {
            Some(Event::Start { kind: slot, .. }) => *slot = kind,
            _ => debug_assert!(false, "a marker points at a Start event"),
        }
        p.events.push(Event::Finish);
        CompletedMarker {
            start_event: self.event_index,
            kind,
        }
    }

    /// Abandons the node: its children attach to the surrounding node.
    pub fn abandon(mut self, p: &mut Parser<'_>) {
        self.settled = true;
        if self.event_index as usize + 1 == p.events.len() {
            // Nothing since: the placeholder goes.
            p.events.pop();
        }
        // Otherwise the tombstone stays and the builder passes over it.
    }
}

impl CompletedMarker {
    /// Re-opens this node's position so a new node wraps it: the Pratt
    /// core's left operand.
    pub fn precede(self, p: &mut Parser<'_>) -> Marker {
        let new = p.start();
        match p.events.get_mut(self.start_event as usize) {
            Some(Event::Start { forward_parent, .. }) => {
                *forward_parent = Some(new.event_index - self.start_event);
            }
            _ => debug_assert!(false, "a completed marker points at a Start event"),
        }
        new
    }
}

/// One of php's three bracket pairs. Every delimiter token classifies
/// through this one type, so the opener glyph, the closer glyph and the
/// matching are read from one place.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Bracket {
    Paren,
    Curly,
    Brack,
}

impl Bracket {
    /// The pair an opener opens, `#[` and `${` sharing `[` and `{`.
    fn from_opener(opener: SyntaxKind) -> Option<Bracket> {
        Some(match opener {
            SyntaxKind::L_PAREN => Bracket::Paren,
            SyntaxKind::L_CURLY | SyntaxKind::DOLLAR_L_CURLY => Bracket::Curly,
            SyntaxKind::L_BRACK | SyntaxKind::ATTRIBUTE_OPEN => Bracket::Brack,
            _ => return None,
        })
    }

    /// The pair a closer closes.
    fn from_closer(closer: SyntaxKind) -> Option<Bracket> {
        Some(match closer {
            SyntaxKind::R_PAREN => Bracket::Paren,
            SyntaxKind::R_CURLY => Bracket::Curly,
            SyntaxKind::R_BRACK => Bracket::Brack,
            _ => return None,
        })
    }

    /// The opener glyph php names in a balance message.
    fn open_glyph(self) -> char {
        match self {
            Bracket::Paren => '(',
            Bracket::Curly => '{',
            Bracket::Brack => '[',
        }
    }

    /// The closer glyph php names in a balance message.
    fn close_glyph(self) -> char {
        match self {
            Bracket::Paren => ')',
            Bracket::Curly => '}',
            Bracket::Brack => ']',
        }
    }
}

/// How many bytes of a token's text php prints before `...`.
const SHOWN: usize = 30;

/// The unexpected token as php names it in `syntax error, unexpected …`:
/// `zend_yytnamerr`'s unexpected-token half (`Zend/zend_language_parser.y`
/// at 8.5.9), over the names its `%token` lines declare. A token bison
/// names in single quotes, every keyword, cast and punctuation mark, is
/// `token "<canonical>"` whatever the source spelled: `DIE` is `exit`,
/// `( Int )` is `(int)`, `<>` is `!=`, `__class__` is `__CLASS__`. A token
/// bison names with a class word carries its text: `identifier "foo"`,
/// `variable "$x"`, `integer "0x1"`, `floating-point number ".5"`, the
/// three namespaced-name kinds, `heredoc start "<<<EOT"`. The text is cut
/// at its first newline, one layer of quotes is taken off it, and past
/// thirty bytes it ends in `...`; a quoted string says which quote it
/// opened with. A bare `"` is `double-quote mark` (a `"` opening a
/// string with no interpolation is the parser's `double-quoted string`,
/// which needs the tokens after it), and an invalid byte is
/// `character 0x00`. The end of the input is the caller's `end of file`.
///
/// Measured under the pin at 8.5.9: `const die` reports `unexpected
/// token "exit", expecting identifier`; `1 (   float  )` reports `token
/// "(float)"`; `$'abc'` reports `single-quoted string "abc"`;
/// `1 $abcdefghijklmnopqrstuvwxyz0123456` reports `variable
/// "$abcdefghijklmnopqrstuvwxyz012..."`; `1 b'abc'` reports `quoted
/// string "B'abc"`, the prefix keeping php from seeing the quote.
///
/// ```php
/// echo 1 "abc";
/// ```
/// ```expect exit=255
/// Parse error: syntax error, unexpected double-quoted string "abc", expecting "," or ";" in %s on line 2
/// ```
pub fn php_token_name(kind: SyntaxKind, text: &[u8]) -> String {
    if kind == SyntaxKind::DOUBLE_QUOTE {
        return "double-quote mark".to_owned();
    }
    if let Some(canonical) = canonical_spelling(kind, text) {
        return format!("token \"{canonical}\"");
    }
    if kind == SyntaxKind::ERROR
        && let Some(byte) = text.first()
    {
        // One ERROR token spans every stray byte in a run; php stops at
        // the first.
        return format!("character 0x{byte:02X}");
    }
    let mut class = match kind {
        SyntaxKind::INT => "integer",
        SyntaxKind::FLOAT => "floating-point number",
        SyntaxKind::IDENT | SyntaxKind::MEMBER_IDENT => "identifier",
        SyntaxKind::NAME_FULLY_QUALIFIED => "fully qualified name",
        SyntaxKind::NAME_RELATIVE => "namespace-relative name",
        SyntaxKind::NAME_QUALIFIED => "namespaced name",
        SyntaxKind::VARIABLE => "variable",
        SyntaxKind::STRING_TEXT => "string content",
        SyntaxKind::SINGLE_QUOTED => "quoted string",
        SyntaxKind::ENCAPS_INT => "number",
        SyntaxKind::HEREDOC_START => "heredoc start",
        SyntaxKind::HEREDOC_END => "heredoc end",
        SyntaxKind::OPEN_TAG => "open tag",
        // A `%token` line without a name prints bison's own.
        SyntaxKind::INLINE_HTML => "T_INLINE_HTML",
        _ => "token",
    };
    let mut content = text.split(|&b| b == b'\n').next().unwrap_or(&[]);
    if class == "quoted string" {
        match content.first() {
            Some(b'"') => class = "double-quoted string",
            Some(b'\'') => class = "single-quoted string",
            _ => {}
        }
    }
    if let Some((b'\'' | b'"', rest)) = content.split_first() {
        content = rest;
    }
    if let Some((b'\'' | b'"', rest)) = content.split_last() {
        content = rest;
    }
    if content.len() > SHOWN + 3 {
        return format!(
            "{class} \"{}...\"",
            String::from_utf8_lossy(&content[..SHOWN])
        );
    }
    format!("{class} \"{}\"", String::from_utf8_lossy(content))
}

/// The single-quoted name bison gives a token that has one form: the
/// keyword lowercased, the cast in its canonical spelling, the mark as
/// written; `None` for a token bison names by a class word.
fn canonical_spelling(kind: SyntaxKind, text: &[u8]) -> Option<String> {
    Some(match kind {
        SyntaxKind::DIE_KW | SyntaxKind::EXIT_KW => "exit".to_owned(),
        SyntaxKind::LT_GT => "!=".to_owned(),
        SyntaxKind::CAST | SyntaxKind::CAST_REMOVED => {
            let name = match crate::php_names::cast_name(text) {
                "T_INT_CAST" => "int",
                "T_BOOL_CAST" => "bool",
                "T_DOUBLE_CAST" => "float",
                "T_STRING_CAST" => "string",
                "T_ARRAY_CAST" => "array",
                "T_OBJECT_CAST" => "object",
                "T_UNSET_CAST" => "unset",
                _ => "void",
            };
            format!("({name})")
        }
        SyntaxKind::MAGIC_CONST => String::from_utf8_lossy(text).to_ascii_uppercase(),
        // `callable` is `T_CALLABLE` to php's scanner and an `IDENT` to
        // the lexer here: spelled as the keyword it is there.
        SyntaxKind::IDENT if text.eq_ignore_ascii_case(b"callable") => "callable".to_owned(),
        _ if kind.is_keyword() => String::from_utf8_lossy(text).to_ascii_lowercase(),
        // The scanner hands the parser `;` for `?>`.
        SyntaxKind::CLOSE_TAG => ";".to_owned(),
        _ if kind.is_punct() || kind == SyntaxKind::OPEN_TAG_ECHO => {
            String::from_utf8_lossy(text).into_owned()
        }
        _ => return None,
    })
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]
mod tests;

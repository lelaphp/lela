//! The lexer (design §4.9): a scanner over `&[u8]` driven by an explicit
//! state stack that mirrors Zend's scanner states, one variant per `ST_*`
//! of `zend_language_scanner.l`. Whitespace, comments, inline HTML, the
//! halt-compiler trailer and heredoc indentation records are tokens of
//! their own, never attachments, so the token texts concatenated in order
//! reproduce the input byte for byte.
//!
//! ## The single-`&` decision
//!
//! php-src 8.1 split `&` into `T_AMPERSAND_FOLLOWED_BY_VAR_OR_VARARG` and
//! `T_AMPERSAND_NOT_FOLLOWED_BY_VAR_OR_VARARG` to de-fang the
//! `function f(A&B $x)` against `function f(A &$x)` ambiguity in its LALR
//! grammar. This lexer emits one [`SyntaxKind::AMP`]: a recursive-descent
//! parser resolves the same ambiguity with one token of lookahead.

mod heredoc;
mod numbers;
mod scripting;
mod strings;
#[cfg(test)]
mod tests;

use php_heap::MappedVec;

use crate::SyntaxKind;
use crate::line_index::LineIndex;
use crate::source::{Diagnostic, FileId, Span};

/// One lexed token: its kind, where it starts and how long it is. The
/// tokens of one lex are contiguous, so the offset of each is the sum of
/// the lengths before it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    /// The token's kind (a token-level [`SyntaxKind`], never a node kind).
    pub kind: SyntaxKind,
    /// Byte offset of the token's first byte.
    pub offset: u32,
    /// Byte length of the token text.
    pub len: u32,
}

/// What the lexer is told before it starts.
#[derive(Clone, Debug)]
pub struct LexOptions {
    /// Honour `<?` short open tags: php's `short_open_tag`, whose built-in
    /// default is on. `<?=` is an open tag regardless of this setting.
    pub short_open_tag: bool,
    /// Where in php's grammar the first byte stands.
    pub position: Position,
}

impl Default for LexOptions {
    fn default() -> Self {
        LexOptions {
            short_open_tag: true,
            position: Position::AtOpenTag,
        }
    }
}

/// Where the scanner begins, php's `zend_compile_position`
/// (`Z/Zend/zend_compile.h:857`): a file begins in `INITIAL`, where bytes
/// are inline HTML until an open tag; the string `compile_string` is
/// handed for `eval` begins in `ST_IN_SCRIPTING`, as though an open tag
/// stood before its first byte (`ZEND_COMPILE_POSITION_AFTER_OPEN_TAG`,
/// `Z/Zend/zend_language_scanner.l:824`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Position {
    /// `ZEND_COMPILE_POSITION_AT_OPEN_TAG`: inline HTML first.
    #[default]
    AtOpenTag,
    /// `ZEND_COMPILE_POSITION_AFTER_OPEN_TAG`: code first.
    AfterOpenTag,
    /// `ZEND_COMPILE_POSITION_AT_SHEBANG`, where `open_file_for_scanning`
    /// begins a file under `CG(skip_shebang)`: a first line that opens
    /// with `#!` is passed over through its line end, and inline HTML
    /// follows (`Z/Zend/zend_language_scanner.l:2288-2297`).
    AtShebang,
}

/// What one lex produced: the token stream and the scanner's diagnostics.
#[derive(Debug)]
pub struct Lexed {
    /// The tokens, in source order. No EOF sentinel; the sum of the
    /// lengths equals the input length.
    pub tokens: MappedVec<Token>,
    /// What php's scanner raises on its own, in emission order: what it
    /// throws (`Unterminated comment starting line 3`,
    /// `Invalid numeric literal`, the heredoc indentation rules, the
    /// `(real)` cast) and
    /// what it warns of and scans past (the octal escape overflow, the
    /// non-canonical cast spelling). What the scanner hands the parser
    /// as a token (a stray byte, an unterminated string) is the parser's
    /// to name, and raises nothing here.
    pub diagnostics: Vec<Diagnostic>,
}

/// Lexes `text` from the beginning, in the state
/// [`LexOptions::position`] names: [`State::InlineHtml`], Zend's
/// `INITIAL`, for a file, and [`State::Scripting`] after an open tag.
#[must_use]
pub fn lex(file: FileId, text: &[u8], options: &LexOptions) -> Lexed {
    // In no arena: a lone lex, as `token_get_all` makes one per call, has
    // nothing else to bump and would map a chunk for this table alone.
    let breaks = crate::line_index::breaks(text);
    lex_with_lines(
        file,
        text,
        options,
        &LineIndex::over(&breaks, text),
        MappedVec::new(),
        &mut Vec::new(),
        &mut MappedVec::new(),
    )
}

/// [`lex`] over a line table already built for `text`, which `parse`
/// keeps for the parser and hands back on `Parsed`, into `tokens`, with
/// `states` the room of the state stack: both a parse lends and takes
/// back, the stack's handed back empty in `states`. `significant` is
/// filled with the index of every token that is no whitespace, comment
/// or heredoc indentation, as each is emitted, which is the parser's
/// cursor ([`crate::parser::Input`]).
#[must_use]
pub(crate) fn lex_with_lines(
    file: FileId,
    text: &[u8],
    options: &LexOptions,
    lines: &LineIndex<'_>,
    tokens: MappedVec<Token>,
    states: &mut Vec<StateFrame>,
    significant: &mut MappedVec<u32>,
) -> Lexed {
    let mut scan = Lexer::new(file, text, options, lines, tokens, std::mem::take(states));
    scan.significant = std::mem::take(significant);
    scan.significant.clear();
    let (lexed, mut above, indexed) = scan.lex(options.position);
    above.clear();
    *states = above;
    *significant = indexed;
    lexed
}

/// A scanner state. The stack's factoring and the names are Zend's, so a
/// conformance question reads against `zend_language_scanner.l` directly.
/// The last element of the stack is the active state; Zend's `BEGIN(S)`
/// replaces the top, `yy_push_state` and `yy_pop_state` push and pop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum State {
    /// Verbatim HTML up to the next open tag. Zend: `INITIAL`.
    InlineHtml,
    /// Ordinary PHP code. Zend: `ST_IN_SCRIPTING`. Also pushed by `{`, both
    /// the scripting `{` and the `{$` interpolation opener: brace depth is
    /// repeated `Scripting` entries, exactly Zend's
    /// `yy_push_state(ST_IN_SCRIPTING)`.
    Scripting,
    /// Inside `"…"`. Zend: `ST_DOUBLE_QUOTES`.
    DoubleQuoted,
    /// Inside an interpolating heredoc body. Zend: `ST_HEREDOC`. Carries
    /// the label the closing marker must match.
    Heredoc {
        /// The heredoc label, raw bytes (labels admit `\x80-\xFF`).
        label: Vec<u8>,
    },
    /// Inside a nowdoc (`<<<'L'`) body. Zend: `ST_NOWDOC`.
    Nowdoc {
        /// The nowdoc label, raw bytes.
        label: Vec<u8>,
    },
    /// Inside `` `…` ``. Zend: `ST_BACKQUOTE`.
    Backquote,
    /// After `->` or `?->`, where a keyword lexes as a plain identifier.
    /// Zend: `ST_LOOKING_FOR_PROPERTY`.
    LookingForProperty,
    /// After `${` in an interpolating string. Zend: `ST_LOOKING_FOR_VARNAME`.
    LookingForVarName,
    /// Inside `$a[…]` within a string (the restricted dim grammar).
    /// Zend: `ST_VAR_OFFSET`.
    VarOffset,
    /// Everything after `__halt_compiler();`, one verbatim data token. No
    /// Zend scanner state: php-src halts in the parser
    /// (`zend_do_halt_compiler`); the trailer is a token here so the
    /// stream stays lossless and `__COMPILER_HALT_OFFSET__` reads it.
    HaltCompiler,
}

/// Per-state bookkeeping the [`State`] itself does not carry: the opener
/// span, for an unterminated-construct diagnostic, and the lazily computed
/// heredoc terminator facts.
pub(crate) struct StateAux {
    /// Where the construct opened: the `"` or `` ` `` token, or the `<<<`.
    pub(crate) opener: Span,
    /// Lazily computed flexible-heredoc facts (`Heredoc` and `Nowdoc` only).
    pub(crate) heredoc: Option<heredoc::HeredocInfo>,
}

/// One entry of the state stack: the [`State`] with its [`StateAux`]. One
/// stack carries both so every push, pop and replacement moves them together.
pub(crate) struct StateFrame {
    /// The scanner state this entry is.
    pub(crate) state: State,
    /// The entry's bookkeeping.
    pub(crate) aux: StateAux,
}

/// The state stack: the base state, which is never popped, and the
/// states pushed above it, the last of them the active one. The base
/// stands inline, so a source that pushes no state, an `eval` of a
/// statement or two, allocates nothing for the stack.
pub(crate) struct States {
    base: StateFrame,
    above: Vec<StateFrame>,
}

impl States {
    /// The stack of `base` alone, its states above pushed into `above`'s
    /// room.
    fn new(base: StateFrame, mut above: Vec<StateFrame>) -> States {
        above.clear();
        States { base, above }
    }

    /// The active entry.
    pub(crate) fn top(&self) -> &StateFrame {
        self.above.last().unwrap_or(&self.base)
    }

    /// The active entry, to replace or annotate.
    pub(crate) fn top_mut(&mut self) -> &mut StateFrame {
        match self.above.last_mut() {
            Some(top) => top,
            None => &mut self.base,
        }
    }

    /// How many entries stand, the base among them.
    pub(crate) fn depth(&self) -> usize {
        self.above.len() + 1
    }

    /// Every entry, the base first.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &StateFrame> {
        std::iter::once(&self.base).chain(&self.above)
    }

    /// A state pushed above the active one.
    fn push(&mut self, frame: StateFrame) {
        self.above.push(frame);
    }

    /// The active state popped, unless it is the base, which stays.
    fn pop(&mut self) {
        self.above.pop();
    }
}

/// Progress of the `__halt_compiler ( ) ;` recognizer, `?>` standing as
/// the `;`. Armed when `HALT_COMPILER_KW` is emitted in `Scripting`,
/// disarmed when the next significant tokens deviate from `( ) ;`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum HaltStage {
    Off,
    ExpectLParen,
    ExpectRParen,
    ExpectSemicolon,
}

/// The active state's discriminant, so dispatch can match without borrowing
/// the stack.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum StateKind {
    InlineHtml,
    Scripting,
    DoubleQuoted,
    Heredoc,
    Nowdoc,
    Backquote,
    LookingForProperty,
    LookingForVarName,
    VarOffset,
    HaltCompiler,
}

pub(crate) struct Lexer<'a> {
    pub(crate) file: FileId,
    pub(crate) text: &'a [u8],
    /// The line table, for the line a diagnostic reports.
    lines: &'a LineIndex<'a>,
    pub(crate) short_open_tag: bool,
    /// Current scan position; always a token boundary between dispatches.
    pub(crate) pos: usize,
    /// The state stack ([`States`]).
    pub(crate) frames: States,
    pub(crate) tokens: MappedVec<Token>,
    pub(crate) diags: Vec<Diagnostic>,
    halt: HaltStage,
    /// End offset of the last emitted token (the coverage invariant).
    emitted: usize,
    /// The index of every significant token emitted ([`lex_with_lines`]).
    significant: MappedVec<u32>,
}

impl<'a> Lexer<'a> {
    pub(crate) fn new(
        file: FileId,
        text: &'a [u8],
        options: &LexOptions,
        lines: &'a LineIndex<'a>,
        mut tokens: MappedVec<Token>,
        above: Vec<StateFrame>,
    ) -> Lexer<'a> {
        // The 4 GiB cap `load_bytes` holds is what makes every `narrow`
        // below exact. Assert it once, at the one construction site.
        debug_assert!(
            u32::try_from(text.len()).is_ok(),
            "lexer input must be <= 4 GiB (load_bytes caps it before the lexer)"
        );
        let here = Span {
            file,
            start: 0,
            len: 0,
        };
        Lexer {
            file,
            text,
            lines,
            short_open_tag: options.short_open_tag,
            pos: 0,
            frames: States::new(
                StateFrame {
                    state: match options.position {
                        Position::AtOpenTag | Position::AtShebang => State::InlineHtml,
                        Position::AfterOpenTag => State::Scripting,
                    },
                    aux: StateAux {
                        opener: here,
                        heredoc: None,
                    },
                },
                above,
            ),
            // php source runs four to six bytes a token: room for a
            // token every four, so a small source never grows the stream
            // and a large one seldom does.
            tokens: {
                tokens.clear();
                tokens.reserve(text.len() / 4 + 4);
                tokens
            },
            diags: Vec::new(),
            halt: HaltStage::Off,
            emitted: 0,
            significant: MappedVec::new(),
        }
    }

    /// The stream and the diagnostics, the room of the state stack
    /// above its base, and the index of the significant tokens.
    pub(crate) fn lex(mut self, position: Position) -> (Lexed, Vec<StateFrame>, MappedVec<u32>) {
        if position == Position::AtShebang {
            self.lex_shebang();
        }
        while self.pos < self.text.len() {
            #[cfg(debug_assertions)]
            let before = (self.pos, self.frames.depth(), self.state_kind());
            match self.state_kind() {
                StateKind::InlineHtml => self.lex_inline_html(),
                StateKind::Scripting => self.lex_scripting(),
                StateKind::DoubleQuoted => self.lex_quoted(b'"', SyntaxKind::DOUBLE_QUOTE),
                StateKind::Backquote => self.lex_quoted(b'`', SyntaxKind::BACKQUOTE),
                StateKind::Heredoc => self.lex_heredoc_body(true),
                StateKind::Nowdoc => self.lex_heredoc_body(false),
                StateKind::LookingForProperty => self.lex_looking_for_property(),
                StateKind::LookingForVarName => self.lex_looking_for_varname(),
                StateKind::VarOffset => self.lex_var_offset(),
                StateKind::HaltCompiler => self.lex_halt_data(),
            }
            // Forward progress: every step consumes a byte or changes the
            // state stack.
            #[cfg(debug_assertions)]
            debug_assert!(
                (self.pos, self.frames.depth(), self.state_kind()) != before,
                "lexer made no progress at offset {} in {:?}",
                self.pos,
                before.2,
            );
        }
        debug_assert_eq!(self.emitted, self.text.len(), "token coverage gap");
        (
            Lexed {
                tokens: self.tokens,
                diagnostics: self.diags,
            },
            self.frames.above,
            self.significant,
        )
    }

    /// The active (top-of-stack) state. The base state is never popped, so
    /// the stack is never empty.
    pub(crate) fn active_state(&self) -> &State {
        &self.frames.top().state
    }

    fn state_kind(&self) -> StateKind {
        match self.active_state() {
            State::InlineHtml => StateKind::InlineHtml,
            State::Scripting => StateKind::Scripting,
            State::DoubleQuoted => StateKind::DoubleQuoted,
            State::Heredoc { .. } => StateKind::Heredoc,
            State::Nowdoc { .. } => StateKind::Nowdoc,
            State::Backquote => StateKind::Backquote,
            State::LookingForProperty => StateKind::LookingForProperty,
            State::LookingForVarName => StateKind::LookingForVarName,
            State::VarOffset => StateKind::VarOffset,
            State::HaltCompiler => StateKind::HaltCompiler,
        }
    }

    // ----- token and diagnostic plumbing ------------------------------------

    /// Emits the token covering `start..self.pos`. Every dispatch site emits
    /// a token before it changes the state stack for it.
    pub(crate) fn emit(&mut self, kind: SyntaxKind, start: usize) {
        debug_assert!(self.pos > start, "empty token");
        self.emit_upto(kind, start);
    }

    /// Emits the one zero-width token php's scanner returns, `yyless(0)`
    /// before `RETURN_TOKEN`: `T_ENCAPSED_AND_WHITESPACE` at the byte
    /// that pops `ST_VAR_OFFSET` ([`Lexer::lex_var_offset`]). The byte is
    /// not consumed, so the stream stays contiguous and the next token
    /// starts where this one does.
    pub(crate) fn emit_empty(&mut self, kind: SyntaxKind) {
        self.emit_upto(kind, self.pos);
    }

    fn emit_upto(&mut self, kind: SyntaxKind, start: usize) {
        debug_assert_eq!(start, self.emitted, "token not contiguous");
        let index = narrow(self.tokens.len());
        self.tokens.push(Token {
            kind,
            offset: narrow(start),
            len: narrow(self.pos - start),
        });
        self.emitted = self.pos;
        if !kind.is_trivia() {
            self.significant.push(index);
            if self.halt != HaltStage::Off || kind == SyntaxKind::HALT_COMPILER_KW {
                self.note_halt_progress(kind);
            }
        }
    }

    pub(crate) fn diag(&mut self, d: Diagnostic) {
        self.diags.push(d);
    }

    /// The 1-based line of `offset`, for the line a diagnostic reports.
    pub(crate) fn line(&self, offset: usize) -> u32 {
        self.lines.line_number(narrow(offset))
    }

    pub(crate) fn span(&self, start: usize, end: usize) -> Span {
        Span::new(self.file, narrow(start), narrow(end))
    }

    // ----- state-stack plumbing ----------------------------------------------

    /// Zend `yy_push_state(state)`.
    pub(crate) fn push_state(&mut self, state: State, opener: Span) {
        self.frames.push(StateFrame {
            state,
            aux: StateAux {
                opener,
                heredoc: None,
            },
        });
    }

    /// Zend `yy_pop_state()`. The base state stays.
    pub(crate) fn pop_state(&mut self) {
        debug_assert!(self.frames.depth() > 1, "popping the base state");
        self.frames.pop();
    }

    /// Zend `BEGIN(state)`: replace the active state, keep the stack below.
    pub(crate) fn begin_state(&mut self, state: State, opener: Span) {
        let frame = StateFrame {
            state,
            aux: StateAux {
                opener,
                heredoc: None,
            },
        };
        *self.frames.top_mut() = frame;
    }

    /// May `}` pop? Zend pops only when its state stack is non-empty.
    pub(crate) fn can_pop_brace(&self) -> bool {
        self.frames.depth() > 1
    }

    /// `<SHEBANG>"#!" .* {NEWLINE}`: the line and its end, `.*` running
    /// to the last byte before a `\n` and backing off to a `\r` where the
    /// file has none; any other first byte is `yyless(0)` into `INITIAL`.
    fn lex_shebang(&mut self) {
        if !self.text.starts_with(b"#!") {
            return;
        }
        let end = match self.text.iter().position(|&b| b == b'\n') {
            Some(newline) => newline + 1,
            None => match self.text.iter().rposition(|&b| b == b'\r') {
                Some(carriage) => carriage + 1,
                None => return,
            },
        };
        self.pos = end;
        self.emit(SyntaxKind::SHEBANG, 0);
    }

    // ----- byte helpers ------------------------------------------------------

    /// Byte at `i`, or 0 past the end. NUL is never a positive match in any
    /// rule (it is an `ERROR` byte in scripting), so the sentinel is safe.
    pub(crate) fn byte(&self, i: usize) -> u8 {
        self.text.get(i).copied().unwrap_or(0)
    }

    pub(crate) fn starts_with_ci(&self, at: usize, pat: &[u8]) -> bool {
        self.text.get(at..).is_some_and(|rest| {
            rest.len() >= pat.len() && rest[..pat.len()].eq_ignore_ascii_case(pat)
        })
    }

    // ----- the __halt_compiler recognizer ------------------------------------

    /// The recognizer's next stage after a significant token `kind`:
    /// whitespace and comments between the pieces are fine, and are not
    /// handed here ([`Lexer::emit_upto`]), nor is any token while the
    /// recognizer is off and the token does not arm it.
    fn note_halt_progress(&mut self, kind: SyntaxKind) {
        self.halt = match (self.halt, kind) {
            (_, SyntaxKind::HALT_COMPILER_KW) => {
                // Arm only for the statement form in the outermost scripting
                // entry: every `{` pushes a nested `State::Scripting`, so
                // "at brace depth zero" is exactly "the active Scripting
                // entry is the only one on the stack". Zend stops lexing
                // only when the top-level halt production reduces; inside
                // any `{ … }` the trailer is ordinary code (bug60099: the
                // unclosed `{` must stay visible to the parser). The
                // LookingForProperty remap already turned
                // `->__halt_compiler` into IDENT before this point.
                let outermost_scripting = matches!(self.active_state(), State::Scripting)
                    && self
                        .frames
                        .iter()
                        .filter(|f| matches!(f.state, State::Scripting))
                        .count()
                        == 1;
                if outermost_scripting {
                    HaltStage::ExpectLParen
                } else {
                    HaltStage::Off
                }
            }
            (HaltStage::ExpectLParen, SyntaxKind::L_PAREN) => HaltStage::ExpectRParen,
            (HaltStage::ExpectRParen, SyntaxKind::R_PAREN) => HaltStage::ExpectSemicolon,
            (HaltStage::ExpectSemicolon, SyntaxKind::SEMICOLON | SyntaxKind::CLOSE_TAG) => {
                // Everything after the `;` is the verbatim data trailer. A
                // close tag stands as the `;` (Zend returns `?>` as `';'`,
                // and the halt production reduces on it), so the trailer
                // begins after the tag and the one newline it swallowed:
                // `__COMPILER_HALT_OFFSET__` is that byte (probed 8.5.9).
                let here = self.span(self.pos, self.pos);
                self.push_state(State::HaltCompiler, here);
                HaltStage::Off
            }
            _ => HaltStage::Off,
        };
    }

    /// `HaltCompiler`: the rest of the input is one `HALT_DATA` token (an
    /// empty trailer emits nothing). Composer and phar stubs land here.
    fn lex_halt_data(&mut self) {
        let start = self.pos;
        self.pos = self.text.len();
        if self.pos > start {
            self.emit(SyntaxKind::HALT_DATA, start);
        }
    }

    // ----- InlineHtml (Zend INITIAL) -----------------------------------------

    /// The verbatim segment up to the next open tag, then the open tag.
    fn lex_inline_html(&mut self) {
        let start = self.pos;
        let mut i = self.pos;
        while i < self.text.len() {
            if self.text[i] == b'<'
                && let Some((kind, len)) = self.open_tag_at(i)
            {
                if i > start {
                    self.pos = i;
                    self.emit(SyntaxKind::INLINE_HTML, start);
                }
                let tag_start = i;
                self.pos = i + len;
                self.emit(kind, tag_start);
                // Zend: BEGIN(ST_IN_SCRIPTING).
                self.begin_state(State::Scripting, self.span(tag_start, tag_start + len));
                return;
            }
            i += 1;
        }
        self.pos = self.text.len();
        if self.pos > start {
            self.emit(SyntaxKind::INLINE_HTML, start);
        }
    }

    /// Is there an open tag at `i`? Its kind and byte length if so.
    ///
    /// Zend's `INITIAL` rules, in order: `"<?="` (always); `"<?php"`
    /// followed by one whitespace character, which is part of the token as
    /// it is of `T_OPEN_TAG`, or sitting at EOF; `"<?"` under
    /// `short_open_tag`. Tags match case-insensitively (`<?PHP` is real).
    fn open_tag_at(&self, i: usize) -> Option<(SyntaxKind, usize)> {
        if self.byte(i + 1) != b'?' {
            return None;
        }
        if self.byte(i + 2) == b'=' {
            return Some((SyntaxKind::OPEN_TAG_ECHO, 3));
        }
        if self.starts_with_ci(i, b"<?php") {
            // Zend: "<?php"([ \t]|{NEWLINE}); NEWLINE is \r\n, \n, or \r.
            match self.byte(i + 5) {
                b'\r' if self.byte(i + 6) == b'\n' => return Some((SyntaxKind::OPEN_TAG, 7)),
                b' ' | b'\t' | b'\n' | b'\r' => return Some((SyntaxKind::OPEN_TAG, 6)),
                _ if i + 5 == self.text.len() => return Some((SyntaxKind::OPEN_TAG, 5)),
                _ => {} // `<?phpx`: fall through to the short-tag rule
            }
        }
        if self.short_open_tag {
            return Some((SyntaxKind::OPEN_TAG, 2));
        }
        None
    }
}

/// Offsets fit `u32` by construction: `load_bytes` refuses a file over
/// 4 GiB before it reaches the lexer.
pub(crate) fn narrow(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

// ----- shared character classes (Zend LABEL) ----------------------------------

/// First byte of a PHP label: `[a-zA-Z_\x80-\xFF]`.
pub(crate) fn is_label_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_' || b >= 0x80
}

/// A later byte of a PHP label: `[a-zA-Z0-9_\x80-\xFF]`, read from a
/// table, as re2c's scanner reads its classes, since a name's every byte
/// asks.
pub(crate) fn is_label_continue(b: u8) -> bool {
    LABEL_CONTINUE[usize::from(b)]
}

/// [`is_label_continue`] for every byte.
static LABEL_CONTINUE: [bool; 256] = {
    let mut table = [false; 256];
    let mut b = 0;
    while b < 256 {
        table[b] = matches!(b, 0x30..=0x39 | 0x41..=0x5A | 0x5F | 0x61..=0x7A | 0x80..=0xFF);
        b += 1;
    }
    table
};

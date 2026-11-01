//! php's scanner view: one [`ScannedToken`] per token php's scanner
//! returns under `lex_scan`, named as php names it where a kind is one
//! token there too, and reconstituted here where the two scanners cut the
//! source differently (design §4.9). `token_get_all`, `highlight_string`,
//! `php_strip_whitespace` and the CLI's `-s` and `-w` read this one view.
//! What is decided here and nowhere else:
//!
//! - the keyword names, with `private(set)` and its two siblings fused
//!   from four tokens, `yield from` fused across the whitespace and
//!   comments between, `enum` a keyword only before a name, and `callable`,
//!   `set` and `from` read as php's scanner reads them;
//! - `T_CONSTANT_ENCAPSED_STRING` for a double-quoted string nothing
//!   interpolates in, which the lexer always opens as three tokens;
//! - one `T_ENCAPSED_AND_WHITESPACE` for a stretch of string text, the
//!   heredoc's `HEREDOC_INDENT` records put back into it, and the last
//!   one put back onto `T_END_HEREDOC`;
//! - `T_CURLY_OPEN`, `T_DOLLAR_OPEN_CURLY_BRACES` and `T_STRING_VARNAME`
//!   from the scanner state a `{` or a name stands in, which is a stack
//!   here as it is in Zend;
//! - `T_BAD_CHARACTER`, one per byte of a stretch the lexer refused, and
//!   `T_ENCAPSED_AND_WHITESPACE` for a `'` no `'` closes, which php's
//!   scanner returns as that rather than as a string;
//! - the `T_DNUMBER` an integer written past `ZEND_LONG_MAX` becomes;
//! - which tokens carry a value, `zendlval` set by
//!   `RETURN_TOKEN_WITH_VAL` or `RETURN_TOKEN_WITH_STR`, which is what
//!   `zend_highlight` colours a token that is none of its named kinds by;
//! - the line a token starts on, counted as `HANDLE_NEWLINES` counts, a
//!   `\n` or a `\r` no `\n` follows.
//!
//! What `token_get_all` decides on its own, the stop three tokens after
//! `T_HALT_COMPILER` that `tokenize()` counts, stays with `ext/tokenizer`.

use crate::value::{Quote, single_quoted, unescape};
use crate::{
    SyntaxKind, Token, amp_name, cast_name, int_overflows, magic_const_name, operator_name,
};

/// One token as php's scanner returns it: php's name where the token
/// has one and none for a single character php returns as itself, the
/// bytes it spans, and the line it starts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedToken {
    pub name: Option<&'static str>,
    pub start: usize,
    pub end: usize,
    pub line: u32,
}

impl ScannedToken {
    /// Whether php's scanner sets `zendlval` for the token,
    /// `RETURN_TOKEN_WITH_VAL` and `RETURN_TOKEN_WITH_STR`; a keyword, an
    /// operator and a character carry none.
    pub(crate) fn carries_value(&self) -> bool {
        matches!(
            self.name,
            Some(
                "T_LNUMBER"
                    | "T_DNUMBER"
                    | "T_STRING"
                    | "T_NAME_QUALIFIED"
                    | "T_NAME_FULLY_QUALIFIED"
                    | "T_NAME_RELATIVE"
                    | "T_VARIABLE"
                    | "T_STRING_VARNAME"
                    | "T_NUM_STRING"
                    | "T_INLINE_HTML"
                    | "T_ENCAPSED_AND_WHITESPACE"
                    | "T_CONSTANT_ENCAPSED_STRING"
            )
        )
    }
}

/// php's tokens for `source` over the lexer's stream of it, the
/// scanner's view whether a parser runs above it or not: what
/// `TOKEN_PARSE` changes is `ext/tokenizer`'s to put on these rows after.
pub fn scanned(source: &[u8], tokens: &[Token]) -> Vec<ScannedToken> {
    let mut walk = Walk {
        source,
        tokens,
        out: Vec::new(),
        contexts: vec![Context::Scripting],
        run: None,
        quotes: Vec::new(),
        line: 1,
        counted_to: 0,
        heap: None,
    };
    walk.all();
    walk.out
}

/// What php's scanner lays on the request's heap as it scans one source
/// under `zend_compile`, and holds until the compile ends: the
/// `zend_string` each token that carries a string is given
/// (`zend_copy_value`, `ZVAL_STRINGL`), which the tree holds until
/// `zend_ast_destroy` (`Z/Zend/zend_language_scanner.l:632`), and the
/// deepest its two `zend_stack`s go, which `zend_restore_lexical_state`
/// destroys after it. A string of no byte or one is php's interned one,
/// and lays nothing.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ScannerHeap {
    /// The bytes of each string, one after another.
    bytes: Vec<u8>,
    /// Each string: the length `zend_string_alloc` was asked for, the
    /// bytes scanned before an escape was read, and where its bytes end
    /// in `bytes`.
    strings: Vec<(usize, usize)>,
    /// The deepest `SCNG(nest_location_stack)` goes: `enter_nesting` at
    /// each `(`, `[`, `#[`, `{`, `{$` and `${`, `exit_nesting` at each
    /// `)`, `]` and `}` (`:1273-1300`).
    pub nesting: usize,
    /// The deepest `SCNG(state_stack)` goes: `yy_push_state` at each `{`,
    /// `{$` and `${`, popped at its `}`, and for the name after each `->`
    /// and `?->` and the offset after a variable in a string (`:164`).
    pub states: usize,
}

impl ScannerHeap {
    /// Each string as its length asked of `zend_string_alloc`, and its
    /// bytes, its escapes read.
    pub fn strings(&self) -> impl Iterator<Item = (usize, &[u8])> {
        let mut from = 0;
        self.strings.iter().map(move |&(len, end)| {
            let bytes = self.bytes.get(from..end).unwrap_or(&[]);
            from = end;
            (len, bytes)
        })
    }

    /// A string of `len` bytes scanned whose value is `value`, where it
    /// lays one.
    fn string(&mut self, len: usize, value: &[u8]) {
        if len > 1 {
            self.bytes.extend_from_slice(value);
            self.strings.push((len, self.bytes.len()));
        }
    }
}

/// What php's scanner lays scanning `source` ([`ScannerHeap`]), read
/// off the lexer's `tokens` of it and kept in `arena` until
/// [`take_scanner_heap`] takes it, for the request whose compile in the
/// arena scanned it to charge. A function of the
/// crate's, not a method of the arena, so the facade extensions build
/// on, which names the arena, does not carry the compiler's bookkeeping.
pub fn note_scanner_heap(arena: &crate::Arena, source: &[u8], tokens: &[Token]) {
    arena.note_scanned(scanner_heap(source, tokens));
}

/// What [`note_scanner_heap`] kept in `arena` last, taken out.
#[must_use]
pub fn take_scanner_heap(arena: &crate::Arena) -> ScannerHeap {
    arena.take_scanned()
}

/// [`ScannerHeap`] for `source`, read off the lexer's `tokens` of it as
/// [`scanned`] reads php's tokens: a name, a variable's without its `$`,
/// a quoted string's between its quotes, a stretch of string text and a
/// stretch of inline HTML, each laid at the length scanned and holding
/// its bytes once its escapes are read.
fn scanner_heap(source: &[u8], tokens: &[Token]) -> ScannerHeap {
    let mut walk = Walk {
        source,
        tokens,
        out: Vec::new(),
        contexts: vec![Context::Scripting],
        run: None,
        quotes: Vec::new(),
        line: 1,
        counted_to: 0,
        heap: Some(Laying::default()),
    };
    walk.all();
    walk.heap.map(|laying| laying.heap).unwrap_or_default()
}

/// [`ScannerHeap`] as a walk lays it, with how deep each stack stands.
#[derive(Default)]
struct Laying {
    heap: ScannerHeap,
    nesting: usize,
    states: usize,
}

impl Laying {
    /// One more on the nesting stack.
    fn enter(&mut self) {
        self.nesting += 1;
        self.heap.nesting = self.heap.nesting.max(self.nesting);
    }

    /// One fewer on the nesting stack.
    fn exit(&mut self) {
        self.nesting = self.nesting.saturating_sub(1);
    }

    /// One more on the state stack.
    fn push(&mut self) {
        self.states += 1;
        self.heap.states = self.heap.states.max(self.states);
    }

    /// One fewer on the state stack.
    fn pop(&mut self) {
        self.states = self.states.saturating_sub(1);
    }

    /// One pushed and popped again before the next token stands.
    fn pass(&mut self) {
        self.heap.states = self.heap.states.max(self.states + 1);
    }
}

/// One of Zend's scanner states this walk must know to name a token: the
/// state decides whether a `{` is `T_CURLY_OPEN`, whether a `"` opens or
/// closes, and whether a `&` is a bare character.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Context {
    /// `ST_IN_SCRIPTING`, pushed again by `{`, `{$` and `${`.
    Scripting,
    /// `ST_DOUBLE_QUOTES` or `ST_BACKQUOTE`, by its delimiter kind.
    Quoted(SyntaxKind),
    /// `ST_HEREDOC` and `ST_NOWDOC`.
    Heredoc,
    /// `ST_VAR_OFFSET`, inside `$a[…]` in a string.
    VarOffset,
}

/// A stretch of string text not yet emitted: `STRING_TEXT` and
/// `HEREDOC_INDENT` tokens back to back, which php returns as one
/// `T_ENCAPSED_AND_WHITESPACE`.
struct Run {
    start: usize,
    end: usize,
    line: u32,
    /// Where the stretch's last token starts when it is a `HEREDOC_INDENT`,
    /// which belongs to the `T_END_HEREDOC` after it, not to the text.
    indent: Option<usize>,
}

struct Walk<'a> {
    source: &'a [u8],
    tokens: &'a [Token],
    out: Vec<ScannedToken>,
    contexts: Vec<Context>,
    run: Option<Run>,
    /// The index in `out` of each `"` still open, innermost last, for the
    /// fusion into `T_CONSTANT_ENCAPSED_STRING` at its close.
    quotes: Vec<usize>,
    line: u32,
    counted_to: usize,
    /// What the scanner lays, where [`scanner_heap`] asks.
    heap: Option<Laying>,
}

impl Walk<'_> {
    /// Every token.
    fn all(&mut self) {
        let mut i = 0;
        while i < self.tokens.len() {
            i = self.one(i);
        }
        self.flush();
    }

    fn span(&self, i: usize) -> (usize, usize) {
        let token = &self.tokens[i];
        let start = token.offset as usize;
        (start, start + token.len as usize)
    }

    fn text(&self, i: usize) -> &[u8] {
        let (start, end) = self.span(i);
        self.source.get(start..end).unwrap_or(&[])
    }

    /// The line `at` stands on: `HANDLE_NEWLINES` over the bytes since
    /// the last answer, which is what makes `\r\n` one line and a lone
    /// `\r` one too.
    fn line_at(&mut self, at: usize) -> u32 {
        let mut i = self.counted_to;
        while i < at {
            match self.source.get(i) {
                Some(b'\n') => self.line += 1,
                Some(b'\r') if self.source.get(i + 1) != Some(&b'\n') => self.line += 1,
                _ => {}
            }
            i += 1;
        }
        self.counted_to = at.max(self.counted_to);
        self.line
    }

    fn top(&self) -> Context {
        self.contexts.last().copied().unwrap_or(Context::Scripting)
    }

    fn in_string(&self) -> bool {
        matches!(self.top(), Context::Quoted(_) | Context::Heredoc)
    }

    fn push(&mut self, name: Option<&'static str>, start: usize, end: usize) {
        if let Some(name) = name {
            self.lay_token(name, start, end);
        }
        let line = self.line_at(start);
        self.out.push(ScannedToken {
            name,
            start,
            end,
            line,
        });
    }

    fn named(&mut self, name: &'static str, i: usize) {
        let (start, end) = self.span(i);
        self.push(Some(name), start, end);
    }

    fn character(&mut self, i: usize) {
        let (start, end) = self.span(i);
        self.push(None, start, end);
    }

    /// The pending text, as one `T_ENCAPSED_AND_WHITESPACE`.
    fn flush(&mut self) {
        if let Some(run) = self.run.take()
            && run.end > run.start
        {
            if let Some(laying) = &mut self.heap {
                let quote = match self.contexts.last() {
                    Some(Context::Quoted(SyntaxKind::BACKQUOTE)) => Quote::Backquote,
                    Some(Context::Heredoc) => Quote::Heredoc,
                    _ => Quote::Double,
                };
                let text = self.source.get(run.start..run.end).unwrap_or(&[]);
                laying.heap.string(text.len(), &unescape(text, quote));
            }
            self.out.push(ScannedToken {
                name: Some("T_ENCAPSED_AND_WHITESPACE"),
                start: run.start,
                end: run.end,
                line: run.line,
            });
        }
    }

    fn extend_run(&mut self, i: usize, indent: bool) {
        let (start, end) = self.span(i);
        let line = self.line_at(start);
        match &mut self.run {
            Some(run) => {
                run.end = end;
                run.indent = indent.then_some(start);
            }
            None => {
                self.run = Some(Run {
                    start,
                    end,
                    line,
                    indent: indent.then_some(start),
                });
            }
        }
    }

    /// The next token after `i` that is not whitespace or a comment, when
    /// at least one of those stands between: Zend's
    /// `{WHITESPACE_OR_COMMENTS}` in the `yield from` and `enum` rules.
    fn after_trivia(&self, i: usize) -> Option<usize> {
        let mut j = i + 1;
        while self.tokens.get(j).is_some_and(|t| {
            matches!(
                t.kind,
                SyntaxKind::WHITESPACE
                    | SyntaxKind::LINE_COMMENT
                    | SyntaxKind::BLOCK_COMMENT
                    | SyntaxKind::DOC_COMMENT
            )
        }) {
            j += 1;
        }
        (j > i + 1 && j < self.tokens.len()).then_some(j)
    }

    /// Whether the token before `i`, past whitespace alone, is `->` or
    /// `?->`: Zend's `ST_LOOKING_FOR_PROPERTY`, where a keyword is a name.
    fn names_a_property(&self, i: usize) -> bool {
        let mut j = i;
        while j > 0 && self.tokens[j - 1].kind == SyntaxKind::WHITESPACE {
            j -= 1;
        }
        j > 0
            && matches!(
                self.tokens[j - 1].kind,
                SyntaxKind::ARROW | SyntaxKind::NULLSAFE_ARROW
            )
    }

    /// Whether `i` and `i + 1` touch.
    fn adjacent(&self, i: usize) -> bool {
        self.tokens
            .get(i + 1)
            .is_some_and(|next| next.offset as usize == self.span(i).1)
    }

    /// One token, and the index of the next to read.
    fn one(&mut self, i: usize) -> usize {
        use SyntaxKind as K;
        let kind = self.tokens[i].kind;
        if let Some(next) = self.string_piece(i) {
            return next;
        }
        if self.top() == Context::VarOffset {
            return self.offset_token(i);
        }
        self.flush();
        self.lay_stacks(kind);
        if kind.is_keyword() {
            return self.keyword(i);
        }
        match kind {
            K::WHITESPACE => self.named("T_WHITESPACE", i),
            K::LINE_COMMENT | K::BLOCK_COMMENT => self.named("T_COMMENT", i),
            K::DOC_COMMENT => self.named("T_DOC_COMMENT", i),
            K::OPEN_TAG => self.named("T_OPEN_TAG", i),
            K::OPEN_TAG_ECHO => self.named("T_OPEN_TAG_WITH_ECHO", i),
            K::CLOSE_TAG => self.named("T_CLOSE_TAG", i),
            K::INLINE_HTML | K::HALT_DATA => self.named("T_INLINE_HTML", i),
            K::VARIABLE => {
                self.named("T_VARIABLE", i);
                if self.in_string() && self.adjacent(i) && self.tokens[i + 1].kind == K::L_BRACK {
                    self.contexts.push(Context::VarOffset);
                    if let Some(laying) = &mut self.heap {
                        laying.push();
                    }
                }
            }
            K::IDENT => self.ident(i),
            K::NAME_QUALIFIED => self.named("T_NAME_QUALIFIED", i),
            K::NAME_FULLY_QUALIFIED => self.named("T_NAME_FULLY_QUALIFIED", i),
            K::NAME_RELATIVE => self.named("T_NAME_RELATIVE", i),
            K::INT if int_overflows(self.text(i)) => self.named("T_DNUMBER", i),
            K::INT => self.named("T_LNUMBER", i),
            K::FLOAT => self.named("T_DNUMBER", i),
            K::SINGLE_QUOTED if unterminated(self.text(i)) => {
                self.named("T_ENCAPSED_AND_WHITESPACE", i);
            }
            K::SINGLE_QUOTED => self.named("T_CONSTANT_ENCAPSED_STRING", i),
            K::MAGIC_CONST => {
                let name = magic_const_name(self.text(i));
                self.named(name, i);
            }
            K::CAST | K::CAST_REMOVED => {
                let name = cast_name(self.text(i));
                self.named(name, i);
            }
            K::AMP => {
                let name = amp_name(self.source, self.span(i).1);
                self.named(name, i);
            }
            K::ERROR => self.bad_bytes(i),
            K::HEREDOC_START => {
                self.named("T_START_HEREDOC", i);
                self.contexts.push(Context::Heredoc);
            }
            K::DOUBLE_QUOTE | K::BACKQUOTE => self.delimiter(i),
            K::L_CURLY => {
                if self.in_string() {
                    self.named("T_CURLY_OPEN", i);
                } else {
                    self.character(i);
                }
                self.contexts.push(Context::Scripting);
            }
            K::DOLLAR_L_CURLY => {
                self.named("T_DOLLAR_OPEN_CURLY_BRACES", i);
                self.contexts.push(Context::Scripting);
            }
            K::R_CURLY => {
                self.character(i);
                if self.top() == Context::Scripting && self.contexts.len() > 1 {
                    self.contexts.pop();
                }
            }
            _ => match operator_name(kind) {
                Some(name) => self.named(name, i),
                None => self.character(i),
            },
        }
        i + 1
    }

    /// String text and the heredoc's records, which join the pending text
    /// rather than becoming tokens, and the heredoc's close, which takes
    /// the pending text's last indentation record: the index after them, or none
    /// when `i` is no piece of a string.
    fn string_piece(&mut self, i: usize) -> Option<usize> {
        use SyntaxKind as K;
        let kind = self.tokens[i].kind;
        match kind {
            // The zero-width text `<ST_VAR_OFFSET>[ \n\r\t\\'#]` returns
            // before the byte it pops on: a row of its own, empty, and
            // never the head of the run that starts at the same byte.
            K::STRING_TEXT if self.tokens[i].len == 0 => {
                self.leave_offset();
                self.flush();
                let (start, end) = self.span(i);
                self.push(Some("T_ENCAPSED_AND_WHITESPACE"), start, end);
            }
            K::STRING_TEXT | K::HEREDOC_INDENT => {
                self.leave_offset();
                self.extend_run(i, kind == K::HEREDOC_INDENT);
            }
            K::HEREDOC_END => {
                self.leave_offset();
                let (_, end) = self.span(i);
                let indent = self.run.as_mut().and_then(|run| {
                    let at = run.indent.take()?;
                    run.end = at;
                    Some(at)
                });
                self.flush();
                let start = indent.unwrap_or(self.span(i).0);
                self.push(Some("T_END_HEREDOC"), start, end);
                if self.top() == Context::Heredoc {
                    self.contexts.pop();
                }
            }
            _ => return None,
        }
        Some(i + 1)
    }

    /// A keyword, with the two fusions php's scanner makes over more than
    /// one of the lexer's tokens: `yield from` across whitespace and
    /// comments, and `private(set)` and its siblings over exactly four
    /// touching tokens. The index after what was read.
    fn keyword(&mut self, i: usize) -> usize {
        use SyntaxKind as K;
        let kind = self.tokens[i].kind;
        match kind {
            K::YIELD_KW => {
                if let Some(j) = self.after_trivia(i)
                    && self.tokens[j].kind == K::FROM_KW
                {
                    let (start, _) = self.span(i);
                    let (_, end) = self.span(j);
                    self.push(Some("T_YIELD_FROM"), start, end);
                    return j + 1;
                }
                self.named("T_YIELD", i);
            }
            K::PRIVATE_KW | K::PROTECTED_KW | K::PUBLIC_KW => {
                let fused = self.adjacent(i)
                    && self.adjacent(i + 1)
                    && self.adjacent(i + 2)
                    && self.tokens[i + 1].kind == K::L_PAREN
                    && self.tokens[i + 2].kind == K::SET_KW
                    && self.tokens[i + 3].kind == K::R_PAREN;
                if fused {
                    let name = match kind {
                        K::PRIVATE_KW => "T_PRIVATE_SET",
                        K::PROTECTED_KW => "T_PROTECTED_SET",
                        _ => "T_PUBLIC_SET",
                    };
                    let (start, _) = self.span(i);
                    let (_, end) = self.span(i + 3);
                    self.push(Some(name), start, end);
                    return i + 4;
                }
                self.named(keyword_name(kind), i);
            }
            K::ENUM_KW => {
                let name = self.enum_name(i);
                self.named(name, i);
            }
            _ => self.named(keyword_name(kind), i),
        }
        i + 1
    }

    /// A stretch the lexer refused, one `T_BAD_CHARACTER` per byte.
    fn bad_bytes(&mut self, i: usize) {
        let (start, end) = self.span(i);
        for at in start..end {
            self.push(Some("T_BAD_CHARACTER"), at, at + 1);
        }
    }

    /// A token inside `$a[…]` in a string: `ST_VAR_OFFSET` returns a
    /// number as `T_NUM_STRING`, a name as `T_STRING`, a variable, and
    /// every other byte as itself, `&` included; `]` leaves the state.
    fn offset_token(&mut self, i: usize) -> usize {
        use SyntaxKind as K;
        self.flush();
        match self.tokens[i].kind {
            K::R_BRACK => {
                self.character(i);
                self.contexts.pop();
                if let Some(laying) = &mut self.heap {
                    laying.pop();
                }
            }
            K::ENCAPS_INT => self.named("T_NUM_STRING", i),
            K::IDENT => self.named("T_STRING", i),
            K::VARIABLE => self.named("T_VARIABLE", i),
            K::ERROR => self.bad_bytes(i),
            _ => self.character(i),
        }
        i + 1
    }

    /// Zend's `ST_VAR_OFFSET` pops on a byte its rules do not name and
    /// the string state reads it; the lexer did the same without a token,
    /// so the state is left here when what follows is string text.
    fn leave_offset(&mut self) {
        if self.top() == Context::VarOffset {
            self.contexts.pop();
            if let Some(laying) = &mut self.heap {
                laying.pop();
            }
        }
    }

    /// The string php's scanner lays for the token `name` over
    /// `start..end`, where [`scanner_heap`] asks: `zend_copy_value` past
    /// the `$` of a variable and the `\` or `namespace\` of a name
    /// (`RETURN_TOKEN_WITH_STR`), the bytes between the quotes of a `'…'`
    /// with its two escapes read, and an offset in a string that is not
    /// a whole number a long holds.
    fn lay_token(&mut self, name: &'static str, start: usize, end: usize) {
        let Some(laying) = &mut self.heap else {
            return;
        };
        let text = self.source.get(start..end).unwrap_or(&[]);
        if name == "T_NUM_STRING" && matches!(crate::value::offset(text), crate::TokenValue::Int(_))
        {
            return;
        }
        let skip = match name {
            "T_STRING" | "T_NAME_QUALIFIED" | "T_STRING_VARNAME" | "T_INLINE_HTML"
            | "T_NUM_STRING" => 0,
            "T_VARIABLE" | "T_NAME_FULLY_QUALIFIED" => 1,
            "T_NAME_RELATIVE" => b"namespace\\".len(),
            "T_CONSTANT_ENCAPSED_STRING" => {
                let prefixed = matches!(text.first(), Some(b'b' | b'B'));
                let inner = text.len().saturating_sub(2 + usize::from(prefixed));
                laying.heap.string(inner, &single_quoted(text));
                return;
            }
            _ => return,
        };
        let text = text.get(skip..).unwrap_or(&[]);
        laying.heap.string(text.len(), text);
    }

    /// The scanner's two stacks at a token of `kind` in `ST_IN_SCRIPTING`,
    /// where [`scanner_heap`] asks.
    fn lay_stacks(&mut self, kind: SyntaxKind) {
        use SyntaxKind as K;
        let Some(laying) = &mut self.heap else {
            return;
        };
        match kind {
            K::L_PAREN | K::L_BRACK | K::ATTRIBUTE_OPEN => laying.enter(),
            K::R_PAREN | K::R_BRACK => laying.exit(),
            K::L_CURLY | K::DOLLAR_L_CURLY => {
                laying.enter();
                laying.push();
            }
            K::R_CURLY => {
                laying.exit();
                laying.pop();
            }
            K::ARROW | K::NULLSAFE_ARROW => laying.pass(),
            _ => {}
        }
    }

    /// A `"` or a `` ` ``: the close of the string it delimits when that
    /// is the state, else its open. A double-quoted string closed over
    /// nothing but text is php's one `T_CONSTANT_ENCAPSED_STRING`, since
    /// its scanner looks ahead for an interpolation before it opens the
    /// state at all; a backquoted one never is.
    fn delimiter(&mut self, i: usize) {
        let kind = self.tokens[i].kind;
        if self.top() == Context::Quoted(kind) {
            self.contexts.pop();
            let opened = if kind == SyntaxKind::DOUBLE_QUOTE {
                self.quotes.pop()
            } else {
                None
            };
            if let Some(at) = opened
                && self.out.len() - at <= 2
                && self.out[at + 1..]
                    .iter()
                    .all(|t| t.name == Some("T_ENCAPSED_AND_WHITESPACE"))
            {
                let (start, line) = (self.out[at].start, self.out[at].line);
                let (_, end) = self.span(i);
                self.out.truncate(at);
                self.out.push(ScannedToken {
                    name: Some("T_CONSTANT_ENCAPSED_STRING"),
                    start,
                    end,
                    line,
                });
                return;
            }
            self.character(i);
            return;
        }
        if kind == SyntaxKind::DOUBLE_QUOTE {
            self.quotes.push(self.out.len());
        }
        self.character(i);
        self.contexts.push(Context::Quoted(kind));
    }

    /// A name: `T_STRING_VARNAME` right after `${` when `}` or `[`
    /// follows it (`ST_LOOKING_FOR_VARNAME`), `T_CALLABLE` for the one
    /// keyword the lexer leaves a name, unless it names a property, and
    /// `T_STRING` otherwise.
    fn ident(&mut self, i: usize) {
        let (start, end) = self.span(i);
        let after_dollar_curly =
            i > 0 && self.tokens[i - 1].kind == SyntaxKind::DOLLAR_L_CURLY && self.adjacent(i - 1);
        let name = if after_dollar_curly && matches!(self.source.get(end), Some(b'}' | b'[')) {
            "T_STRING_VARNAME"
        } else if self.text(i).eq_ignore_ascii_case(b"callable") && !self.names_a_property(i) {
            "T_CALLABLE"
        } else {
            "T_STRING"
        };
        self.push(Some(name), start, end);
    }

    /// `enum` is `T_ENUM` before whitespace or comments and a name, and
    /// `T_STRING` everywhere else, `enum extends` and `enum implements`
    /// included, which is how php keeps `enum` usable as a name.
    fn enum_name(&self, i: usize) -> &'static str {
        let Some(j) = self.after_trivia(i) else {
            return "T_STRING";
        };
        let (start, _) = self.span(j);
        let rest = self.source.get(start..).unwrap_or(&[]);
        let excluded =
            |word: &[u8]| rest.len() >= word.len() && rest[..word.len()].eq_ignore_ascii_case(word);
        if excluded(b"extends") || excluded(b"implements") {
            return "T_STRING";
        }
        match rest.first() {
            Some(b) if b.is_ascii_alphabetic() || *b == b'_' || *b >= 0x80 => "T_ENUM",
            _ => "T_STRING",
        }
    }
}

/// Whether a `SINGLE_QUOTED` token ran to the end of the input with no
/// `'` closing it, which php's scanner returns as one
/// `T_ENCAPSED_AND_WHITESPACE` from the quote on, the parser being what
/// refuses it.
fn unterminated(text: &[u8]) -> bool {
    let mut i = if text.first() == Some(&b'\'') { 1 } else { 2 };
    while i < text.len() {
        match text[i] {
            b'\\' => i += 2,
            b'\'' => return false,
            _ => i += 1,
        }
    }
    true
}

/// php's name for a keyword kind. `set` and `from` are php's names only
/// inside `private(set)` and `yield from`, which fuse before this is
/// asked; alone they are `T_STRING`. `enum` is decided by
/// [`Walk::enum_name`] and never reaches here.
fn keyword_name(kind: SyntaxKind) -> &'static str {
    use SyntaxKind as K;
    match kind {
        K::ABSTRACT_KW => "T_ABSTRACT",
        K::AND_KW => "T_LOGICAL_AND",
        K::ARRAY_KW => "T_ARRAY",
        K::AS_KW => "T_AS",
        K::BREAK_KW => "T_BREAK",
        K::CASE_KW => "T_CASE",
        K::CATCH_KW => "T_CATCH",
        K::CLASS_KW => "T_CLASS",
        K::CLONE_KW => "T_CLONE",
        K::CONST_KW => "T_CONST",
        K::CONTINUE_KW => "T_CONTINUE",
        K::DECLARE_KW => "T_DECLARE",
        K::DEFAULT_KW => "T_DEFAULT",
        K::DIE_KW | K::EXIT_KW => "T_EXIT",
        K::DO_KW => "T_DO",
        K::ECHO_KW => "T_ECHO",
        K::ELSEIF_KW => "T_ELSEIF",
        K::ELSE_KW => "T_ELSE",
        K::EMPTY_KW => "T_EMPTY",
        K::ENDDECLARE_KW => "T_ENDDECLARE",
        K::ENDFOREACH_KW => "T_ENDFOREACH",
        K::ENDFOR_KW => "T_ENDFOR",
        K::ENDIF_KW => "T_ENDIF",
        K::ENDSWITCH_KW => "T_ENDSWITCH",
        K::ENDWHILE_KW => "T_ENDWHILE",
        K::EVAL_KW => "T_EVAL",
        K::EXTENDS_KW => "T_EXTENDS",
        K::FINALLY_KW => "T_FINALLY",
        K::FINAL_KW => "T_FINAL",
        K::FN_KW => "T_FN",
        K::FOREACH_KW => "T_FOREACH",
        K::FOR_KW => "T_FOR",
        K::FUNCTION_KW => "T_FUNCTION",
        K::GLOBAL_KW => "T_GLOBAL",
        K::GOTO_KW => "T_GOTO",
        K::HALT_COMPILER_KW => "T_HALT_COMPILER",
        K::IF_KW => "T_IF",
        K::IMPLEMENTS_KW => "T_IMPLEMENTS",
        K::INCLUDE_KW => "T_INCLUDE",
        K::INCLUDE_ONCE_KW => "T_INCLUDE_ONCE",
        K::INSTANCEOF_KW => "T_INSTANCEOF",
        K::INSTEADOF_KW => "T_INSTEADOF",
        K::INTERFACE_KW => "T_INTERFACE",
        K::ISSET_KW => "T_ISSET",
        K::LIST_KW => "T_LIST",
        K::MATCH_KW => "T_MATCH",
        K::NAMESPACE_KW => "T_NAMESPACE",
        K::NEW_KW => "T_NEW",
        K::OR_KW => "T_LOGICAL_OR",
        K::PRINT_KW => "T_PRINT",
        K::PRIVATE_KW => "T_PRIVATE",
        K::PROTECTED_KW => "T_PROTECTED",
        K::PUBLIC_KW => "T_PUBLIC",
        K::READONLY_KW => "T_READONLY",
        K::REQUIRE_KW => "T_REQUIRE",
        K::REQUIRE_ONCE_KW => "T_REQUIRE_ONCE",
        K::RETURN_KW => "T_RETURN",
        K::STATIC_KW => "T_STATIC",
        K::SWITCH_KW => "T_SWITCH",
        K::THROW_KW => "T_THROW",
        K::TRAIT_KW => "T_TRAIT",
        K::TRY_KW => "T_TRY",
        K::UNSET_KW => "T_UNSET",
        K::USE_KW => "T_USE",
        K::VAR_KW => "T_VAR",
        K::WHILE_KW => "T_WHILE",
        K::XOR_KW => "T_LOGICAL_XOR",
        K::YIELD_KW => "T_YIELD",
        _ => "T_STRING",
    }
}

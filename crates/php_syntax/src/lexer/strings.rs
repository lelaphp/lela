//! The string states: single-quoted scanning, `DoubleQuoted` and
//! `Backquote` interpolation (Zend `ST_DOUBLE_QUOTES` and `ST_BACKQUOTE`),
//! and the three states beside them, `LookingForProperty`,
//! `LookingForVarName` and `VarOffset`.
//!
//! Interpolation does not re-lex: the simple forms (`$var`, `$var[dim]`,
//! `$var->prop`) come out as structured sub-token runs, `{$` pushes a full
//! `Scripting` state, and `${` goes through `LookingForVarName`, exactly
//! like Zend.

use crate::source::{Diagnostic, Level};
use crate::value::{self, Defect, Quote};

use super::{Lexer, State, is_label_start};
use crate::SyntaxKind;

#[derive(Clone, Copy)]
pub(crate) enum Trigger {
    /// `$label…`: Zend `"$"{LABEL}` (with the `[` and `->` lookaheads).
    Variable,
    /// `${`: Zend `T_DOLLAR_OPEN_CURLY_BRACES`.
    DollarCurly,
    /// `{$`: Zend `T_CURLY_OPEN` (the token is the `{` alone).
    CurlyDollar,
}

impl Lexer<'_> {
    // ----- single-quoted -----------------------------------------------------

    /// `'…'` with `\\` and `\'` escapes (lexically `\` always consumes the
    /// next byte; the flex pattern is `b?'([^'\\]|\\[^])*'`), with the
    /// optional `b`/`B` prefix.
    #[inline(never)]
    pub(crate) fn lex_single_quoted(&mut self, start: usize) {
        let quote_at = if matches!(self.byte(start), b'b' | b'B') {
            start + 1
        } else {
            start
        };
        self.pos = quote_at + 1;
        let mut terminated = false;
        while self.pos < self.text.len() {
            match self.byte(self.pos) {
                b'\\' => self.pos = (self.pos + 2).min(self.text.len()),
                b'\'' => {
                    self.pos += 1;
                    terminated = true;
                    break;
                }
                _ => self.pos += 1,
            }
        }
        // Unclosed: php hands the parser the rest of the file as one
        // `T_ENCAPSED_AND_WHITESPACE`, the quote included, and the parser
        // says `unexpected string content "abc"` where it stands.
        let kind = if terminated {
            SyntaxKind::SINGLE_QUOTED
        } else {
            SyntaxKind::STRING_TEXT
        };
        self.emit(kind, start);
    }

    // ----- DoubleQuoted and Backquote ------------------------------------------

    /// One dispatch step inside `"…"` or `` `…` ``.
    pub(crate) fn lex_quoted(&mut self, delim: u8, delim_kind: SyntaxKind) {
        let start = self.pos;
        let quote = if delim == b'"' {
            Quote::Double
        } else {
            Quote::Backquote
        };
        if self.byte(start) == delim {
            self.pos = start + 1;
            self.emit(delim_kind, start);
            self.pop_state();
            return;
        }
        if let Some(trigger) = self.interp_trigger_at(start) {
            self.lex_trigger(trigger);
            return;
        }
        // A text span up to the delimiter, the next trigger, or EOF.
        while !self.scan_text_step(delim, quote) {}
        self.emit(SyntaxKind::STRING_TEXT, start);
    }

    /// Advances over one text element; true when the span must end before
    /// the element at the current position (the closing delimiter, an
    /// interpolation trigger, or EOF).
    fn scan_text_step(&mut self, delim: u8, quote: Quote) -> bool {
        if self.pos >= self.text.len() {
            return true;
        }
        let b = self.byte(self.pos);
        if b == delim || self.interp_trigger_at(self.pos).is_some() {
            return true;
        }
        if b == b'\\' {
            // Zend ST_DOUBLE_QUOTES and ST_BACKQUOTE: `\` skips any next
            // byte, newline included (heredoc differs; see heredoc.rs).
            self.eat_escape(quote);
        } else {
            self.pos += 1;
        }
        false
    }

    /// Steps over one `\` and what it escapes, as [`value::escape`]
    /// reads it under the string's own mark, and raises what php's
    /// scanner raises there: `Octal escape sequence overflow` as a
    /// compile warning on three octal digits past `\377`, and `Invalid
    /// UTF-8 codepoint escape sequence` as a parse error on a malformed
    /// `\u{…}`, with the `: Codepoint too large` tail when the shape is
    /// right and the number is not. After a malformed `\u{…}` the text
    /// goes on from the `{`, as the value does.
    ///
    /// ```php
    /// echo "\u{zz}";
    /// ```
    /// ```expect exit=255
    /// Parse error: Invalid UTF-8 codepoint escape sequence in %s on line 2
    /// ```
    pub(crate) fn eat_escape(&mut self, quote: Quote) {
        let start = self.pos;
        let escape = value::escape(self.text, start, quote);
        match escape.defect {
            Some(Defect::OctalOverflow) => {
                let span = self.span(start, start + escape.len);
                let line = self.line(start);
                let digits =
                    String::from_utf8_lossy(&self.text[start + 1..start + escape.len]).into_owned();
                self.diag(Diagnostic::of(
                    Level::CompileWarning,
                    span,
                    line,
                    format!("Octal escape sequence overflow \\{digits} is greater than \\377"),
                ));
            }
            Some(Defect::Codepoint { too_large, end }) => {
                let span = self.span(start, end.min(self.text.len()));
                let text = if too_large {
                    "Invalid UTF-8 codepoint escape sequence: Codepoint too large"
                } else {
                    "Invalid UTF-8 codepoint escape sequence"
                };
                let line = self.line(start);
                self.diag(Diagnostic::new(span, line, text));
            }
            None => {}
        }
        self.pos = (start + escape.len).min(self.text.len());
    }

    /// Zend's interpolation triggers, shared by `"…"`, `` `…` `` and
    /// heredoc.
    pub(crate) fn interp_trigger_at(&self, i: usize) -> Option<Trigger> {
        match self.byte(i) {
            b'$' if is_label_start(self.byte(i + 1)) => Some(Trigger::Variable),
            b'$' if self.byte(i + 1) == b'{' => Some(Trigger::DollarCurly),
            b'{' if self.byte(i + 1) == b'$' => Some(Trigger::CurlyDollar),
            _ => None,
        }
    }

    /// Emits the trigger's token and pushes the state that follows it.
    pub(crate) fn lex_trigger(&mut self, trigger: Trigger) {
        let start = self.pos;
        match trigger {
            Trigger::Variable => {
                self.pos = start + 1 + self.label_len(start + 1);
                self.emit(SyntaxKind::VARIABLE, start);
                let var_span = self.span(start, self.pos);
                // Zend lookaheads: `$a[` is ST_VAR_OFFSET; `$a->x` and
                // `$a?->x` (a label start after the arrow; `$a->3` stays
                // text) are ST_LOOKING_FOR_PROPERTY. The `?->` simple form
                // interpolates on 8.3 and later.
                let arrow_prop = self.byte(self.pos) == b'-'
                    && self.byte(self.pos + 1) == b'>'
                    && is_label_start(self.byte(self.pos + 2));
                let nullsafe_prop = self.byte(self.pos) == b'?'
                    && self.byte(self.pos + 1) == b'-'
                    && self.byte(self.pos + 2) == b'>'
                    && is_label_start(self.byte(self.pos + 3));
                if self.byte(self.pos) == b'[' {
                    self.push_state(State::VarOffset, var_span);
                } else if arrow_prop || nullsafe_prop {
                    self.push_state(State::LookingForProperty, var_span);
                }
            }
            Trigger::DollarCurly => {
                self.pos = start + 2;
                self.emit(SyntaxKind::DOLLAR_L_CURLY, start);
                self.push_state(State::LookingForVarName, self.span(start, start + 2));
            }
            Trigger::CurlyDollar => {
                // The token is the `{` alone (Zend yyless(1) on T_CURLY_OPEN);
                // the `$…` then lexes as ordinary scripting tokens until the
                // matching `}` pops back to this string state.
                self.pos = start + 1;
                self.emit(SyntaxKind::L_CURLY, start);
                self.push_state(State::Scripting, self.span(start, start + 1));
            }
        }
    }

    // ----- LookingForProperty (Zend ST_LOOKING_FOR_PROPERTY) ------------------

    /// After `->` or `?->`: whitespace and a comment stay, another arrow
    /// stays, a label pops and lexes as IDENT even when it is a keyword
    /// (Zend's lexical member-name remap). Anything else, exactly like
    /// Zend's `{ANY_CHAR}` fallback, pops without consuming. `#[` is a
    /// comment here: the attribute's open is `ST_IN_SCRIPTING`'s alone.
    ///
    /// ```php
    /// foreach (PhpToken::tokenize('<?php $x->/*c*/ __PROPERTY__; $x?->#[a
    /// __class__;') as $t) echo $t->getTokenName(), ' ';
    /// ```
    /// ```expect
    /// T_OPEN_TAG T_VARIABLE T_OBJECT_OPERATOR T_COMMENT T_WHITESPACE T_STRING ; T_WHITESPACE T_VARIABLE T_NULLSAFE_OBJECT_OPERATOR T_COMMENT T_WHITESPACE T_STRING ;
    /// ```
    pub(crate) fn lex_looking_for_property(&mut self) {
        let start = self.pos;
        match self.byte(start) {
            b' ' | b'\t' | b'\r' | b'\n' => self.lex_whitespace(),
            b'#' => self.lex_line_comment(),
            b'/' if self.byte(start + 1) == b'/' => self.lex_line_comment(),
            b'/' if self.byte(start + 1) == b'*' => self.lex_block_comment(),
            b'-' if self.byte(start + 1) == b'>' => {
                self.pos = start + 2;
                self.emit(SyntaxKind::ARROW, start);
            }
            b'?' if self.byte(start + 1) == b'-' && self.byte(start + 2) == b'>' => {
                self.pos = start + 3;
                self.emit(SyntaxKind::NULLSAFE_ARROW, start);
            }
            b if is_label_start(b) => {
                self.pos = start + self.label_len(start);
                self.emit(SyntaxKind::IDENT, start);
                self.pop_state();
            }
            _ => self.pop_state(), // yyless(0); re-dispatch in the outer state
        }
    }

    // ----- LookingForVarName (Zend ST_LOOKING_FOR_VARNAME) --------------------

    /// After `${`: a label directly followed by `}` or `[` is the
    /// `T_STRING_VARNAME` case (IDENT inside `DollarBraceInterpolation`);
    /// anything else reinterprets in `Scripting` (`${$x}` and `${'a'.'b'}`
    /// are full expressions). Both paths replace this state with
    /// `Scripting` (Zend: `yy_pop_state` then `yy_push_state(ST_IN_SCRIPTING)`),
    /// so the closing `}` pops back to the string state.
    pub(crate) fn lex_looking_for_varname(&mut self) {
        let start = self.pos;
        let len = self.label_len(start);
        let here = self.span(start, start);
        if len > 0 && matches!(self.byte(start + len), b'}' | b'[') {
            self.pos = start + len;
            self.emit(SyntaxKind::IDENT, start);
        }
        self.begin_state(State::Scripting, here);
    }

    // ----- VarOffset (Zend ST_VAR_OFFSET) --------------------------------------

    /// The restricted in-string dim grammar of `$a[…]`: integer offsets
    /// (`ENCAPS_INT`, php's `T_NUM_STRING`), bare-word keys (IDENT), `$var`,
    /// and single-character operator tokens. `]` pops back to the string.
    pub(crate) fn lex_var_offset(&mut self) {
        let start = self.pos;
        let b = self.byte(start);
        match b {
            b']' => {
                self.pos = start + 1;
                self.emit(SyntaxKind::R_BRACK, start);
                self.pop_state();
            }
            b'[' => self.offset_punct(SyntaxKind::L_BRACK),
            b'0'..=b'9' => self.lex_encaps_int(),
            b'$' if is_label_start(self.byte(start + 1)) => {
                self.pos = start + 1 + self.label_len(start + 1);
                self.emit(SyntaxKind::VARIABLE, start);
            }
            _ if is_label_start(b) => {
                self.pos = start + self.label_len(start);
                self.emit(SyntaxKind::IDENT, start);
            }
            // Zend `<ST_VAR_OFFSET>[ \n\r\t\\'#]`: `yyless(0)`, pop, and a
            // zero-width `T_ENCAPSED_AND_WHITESPACE` returned at the byte,
            // which then re-scans as string text. The parser's word on it
            // is `unexpected string content ""`, and `token_get_all` has
            // the empty row; both are php's only with the token.
            b' ' | b'\n' | b'\r' | b'\t' | b'\\' | b'\'' | b'#' => {
                self.emit_empty(SyntaxKind::STRING_TEXT);
                self.pop_state();
            }
            // Zend {TOKENS}|[{}"`]: bare single-character tokens.
            b';' => self.offset_punct(SyntaxKind::SEMICOLON),
            b':' => self.offset_punct(SyntaxKind::COLON),
            b',' => self.offset_punct(SyntaxKind::COMMA),
            b'.' => self.offset_punct(SyntaxKind::DOT),
            b'(' => self.offset_punct(SyntaxKind::L_PAREN),
            b')' => self.offset_punct(SyntaxKind::R_PAREN),
            b'|' => self.offset_punct(SyntaxKind::PIPE),
            b'^' => self.offset_punct(SyntaxKind::CARET),
            b'&' => self.offset_punct(SyntaxKind::AMP),
            b'+' => self.offset_punct(SyntaxKind::PLUS),
            b'-' => self.offset_punct(SyntaxKind::MINUS),
            b'/' => self.offset_punct(SyntaxKind::SLASH),
            b'*' => self.offset_punct(SyntaxKind::STAR),
            b'=' => self.offset_punct(SyntaxKind::EQ),
            b'%' => self.offset_punct(SyntaxKind::PERCENT),
            b'!' => self.offset_punct(SyntaxKind::BANG),
            b'~' => self.offset_punct(SyntaxKind::TILDE),
            b'$' => self.offset_punct(SyntaxKind::DOLLAR),
            b'<' => self.offset_punct(SyntaxKind::LT),
            b'>' => self.offset_punct(SyntaxKind::GT),
            b'?' => self.offset_punct(SyntaxKind::QUESTION),
            b'@' => self.offset_punct(SyntaxKind::AT),
            b'{' => self.offset_punct(SyntaxKind::L_CURLY),
            b'}' => self.offset_punct(SyntaxKind::R_CURLY),
            b'"' => self.offset_punct(SyntaxKind::DOUBLE_QUOTE),
            b'`' => self.offset_punct(SyntaxKind::BACKQUOTE),
            _ => {
                // Zend {ANY_CHAR}: the byte comes back as a token of its own
                // and the parser says `unexpected character 0x%02X`.
                self.pos = start + 1;
                self.emit(SyntaxKind::ERROR, start);
            }
        }
    }

    fn offset_punct(&mut self, kind: SyntaxKind) {
        let start = self.pos;
        self.pos = start + 1;
        self.emit(kind, start);
    }

    /// Integer-shaped offsets (php's `T_NUM_STRING`: the offset is a string
    /// until the compiler reads it, so no numeric diagnostic here): decimal,
    /// hex, binary and octal spans, underscores included.
    fn lex_encaps_int(&mut self) {
        let start = self.pos;
        let (mut i, digit): (usize, fn(u8) -> bool) = if self.byte(start) == b'0' {
            match self.byte(start + 1) {
                b'x' | b'X' => (start + 2, |b| b.is_ascii_hexdigit()),
                b'b' | b'B' => (start + 2, |b| matches!(b, b'0' | b'1')),
                b'o' | b'O' => (start + 2, |b| matches!(b, b'0'..=b'7')),
                _ => (start, |b| b.is_ascii_digit()),
            }
        } else {
            (start, |b| b.is_ascii_digit())
        };
        // A bare `0x` prefix without digits falls back to plain `0`.
        if i > start && !digit(self.byte(i)) && self.byte(i) != b'_' {
            i = start + 1;
        } else {
            while digit(self.byte(i)) || self.byte(i) == b'_' {
                i += 1;
            }
        }
        self.pos = i;
        self.emit(SyntaxKind::ENCAPS_INT, start);
    }
}

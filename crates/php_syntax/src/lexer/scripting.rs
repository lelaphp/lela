//! `Scripting` (Zend `ST_IN_SCRIPTING`): operators, keywords, names,
//! numbers, casts, comments, and the entries into the string and heredoc
//! states.

use crate::source::{Diagnostic, Level};

use super::{Lexer, State, is_label_continue, is_label_start};
use crate::SyntaxKind;

/// The case-insensitive magic-constant set. These are not in
/// `SyntaxKind::from_keyword` (it covers schema keywords only), so they
/// are classified here, before the keyword test.
const MAGIC_CONSTS: &[&[u8]] = &[
    b"__line__",
    b"__file__",
    b"__dir__",
    b"__function__",
    b"__class__",
    b"__trait__",
    b"__method__",
    b"__namespace__",
    b"__property__",
];

/// The 8.x cast set (`void` joined in 8.5: it lexes `T_VOID_CAST`), plus
/// the removed pair.
const CASTS: &[&[u8]] = &[
    b"int", b"integer", b"bool", b"boolean", b"float", b"double", b"string", b"binary", b"array",
    b"object", b"void",
];
const CASTS_REMOVED: &[&[u8]] = &[b"real", b"unset"];

impl Lexer<'_> {
    /// One dispatch step in `ST_IN_SCRIPTING`.
    // A flat first-byte dispatch reads best as one match; splitting it into
    // per-byte-class functions would only scatter the operator table.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn lex_scripting(&mut self) {
        let start = self.pos;
        let b = self.byte(start);
        match b {
            b' ' | b'\t' | b'\r' | b'\n' => self.lex_whitespace(),
            b'0'..=b'9' => self.lex_number(),
            b'$' => self.lex_dollar(),
            b'\'' => self.lex_single_quoted(start),
            b'"' => {
                self.pos = start + 1;
                self.emit(SyntaxKind::DOUBLE_QUOTE, start);
                self.push_state(State::DoubleQuoted, self.span(start, start + 1));
            }
            b'`' => {
                self.pos = start + 1;
                self.emit(SyntaxKind::BACKQUOTE, start);
                self.push_state(State::Backquote, self.span(start, start + 1));
            }
            b'\\' => self.lex_backslash(),
            b'#' => {
                if self.byte(start + 1) == b'[' {
                    self.pos = start + 2;
                    self.emit(SyntaxKind::ATTRIBUTE_OPEN, start);
                } else {
                    self.lex_line_comment();
                }
            }
            b'/' => match self.byte(start + 1) {
                b'/' => self.lex_line_comment(),
                b'*' => self.lex_block_comment(),
                b'=' => self.punct(SyntaxKind::SLASH_EQ, 2),
                _ => self.punct(SyntaxKind::SLASH, 1),
            },
            b'(' => self.lex_paren_or_cast(),
            b')' => self.punct(SyntaxKind::R_PAREN, 1),
            b'[' => self.punct(SyntaxKind::L_BRACK, 1),
            b']' => self.punct(SyntaxKind::R_BRACK, 1),
            b'{' => {
                // Zend: yy_push_state(ST_IN_SCRIPTING). Brace depth lives in
                // the state stack, which is what lets `}` close a `{$`
                // interpolation only at depth zero.
                self.pos = start + 1;
                self.emit(SyntaxKind::L_CURLY, start);
                self.push_state(State::Scripting, self.span(start, start + 1));
            }
            b'}' => {
                self.pos = start + 1;
                self.emit(SyntaxKind::R_CURLY, start);
                // Zend pops only when its state stack is non-empty: a stray
                // `}` at top level stays in scripting.
                if self.can_pop_brace() {
                    self.pop_state();
                }
            }
            b';' => self.punct(SyntaxKind::SEMICOLON, 1),
            b',' => self.punct(SyntaxKind::COMMA, 1),
            b'@' => self.punct(SyntaxKind::AT, 1),
            b'~' => self.punct(SyntaxKind::TILDE, 1),
            b'?' => self.lex_question(),
            b':' => {
                if self.byte(start + 1) == b':' {
                    self.punct(SyntaxKind::COLON_COLON, 2);
                } else {
                    self.punct(SyntaxKind::COLON, 1);
                }
            }
            b'=' => match (self.byte(start + 1), self.byte(start + 2)) {
                (b'=', b'=') => self.punct(SyntaxKind::EQ_EQ_EQ, 3),
                (b'=', _) => self.punct(SyntaxKind::EQ_EQ, 2),
                (b'>', _) => self.punct(SyntaxKind::FAT_ARROW, 2),
                _ => self.punct(SyntaxKind::EQ, 1),
            },
            b'!' => match (self.byte(start + 1), self.byte(start + 2)) {
                (b'=', b'=') => self.punct(SyntaxKind::BANG_EQ_EQ, 3),
                (b'=', _) => self.punct(SyntaxKind::BANG_EQ, 2),
                _ => self.punct(SyntaxKind::BANG, 1),
            },
            b'<' => self.lex_lt(),
            b'>' => match (self.byte(start + 1), self.byte(start + 2)) {
                (b'>', b'=') => self.punct(SyntaxKind::SHR_EQ, 3),
                (b'>', _) => self.punct(SyntaxKind::SHR, 2),
                (b'=', _) => self.punct(SyntaxKind::GT_EQ, 2),
                _ => self.punct(SyntaxKind::GT, 1),
            },
            b'+' => match self.byte(start + 1) {
                b'+' => self.punct(SyntaxKind::PLUS_PLUS, 2),
                b'=' => self.punct(SyntaxKind::PLUS_EQ, 2),
                _ => self.punct(SyntaxKind::PLUS, 1),
            },
            b'-' => match self.byte(start + 1) {
                b'>' => {
                    self.punct(SyntaxKind::ARROW, 2);
                    // Zend: yy_push_state(ST_LOOKING_FOR_PROPERTY).
                    self.push_state(State::LookingForProperty, self.span(start, start + 2));
                }
                b'-' => self.punct(SyntaxKind::MINUS_MINUS, 2),
                b'=' => self.punct(SyntaxKind::MINUS_EQ, 2),
                _ => self.punct(SyntaxKind::MINUS, 1),
            },
            b'*' => match (self.byte(start + 1), self.byte(start + 2)) {
                (b'*', b'=') => self.punct(SyntaxKind::STAR_STAR_EQ, 3),
                (b'*', _) => self.punct(SyntaxKind::STAR_STAR, 2),
                (b'=', _) => self.punct(SyntaxKind::STAR_EQ, 2),
                _ => self.punct(SyntaxKind::STAR, 1),
            },
            b'%' => {
                if self.byte(start + 1) == b'=' {
                    self.punct(SyntaxKind::PERCENT_EQ, 2);
                } else {
                    self.punct(SyntaxKind::PERCENT, 1);
                }
            }
            b'&' => match self.byte(start + 1) {
                b'&' => self.punct(SyntaxKind::AMP_AMP, 2),
                b'=' => self.punct(SyntaxKind::AMP_EQ, 2),
                // One AMP kind always; the 8.1 two-flavour split is parser
                // lookahead (the crate doc says why).
                _ => self.punct(SyntaxKind::AMP, 1),
            },
            b'|' => match self.byte(start + 1) {
                b'|' => self.punct(SyntaxKind::PIPE_PIPE, 2),
                b'=' => self.punct(SyntaxKind::PIPE_EQ, 2),
                b'>' => self.punct(SyntaxKind::PIPE_GT, 2),
                _ => self.punct(SyntaxKind::PIPE, 1),
            },
            b'^' => {
                if self.byte(start + 1) == b'=' {
                    self.punct(SyntaxKind::CARET_EQ, 2);
                } else {
                    self.punct(SyntaxKind::CARET, 1);
                }
            }
            b'.' => self.lex_dot(),
            _ if is_label_start(b) => self.lex_ident_like(),
            _ => self.lex_bad_bytes(),
        }
    }

    fn punct(&mut self, kind: SyntaxKind, len: usize) {
        let start = self.pos;
        self.pos = start + len;
        self.emit(kind, start);
    }

    pub(crate) fn lex_whitespace(&mut self) {
        let start = self.pos;
        while matches!(self.byte(self.pos), b' ' | b'\t' | b'\r' | b'\n')
            && self.pos < self.text.len()
        {
            self.pos += 1;
        }
        self.emit(SyntaxKind::WHITESPACE, start);
    }

    // ----- ?  ?>  ?->  ??  ??= -----------------------------------------------

    fn lex_question(&mut self) {
        let start = self.pos;
        match (self.byte(start + 1), self.byte(start + 2)) {
            // Zend: <ST_IN_SCRIPTING>"?>"{NEWLINE}?, the close tag eats one
            // following newline (\r\n, \n, or \r) into its token text.
            (b'>', _) => {
                self.pos = start + 2;
                if self.byte(self.pos) == b'\r' {
                    self.pos += 1;
                }
                if self.byte(self.pos) == b'\n' && self.pos < self.text.len() {
                    self.pos += 1;
                }
                self.emit(SyntaxKind::CLOSE_TAG, start);
                // Zend: BEGIN(INITIAL), replace and keep the stack below.
                // Unless the emit completed `__halt_compiler ( )` and
                // pushed the trailer state: Zend stops lexing there, and
                // the state it left behind is never read.
                if !matches!(self.active_state(), State::HaltCompiler) {
                    self.begin_state(State::InlineHtml, self.span(start, self.pos));
                }
            }
            (b'-', b'>') => {
                self.punct(SyntaxKind::NULLSAFE_ARROW, 3);
                self.push_state(State::LookingForProperty, self.span(start, start + 3));
            }
            (b'?', b'=') => self.punct(SyntaxKind::QUESTION_QUESTION_EQ, 3),
            (b'?', _) => self.punct(SyntaxKind::QUESTION_QUESTION, 2),
            _ => self.punct(SyntaxKind::QUESTION, 1),
        }
    }

    // ----- <  <=  <>  <<  <<=  <=>  <<<heredoc --------------------------------

    fn lex_lt(&mut self) {
        let start = self.pos;
        if self.starts_with_ci(start, b"<<<") && self.try_heredoc_start(start) {
            return;
        }
        match (self.byte(start + 1), self.byte(start + 2)) {
            (b'<', b'=') => self.punct(SyntaxKind::SHL_EQ, 3),
            (b'=', b'>') => self.punct(SyntaxKind::SPACESHIP, 3),
            (b'<', _) => self.punct(SyntaxKind::SHL, 2),
            (b'=', _) => self.punct(SyntaxKind::LT_EQ, 2),
            (b'>', _) => self.punct(SyntaxKind::LT_GT, 2),
            _ => self.punct(SyntaxKind::LT, 1),
        }
    }

    // ----- .  .=  ...  .5 ------------------------------------------------------

    fn lex_dot(&mut self) {
        let start = self.pos;
        if self.byte(start + 1) == b'.' && self.byte(start + 2) == b'.' {
            self.punct(SyntaxKind::DOT_DOT_DOT, 3);
        } else if self.byte(start + 1).is_ascii_digit() {
            self.lex_number(); // leading-dot float `.5`
        } else if self.byte(start + 1) == b'=' {
            self.punct(SyntaxKind::DOT_EQ, 2);
        } else {
            self.punct(SyntaxKind::DOT, 1);
        }
    }

    // ----- $var ----------------------------------------------------------------

    fn lex_dollar(&mut self) {
        let start = self.pos;
        if is_label_start(self.byte(start + 1)) {
            self.pos = start + 1 + self.label_len(start + 1);
            self.emit(SyntaxKind::VARIABLE, start);
        } else {
            // `$$x` is DOLLAR + VARIABLE; `${` is DOLLAR + L_CURLY (the
            // DynamicVarExpr form is parser-assembled outside strings).
            self.punct(SyntaxKind::DOLLAR, 1);
        }
    }

    // ----- comments --------------------------------------------------------------

    /// `//` and `#` comments, ending before the newline (php 8 puts the
    /// newline in `T_WHITESPACE`) and before `?>` without consuming it.
    fn lex_line_comment(&mut self) {
        let start = self.pos;
        self.pos += if self.byte(start) == b'/' { 2 } else { 1 };
        while self.pos < self.text.len() {
            match self.byte(self.pos) {
                b'?' if self.byte(self.pos + 1) == b'>' => break,
                b'\r' | b'\n' => break,
                _ => self.pos += 1,
            }
        }
        self.emit(SyntaxKind::LINE_COMMENT, start);
    }

    /// `/* … */` and `/** … */`. Zend: a doc comment requires whitespace
    /// after the `/**` (the pattern is `"/**"{WHITESPACE}`), so `/**/` and
    /// `/**x*/` are plain comments. Unterminated at EOF is php's
    /// `Unterminated comment starting line <n>`, the 1-based line of the
    /// opening `/*`.
    ///
    /// ```php
    /// $x = /* a
    /// ```
    /// ```expect exit=255
    /// Parse error: Unterminated comment starting line 2 in %s on line 2
    /// ```
    fn lex_block_comment(&mut self) {
        let start = self.pos;
        let is_doc = self.byte(start + 2) == b'*'
            && matches!(self.byte(start + 3), b' ' | b'\t' | b'\r' | b'\n');
        self.pos = start + 2;
        let mut terminated = false;
        while self.pos < self.text.len() {
            if self.byte(self.pos) == b'*' && self.byte(self.pos + 1) == b'/' {
                self.pos += 2;
                terminated = true;
                break;
            }
            self.pos += 1;
        }
        if !terminated {
            // php throws before it counts the comment's newlines, so the
            // line it reports is the one the comment opened on.
            let eof = self.span(self.text.len(), self.text.len());
            let open_line = self.line(start);
            self.diag(Diagnostic::new(
                eof,
                open_line,
                format!("Unterminated comment starting line {open_line}"),
            ));
        }
        let kind = if is_doc {
            SyntaxKind::DOC_COMMENT
        } else {
            SyntaxKind::BLOCK_COMMENT
        };
        self.emit(kind, start);
    }

    // ----- ( … casts --------------------------------------------------------------

    /// Zend applies the cast rule lexically everywhere in `ST_IN_SCRIPTING`:
    /// `"("{TABS_AND_SPACES}("int"|…){TABS_AND_SPACES}")"` is one token,
    /// interior whitespace included. The `(real)` cast is the one the
    /// scanner itself throws on, in the sentence 8.0 gave it:
    ///
    /// ```php
    /// ( Real )$x;
    /// ```
    /// ```expect exit=255
    /// Parse error: The (real) cast has been removed, use (float) instead in %s on line 2
    /// ```
    fn lex_paren_or_cast(&mut self) {
        let start = self.pos;
        let mut i = start + 1;
        while matches!(self.byte(i), b' ' | b'\t') {
            i += 1;
        }
        let word_start = i;
        while self.byte(i).is_ascii_alphabetic() {
            i += 1;
        }
        let word = &self.text[word_start..i];
        while matches!(self.byte(i), b' ' | b'\t') {
            i += 1;
        }
        if self.byte(i) == b')' && !word.is_empty() {
            let removed = CASTS_REMOVED.iter().any(|c| word.eq_ignore_ascii_case(c));
            if removed || CASTS.iter().any(|c| word.eq_ignore_ascii_case(c)) {
                self.pos = i + 1;
                let span = self.span(start, self.pos);
                let line = self.line(start);
                let kind = if removed {
                    // `(real)` and `(unset)` lex as one token. php's
                    // scanner throws on `(real)` as it scans it, so that
                    // one is raised here; `(unset)` reaches php's
                    // compiler as a cast and is the parser's to refuse
                    // (design §6.1's lexical fatals).
                    if word.eq_ignore_ascii_case(b"real") {
                        self.diag(Diagnostic::new(
                            span,
                            line,
                            "The (real) cast has been removed, use (float) instead",
                        ));
                    }
                    SyntaxKind::CAST_REMOVED
                } else {
                    // The four spellings 8.5 deprecates, each naming the
                    // canonical one, `zend_language_scanner.l` at the pin.
                    let canonical = match word.to_ascii_lowercase().as_slice() {
                        b"integer" => Some(("integer", "int")),
                        b"double" => Some(("double", "float")),
                        b"binary" => Some(("binary", "string")),
                        b"boolean" => Some(("boolean", "bool")),
                        _ => None,
                    };
                    if let Some((spelled, canonical)) = canonical {
                        self.diag(Diagnostic::of(
                            Level::Deprecated,
                            span,
                            line,
                            format!(
                                "Non-canonical cast ({spelled}) is deprecated, use the ({canonical}) cast instead"
                            ),
                        ));
                    }
                    SyntaxKind::CAST
                };
                self.emit(kind, start);
                return;
            }
        }
        self.punct(SyntaxKind::L_PAREN, 1);
    }

    // ----- \  \A\B ------------------------------------------------------------------

    fn lex_backslash(&mut self) {
        let start = self.pos;
        if is_label_start(self.byte(start + 1)) {
            // ("\\"{LABEL})+ is T_NAME_FULLY_QUALIFIED, keywords included.
            let mut i = start;
            while self.byte(i) == b'\\' && is_label_start(self.byte(i + 1)) {
                i += 1 + self.label_len(i + 1);
            }
            self.pos = i;
            self.emit(SyntaxKind::NAME_FULLY_QUALIFIED, start);
        } else {
            self.punct(SyntaxKind::BACKSLASH, 1);
        }
    }

    // ----- identifiers, keywords, names, b-prefixed strings ---------------------------

    pub(crate) fn label_len(&self, at: usize) -> usize {
        let rest = self.text.get(at..).unwrap_or_default();
        match rest.split_first() {
            Some((&first, tail)) if is_label_start(first) => {
                1 + tail
                    .iter()
                    .position(|&b| !is_label_continue(b))
                    .unwrap_or(tail.len())
            }
            _ => 0,
        }
    }

    fn lex_ident_like(&mut self) {
        let start = self.pos;
        // b'…', b"…" and b<<<… prefixes (Zend: b?['] and kin).
        if matches!(self.byte(start), b'b' | b'B') && !is_label_continue(self.byte(start + 1)) {
            match self.byte(start + 1) {
                b'\'' => {
                    self.lex_single_quoted(start);
                    return;
                }
                b'"' => {
                    // The opening delimiter token text is `b"` (kind
                    // DOUBLE_QUOTE); the closing one is a bare `"`.
                    self.pos = start + 2;
                    self.emit(SyntaxKind::DOUBLE_QUOTE, start);
                    self.push_state(State::DoubleQuoted, self.span(start, start + 2));
                    return;
                }
                _ => {}
            }
        }
        if matches!(self.byte(start), b'b' | b'B')
            && self.starts_with_ci(start + 1, b"<<<")
            && self.try_heredoc_start(start)
        {
            return;
        }

        let label_end = start + self.label_len(start);

        // The qualified-name scan wins over keyword classification whenever
        // a `\` followed by a label trails the first segment (php-src lexes
        // {LABEL}("\\"{LABEL})+ by longest match, so `list\B` is a name).
        if self.byte(label_end) == b'\\' && is_label_start(self.byte(label_end + 1)) {
            let mut i = label_end;
            while self.byte(i) == b'\\' && is_label_start(self.byte(i + 1)) {
                i += 1 + self.label_len(i + 1);
            }
            self.pos = i;
            let kind = if self.text[start..label_end].eq_ignore_ascii_case(b"namespace") {
                SyntaxKind::NAME_RELATIVE
            } else {
                SyntaxKind::NAME_QUALIFIED
            };
            self.emit(kind, start);
            return;
        }

        self.pos = label_end;
        let word = &self.text[start..label_end];
        // Every magic constant opens with `__`, so a label that does not
        // is read against the keywords alone.
        let kind = if word.starts_with(b"__")
            && MAGIC_CONSTS.iter().any(|m| word.eq_ignore_ascii_case(m))
        {
            SyntaxKind::MAGIC_CONST
        } else {
            SyntaxKind::from_keyword(word).unwrap_or(SyntaxKind::IDENT)
        };
        self.emit(kind, start);
    }

    // ----- error bytes ---------------------------------------------------------------------

    /// Bytes no scripting rule accepts (controls, DEL): Zend's `{ANY_CHAR}`
    /// hands each back as `T_BAD_CHARACTER` and its parser says
    /// `unexpected character 0x%02X` for the first it meets, so the span
    /// is one ERROR token, named by the parser where it reaches it.
    fn lex_bad_bytes(&mut self) {
        let start = self.pos;
        while self.pos < self.text.len() && is_bad_scripting_byte(self.byte(self.pos)) {
            self.pos += 1;
        }
        self.emit(SyntaxKind::ERROR, start);
    }
}

fn is_bad_scripting_byte(b: u8) -> bool {
    // Everything with a rule above is "good"; the leftovers are control
    // characters (less \t \r \n) and DEL.
    matches!(b, 0x00..=0x08 | 0x0B | 0x0C | 0x0E..=0x1F | 0x7F)
}

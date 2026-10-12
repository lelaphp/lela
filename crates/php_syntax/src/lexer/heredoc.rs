//! Heredoc and nowdoc (Zend `ST_HEREDOC` and `ST_NOWDOC`), with the 7.3
//! flexible rules: an indented closing marker, the stripped indentation
//! recorded as `HEREDOC_INDENT` tokens so the stream stays lossless, and a
//! body line that begins with the label but goes on with label characters
//! not closing it.

use crate::source::Diagnostic;
use crate::value::Quote;

use super::strings::Trigger;
use super::{Lexer, State, is_label_continue, is_label_start};
use crate::SyntaxKind;

/// Lazily computed facts about a heredoc's closing marker, cached in the
/// state's aux slot once the body is first scanned.
///
/// Zend records the same shape, an indentation count and a spaces-or-tabs
/// bit rather than the byte string, which is why a body line indented with
/// the other character class is the "cannot be mixed" error even at equal
/// width.
#[derive(Clone, Copy)]
pub(crate) struct HeredocInfo {
    /// Closing-marker indentation width in bytes (0 = nothing to strip).
    indent_len: usize,
    /// Whether the closing indentation uses spaces (its first byte).
    uses_spaces: bool,
    /// The closing indentation itself mixes tabs and spaces.
    mixed_closing: bool,
}

/// php's text for a body line, or the closing marker, indented with both
/// characters.
const MIXED_INDENTATION: &str = "Invalid indentation - tabs and spaces cannot be mixed";

impl Lexer<'_> {
    // ----- <<<LABEL intro ------------------------------------------------------

    /// Tries Zend's `b?"<<<"{TABS_AND_SPACES}({LABEL}|'{LABEL}'|"{LABEL}"){NEWLINE}`
    /// at `start`. On a match, emits `HEREDOC_START`, `<<<` through the
    /// newline inclusive, the extent of `T_START_HEREDOC`, and pushes the
    /// body state. False, consuming nothing, when the pattern fails, so
    /// `<<` and `<` lex as operators.
    pub(crate) fn try_heredoc_start(&mut self, start: usize) -> bool {
        let intro = if matches!(self.byte(start), b'b' | b'B') {
            start + 1
        } else {
            start
        };
        debug_assert!(self.starts_with_ci(intro, b"<<<"));
        let mut i = intro + 3;
        while matches!(self.byte(i), b' ' | b'\t') {
            i += 1;
        }
        let quote = match self.byte(i) {
            q @ (b'\'' | b'"') => {
                i += 1;
                Some(q)
            }
            _ => None,
        };
        if !is_label_start(self.byte(i)) {
            return false;
        }
        let label_start = i;
        i += self.label_len(i);
        let label_end = i;
        if let Some(q) = quote {
            if self.byte(i) != q {
                return false;
            }
            i += 1;
        }
        // NEWLINE = \r\n | \n | \r, required directly after the label.
        match self.byte(i) {
            b'\r' if self.byte(i + 1) == b'\n' => i += 2,
            b'\n' => i += 1,
            b'\r' if i < self.text.len() => i += 1,
            _ => return false,
        }

        let label = self.text[label_start..label_end].to_vec();
        self.pos = i;
        self.emit(SyntaxKind::HEREDOC_START, start);
        let opener = self.span(intro, intro + 3); // the `<<<`, for the EOF diagnostic
        let state = if quote == Some(b'\'') {
            State::Nowdoc { label }
        } else {
            State::Heredoc { label } // bare and "LABEL" both interpolate
        };
        self.push_state(state, opener);
        true
    }

    // ----- body -----------------------------------------------------------------

    /// One dispatch step inside a heredoc or nowdoc body.
    pub(crate) fn lex_heredoc_body(&mut self, interpolating: bool) {
        let label = match self.active_state() {
            State::Heredoc { label } | State::Nowdoc { label } => label.clone(),
            // The dispatch loop reaches here from those two states alone.
            _ => Vec::new(),
        };
        let info = self.ensure_heredoc_info(&label);
        let start = self.pos;

        // A token boundary whose backward walk to the line start is all
        // [ \t] is a "logical line start": terminator detection and
        // indentation stripping apply there.
        if let Some(line_start) = self.ws_prefix_line_start(start) {
            if let Some((ws_end, label_end)) = self.terminator_at(line_start, &label) {
                // The closing marker: its leading indentation is a
                // HEREDOC_INDENT token, the label itself HEREDOC_END.
                if start < ws_end {
                    self.pos = ws_end;
                    if info.mixed_closing {
                        // php finds the closing marker's mix while it
                        // scans the body ahead, and reports it on the
                        // line after the opener whichever line the
                        // marker stands on (probed 8.5.9).
                        let span = self.span(line_start, ws_end);
                        let line = self.heredoc_opener_line().saturating_add(1);
                        self.diag(Diagnostic::new(span, line, MIXED_INDENTATION));
                    }
                    self.emit(SyntaxKind::HEREDOC_INDENT, start);
                }
                let end_start = self.pos;
                self.pos = label_end;
                self.emit(SyntaxKind::HEREDOC_END, end_start);
                self.pop_state();
                return;
            }
            // A body line: strip up to indent_len leading [ \t] bytes as a
            // HEREDOC_INDENT token, diagnosing once per line (only when the
            // boundary sits exactly at the line start, so the post-indent
            // dispatch does not re-report).
            if info.indent_len > 0 {
                let strip_end = self.body_line_strip_end(line_start, &info);
                if start == line_start {
                    self.hold_body_indent(line_start, strip_end, &info);
                }
                if start < strip_end {
                    self.pos = strip_end;
                    self.emit(SyntaxKind::HEREDOC_INDENT, start);
                    return;
                }
            }
        }

        // Content: an interpolation trigger, or a text span.
        if interpolating && let Some(trigger) = self.interp_trigger_at(start) {
            self.lex_trigger(trigger);
            return;
        }
        self.scan_body_text(&label, interpolating, info.indent_len);
        debug_assert!(self.pos > start, "empty heredoc text span");
        self.emit(SyntaxKind::STRING_TEXT, start);
    }

    /// Scans a `STRING_TEXT` span: stops at an interpolation trigger
    /// (heredoc only), at EOF, and at a line start that needs handling of
    /// its own (the terminator line, or any line while indentation
    /// stripping is on).
    fn scan_body_text(&mut self, label: &[u8], interpolating: bool, indent_len: usize) {
        loop {
            if self.pos >= self.text.len() {
                return;
            }
            if interpolating && self.interp_trigger_at(self.pos).is_some() {
                return;
            }
            match self.byte(self.pos) {
                // Zend's ST_HEREDOC escape rule deliberately does not skip a
                // newline after `\` (unlike ST_DOUBLE_QUOTES), so the line
                // accounting, and terminator detection, still see the break.
                b'\\' if interpolating => {
                    if matches!(self.byte(self.pos + 1), b'\r' | b'\n') {
                        self.pos += 1;
                    } else {
                        self.eat_escape(Quote::Heredoc);
                    }
                    continue;
                }
                b'\r' => {
                    self.pos += 1;
                    if self.byte(self.pos) == b'\n' {
                        self.pos += 1; // \r\n is one newline
                    }
                }
                b'\n' => self.pos += 1,
                _ => {
                    self.pos += 1;
                    continue;
                }
            }
            // At a fresh line start: stop before the terminator, and break
            // per line whenever stripping must interleave indent tokens.
            if indent_len > 0 || self.terminator_at(self.pos, label).is_some() {
                return;
            }
        }
    }

    /// Is the line beginning at `line_start` the closing-marker line? If so,
    /// the end of its leading whitespace and the end of the label. The
    /// label match is byte-exact, and a label character after it defeats
    /// it: the body line `ENDING` does not close the label `END`.
    fn terminator_at(&self, line_start: usize, label: &[u8]) -> Option<(usize, usize)> {
        let mut i = line_start;
        while matches!(self.byte(i), b' ' | b'\t') {
            i += 1;
        }
        let rest = self.text.get(i..)?;
        if rest.len() >= label.len()
            && &rest[..label.len()] == label
            && !is_label_continue(self.byte(i + label.len()))
        {
            Some((i, i + label.len()))
        } else {
            None
        }
    }

    /// Walks back over [ \t] from a token boundary; the line start when the
    /// boundary is separated from it by whitespace only.
    fn ws_prefix_line_start(&self, pos: usize) -> Option<usize> {
        let mut i = pos;
        while i > 0 {
            match self.text[i - 1] {
                b' ' | b'\t' => i -= 1,
                b'\n' | b'\r' => return Some(i),
                _ => return None,
            }
        }
        Some(0)
    }

    /// How far indentation stripping reaches on a body line: up to
    /// `indent_len` bytes of [ \t].
    fn body_line_strip_end(&self, line_start: usize, info: &HeredocInfo) -> usize {
        let mut i = line_start;
        while i - line_start < info.indent_len && matches!(self.byte(i), b' ' | b'\t') {
            i += 1;
        }
        i
    }

    /// The 7.3 indentation rules, once per body line, as php-src's
    /// per-character walk applies them: a whitespace byte of the wrong
    /// class is the mix error; the first non-whitespace byte before the
    /// strip width is the under-indentation error, unless it is the line's
    /// newline (an empty line, or whitespace then a newline, is exempt).
    /// One defect per line, and the wrong-class byte always precedes the
    /// content byte, so the mix wins when both apply, Zend's order.
    ///
    /// ```php
    /// echo <<<EOT
    ///   a
    ///  b
    ///   EOT;
    /// ```
    /// ```expect exit=255
    /// Parse error: Invalid body indentation level (expecting an indentation level of at least 2) in %s on line 4
    /// ```
    fn hold_body_indent(&mut self, line_start: usize, strip_end: usize, info: &HeredocInfo) {
        // php finds the closing marker before it judges a body line, and
        // a marker that mixes tabs and spaces is where it throws: no
        // body line is judged after that.
        if info.mixed_closing {
            return;
        }
        let prefix = self.span(line_start, strip_end);
        let bad_mix = (line_start..strip_end).any(|i| {
            let is_space = self.byte(i) == b' ';
            is_space != info.uses_spaces
        });
        let line = self.line(line_start);
        if bad_mix {
            self.diag(Diagnostic::new(prefix, line, MIXED_INDENTATION));
            return;
        }
        let stripped = strip_end - line_start;
        if stripped < info.indent_len
            && strip_end < self.text.len()
            && !matches!(self.byte(strip_end), b'\r' | b'\n')
        {
            self.diag(Diagnostic::new(
                prefix,
                line,
                format!(
                    "Invalid body indentation level (expecting an indentation level of at least {})",
                    info.indent_len
                ),
            ));
        }
    }

    /// The line of the open heredoc's `<<<`, read off the state frame.
    fn heredoc_opener_line(&self) -> u32 {
        let opener = self.frames.top().aux.opener.start as usize;
        self.line(opener)
    }

    /// Computes, once per heredoc and cached in the aux slot, the closing
    /// marker's indentation facts by scanning forward line by line from the
    /// current position. One forward pass per heredoc; body lexing then
    /// reads the cache, so there is no per-line rescan.
    fn ensure_heredoc_info(&mut self, label: &[u8]) -> HeredocInfo {
        if let Some(info) = self.frames.top().aux.heredoc {
            return info;
        }
        // Start at the current line if the position sits in its whitespace
        // prefix (the line could itself be the closing marker); a
        // mid-content position means the current line cannot close, so
        // scan from the next one.
        let scan_from = self
            .ws_prefix_line_start(self.pos)
            .or_else(|| self.next_line_start(self.pos));
        let mut line = scan_from;
        let mut computed = HeredocInfo {
            indent_len: 0,
            uses_spaces: false,
            mixed_closing: false,
        };
        // Interpolation awareness: a label-shaped line inside a `${…}` or
        // `{$…}` belongs to an interpolated expression (possibly a nested
        // heredoc's closer; php-src's flexible-heredoc-complex tests nest
        // the same label), not to this heredoc. Track brace depth textually
        // and accept a candidate at depth 0 only. The openers are exactly
        // the two-byte interpolation triggers (`Trigger::DollarCurly` and
        // `CurlyDollar`, shared with live lexing so the two scans agree on
        // what opens one), plus a bare `{` once already inside; each closes
        // with one `}`. The two-byte openers advance past both bytes so the
        // `{` of a `${` is never re-counted as a bare brace. A brace
        // imbalance hidden inside a string within an interpolation can
        // still fool the count; php-src sidesteps it by stripping at
        // compile time, after nesting is fully known.
        let mut depth: usize = 0;
        let mut prev = scan_from.unwrap_or(self.pos);
        while let Some(ls) = line {
            let mut i = prev;
            while i < ls {
                if let Some(Trigger::DollarCurly | Trigger::CurlyDollar) = self.interp_trigger_at(i)
                {
                    depth += 1;
                    i += 2; // `${` and `{$` are one opener, not opener plus brace
                    continue;
                }
                match self.byte(i) {
                    b'{' if depth > 0 => depth += 1,
                    b'}' if depth > 0 => depth -= 1,
                    _ => {}
                }
                i += 1;
            }
            prev = ls;
            if depth == 0
                && let Some((ws_end, _)) = self.terminator_at(ls, label)
            {
                let indent = &self.text[ls..ws_end];
                computed.indent_len = indent.len();
                computed.uses_spaces = indent.first() == Some(&b' ');
                computed.mixed_closing = indent.contains(&b' ') && indent.contains(&b'\t');
                break;
            }
            line = self.next_line_start(ls);
            // line == None: unterminated, nothing to strip (the EOF diagnostic says so).
        }
        self.frames.top_mut().aux.heredoc = Some(computed);
        computed
    }

    pub(crate) fn next_line_start(&self, from: usize) -> Option<usize> {
        let mut i = from;
        while i < self.text.len() {
            match self.text[i] {
                b'\n' => return Some(i + 1),
                b'\r' => {
                    return Some(if self.byte(i + 1) == b'\n' {
                        i + 2
                    } else {
                        i + 1
                    });
                }
                _ => i += 1,
            }
        }
        None
    }
}

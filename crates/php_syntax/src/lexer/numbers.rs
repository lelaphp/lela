//! Numeric tokens: decimal, hex, binary, `0o` and legacy-octal integers,
//! exponent floats, and digit-separator underscores.
//!
//! Recovery is one token, one diagnostic: a misplaced underscore, a bad
//! octal digit or a bare radix prefix raises the text php's parser would
//! print, and the whole numeric span is still one INT or FLOAT. Overflow is
//! not read here: an integer that does not fit is the compiler's to fold
//! to a float, as `zend_compile.c` does.

use crate::source::Diagnostic;

use super::Lexer;
use crate::SyntaxKind;

impl Lexer<'_> {
    /// Entry; `self.pos` sits on a digit, or on `.` with a digit next.
    pub(crate) fn lex_number(&mut self) {
        let start = self.pos;
        // Radix-prefixed forms.
        if self.byte(start) == b'0' {
            match self.byte(start + 1) {
                b'x' | b'X' => {
                    self.lex_radix(start, u8::is_ascii_hexdigit);
                    return;
                }
                b'b' | b'B' => {
                    self.lex_radix(start, |b| matches!(b, b'0' | b'1'));
                    return;
                }
                b'o' | b'O' => {
                    self.lex_radix(start, |b| matches!(b, b'0'..=b'7'));
                    return;
                }
                _ => {}
            }
        }

        // Decimal and float. Zend's grammar, mirrored by longest match:
        //   LNUM   [0-9]+(_[0-9]+)*
        //   DNUM   ({LNUM}?"."{LNUM})|({LNUM}"."{LNUM}?)
        //   EXPONENT_DNUM (({LNUM}|{DNUM})[eE][+-]?{LNUM})
        let mut underscore_issue: Option<usize> = None;
        let mut is_float = false;

        self.eat_digits(&mut underscore_issue);
        if self.byte(self.pos) == b'.' {
            let has_int_part = self.pos > start;
            // `.5` (the caller guarantees the digit), `1.5`, and trailing-dot
            // `1.` are all DNUM. A second `.` is never consumed (`1..2` is
            // FLOAT DOT INT, flex's longest match).
            if has_int_part || self.byte(self.pos + 1).is_ascii_digit() {
                if has_int_part && self.byte(self.pos - 1) == b'_' && underscore_issue.is_none() {
                    underscore_issue = Some(self.pos - 1); // `1_.5`
                }
                self.pos += 1;
                is_float = true;
                if self.byte(self.pos) == b'_' && underscore_issue.is_none() {
                    underscore_issue = Some(self.pos); // `1._5`
                }
                self.eat_digits(&mut underscore_issue);
            }
        }
        // An exponent only when [eE][+-]?[0-9] follows: `1e` is INT(1)
        // IDENT(e) in php-src (EXPONENT_DNUM fails, LNUM wins).
        if matches!(self.byte(self.pos), b'e' | b'E') {
            let mut i = self.pos + 1;
            if matches!(self.byte(i), b'+' | b'-') {
                i += 1;
            }
            if self.byte(i).is_ascii_digit() {
                if self.pos > start && self.byte(self.pos - 1) == b'_' && underscore_issue.is_none()
                {
                    underscore_issue = Some(self.pos - 1); // `1_e5`
                }
                self.pos = i;
                is_float = true;
                self.eat_digits(&mut underscore_issue);
            }
        }

        if let Some(at) = underscore_issue {
            self.report_underscore(at);
        } else if !is_float && self.byte(start) == b'0' && self.pos - start > 1 {
            // Legacy octal: php-src lexes the digit span and throws
            // `Invalid numeric literal` on an 8 or a 9 (probed 8.5.7: `08`,
            // `0779`); one INT token here, the diagnostic on the digit.
            if let Some(bad) = (start..self.pos).find(|&i| matches!(self.byte(i), b'8' | b'9')) {
                let span = self.span(bad, bad + 1);
                self.diag(Diagnostic::new(span, "Invalid numeric literal"));
            }
        }

        let kind = if is_float {
            SyntaxKind::FLOAT
        } else {
            SyntaxKind::INT
        };
        self.emit(kind, start);
    }

    /// `0x`, `0b` and `0o` forms: the prefix plus the maximal digit-or-
    /// underscore span; a bare prefix or a misplaced underscore raises a
    /// diagnostic but stays one INT token.
    fn lex_radix(&mut self, start: usize, is_digit: fn(&u8) -> bool) {
        self.pos = start + 2; // `0x` and kin
        let digits_start = self.pos;
        let mut digits = 0usize;
        let mut underscore_issue: Option<usize> = None;
        while is_digit(&self.byte(self.pos)) || self.byte(self.pos) == b'_' {
            if self.byte(self.pos) == b'_' {
                // Legal only between digits: `0x_1`, `0x1__2`, `0x1_` are out.
                let prev_digit = self.pos > digits_start && is_digit(&self.byte(self.pos - 1));
                let next_digit = is_digit(&self.byte(self.pos + 1));
                if (!prev_digit || !next_digit) && underscore_issue.is_none() {
                    underscore_issue = Some(self.pos);
                }
            } else {
                digits += 1;
            }
            self.pos += 1;
        }
        if digits == 0 {
            let span = self.span(start, self.pos);
            // No digit after the prefix: php-src lexes `0`, then the prefix
            // letter and the rest as an identifier (`0x_0123` is `0` and
            // `x_0123`), and its parser says `unexpected identifier "<id>"`.
            let text = unexpected_identifier(&self.ident_span(start + 1));
            self.diag(Diagnostic::new(span, text));
        } else if let Some(at) = underscore_issue {
            // The identifier php tokenizes: when the misplaced `_` is the
            // first byte after the prefix (`0x_0123`, no leading hex digit),
            // php-src fails the `0x` form entirely and lexes `0` and the
            // prefix letter onward (`x_0123`); otherwise (`0x1_…`) the
            // valid hex span stands and the identifier begins at the `_`.
            let id_start = if at == digits_start { start + 1 } else { at };
            let span = self.span(at, at + 1);
            let text = unexpected_identifier(&self.ident_span(id_start));
            self.diag(Diagnostic::new(span, text));
        }
        self.emit(SyntaxKind::INT, start);
    }

    /// Consumes a `[0-9_]*` span, recording the first underscore that is
    /// not strictly between two digits.
    fn eat_digits(&mut self, underscore_issue: &mut Option<usize>) {
        let digits_start = self.pos;
        while self.byte(self.pos).is_ascii_digit() || self.byte(self.pos) == b'_' {
            if self.byte(self.pos) == b'_' {
                let prev_digit =
                    self.pos > digits_start && self.byte(self.pos - 1).is_ascii_digit();
                let next_digit = self.byte(self.pos + 1).is_ascii_digit();
                if (!prev_digit || !next_digit) && underscore_issue.is_none() {
                    *underscore_issue = Some(self.pos);
                }
            }
            self.pos += 1;
        }
    }

    /// php-src has no diagnostic of its own for a misplaced underscore: it
    /// lexes the longest valid number, then the `_…` span as an identifier,
    /// and its parser says `syntax error, unexpected identifier "<id>"`.
    /// The identifier php sees begins at the misplaced underscore.
    fn report_underscore(&mut self, at: usize) {
        let span = self.span(at, at + 1);
        let text = unexpected_identifier(&self.ident_span(at));
        self.diag(Diagnostic::new(span, text));
    }

    /// The maximal span of identifier bytes at `from` (`[A-Za-z0-9_]` plus
    /// high bytes): the identifier php-src tokenizes after a malformed
    /// number.
    fn ident_span(&self, from: usize) -> Vec<u8> {
        let mut i = from;
        loop {
            let b = self.byte(i);
            if b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80 {
                i += 1;
            } else {
                break;
            }
        }
        (from..i).map(|j| self.byte(j)).collect()
    }
}

/// `syntax error, unexpected identifier "<id>"`: php-src's parse-error
/// text for a stray identifier.
fn unexpected_identifier(id: &[u8]) -> String {
    format!(
        "syntax error, unexpected identifier \"{}\"",
        String::from_utf8_lossy(id)
    )
}

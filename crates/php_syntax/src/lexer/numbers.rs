//! Numeric tokens: decimal, hex, binary, `0o` and legacy-octal integers,
//! exponent floats, and digit-separator underscores.
//!
//! The token ends where Zend's pattern ends, longest match: `0x_1` is
//! `0` and the identifier `x_1`, `1__2` is `1` and `__2`, `1_.5` is `1`,
//! `_` and `.5`, exactly as `token_get_all` cuts them, so what php's
//! parser then says (`unexpected identifier "x_1"`, with the site's
//! clause) this parser says at the same token. The one diagnostic raised
//! here is the scanner's own: a legacy octal number with an `8` or a
//! `9`, which php lexes whole and throws `Invalid numeric literal` on,
//! found by [`value::number`], the one reading of the digits in the
//! crate. The value is not read here: an integer that does not fit is
//! `TokenValue::Float` when the token's value is asked for, as the `LNUM`
//! rule's retry makes it, and `T_DNUMBER` to `token_get_all`.

use crate::source::Diagnostic;
use crate::value::{self, Radix};

use super::Lexer;
use crate::SyntaxKind;

impl Lexer<'_> {
    /// Entry; `self.pos` sits on a digit, or on `.` with a digit next.
    ///
    /// ```php
    /// echo 08;
    /// ```
    /// ```expect exit=255
    /// Parse error: Invalid numeric literal in %s on line 2
    /// ```
    #[inline(never)]
    pub(crate) fn lex_number(&mut self) {
        let start = self.pos;
        // Radix-prefixed forms: HNUM, BNUM, ONUM. Without a digit after
        // the prefix the pattern fails and LNUM matches the `0` alone.
        if let Some(radix) = Radix::prefixed(self.text, start) {
            self.pos = start + 2;
            if !self.eat_digit_run(|b| radix.takes(b)) {
                self.pos = start + 1;
            }
            self.emit(SyntaxKind::INT, start);
            return;
        }

        // Decimal and float. Zend's grammar, mirrored by longest match:
        //   LNUM   [0-9]+(_[0-9]+)*
        //   DNUM   ({LNUM}?"."{LNUM})|({LNUM}"."{LNUM}?)
        //   EXPONENT_DNUM (({LNUM}|{DNUM})[eE][+-]?{LNUM})
        let mut is_float = false;
        let has_int_part = self.eat_digit_run(|b| Radix::Decimal.takes(b));
        if self.byte(self.pos) == b'.' {
            // `.5` (the caller guarantees the digit), `1.5`, and trailing-dot
            // `1.` are all DNUM. A second `.` is never consumed (`1..2` is
            // FLOAT DOT INT, flex's longest match).
            if has_int_part || self.byte(self.pos + 1).is_ascii_digit() {
                self.pos += 1;
                is_float = true;
                self.eat_digit_run(|b| Radix::Decimal.takes(b));
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
                self.pos = i;
                is_float = true;
                self.eat_digit_run(|b| Radix::Decimal.takes(b));
            }
        }

        let kind = if is_float {
            SyntaxKind::FLOAT
        } else {
            SyntaxKind::INT
        };
        // Legacy octal: php-src lexes the digit span and throws
        // `Invalid numeric literal` on an 8 or a 9 (probed 8.5.7: `08`,
        // `0779`); one INT token here, the diagnostic on the digit.
        if let Some(bad) = value::number(kind, &self.text[start..self.pos]).bad_digit {
            let bad = start + bad;
            let span = self.span(bad, bad + 1);
            let line = self.line(bad);
            self.diag(Diagnostic::new(span, line, "Invalid numeric literal"));
        }
        self.emit(kind, start);
    }

    /// Consumes `D+(_D+)*` for the digit class: an underscore only
    /// between two digits, the span ending before one that is not. Whether
    /// a digit was consumed at all.
    fn eat_digit_run(&mut self, is_digit: impl Fn(u8) -> bool) -> bool {
        let start = self.pos;
        while is_digit(self.byte(self.pos)) {
            self.pos += 1;
        }
        if self.pos == start {
            return false;
        }
        while self.byte(self.pos) == b'_' && is_digit(self.byte(self.pos + 1)) {
            self.pos += 1;
            while is_digit(self.byte(self.pos)) {
                self.pos += 1;
            }
        }
        true
    }
}

//! What a token stands for (design §4.9, ledger 27): the value php's
//! scanner makes of an `INT`, a `FLOAT`, a `SINGLE_QUOTED` or a
//! `STRING_TEXT` token before its parser's zval node holds it, answered
//! on demand from the token by [`TokenValue::of`]. php produces it as it
//! scans, and this file copies the rules it uses: `ZEND_STRTOL` with its
//! overflow retry to `zend_strtod`, and `zend_hex_strtod`,
//! `zend_oct_strtod` and `zend_bin_strtod` for the prefixed forms, from
//! the `LNUM`, `HNUM`, `ONUM`, `BNUM`, `DNUM` and `EXPONENT_DNUM` rules
//! of `zend_language_scanner.l` ([`number`]); `zend_scan_escape_string`
//! for a `"…"`, a heredoc or a `` `…` `` part, the mark it takes as an
//! escape being the string's own ([`escape`]); the two escapes of the
//! `'…'` rule; and nothing for a nowdoc. The token's kind and the node
//! that holds it ([`Holder`]) decide which, never the caller.
//!
//! **One table, two readers.** The lexer walks the same two functions
//! to diagnose: [`number`] is where the `8` or `9` of a legacy octal is
//! found, which the scanner throws `Invalid numeric literal` on, and
//! [`escape`] is where an octal escape past `\377` and a malformed
//! `\u{…}` are found, php's compile warning and parse error. The lexer
//! reads the defect and steps over the escape; [`TokenValue::of`] reads the
//! bytes; [`int_overflows`](crate::int_overflows) reads whether the
//! retry to a double happened. Nothing else in the crate spells a radix
//! or an escape, so php's table stands here once.

use crate::lexer::Token;
use crate::tree::{
    HeredocExpr, InterpolatedStringExpr, LiteralExpr, NowdocExpr, ShellExecExpr, SimpleDim, TokenId,
};
use crate::{Parsed, SyntaxKind};

/// The value of a token, as php's scanner hands it to the parser
/// (`RETURN_TOKEN_WITH_VAL`): a long, a double, or the bytes of a
/// string. An `INT` whose digits do not fit a long is a `Float`, as
/// `ZEND_STRTOL`'s `ERANGE` retry makes it.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenValue {
    /// php's `IS_LONG`: `T_LNUMBER`.
    Int(i64),
    /// php's `IS_DOUBLE`: `T_DNUMBER`, and a `T_LNUMBER` that overflowed.
    Float(f64),
    /// php's `IS_STRING`: `T_CONSTANT_ENCAPSED_STRING` and
    /// `T_ENCAPSED_AND_WHITESPACE`, the escapes read.
    Bytes(Vec<u8>),
}

/// The node that holds the token, which is what decides how its text is
/// read: the kind of string a `STRING_TEXT` part belongs to is the
/// node's, php's `quote_type` argument to `zend_scan_escape_string`,
/// `'"'` from the `"…"` rule, `` '`' `` from the backquote rule and `0`
/// from the heredoc rule, and a nowdoc's text takes no escape at all.
#[derive(Debug, Clone, Copy)]
pub enum Holder<'a> {
    /// A `LiteralExpr`, php's `scalar`: an `INT`, a `FLOAT` or a
    /// `SINGLE_QUOTED` token.
    Scalar(&'a LiteralExpr),
    /// An `InterpolatedStringExpr`: the parts of a `"…"`.
    DoubleQuoted(&'a InterpolatedStringExpr),
    /// A `HeredocExpr`: the parts of a heredoc body.
    Heredoc(&'a HeredocExpr),
    /// A `NowdocExpr`: the text of a nowdoc body.
    Nowdoc(&'a NowdocExpr),
    /// A `ShellExecExpr`: the parts of a `` `…` ``.
    ShellExec(&'a ShellExecExpr),
    /// A `SimpleDim`, the `[…]` after a variable inside a string: its
    /// `ENCAPS_INT` or `INT` index, php's `T_NUM_STRING`.
    Offset(&'a SimpleDim),
}

impl TokenValue {
    /// The value of `token` under `holder`, from the token's text in the
    /// source; `None` for a token the holder does not read as one, a
    /// `HEREDOC_START` say. A heredoc or nowdoc part that ends the body
    /// loses the newline before the closing label, which php counts as
    /// the marker's and not the body's (`yyleng - newline` in the
    /// `ST_HEREDOC` and `ST_NOWDOC` rules).
    ///
    /// A decimal `INT`, its `_` separators, the retry to a double past
    /// `ZEND_LONG_MAX`, and the `DNUM` and `EXPONENT_DNUM` forms, the
    /// exponent that does not fit a double among them:
    ///
    /// ```php
    /// var_dump(42, 1_000_000, 9223372036854775807, 9223372036854775808, 1.5, .5, 1., 1e3, 1E-3, 1_0.5_0, 1e1000);
    /// ```
    /// ```expect
    /// int(42)
    /// int(1000000)
    /// int(9223372036854775807)
    /// float(9.223372036854776E+18)
    /// float(1.5)
    /// float(0.5)
    /// float(1)
    /// float(1000)
    /// float(0.001)
    /// float(10.5)
    /// float(INF)
    /// ```
    ///
    /// A `SINGLE_QUOTED` token takes `\\` and `\'` and leaves every
    /// other backslash where it stands, and the `b` prefix, in either
    /// case, goes with the quotes:
    ///
    /// ```php
    /// var_dump('a\\b\'c\nd\\', b'e', B'e');
    /// ```
    /// ```expect
    /// string(9) "a\b'c\nd\"
    /// string(1) "e"
    /// string(1) "e"
    /// ```
    ///
    /// The `[…]` index after a variable inside a string, the scanner's
    /// `ST_VAR_OFFSET` state: a decimal with no leading zero that fits
    /// a long is `IS_LONG`, and every other spelling, `01`, `0x1`,
    /// `1_0` and a decimal past `ZEND_LONG_MAX`, is the string as
    /// written (`Z/Zend/zend_language_scanner.l:2207-2230` at the pin),
    /// so `$a[1]` and `$a[01]` read two different keys:
    ///
    /// ```php
    /// $a = [1 => 'int', '01' => 'str', '0x1' => 'hex', '1_0' => 'sep'];
    /// echo "$a[1] $a[01] $a[0x1] $a[1_0]\n";
    /// ```
    /// ```expect
    /// int str hex sep
    /// ```
    #[must_use]
    pub fn of(parsed: &Parsed, holder: Holder<'_>, token: TokenId) -> Option<TokenValue> {
        let at = parsed.tokens.get(token.0 as usize)?;
        if matches!(at.kind, SyntaxKind::SINGLE_QUOTED | SyntaxKind::STRING_TEXT)
            && let Some(filtered) = parsed.filtered.get(token)
        {
            return Some(TokenValue::Bytes(filtered.to_vec()));
        }
        let text = parsed
            .source
            .get(at.offset as usize..(at.offset + at.len) as usize)?;
        Some(match (holder, at.kind) {
            (Holder::Scalar(_), SyntaxKind::INT | SyntaxKind::FLOAT) => number(at.kind, text).value,
            (Holder::Scalar(_), SyntaxKind::SINGLE_QUOTED) => {
                TokenValue::Bytes(single_quoted(text))
            }
            (Holder::DoubleQuoted(_), SyntaxKind::STRING_TEXT) => {
                TokenValue::Bytes(unescape(text, Quote::Double))
            }
            (Holder::ShellExec(_), SyntaxKind::STRING_TEXT) => {
                TokenValue::Bytes(unescape(text, Quote::Backquote))
            }
            (Holder::Heredoc(node), SyntaxKind::STRING_TEXT) => {
                let text = body(text, parsed.tokens, token, node.end);
                TokenValue::Bytes(unescape(text, Quote::Heredoc))
            }
            (Holder::Nowdoc(node), SyntaxKind::STRING_TEXT) => {
                TokenValue::Bytes(body(text, parsed.tokens, token, node.end).to_vec())
            }
            (Holder::Offset(_), SyntaxKind::ENCAPS_INT | SyntaxKind::INT) => offset(text),
            _ => return None,
        })
    }
}

/// `T_NUM_STRING`'s value: `[0]|([1-9][0-9]*)` that `ZEND_STRTOL` takes
/// without `ERANGE` is a long, and anything else the text.
pub(crate) fn offset(text: &[u8]) -> TokenValue {
    let canonical = text == b"0"
        || (text.first().is_some_and(|b| (b'1'..=b'9').contains(b))
            && text.iter().all(u8::is_ascii_digit));
    if canonical && let Ok(n) = std::str::from_utf8(text).unwrap_or_default().parse::<i64>() {
        return TokenValue::Int(n);
    }
    TokenValue::Bytes(text.to_vec())
}

/// The text of a heredoc or nowdoc part, less the newline before the
/// closing label when this part is the one before it: every token
/// between the two is the marker's own indentation, or there is none.
///
/// The newline goes in a heredoc and in a nowdoc alike, and a flexible
/// heredoc's indentation stands between the body and the label without
/// making that newline the body's (a lone `\r` before the label, which
/// php also counts as the marker's, is held in `tests/value.rs`, where
/// the byte can be written):
///
/// ```php
/// $v = 'v';
/// var_dump(<<<E
/// a
/// $v
/// E, <<<E
///   a
///   b
///   E, <<<'E'
/// a
/// E);
/// ```
/// ```expect
/// string(3) "a
/// v"
/// string(3) "a
/// b"
/// string(1) "a"
/// ```
fn body<'a>(text: &'a [u8], tokens: &[Token], token: TokenId, end: Option<TokenId>) -> &'a [u8] {
    let Some(end) = end else {
        return text;
    };
    let between = tokens
        .get(token.0 as usize + 1..end.0 as usize)
        .unwrap_or(&[]);
    if !between.iter().all(|t| t.kind == SyntaxKind::HEREDOC_INDENT) {
        return text;
    }
    // php: two bytes for `\r\n`, one for `\n` or a lone `\r`.
    if let Some(rest) = text.strip_suffix(b"\r\n") {
        rest
    } else if let Some(rest) = text
        .strip_suffix(b"\n")
        .or_else(|| text.strip_suffix(b"\r"))
    {
        rest
    } else {
        text
    }
}

// ----- numbers ----------------------------------------------------------------

/// The four radices of php's number rules: which bytes a rule takes as
/// digits and what each is worth. `HNUM`, `BNUM` and `ONUM` name theirs
/// by prefix; `LNUM` is octal under a leading `0` and decimal otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Radix {
    /// `0b`: `BNUM`.
    Binary,
    /// `0o`, and `LNUM` under a leading `0`: `ONUM` and the legacy form.
    Octal,
    /// `LNUM` otherwise.
    Decimal,
    /// `0x`: `HNUM`.
    Hex,
}

impl Radix {
    /// The rule a two-byte prefix at `at` names: `0x`, `0b` or `0o`, in
    /// either case; none for bare digits.
    pub(crate) fn prefixed(text: &[u8], at: usize) -> Option<Radix> {
        if text.get(at) != Some(&b'0') {
            return None;
        }
        match text.get(at + 1) {
            Some(b'x' | b'X') => Some(Radix::Hex),
            Some(b'b' | b'B') => Some(Radix::Binary),
            Some(b'o' | b'O') => Some(Radix::Octal),
            _ => None,
        }
    }

    /// Whether the rule takes `b` as a digit.
    pub(crate) fn takes(self, b: u8) -> bool {
        self.digit(b).is_some()
    }

    /// What `b` is worth as a digit of the rule; none for a byte the
    /// rule does not take, an `8` in octal among them.
    fn digit(self, b: u8) -> Option<u8> {
        let d = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return None,
        };
        (d < self.base()).then_some(d)
    }

    fn base(self) -> u8 {
        match self {
            Radix::Binary => 2,
            Radix::Octal => 8,
            Radix::Decimal => 10,
            Radix::Hex => 16,
        }
    }
}

/// What the scanner makes of an `INT` or a `FLOAT` token's text.
pub(crate) struct Number {
    /// The value: a long, or the double the retry produced.
    pub(crate) value: TokenValue,
    /// The offset in the text of the `8` or `9` a legacy octal carries,
    /// which php's `LNUM` rule throws `Invalid numeric literal` on. The
    /// value is then what the digits before it spell, as the rule reads
    /// them outside the parser (`len = i`), so `token_get_all` splits
    /// `T_LNUMBER` from `T_DNUMBER` on that prefix.
    pub(crate) bad_digit: Option<usize>,
}

/// php's number rules over one token's text. A `FLOAT` is `zend_strtod`
/// over the text with its `_` stripped, which is a correctly rounded
/// parse, as Rust's is. An `INT` is `ZEND_STRTOL` in the radix its
/// prefix or leading `0` names, and where the long overflows, the
/// retry: `zend_strtod` for decimal, and for the other three
/// `zend_hex_strtod`, `zend_oct_strtod` and `zend_bin_strtod`, each a
/// double accumulated digit by digit in the C expression as written,
/// which rounds at every step past 53 bits and is not the correctly
/// rounded parse; the difference is observable, so the accumulation is
/// copied, its order of operations included.
///
/// The four radices and what each spells, the `0x`, `0o` and `0b`
/// prefixes, the legacy leading `0` and `_` between digits among them:
///
/// ```php
/// var_dump(0x1F, 0o17, 017, 0b101, 0_7_7, 0xf_f);
/// ```
/// ```expect
/// int(31)
/// int(15)
/// int(15)
/// int(5)
/// int(63)
/// int(255)
/// ```
///
/// The last long and the first double in each radix: the hex, both
/// octal spellings and decimal retry to 2^63, and the binary rule's
/// `value * 2 + c - '0'` lands an ulp below it, which is the
/// accumulation and not a correctly rounded parse (php prints the two
/// rounded; the bit pattern is held in `tests/value.rs`):
///
/// ```php
/// var_dump(0x7FFFFFFFFFFFFFFF, 0x8000000000000000, 0777777777777777777777, 01000000000000000000000, 0o777777777777777777777, 0o1000000000000000000000, 0b111111111111111111111111111111111111111111111111111111111111111, 0b1000000000000000000000000000000000000000000000000000000000000000);
/// ```
/// ```expect
/// int(9223372036854775807)
/// float(9.223372036854776E+18)
/// int(9223372036854775807)
/// float(9.223372036854776E+18)
/// int(9223372036854775807)
/// float(9.223372036854776E+18)
/// int(9223372036854775807)
/// float(9.223372036854775E+18)
/// ```
///
/// Past 53 bits every step of the accumulation rounds, so a double from
/// one radix is not the double the same number reaches from another:
///
/// ```php
/// var_dump(0x1FFFFFFFFFFFFFFFF, 0xFFFFFFFFFFFFFFFFFF, 0x123456789ABCDEF12345, 0b1111111111111111111111111111111111111111111111111111111111111111, 0o7777777777777777777777, 01234567012345670123456701234567, 99999999999999999999999);
/// ```
/// ```expect
/// float(3.6893488147419103E+19)
/// float(4.722366482869645E+21)
/// float(8.596805828370696E+22)
/// float(1.844674407370955E+19)
/// float(7.378697629483821E+19)
/// float(1.6168958788107255E+27)
/// float(1.0E+23)
/// ```
///
/// Which side of `ZEND_LONG_MAX` a token falls on, in every radix and
/// with `_` between digits, is what `token_get_all` spells as
/// `T_LNUMBER` or `T_DNUMBER`:
///
/// ```php
/// foreach (token_get_all('<?php 9223372036854775807 9223372036854775808 0x7FFF_FFFF_FFFF_FFFF 0X8000000000000000 0777 01000000000000000000000 0o777 0O1000000000000000000000 0b1 0B1000000000000000000000000000000000000000000000000000000000000000') as $t) {
///     if (is_array($t) && $t[0] !== T_OPEN_TAG && $t[0] !== T_WHITESPACE) echo token_name($t[0]), "\n";
/// }
/// ```
/// ```expect
/// T_LNUMBER
/// T_DNUMBER
/// T_LNUMBER
/// T_DNUMBER
/// T_LNUMBER
/// T_DNUMBER
/// T_LNUMBER
/// T_DNUMBER
/// T_LNUMBER
/// T_DNUMBER
/// ```
pub(crate) fn number(kind: SyntaxKind, text: &[u8]) -> Number {
    // The common literal, a decimal of up to eighteen digits with no
    // `_` and no leading zero, fits a long whatever its digits: read in
    // one pass.
    if kind == SyntaxKind::INT
        && text.len() <= 18
        && text.first().is_some_and(|b| (b'1'..=b'9').contains(b))
        && text.iter().all(u8::is_ascii_digit)
    {
        let long = text
            .iter()
            .fold(0i64, |long, digit| long * 10 + i64::from(digit - b'0'));
        return Number {
            value: TokenValue::Int(long),
            bad_digit: None,
        };
    }
    if kind == SyntaxKind::FLOAT {
        let digits: String = text
            .iter()
            .filter(|b| **b != b'_')
            .map(|b| char::from(*b))
            .collect();
        return Number {
            // The lexer's `DNUM` and `EXPONENT_DNUM` shapes are inside
            // Rust's grammar for a double, so this parses.
            value: TokenValue::Float(digits.parse().unwrap_or(0.0)),
            bad_digit: None,
        };
    }
    let (radix, first) = match Radix::prefixed(text, 0) {
        Some(radix) => (radix, 2),
        None if text.first() == Some(&b'0') => (Radix::Octal, 0),
        None => (Radix::Decimal, 0),
    };
    // The digits as written, `_` dropped, up to the byte the radix
    // does not take: read in place, since a literal is read once per
    // compile and nearly every one fits a long.
    let bad_digit = text
        .iter()
        .enumerate()
        .skip(first)
        .find(|&(_, &b)| b != b'_' && radix.digit(b).is_none())
        .map(|(i, _)| i);
    let written = text
        .get(first..bad_digit.unwrap_or(text.len()))
        .unwrap_or_default();
    let digits = || written.iter().filter(|&&b| b != b'_');
    let base = radix.base();
    let mut long: i64 = 0;
    let fits = digits().all(|&b| {
        match long
            .checked_mul(i64::from(base))
            .and_then(|v| v.checked_add(i64::from(radix.digit(b).unwrap_or(0))))
        {
            Some(v) => {
                long = v;
                true
            }
            None => false,
        }
    });
    let value = if fits {
        TokenValue::Int(long)
    } else if radix == Radix::Decimal {
        let digits: String = digits().map(|&b| char::from(b)).collect();
        TokenValue::Float(digits.parse().unwrap_or(0.0))
    } else {
        TokenValue::Float(digits().fold(0.0, |value, &b| {
            let step = value * f64::from(base);
            match radix {
                // `value = value * 16 + c`, `c` the digit's worth.
                Radix::Hex => step + f64::from(radix.digit(b).unwrap_or(0)),
                // `value = value * 8 + c - '0'`, `c` the byte as
                // written: C reads it left to right, so the byte is
                // added and `'0'` taken away as two roundings, and
                // past 53 bits the first can carry the sum over a
                // power of two the second then falls short of
                // (`0b1` and 63 zeros is 2^63 - 1024 to php).
                Radix::Octal | Radix::Binary => (step + f64::from(b)) - f64::from(b'0'),
                Radix::Decimal => step,
            }
        }))
    };
    Number { value, bad_digit }
}

// ----- strings ----------------------------------------------------------------

/// php's `quote_type`: which mark `\` escapes in the string this text
/// is part of. `\"` is `"` in a `"…"` and the two bytes as written in a
/// heredoc or a `` `…` ``; `` \` `` the same the other way round.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Quote {
    /// The `"…"` rule: `'"'`.
    Double,
    /// The `` `…` `` rule: `` '`' ``.
    Backquote,
    /// The heredoc rule: `0`, so neither mark is an escape.
    Heredoc,
}

/// What php's scanner does with one `\`: how many bytes it spans, from
/// the `\` on, and the bytes it stands for.
pub(crate) struct Escape {
    /// The bytes spanned, the `\` included: 1 for a `\` that ends the
    /// text, 2 for most, up to 4 for `\x41` and `\101`.
    pub(crate) len: usize,
    bytes: [u8; 4],
    count: u8,
    /// What the scanner raises on it, if anything.
    pub(crate) defect: Option<Defect>,
}

impl Escape {
    fn of(len: usize, bytes: &[u8]) -> Escape {
        let mut held = [0u8; 4];
        let count = bytes.len().min(4);
        held[..count].copy_from_slice(&bytes[..count]);
        Escape {
            len,
            bytes: held,
            count: u8::try_from(count).unwrap_or(4),
            defect: None,
        }
    }

    /// The bytes the escape stands for.
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes[..usize::from(self.count)]
    }
}

/// What the scanner raises on an escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Defect {
    /// `\NNN` with three digits and the first past `3`: php's compile
    /// warning names the digits, and the byte is the low eight bits of
    /// what they spell (`(char) ZEND_STRTOL(octal_buf, NULL, 8)`).
    OctalOverflow,
    /// `\u{…}` that is not `{`, hex digits, `}`, or that names a
    /// codepoint past `0x10FFFF`: php's parse error, with the
    /// `: Codepoint too large` tail on the second, which it looks for
    /// only once the shape is right. The span runs to `end`: past the
    /// `}` when one stands, else to the byte that broke the shape.
    Codepoint {
        /// Whether the shape was right and the number too large.
        too_large: bool,
        /// Where the span ends.
        end: usize,
    },
}

/// One `\` at `text[at]`, as `zend_scan_escape_string`'s `switch` reads
/// it under `quote`: the named escapes `\n \r \t \f \v \e`, the two
/// marks and `\\` and `\$`; `\x` and `\X` with one or two hex digits,
/// and as written before anything else; `\u{…}` as the UTF-8 of its
/// codepoint, with `\u` before anything but `{` left as written; up to
/// three octal digits, the byte their low eight bits; and every other
/// byte, the newline included, as written behind its `\`. A `\` that
/// ends the text is itself.
///
/// The named escapes, both marks, `\x` and `\X` with one and two
/// digits, the octal form, one past `\377` with php's warning, and a
/// byte the table has no row for, each spelled out as its bytes:
///
/// ```php
/// $s = "\n\t\r\v\f\e\\\$\"\`\x41\x4\X4a\101\7\400\z";
/// for ($i = 0; $i < strlen($s); $i++) echo ord($s[$i]), " ";
/// ```
/// ```expect
/// Warning: Octal escape sequence overflow \400 is greater than \377 in %s on line 2
/// 10 9 13 11 12 27 92 36 34 92 96 65 4 74 65 7 0 92 122
/// ```
///
/// `\u{…}` names a codepoint, and one past `0x10FFFF` is php's parse
/// error with the tail; a malformed one, `\u{zz}`, is the parse error
/// without it (`Lexer::eat_escape`), and the shape is judged before the
/// size, so `\u{FFFFFFFFzz}` is malformed and not too large:
///
/// ```php
/// echo "\u{110000}";
/// ```
/// ```expect exit=255
/// Parse error: Invalid UTF-8 codepoint escape sequence: Codepoint too large in %s on line 2
/// ```
pub(crate) fn escape(text: &[u8], at: usize, quote: Quote) -> Escape {
    let Some(&b) = text.get(at + 1) else {
        return Escape::of(1, b"\\");
    };
    match b {
        b'n' => Escape::of(2, b"\n"),
        b'r' => Escape::of(2, b"\r"),
        b't' => Escape::of(2, b"\t"),
        b'f' => Escape::of(2, b"\x0c"),
        b'v' => Escape::of(2, b"\x0b"),
        b'e' => Escape::of(2, b"\x1b"),
        b'"' if quote == Quote::Double => Escape::of(2, b"\""),
        b'`' if quote == Quote::Backquote => Escape::of(2, b"`"),
        b'\\' | b'$' => Escape::of(2, &[b]),
        b'x' | b'X' => {
            let mut i = at + 2;
            let mut value: u32 = 0;
            while i < at + 4
                && let Some(d) = text.get(i).and_then(|&h| Radix::Hex.digit(h))
            {
                value = value * 16 + u32::from(d);
                i += 1;
            }
            if i == at + 2 {
                Escape::of(2, &[b'\\', b])
            } else {
                Escape::of(i - at, &value.to_le_bytes()[..1])
            }
        }
        b'u' => codepoint(text, at),
        b'0'..=b'7' => {
            let mut i = at + 1;
            let mut value: u32 = 0;
            while i < at + 4
                && let Some(d) = text.get(i).and_then(|&o| Radix::Octal.digit(o))
            {
                value = value * 8 + u32::from(d);
                i += 1;
            }
            let mut escape = Escape::of(i - at, &value.to_le_bytes()[..1]);
            if i == at + 4 && b > b'3' {
                escape.defect = Some(Defect::OctalOverflow);
            }
            escape
        }
        _ => Escape::of(2, &[b'\\', b]),
    }
}

/// `\u` at `text[at]`: the UTF-8 of the codepoint the braces name, or
/// the two bytes as written with the defect php raises. php walks to
/// the `}` first and refuses any byte on the way that is not hex, then
/// reads the number and refuses one past `0x10FFFF`; `\u{}` is refused
/// with the first message. Malformed, the scanner throws and the
/// lexer's diagnostic stands for it; the bytes here are `\u` and the
/// text goes on after them, which is where the lexer resumes too.
///
/// One byte, two, three and four of UTF-8, `\u{0}` the NUL byte and
/// `0x10FFFF` the last codepoint there is, and `\u` before anything but
/// a `{` the two bytes as written:
///
/// ```php
/// $s = "\u{41}\u{e9}\u{20ac}\u{1F600}\u{0}\u{10FFFF}\u x";
/// for ($i = 0; $i < strlen($s); $i++) echo ord($s[$i]), " ";
/// ```
/// ```expect
/// 65 195 169 226 130 172 240 159 152 128 0 244 143 191 191 92 117 32 120
/// ```
///
/// A surrogate is written as the three bytes its number takes, as
/// `zend_scan_escape_string` writes any codepoint below `0x10000`,
/// though no UTF-8 text holds one:
///
/// ```php
/// var_dump(bin2hex("\u{d800}"), bin2hex("\u{dfff}x"));
/// ```
/// ```expect
/// string(6) "eda080"
/// string(8) "edbfbf78"
/// ```
fn codepoint(text: &[u8], at: usize) -> Escape {
    if text.get(at + 2) != Some(&b'{') {
        return Escape::of(2, b"\\u");
    }
    let mut i = at + 3;
    let mut value: u32 = 0;
    let mut too_large = false;
    while let Some(d) = text.get(i).and_then(|&h| Radix::Hex.digit(h)) {
        value = value.saturating_mul(16).saturating_add(u32::from(d));
        too_large = too_large || value > 0x0010_FFFF;
        i += 1;
    }
    let closed = text.get(i) == Some(&b'}');
    let well_formed = closed && i > at + 3;
    if !well_formed || too_large {
        let end = if closed { i + 1 } else { i };
        let mut escape = Escape::of(2, b"\\u");
        escape.defect = Some(Defect::Codepoint {
            too_large: well_formed && too_large,
            end,
        });
        return escape;
    }
    let (utf8, len) = utf8_of(value);
    Escape::of(i + 1 - at, &utf8[..len])
}

/// A codepoint's UTF-8 bytes as `zend_scan_escape_string` lays them out,
/// by its number alone, so a surrogate takes three as any other
/// codepoint below `0x10000` does.
fn utf8_of(value: u32) -> ([u8; 4], usize) {
    let byte = |bits: u32| u8::try_from(bits & 0xFF).unwrap_or(0);
    match value {
        0..0x80 => ([byte(value), 0, 0, 0], 1),
        0x80..0x800 => (
            [byte(0xC0 | (value >> 6)), byte(0x80 | (value & 0x3F)), 0, 0],
            2,
        ),
        0x800..0x1_0000 => (
            [
                byte(0xE0 | (value >> 12)),
                byte(0x80 | ((value >> 6) & 0x3F)),
                byte(0x80 | (value & 0x3F)),
                0,
            ],
            3,
        ),
        _ => (
            [
                byte(0xF0 | (value >> 18)),
                byte(0x80 | ((value >> 12) & 0x3F)),
                byte(0x80 | ((value >> 6) & 0x3F)),
                byte(0x80 | (value & 0x3F)),
            ],
            4,
        ),
    }
}

/// The bytes of one `"…"`, heredoc or `` `…` `` part: every `\` read by
/// [`escape`] under the string's own mark, every other byte as written.
///
/// A `"…"` takes `\"` and leaves `` \` `` where it stands; a heredoc
/// takes neither mark and every other escape; a nowdoc takes none at
/// all:
///
/// ```php
/// $d = "\"\`\$";
/// $h = <<<E
/// \"\`\$\n\x41\101
/// E;
/// $n = <<<'E'
/// \n\\$x
/// E;
/// foreach ([$d, $h, $n] as $s) {
///     for ($i = 0; $i < strlen($s); $i++) echo $i ? ' ' : '', ord($s[$i]);
///     echo "\n";
/// }
/// ```
/// ```expect
/// 34 92 96 36
/// 92 34 92 96 36 10 65 65
/// 92 110 92 92 36 120
/// ```
///
/// A `` `…` `` is the other way round: it takes `` \` `` and leaves
/// `\"` where it stands. Its bytes are the shell's to see, since php
/// spells them nowhere else, so the same three marks go to `printf`
/// inside single quotes, which hand every byte on as it stands, and
/// what comes back is what the mark made:
///
/// ```php
/// $b = `printf %s '\"\`\$'`;
/// for ($i = 0; $i < strlen($b); $i++) echo $i ? ' ' : '', ord($b[$i]);
/// echo "\n";
/// ```
/// ```expect
/// Deprecated: The backtick (`) operator is deprecated, use shell_exec() instead in %s on line 2
/// 92 34 96 36
/// ```
pub(crate) fn unescape(text: &[u8], quote: Quote) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text[i] == b'\\' {
            let escape = escape(text, i, quote);
            out.extend_from_slice(escape.bytes());
            i += escape.len;
        } else {
            out.push(text[i]);
            i += 1;
        }
    }
    out
}

/// The bytes of a `SINGLE_QUOTED` token: the `b` prefix and the quotes
/// dropped, `\\` and `\'` the one byte, and any other `\` kept with the
/// byte after it, which is the whole of php's `'…'` rule.
pub(crate) fn single_quoted(text: &[u8]) -> Vec<u8> {
    let body = text
        .strip_prefix(b"b")
        .or_else(|| text.strip_prefix(b"B"))
        .unwrap_or(text);
    let body = body
        .strip_prefix(b"'")
        .and_then(|b| b.strip_suffix(b"'"))
        .unwrap_or(body);
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        if body[i] == b'\\' && i + 1 < body.len() {
            match body[i + 1] {
                b @ (b'\\' | b'\'') => out.push(b),
                b => out.extend_from_slice(&[b'\\', b]),
            }
            i += 2;
        } else {
            out.push(body[i]);
            i += 1;
        }
    }
    out
}

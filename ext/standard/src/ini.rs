//! `Z/Zend/zend_ini_scanner.l` and `Z/Zend/zend_ini_parser.y`: php's
//! INI scanner and grammar, the reader behind `parse_ini_string` and
//! `parse_ini_file`. The scanner is re2c's: in each state the longest
//! match among the state's rules wins and a tie goes to the rule
//! written first, and the actions that read on past their match
//! (`ST_RAW`'s value, `ST_VARNAME`'s name, `ST_DOUBLE_QUOTES`'s text)
//! read as php's do. The grammar is the bison one, walked by descent
//! with each default reduction taken before an error is reported, so a
//! syntax error names the token and the expected set php's verbose
//! bison names, and the line the scanner stood at.
//!
//! This is the grammar php's own `php.ini` and `-d` values are read
//! through too; the engine reads those itself and has not got it yet.

use php_ext::call::constant;
use php_ext::coerce::to_string;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value};

/// `ZEND_INI_SCANNER_NORMAL`, `_RAW` and `_TYPED`.
pub(super) const SCANNER_NORMAL: i64 = 0;
pub(super) const SCANNER_RAW: i64 = 1;
pub(super) const SCANNER_TYPED: i64 = 2;

/// A value the grammar builds: php's `zval`, with `Z_EXTRA`'s one bit,
/// `INI_ZVAL_IS_NUMBER`, beside it.
#[derive(Clone, Debug)]
pub(super) struct Val {
    pub(super) value: Scalar,
    number: bool,
}

/// What a value holds: a string everywhere but the typed scanner's
/// values.
#[derive(Clone, Debug)]
pub(super) enum Scalar {
    Str(Vec<u8>),
    Bool(bool),
    Null,
    Long(i64),
    Double(f64),
}

impl Val {
    fn string(bytes: &[u8]) -> Val {
        Val {
            value: Scalar::Str(bytes.to_vec()),
            number: false,
        }
    }
}

/// One call of the parser's callback: `ZEND_INI_PARSER_SECTION`,
/// `_ENTRY` (a bare label has no value) and `_POP_ENTRY`.
pub(super) enum Event {
    Section(Val),
    Entry(Val, Option<Val>),
    PopEntry(Val, Val, Val),
}

/// The scanner's states.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Initial,
    Offset,
    Raw,
    SectionRaw,
    SectionValue,
    Value,
    VarName,
    VarFallback,
    DoubleQuotes,
}

/// A token and the value it carries.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Tok {
    End,
    Section,
    Raw,
    Constant,
    Number,
    Str,
    Whitespace,
    Label,
    Offset,
    DollarCurly,
    VarName,
    QuotedString,
    Fallback,
    BoolTrue,
    BoolFalse,
    Null,
    EndOfLine,
    Char(u8),
}

impl Tok {
    /// The name bison's verbose error writes for the token.
    fn name(self) -> String {
        match self {
            Tok::End => String::from("end of file"),
            Tok::Section => String::from("TC_SECTION"),
            Tok::Raw => String::from("TC_RAW"),
            Tok::Constant => String::from("TC_CONSTANT"),
            Tok::Number => String::from("TC_NUMBER"),
            Tok::Str => String::from("TC_STRING"),
            Tok::Whitespace => String::from("TC_WHITESPACE"),
            Tok::Label => String::from("TC_LABEL"),
            Tok::Offset => String::from("TC_OFFSET"),
            Tok::DollarCurly => String::from("TC_DOLLAR_CURLY"),
            Tok::VarName => String::from("TC_VARNAME"),
            Tok::QuotedString => String::from("TC_QUOTED_STRING"),
            Tok::Fallback => String::from("TC_FALLBACK"),
            Tok::BoolTrue => String::from("BOOL_TRUE"),
            Tok::BoolFalse => String::from("BOOL_FALSE"),
            Tok::Null => String::from("NULL_NULL"),
            Tok::EndOfLine => String::from("END_OF_LINE"),
            Tok::Char(c) => format!("'{}'", char::from(c)),
        }
    }
}

/// `LABEL_CHAR`: `[^=\n\r\t;&|^$~(){}!"[\]\x00]`.
fn label_char(c: u8) -> bool {
    !matches!(
        c,
        b'=' | b'\n'
            | b'\r'
            | b'\t'
            | b';'
            | b'&'
            | b'|'
            | b'^'
            | b'$'
            | b'~'
            | b'('
            | b')'
            | b'{'
            | b'}'
            | b'!'
            | b'"'
            | b'['
            | b']'
            | 0
    )
}

/// `TOKENS`: `[:,.[\]"'()&|^+-/*=%$!~<>?@{}]`, `+-/` the range from
/// `+` to `/`.
fn token_char(c: u8) -> bool {
    matches!(
        c,
        b':' | b',' | b'.' | b'[' | b']' | b'"' | b'\'' | b'(' | b')' | b'&' | b'|' | b'^' | b'+'
            ..=b'/'
                | b'*'
                | b'='
                | b'%'
                | b'$'
                | b'!'
                | b'~'
                | b'<'
                | b'>'
                | b'?'
                | b'@'
                | b'{'
                | b'}'
    )
}

/// The scanner over one buffer, read past its end as NUL padding.
struct Scanner<'a> {
    buf: &'a [u8],
    at: usize,
    state: State,
    stack: Vec<State>,
    lineno: u32,
    mode: i64,
}

impl Scanner<'_> {
    fn byte(&self, i: usize) -> u8 {
        self.buf.get(i).copied().unwrap_or(0)
    }

    /// `{TABS_AND_SPACES}*` from `p`: how many.
    fn tabs(&self, p: usize) -> usize {
        (p..self.buf.len())
            .take_while(|i| matches!(self.byte(*i), b' ' | b'\t'))
            .count()
    }

    /// `{NEWLINE}` at `p`: its length, or none.
    fn newline(&self, p: usize) -> Option<usize> {
        match self.byte(p) {
            b'\r' if self.byte(p + 1) == b'\n' => Some(2),
            b'\r' | b'\n' => Some(1),
            _ => None,
        }
    }

    /// `{TABS_AND_SPACES}*{NEWLINE}`.
    fn tabs_newline(&self, p: usize) -> Option<usize> {
        let t = self.tabs(p);
        self.newline(p + t).map(|n| t + n)
    }

    /// `{TABS_AND_SPACES}*[;][^\r\n]*{NEWLINE}`.
    fn comment(&self, p: usize) -> Option<usize> {
        let t = self.tabs(p);
        if self.byte(p + t) != b';' {
            return None;
        }
        let mut q = p + t + 1;
        while q < self.buf.len() && !matches!(self.byte(q), b'\r' | b'\n') {
            q += 1;
        }
        self.newline(q).map(|n| q + n - p)
    }

    /// `{LABEL}`.
    fn label(&self, p: usize) -> Option<usize> {
        let n = (p..self.buf.len())
            .take_while(|i| label_char(self.byte(*i)))
            .count();
        (n > 0).then_some(n)
    }

    /// `{CONSTANT}`.
    fn constant(&self, p: usize) -> Option<usize> {
        let c = self.byte(p);
        if !(c.is_ascii_alphabetic() || c == b'_') {
            return None;
        }
        Some(
            1 + (p + 1..self.buf.len())
                .take_while(|i| {
                    let c = self.byte(*i);
                    c.is_ascii_alphanumeric() || c == b'_'
                })
                .count(),
        )
    }

    /// `{NUMBER}`: `[-]?{LNUM}|{DNUM}`.
    fn number(&self, p: usize) -> Option<usize> {
        let digits = |from: usize| {
            (from..self.buf.len())
                .take_while(|i| self.byte(*i).is_ascii_digit())
                .count()
        };
        let mut best = 0;
        // `-?[0-9]+`
        let minus = usize::from(self.byte(p) == b'-');
        let d = digits(p + minus);
        if d > 0 {
            best = minus + d;
        }
        // `[0-9]*[.][0-9]+` and `[0-9]+[.][0-9]*`
        let lead = digits(p);
        if self.byte(p + lead) == b'.' {
            let tail = digits(p + lead + 1);
            if tail > 0 || lead > 0 {
                best = best.max(lead + 1 + tail);
            }
        }
        (best > 0).then_some(best)
    }

    /// A keyword and the tabs after it: `("true"|"on"|"yes")`,
    /// `("false"|"off"|"no"|"none")` or `"null"`, in any case, as re2c's
    /// `--case-inverted` reads a double-quoted string.
    fn keyword(&self, p: usize, words: &[&[u8]]) -> Option<usize> {
        let n = words
            .iter()
            .filter(|w| (0..w.len()).all(|i| self.byte(p + i).eq_ignore_ascii_case(&w[i])))
            .map(|w| w.len())
            .max()?;
        Some(n + self.tabs(p + n))
    }

    /// `"'"{SINGLE_QUOTED_CHARS}+"'"`.
    fn single_quoted(&self, p: usize) -> Option<usize> {
        if self.byte(p) != b'\'' {
            return None;
        }
        let mut q = p + 1;
        while q < self.buf.len() && self.byte(q) != b'\'' {
            q += 1;
        }
        (q > p + 1 && self.byte(q) == b'\'').then_some(q + 1 - p)
    }

    /// `LITERAL_DOLLAR` at `p`: `"$"([^{\000]|("\\"{ANY_CHAR}))`.
    fn literal_dollar(&self, p: usize) -> Option<usize> {
        if self.byte(p) != b'$' {
            return None;
        }
        match self.byte(p + 1) {
            b'\\' if p + 2 < self.buf.len() => Some(3),
            b'{' | 0 => None,
            _ => Some(2),
        }
    }

    /// A `(...)+` of units, each a byte `plain` takes, `"\\"{ANY_CHAR}`
    /// where `escapes`, or a `LITERAL_DOLLAR`.
    fn units(&self, p: usize, plain: impl Fn(u8) -> bool, escapes: bool) -> Option<usize> {
        let mut q = p;
        while q < self.buf.len() {
            let c = self.byte(q);
            if escapes && c == b'\\' && q + 1 < self.buf.len() {
                q += 2;
            } else if c == b'$' {
                match self.literal_dollar(q) {
                    Some(n) => q += n,
                    None => break,
                }
            } else if plain(c) {
                q += 1;
            } else {
                break;
            }
        }
        (q > p).then_some(q - p)
    }

    /// The token at the cursor, `ini_lex`.
    #[allow(clippy::too_many_lines)]
    fn lex(&mut self) -> (Tok, Val) {
        use State as S;
        let none = Val::string(b"");
        loop {
            let p = self.at;
            if p >= self.buf.len() {
                if matches!(self.state, State::Value | State::Raw) {
                    self.state = State::Initial;
                    return (Tok::EndOfLine, none);
                }
                return (Tok::End, none);
            }
            if p == 0 && self.buf.len() > 3 && self.buf.starts_with(b"\xef\xbb\xbf") {
                self.at = 3;
                continue;
            }
            // Each state's rules in the file's order: the longest match
            // wins, the first on a tie.
            let mut best: Option<(usize, u8)> = None;
            let mut offer = |len: Option<usize>, rule: u8| {
                if let Some(len) = len
                    && best.is_none_or(|(b, _)| len > b)
                {
                    best = Some((len, rule));
                }
            };
            let st = self.state;
            // 1: "[" in INITIAL.
            if st == S::Initial && self.byte(p) == b'[' {
                offer(Some(1), 1);
            }
            // 2: raw single-quoted string.
            if matches!(st, S::Value | S::SectionValue | S::Offset) {
                offer(self.single_quoted(p), 2);
            }
            // 3: "]"{TABS}*{NEWLINE}? ending a section.
            if matches!(st, S::SectionRaw | S::SectionValue) && self.byte(p) == b']' {
                let t = self.tabs(p + 1);
                offer(Some(1 + t + self.newline(p + 1 + t).unwrap_or(0)), 3);
            }
            // 4: {LABEL}"["{TABS}* starting an offset.
            if st == S::Initial
                && let Some(n) = self.label(p)
                && self.byte(p + n) == b'['
            {
                offer(Some(n + 1 + self.tabs(p + n + 1)), 4);
            }
            // 5: {TABS}*"]" ending an offset.
            if st == S::Offset {
                let t = self.tabs(p);
                if self.byte(p + t) == b']' {
                    offer(Some(t + 1), 5);
                }
            }
            // 6: "${".
            if matches!(
                st,
                S::DoubleQuotes | S::SectionValue | S::Value | S::Offset | S::VarFallback
            ) && self.byte(p) == b'$'
                && self.byte(p + 1) == b'{'
            {
                offer(Some(2), 6);
            }
            if st == S::VarName {
                // 7: ":-".
                if self.byte(p) == b':' && self.byte(p + 1) == b'-' {
                    offer(Some(2), 7);
                }
                // 8: {LABEL_CHAR}.
                if label_char(self.byte(p)) {
                    offer(Some(1), 8);
                }
            }
            // 9: "}" in a name or a fallback.
            if matches!(st, S::VarName | S::VarFallback) && self.byte(p) == b'}' {
                offer(Some(1), 9);
            }
            if matches!(st, S::Initial | S::Value) {
                offer(self.keyword(p, &[b"true", b"on", b"yes"]), 10);
                offer(self.keyword(p, &[b"false", b"off", b"no", b"none"]), 11);
                offer(self.keyword(p, &[b"null"]), 12);
            }
            if st == S::Initial {
                // 13: {LABEL}.
                offer(self.label(p), 13);
                // 14: {TABS}*[=]{TABS}*.
                let t = self.tabs(p);
                if self.byte(p + t) == b'=' {
                    offer(Some(t + 1 + self.tabs(p + t + 1)), 14);
                }
            }
            // 15: {RAW_VALUE_CHARS}, one byte, read on by the action.
            if st == S::Raw && !matches!(self.byte(p), b'\n' | b'\r' | b';' | 0) {
                offer(Some(1), 15);
            }
            // 16: {SECTION_RAW_CHARS}+.
            if st == S::SectionRaw {
                let n = (p..self.buf.len())
                    .take_while(|i| !matches!(self.byte(*i), b']' | b'\n' | b'\r'))
                    .count();
                offer((n > 0).then_some(n), 16);
            }
            // 17: {TABS}*{NEWLINE} ending a value.
            if matches!(st, S::Value | S::Raw) {
                offer(self.tabs_newline(p), 17);
            }
            if matches!(st, S::SectionValue | S::Value | S::VarFallback | S::Offset) {
                // 18: {CONSTANT}.
                offer(self.constant(p), 18);
                // 19: {NUMBER}.
                offer(self.number(p), 19);
            }
            // 20: {TOKENS} in INITIAL.
            if st == S::Initial && token_char(self.byte(p)) {
                offer(Some(1), 20);
            }
            if st == S::Value {
                // 21: {OPERATORS}{TABS}*.
                if matches!(self.byte(p), b'&' | b'|' | b'^' | b'~' | b'(' | b')' | b'!') {
                    offer(Some(1 + self.tabs(p + 1)), 21);
                }
                // 22: [=].
                if self.byte(p) == b'=' {
                    offer(Some(1), 22);
                }
                // 23: {VALUE_CHARS}+.
                offer(
                    self.units(
                        p,
                        |c| {
                            !matches!(
                                c,
                                b'$' | b'='
                                    | b' '
                                    | b'\t'
                                    | b'\n'
                                    | b'\r'
                                    | b';'
                                    | b'&'
                                    | b'|'
                                    | b'^'
                                    | b'~'
                                    | b'('
                                    | b')'
                                    | b'!'
                                    | b'"'
                                    | b'\''
                                    | 0
                            )
                        },
                        false,
                    ),
                    23,
                );
            }
            // 24: {FALLBACK_CHARS}+.
            if st == S::VarFallback {
                offer(
                    self.units(
                        p,
                        |c| !matches!(c, b'$' | b'\n' | b'\r' | b';' | b'"' | b'\'' | b'}' | b'\\'),
                        true,
                    ),
                    24,
                );
            }
            // 25: {SECTION_VALUE_CHARS}+.
            if matches!(st, S::SectionValue | S::Offset) {
                offer(
                    self.units(
                        p,
                        |c| !matches!(c, b'$' | b'\n' | b'\r' | b';' | b'"' | b'\'' | b']' | b'\\'),
                        true,
                    ),
                    25,
                );
            }
            // 26: {TABS}*["] opening a double-quoted string.
            if matches!(st, S::SectionValue | S::Value | S::VarFallback | S::Offset) {
                let t = self.tabs(p);
                if self.byte(p + t) == b'"' {
                    offer(Some(t + 1), 26);
                }
            }
            if st == S::DoubleQuotes {
                // 27: ["]{TABS}* closing it.
                if self.byte(p) == b'"' {
                    offer(Some(1 + self.tabs(p + 1)), 27);
                }
                // 28: [^], read on by the action.
                offer(Some(1), 28);
            }
            // 29: {WHITESPACE}.
            if matches!(st, S::SectionValue | S::Value | S::Offset | S::VarFallback) {
                let t = self.tabs(p);
                offer((t > 0).then_some(t), 29);
            }
            if matches!(st, S::Initial | S::Raw) {
                // 30: {TABS}+, eaten.
                let t = self.tabs(p);
                offer((t > 0).then_some(t), 30);
            }
            if st == S::Initial {
                // 31: {TABS}*{NEWLINE}.
                offer(self.tabs_newline(p), 31);
            }
            if matches!(st, S::Initial | S::Value | S::Raw) {
                // 32: a comment to the end of the line.
                offer(self.comment(p), 32);
            }
            if matches!(st, S::Value | S::Raw) {
                // 33: [^], the value ended before its line did.
                offer(Some(1), 33);
            }
            // 34: <*>[^].
            offer(Some(1), 34);
            let Some((len, rule)) = best else {
                return (Tok::End, none);
            };
            let text = self.buf[p..(p + len).min(self.buf.len())].to_vec();
            self.at = p + len;
            match rule {
                1 => {
                    self.state = if self.mode == SCANNER_RAW {
                        S::SectionRaw
                    } else {
                        S::SectionValue
                    };
                    return (Tok::Section, none);
                }
                2 => {
                    let inner = &text[1..text.len() - 1];
                    return (Tok::Raw, self.token_value(Tok::Raw, inner));
                }
                3 => {
                    self.state = S::Initial;
                    self.lineno += 1;
                    return (Tok::Char(b']'), none);
                }
                4 => {
                    let trimmed = trim_leading(&text);
                    let trimmed = trim_trailing(trimmed, Some(b'['));
                    self.state = S::Offset;
                    return (Tok::Offset, self.token_value(Tok::Offset, trimmed));
                }
                5 => {
                    self.state = S::Initial;
                    return (Tok::Char(b']'), none);
                }
                6 => {
                    self.stack.push(self.state);
                    self.state = S::VarName;
                    return (Tok::DollarCurly, none);
                }
                7 => {
                    self.state = self.stack.pop().unwrap_or(S::Initial);
                    self.stack.push(self.state);
                    self.state = S::VarFallback;
                    return (Tok::Fallback, none);
                }
                8 => {
                    // A one-byte name followed by `:-` goes straight to
                    // the fallback, the name never handed on.
                    if self.byte(self.at) == b':' && self.byte(self.at + 1) == b'-' {
                        self.at += 1;
                        self.state = self.stack.pop().unwrap_or(S::Initial);
                        self.stack.push(self.state);
                        self.state = S::VarFallback;
                        return (Tok::Fallback, none);
                    }
                    // A name that runs to the end keeps the one byte
                    // matched: `yyleng` is set only where the loop stops.
                    let mut end = p + 1;
                    while self.at < self.buf.len() {
                        let c = self.byte(self.at);
                        self.at += 1;
                        let stop = match c {
                            b'=' | b'\n' | b'\r' | b'\t' | b';' | b'&' | b'|' | b'^' | b'$'
                            | b'~' | b'(' | b')' | b'{' | b'}' | b'!' | b'"' | b'[' | b']' => true,
                            b':' => self.byte(self.at) == b'-',
                            _ => false,
                        };
                        if stop {
                            self.at -= 1;
                            end = self.at;
                            break;
                        }
                    }
                    let name = self.buf[p..end].to_vec();
                    let name = trim_trailing(trim_leading(&name), None).to_vec();
                    return (Tok::VarName, self.token_value(Tok::VarName, &name));
                }
                9 => {
                    self.state = self.stack.pop().unwrap_or(S::Initial);
                    return (Tok::Char(b'}'), none);
                }
                10 => return (Tok::BoolTrue, self.token_value(Tok::BoolTrue, b"1")),
                11 => return (Tok::BoolFalse, self.token_value(Tok::BoolFalse, b"")),
                12 => return (Tok::Null, self.token_value(Tok::Null, b"")),
                13 => {
                    let trimmed = trim_trailing(trim_leading(&text), None);
                    return (Tok::Label, self.token_value(Tok::Label, trimmed));
                }
                14 => {
                    self.state = if self.mode == SCANNER_RAW {
                        S::Raw
                    } else {
                        S::Value
                    };
                    return (Tok::Char(b'='), none);
                }
                15 => return self.raw_value(p),
                16 => return (Tok::Raw, self.token_value(Tok::Raw, &text)),
                18 => return (Tok::Constant, self.token_value(Tok::Constant, &text)),
                19 => return (Tok::Number, self.token_value(Tok::Number, &text)),
                20 | 21 => return (Tok::Char(text[0]), none),
                22 => {
                    // `yyless(0)`: the `=` is read again from INITIAL.
                    self.at = p;
                    self.state = S::Initial;
                    return (Tok::EndOfLine, none);
                }
                23..=25 => return (Tok::Str, self.token_value(Tok::Str, &text)),
                26 => {
                    self.stack.push(self.state);
                    self.state = S::DoubleQuotes;
                    return (Tok::Char(b'"'), none);
                }
                27 => {
                    self.state = self.stack.pop().unwrap_or(S::Initial);
                    return (Tok::Char(b'"'), none);
                }
                28 => return self.quoted(p),
                29 => return (Tok::Whitespace, self.token_value(Tok::Whitespace, &text)),
                30 => {}
                31 => {
                    self.lineno += 1;
                    return (Tok::EndOfLine, none);
                }
                17 | 32 => {
                    self.state = S::Initial;
                    self.lineno += 1;
                    return (Tok::EndOfLine, none);
                }
                33 => {
                    self.state = S::Initial;
                    return (Tok::EndOfLine, none);
                }
                _ => return (Tok::End, none),
            }
        }
    }

    /// `RETURN_TOKEN(type, str, len)`: the typed scanner's booleans and
    /// null in a value, a string for everything else.
    fn token_value(&self, tok: Tok, bytes: &[u8]) -> Val {
        if self.mode == SCANNER_TYPED && matches!(self.state, State::Value | State::Raw) {
            let value = match tok {
                Tok::BoolTrue => Scalar::Bool(true),
                Tok::BoolFalse => Scalar::Bool(false),
                Tok::Null => Scalar::Null,
                _ => Scalar::Str(bytes.to_vec()),
            };
            return Val {
                value,
                number: false,
            };
        }
        Val::string(bytes)
    }

    /// `<ST_RAW>{RAW_VALUE_CHARS}`: the value to the line's end, cut at
    /// its first `;` unless a double quote opened it and closes after,
    /// trimmed, and unquoted.
    fn raw_value(&mut self, p: usize) -> (Tok, Val) {
        let mut start = p;
        if matches!(self.byte(start), b' ' | b'\t') {
            start += 1;
        }
        let first = self.byte(start);
        let mut sc: Option<usize> = None;
        while self.at < self.buf.len() {
            match self.byte(self.at) {
                b'\n' | b'\r' => break,
                b';' => {
                    if sc.is_none() {
                        sc = Some(self.at);
                    }
                    self.at += 1;
                }
                b'"' => {
                    if first == b'"' {
                        sc = None;
                    }
                    self.at += 1;
                }
                _ => self.at += 1,
            }
        }
        let end = sc.unwrap_or(self.at);
        let mut text = trim_trailing(&self.buf[start..end.max(start)], None);
        if text.len() > 1 && text[0] == b'"' && text[text.len() - 1] == b'"' {
            text = &text[1..text.len() - 1];
        }
        let text = text.to_vec();
        (Tok::Raw, self.token_value(Tok::Raw, &text))
    }

    /// `<ST_DOUBLE_QUOTES>[^]`: the text to the closing quote or the
    /// next `${`, escapes read by `zend_ini_escape_string`.
    fn quoted(&mut self, p: usize) -> (Tok, Val) {
        let limit = self.buf.len();
        let mut s = p;
        while s < limit {
            let c = self.byte(s);
            s += 1;
            match c {
                b'"' => {
                    s -= 1;
                    break;
                }
                b'$' if s < limit && self.byte(s) == b'{' => {
                    s -= 1;
                    break;
                }
                b'\\' if s < limit => {
                    let escaped = self.byte(s);
                    s += 1;
                    if escaped == b'"' && (s >= limit || matches!(self.byte(s), b'\n' | b'\r')) {
                        s -= 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        self.at = s;
        let raw = self.buf[p..s].to_vec();
        let mut out = Vec::with_capacity(raw.len());
        let mut i = 0;
        while i < raw.len() {
            let c = raw[i];
            if c == b'\\' {
                i += 1;
                if i >= raw.len() {
                    out.push(b'\\');
                    continue;
                }
                match raw[i] {
                    b'"' | b'\\' | b'$' => out.push(raw[i]),
                    other => {
                        out.push(b'\\');
                        out.push(other);
                    }
                }
            } else {
                out.push(c);
            }
            if raw[i] == b'\n' || (raw[i] == b'\r' && raw.get(i + 1) != Some(&b'\n')) {
                self.lineno += 1;
            }
            i += 1;
        }
        (Tok::QuotedString, Val::string(&out))
    }
}

/// `EAT_LEADING_WHITESPACE()`.
fn trim_leading(text: &[u8]) -> &[u8] {
    let n = text
        .iter()
        .take_while(|c| matches!(c, b' ' | b'\t'))
        .count();
    &text[n..]
}

/// `EAT_TRAILING_WHITESPACE_EX(ch)`, and with `None` the plain one.
fn trim_trailing(text: &[u8], extra: Option<u8>) -> &[u8] {
    let mut n = text.len();
    while n > 0
        && (Some(text[n - 1]) == extra || matches!(text[n - 1], b'\n' | b'\r' | b'\t' | b' '))
    {
        n -= 1;
    }
    &text[..n]
}

/// The parser: the scanner, one token of lookahead, and the request
/// the constants and the conversions read.
struct Parser<'a, 'r> {
    scanner: Scanner<'a>,
    tok: Tok,
    val: Val,
    req: &'r mut Request,
    events: Vec<Event>,
}

/// What a syntax error stops the parse with: bison's message.
struct Syntax(String);

impl Parser<'_, '_> {
    fn advance(&mut self) {
        let (tok, val) = self.scanner.lex();
        self.tok = tok;
        self.val = val;
    }

    /// bison's verbose message for the lookahead in a state whose
    /// actions are `expected`; the list is written where it has fewer
    /// than five tokens.
    fn error(&self, expected: &[Tok]) -> Syntax {
        let mut msg = format!("syntax error, unexpected {}", self.tok.name());
        if !expected.is_empty() && expected.len() < 5 {
            let names: Vec<String> = expected.iter().copied().map(Tok::name).collect();
            msg.push_str(", expecting ");
            msg.push_str(&names.join(" or "));
        }
        Syntax(msg)
    }

    fn expect(&mut self, tok: Tok) -> Result<(), Syntax> {
        if self.tok == tok {
            self.advance();
            Ok(())
        } else {
            Err(self.error(&[tok]))
        }
    }

    /// `zval_get_string` of a value, and `convert_to_string` for a
    /// concatenation's right side.
    fn as_bytes(&mut self, val: &Val) -> Result<Vec<u8>, Thrown> {
        Ok(match &val.value {
            Scalar::Str(s) => s.clone(),
            Scalar::Bool(true) => b"1".to_vec(),
            Scalar::Bool(false) | Scalar::Null => Vec::new(),
            Scalar::Long(n) => n.to_string().into_bytes(),
            Scalar::Double(d) => to_string(self.req, &Value::float(*d))?.as_bytes().to_vec(),
        })
    }

    /// `zend_ini_add_string(result, op1, op2)`: the two as one string,
    /// `op1`'s number bit kept.
    fn add(&mut self, a: &Val, b: &Val) -> Result<Val, Thrown> {
        let mut bytes = self.as_bytes(a)?;
        bytes.extend_from_slice(&self.as_bytes(b)?);
        Ok(Val {
            value: Scalar::Str(bytes),
            number: a.number,
        })
    }

    /// `zend_ini_get_constant(result, name)`: the constant's value as a
    /// string, or the name where there is none or it holds a `:`.
    fn get_constant(&mut self, name: Val) -> Result<Val, Thrown> {
        let Scalar::Str(bytes) = &name.value else {
            return Ok(name);
        };
        if bytes.contains(&b':') {
            return Ok(name);
        }
        match constant(self.req, bytes)? {
            Some(value) => {
                let s = to_string(self.req, &value)?;
                Ok(Val::string(s.as_bytes()))
            }
            None => Ok(name),
        }
    }

    /// `zend_ini_get_var(result, name, fallback)`: the configuration
    /// directive, else the environment variable, else the fallback, else
    /// the empty string.
    fn get_var(&mut self, name: &Val, fallback: Option<Val>) -> Result<Val, Thrown> {
        let name = self.as_bytes(name)?;
        if let Some(value) = self.req.ini_get(&name) {
            return Ok(Val::string(value));
        }
        if let Some(value) = self.req.env(&name) {
            return Ok(Val::string(value));
        }
        match fallback {
            Some(fallback) => {
                let bytes = self.as_bytes(&fallback)?;
                Ok(Val::string(&bytes))
            }
            None => Ok(Val::string(b"")),
        }
    }

    /// `cfg_var_ref`, the lookahead at `TC_DOLLAR_CURLY`.
    fn cfg_var_ref(&mut self) -> Result<Result<Val, Syntax>, Thrown> {
        self.advance();
        if self.tok != Tok::VarName {
            return Ok(Err(self.error(&[Tok::VarName])));
        }
        let name = self.val.clone();
        self.advance();
        match self.tok {
            Tok::Char(b'}') => {
                self.advance();
                self.get_var(&name, None).map(Ok)
            }
            Tok::Fallback => {
                self.advance();
                let fallback = match self.var_string_list(false)? {
                    Ok(Some(v)) => v,
                    Ok(None) => Val::string(b""),
                    Err(e) => return Ok(Err(e)),
                };
                if self.tok != Tok::Char(b'}') {
                    return Ok(Err(self.error(&[Tok::Char(b'}')])));
                }
                self.advance();
                self.get_var(&name, Some(fallback)).map(Ok)
            }
            _ => Ok(Err(self.error(&[Tok::Fallback, Tok::Char(b'}')]))),
        }
    }

    /// `'"' encapsed_list '"'`, the lookahead at the opening quote.
    fn encapsed(&mut self) -> Result<Result<Val, Syntax>, Thrown> {
        self.advance();
        let mut acc = Val::string(b"");
        loop {
            match self.tok {
                Tok::DollarCurly => match self.cfg_var_ref()? {
                    Ok(v) => acc = self.add(&acc, &v)?,
                    Err(e) => return Ok(Err(e)),
                },
                Tok::QuotedString => {
                    let v = self.val.clone();
                    self.advance();
                    acc = self.add(&acc, &v)?;
                }
                Tok::Char(b'"') => {
                    self.advance();
                    return Ok(Ok(acc));
                }
                _ => {
                    return Ok(Err(self.error(&[
                        Tok::DollarCurly,
                        Tok::QuotedString,
                        Tok::Char(b'"'),
                    ])));
                }
            }
        }
    }

    /// `var_string_list`, or with `section` `var_string_list_section`:
    /// its items concatenated, `None` where the lookahead starts none.
    /// A value's constants are looked up; a section's are not.
    fn var_string_list(&mut self, section: bool) -> Result<Result<Option<Val>, Syntax>, Thrown> {
        let mut acc: Option<Val> = None;
        loop {
            let item = match self.tok {
                Tok::DollarCurly => match self.cfg_var_ref()? {
                    Ok(v) => v,
                    Err(e) => return Ok(Err(e)),
                },
                Tok::Char(b'"') => match self.encapsed()? {
                    Ok(v) => v,
                    Err(e) => return Ok(Err(e)),
                },
                Tok::Constant | Tok::Raw | Tok::Number | Tok::Str | Tok::Whitespace => {
                    let tok = self.tok;
                    let mut v = self.val.clone();
                    self.advance();
                    if !section {
                        if tok == Tok::Constant {
                            v = self.get_constant(v)?;
                        } else if tok == Tok::Number {
                            v.number = true;
                        }
                    }
                    v
                }
                _ => return Ok(Ok(acc)),
            };
            acc = Some(match acc {
                None => item,
                Some(prev) => self.add(&prev, &item)?,
            });
        }
    }

    /// `get_int_val(op)`.
    fn int_val(val: &Val) -> i32 {
        match &val.value {
            Scalar::Long(n) => {
                i32::from_ne_bytes(u32::try_from(n & 0xFFFF_FFFF).unwrap_or(0).to_ne_bytes())
            }
            #[allow(clippy::cast_possible_truncation)]
            Scalar::Double(d) => *d as i32,
            Scalar::Str(s) => atoi(s),
            Scalar::Bool(b) => i32::from(*b),
            Scalar::Null => 0,
        }
    }

    /// `zend_ini_do_op(type, result, op1, op2)`: the operation over
    /// C `int`s, answered as `%d` or, typed, as a long.
    fn do_op(&mut self, op: u8, lhs: &Val, rhs: Option<&Val>) -> Val {
        let left = Self::int_val(lhs);
        let right = rhs.map_or(0, Self::int_val);
        let out = match op {
            b'|' => left | right,
            b'&' => left & right,
            b'^' => left ^ right,
            b'~' => !left,
            b'!' => i32::from(left == 0),
            _ => 0,
        };
        let value = if self.scanner.mode == SCANNER_TYPED {
            Scalar::Long(i64::from(out))
        } else {
            Scalar::Str(out.to_string().into_bytes())
        };
        Val {
            value,
            number: lhs.number,
        }
    }

    /// The tokens an `expr` may start with, for the error at one that
    /// starts none.
    fn expr_start() -> Vec<Tok> {
        vec![
            Tok::Raw,
            Tok::Constant,
            Tok::Number,
            Tok::Str,
            Tok::Whitespace,
            Tok::DollarCurly,
            Tok::Char(b'"'),
            Tok::Char(b'~'),
            Tok::Char(b'('),
            Tok::Char(b'!'),
        ]
    }

    /// A unary-level `expr`: `'~' expr`, `'!' expr`, `'(' expr ')'`, or
    /// a `var_string_list`.
    fn unary(&mut self) -> Result<Result<Val, Syntax>, Thrown> {
        match self.tok {
            Tok::Char(op @ (b'~' | b'!')) => {
                self.advance();
                match self.unary()? {
                    Ok(v) => Ok(Ok(self.do_op(op, &v, None))),
                    Err(e) => Ok(Err(e)),
                }
            }
            Tok::Char(b'(') => {
                self.advance();
                let inner = match self.expr()? {
                    Ok(v) => v,
                    Err(e) => return Ok(Err(e)),
                };
                if self.tok != Tok::Char(b')') {
                    return Ok(Err(self.error(&[
                        Tok::Char(b'^'),
                        Tok::Char(b'|'),
                        Tok::Char(b'&'),
                        Tok::Char(b')'),
                    ])));
                }
                self.advance();
                Ok(Ok(inner))
            }
            _ => match self.var_string_list(false)? {
                Ok(Some(v)) => Ok(Ok(v)),
                Ok(None) => Ok(Err(self.error(&Self::expr_start()))),
                Err(e) => Ok(Err(e)),
            },
        }
    }

    /// `expr`: unary-level operands joined left to right by `|`, `&`
    /// and `^`, which share one precedence.
    fn expr(&mut self) -> Result<Result<Val, Syntax>, Thrown> {
        let mut acc = match self.unary()? {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        };
        while let Tok::Char(op @ (b'|' | b'&' | b'^')) = self.tok {
            self.advance();
            let rhs = match self.unary()? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            acc = self.do_op(op, &acc, Some(&rhs));
        }
        Ok(Ok(acc))
    }

    /// `normalize_value(zv)`: in the typed scanner, a string that began
    /// with a number and reads whole as one, as that number.
    fn normalize(&self, val: Val) -> Val {
        if self.scanner.mode != SCANNER_TYPED || !val.number {
            return val;
        }
        let Scalar::Str(s) = &val.value else {
            return val;
        };
        match php_ext::coerce::numeric_string(s)
            .as_ref()
            .map(Value::as_ref)
        {
            Some(php_ext::values::ValueRef::Int(n)) => Val {
                value: Scalar::Long(n),
                number: true,
            },
            Some(php_ext::values::ValueRef::Float(d)) if !overflowed(s) => Val {
                value: Scalar::Double(d),
                number: true,
            },
            _ => val,
        }
    }

    /// `string_or_value`.
    fn string_or_value(&mut self) -> Result<Result<Val, Syntax>, Thrown> {
        match self.tok {
            Tok::BoolTrue | Tok::BoolFalse | Tok::Null => {
                let v = self.val.clone();
                self.advance();
                Ok(Ok(v))
            }
            Tok::EndOfLine => {
                self.advance();
                Ok(Ok(Val::string(b"")))
            }
            _ => match self.expr()? {
                Ok(v) => Ok(Ok(self.normalize(v))),
                Err(e) => Ok(Err(e)),
            },
        }
    }

    /// `statement_list`.
    fn statements(&mut self) -> Result<Result<(), Syntax>, Thrown> {
        loop {
            match self.tok {
                Tok::End => return Ok(Ok(())),
                Tok::EndOfLine => self.advance(),
                Tok::Section => {
                    self.advance();
                    let name = match self.var_string_list(true)? {
                        Ok(Some(v)) => v,
                        Ok(None) => Val::string(b""),
                        Err(e) => return Ok(Err(e)),
                    };
                    if let Err(e) = self.expect(Tok::Char(b']')) {
                        return Ok(Err(e));
                    }
                    self.events.push(Event::Section(name));
                }
                Tok::Label => {
                    let label = self.val.clone();
                    self.advance();
                    if self.tok == Tok::Char(b'=') {
                        self.advance();
                        let value = match self.string_or_value()? {
                            Ok(v) => v,
                            Err(e) => return Ok(Err(e)),
                        };
                        self.events.push(Event::Entry(label, Some(value)));
                    } else {
                        self.events.push(Event::Entry(label, None));
                    }
                }
                Tok::Offset => {
                    let label = self.val.clone();
                    self.advance();
                    let offset = match self.var_string_list(false)? {
                        Ok(Some(v)) => v,
                        Ok(None) => Val::string(b""),
                        Err(e) => return Ok(Err(e)),
                    };
                    if let Err(e) = self.expect(Tok::Char(b']')) {
                        return Ok(Err(e));
                    }
                    if let Err(e) = self.expect(Tok::Char(b'=')) {
                        return Ok(Err(e));
                    }
                    let value = match self.string_or_value()? {
                        Ok(v) => v,
                        Err(e) => return Ok(Err(e)),
                    };
                    self.events.push(Event::PopEntry(label, value, offset));
                }
                _ => return Ok(Err(self.error(&[]))),
            }
        }
    }
}

/// Whether `is_numeric_string_ex` sets `oflow` for the text: an integer
/// written in digits alone that does not fit a long, which it answers
/// as a float.
fn overflowed(s: &[u8]) -> bool {
    let t = s
        .iter()
        .filter(|c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
        .collect::<Vec<_>>();
    let digits = t
        .strip_prefix(&[&b'-'])
        .or_else(|| t.strip_prefix(&[&b'+']))
        .unwrap_or(&t);
    !digits.is_empty() && digits.iter().all(|c| c.is_ascii_digit())
}

/// C's `atoi`: leading whitespace, a sign, digits, as `strtol` reads
/// them and truncated to `int`.
fn atoi(s: &[u8]) -> i32 {
    let mut i = s
        .iter()
        .take_while(|c| matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        .count();
    let negative = match s.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let mut value: i64 = 0;
    for &c in s[i.min(s.len())..]
        .iter()
        .take_while(|c| c.is_ascii_digit())
    {
        value = value.saturating_mul(10).saturating_add(i64::from(c - b'0'));
    }
    let value = if negative { -value } else { value };
    i32::from_ne_bytes(
        u32::try_from(value & 0xFFFF_FFFF)
            .unwrap_or(0)
            .to_ne_bytes(),
    )
}

/// `zend_parse_ini_string` and `zend_parse_ini_file`'s common body:
/// the callback's events in order, or bison's message and the line it
/// stopped at.
pub(super) fn parse(
    req: &mut Request,
    buf: &[u8],
    mode: i64,
) -> Result<Result<Vec<Event>, (String, u32)>, Thrown> {
    let mut parser = Parser {
        scanner: Scanner {
            buf,
            at: 0,
            state: State::Initial,
            stack: Vec::new(),
            lineno: 1,
            mode,
        },
        tok: Tok::End,
        val: Val::string(b""),
        req,
        events: Vec::new(),
    };
    parser.advance();
    match parser.statements()? {
        Ok(()) => Ok(Ok(parser.events)),
        Err(Syntax(message)) => Ok(Err((message, parser.scanner.lineno))),
    }
}

/// A key of the table the callbacks build.
#[derive(Clone, PartialEq, Debug)]
enum Slot {
    Int(i64),
    Str(Vec<u8>),
}

impl Slot {
    /// `zend_symtable_update`'s key: a canonical decimal integer in
    /// range is an index, anything else the string.
    fn symtable(bytes: &[u8]) -> Slot {
        let digits = bytes.strip_prefix(b"-").unwrap_or(bytes);
        let canonical = !digits.is_empty()
            && digits.iter().all(u8::is_ascii_digit)
            && (digits[0] != b'0' || (digits.len() == 1 && digits.len() == bytes.len()));
        if canonical && let Ok(n) = std::str::from_utf8(bytes).unwrap_or("x").parse::<i64>() {
            return Slot::Int(n);
        }
        Slot::Str(bytes.to_vec())
    }
}

/// One element: a value or a nested table.
enum Node {
    Leaf(Val),
    Table(Table),
}

/// The array a callback writes into, in insertion order with php's
/// `nNextFreeElement`.
struct Table {
    entries: Vec<(Slot, Node)>,
    next: Option<i64>,
}

impl Table {
    fn new() -> Table {
        Table {
            entries: Vec::new(),
            next: None,
        }
    }

    fn find(&self, key: &Slot) -> Option<usize> {
        self.entries.iter().position(|(k, _)| k == key)
    }

    /// `zend_hash_update`: in place where present, at the end otherwise.
    fn update(&mut self, key: Slot, node: Node) -> usize {
        if let Slot::Int(n) = key
            && self.next.is_none_or(|next| n >= next)
        {
            self.next = Some(n.saturating_add(1));
        }
        if let Some(at) = self.find(&key) {
            self.entries[at].1 = node;
            at
        } else {
            self.entries.push((key, node));
            self.entries.len() - 1
        }
    }

    /// `add_next_index_zval`.
    fn append(&mut self, node: Node) {
        let key = self.next.unwrap_or(0);
        self.update(Slot::Int(key), node);
    }

    /// `php_simple_ini_parser_cb`.
    fn simple(&mut self, event: Event) {
        match event {
            Event::Entry(label, Some(value)) => {
                self.update(Slot::symtable(&label_bytes(&label)), Node::Leaf(value));
            }
            Event::PopEntry(label, value, offset) => {
                let name = label_bytes(&label);
                let numeric = !(name.len() > 1 && name[0] == b'0')
                    && matches!(
                        php_ext::coerce::numeric_string(&name)
                            .as_ref()
                            .map(Value::as_ref),
                        Some(php_ext::values::ValueRef::Int(_))
                    );
                let key = match php_ext::coerce::numeric_string(&name)
                    .as_ref()
                    .map(Value::as_ref)
                {
                    Some(php_ext::values::ValueRef::Int(n)) if numeric => Slot::Int(n),
                    _ => Slot::Str(name),
                };
                let at = match self.find(&key) {
                    Some(at) => at,
                    None => self.update(key, Node::Table(Table::new())),
                };
                if !matches!(self.entries[at].1, Node::Table(_)) {
                    self.entries[at].1 = Node::Table(Table::new());
                }
                let Node::Table(inner) = &mut self.entries[at].1 else {
                    return;
                };
                match &offset.value {
                    Scalar::Str(o) if !o.is_empty() => {
                        inner.update(Slot::symtable(o), Node::Leaf(value));
                    }
                    _ => inner.append(Node::Leaf(value)),
                }
            }
            Event::Entry(_, None) | Event::Section(_) => {}
        }
    }
}

/// A label's bytes: labels are strings from the scanner.
fn label_bytes(val: &Val) -> Vec<u8> {
    match &val.value {
        Scalar::Str(s) => s.clone(),
        _ => Vec::new(),
    }
}

/// The callbacks' array for the parse's events:
/// `php_simple_ini_parser_cb` over them all, or with `sections`
/// `php_ini_parser_cb_with_sections`, a section's entries into the
/// section's own array from its header on.
pub(super) fn assemble(
    req: &mut Request,
    events: Vec<Event>,
    sections: bool,
) -> Result<Value, Thrown> {
    let mut top = Table::new();
    let mut active: Option<usize> = None;
    for event in events {
        if !sections {
            top.simple(event);
            continue;
        }
        match event {
            Event::Section(name) => {
                let at = top.update(
                    Slot::symtable(&label_bytes(&name)),
                    Node::Table(Table::new()),
                );
                active = Some(at);
            }
            Event::Entry(_, None) => {}
            event => match active {
                Some(at) => {
                    if let Node::Table(section) = &mut top.entries[at].1 {
                        section.simple(event);
                    }
                }
                None => top.simple(event),
            },
        }
    }
    to_value(req, top)
}

/// The table as a php array.
fn to_value(req: &mut Request, table: Table) -> Result<Value, Thrown> {
    let mut array = PhpArray::new(req)?;
    for (slot, node) in table.entries {
        let key = match slot {
            Slot::Int(n) => Key::Int(n),
            Slot::Str(s) => Key::Str(PhpString::new(req, &s)?),
        };
        let value = match node {
            Node::Table(inner) => to_value(req, inner)?,
            Node::Leaf(val) => match val.value {
                Scalar::Str(s) => Value::string(PhpString::new(req, &s)?),
                Scalar::Bool(b) => Value::bool(b),
                Scalar::Null => Value::null(),
                Scalar::Long(n) => Value::int(n),
                Scalar::Double(d) => Value::float(d),
            },
        };
        array.set(req, key, value)?;
    }
    Ok(Value::array(array))
}

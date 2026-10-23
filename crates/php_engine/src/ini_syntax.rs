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
//! One grammar, three readers, each php's: `parse_ini_string` and
//! `parse_ini_file` through [`crate::api::syntax::parse_ini`],
//! `get_browser`'s file the same way, and the host's startup entries,
//! every `-d` among them, which [`crate::engine`] parses as
//! `php_init_config` parses `sapi_module.ini_entries`. What differs
//! between them is the [`Host`]: where a constant, a directive and an
//! environment variable are looked up, and what the callback does with
//! each entry.

use crate::api::coerce::numeric_string;
use crate::api::values::{Value, ValueRef};

/// `ZEND_INI_SCANNER_NORMAL`, `_RAW` and `_TYPED`: how the scanner reads
/// a value.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum IniScanner {
    /// Quotes, constants, `${…}`, the operators and the boolean words
    /// read, every value a string.
    Normal,
    /// A value is its text, one pair of quotes stripped.
    Raw,
    /// As `Normal`, with the boolean words, `null` and whole numbers
    /// typed.
    Typed,
}

/// A value the grammar builds: php's `zval`, with `Z_EXTRA`'s one bit,
/// `INI_ZVAL_IS_NUMBER`, beside it, and the string the host holds for
/// it where it keeps an account of the parse's strings ([`Host::hold`]).
#[derive(Clone, Debug)]
pub(crate) struct Val {
    pub(crate) value: IniValue,
    number: bool,
    pub(crate) held: Option<Held>,
}

/// One string a [`Host`] holds for the grammar, php's `zend_string`
/// behind a token's or a value's `zval`: made by [`Host::hold`], grown by
/// [`Host::extend`], and given up by [`Host::free`] or with the event
/// that carries it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Held(pub(crate) usize);

/// `STACK_BLOCK_SIZE`: the slots `zend_stack_push` grows the scanner's
/// state stack by.
const STACK_BLOCK: usize = 16;

/// What an entry's value, its label or its offset holds: a string
/// everywhere but the typed scanner's values.
#[derive(Clone, Debug, PartialEq)]
pub enum IniValue {
    Str(Vec<u8>),
    Bool(bool),
    Null,
    Long(i64),
    Double(f64),
}

impl Val {
    fn string(bytes: &[u8]) -> Val {
        Val {
            value: IniValue::Str(bytes.to_vec()),
            number: false,
            held: None,
        }
    }
}

/// One call of the parser's callback: `ZEND_INI_PARSER_SECTION`,
/// `_ENTRY` (a bare label has no value) and `_POP_ENTRY`, the label,
/// the value and the offset in that order; each an [`IniValue`] where
/// the parse keeps no strings, and a request's value where it does
/// ([`crate::api::syntax::parse_ini`]).
#[derive(Clone, Debug, PartialEq)]
pub enum IniEvent<V = IniValue> {
    Section(V),
    Entry(V, Option<V>),
    PopEntry(V, V, V),
}

impl<V> IniEvent<V> {
    /// The event with each of its values mapped.
    pub(crate) fn map<W>(self, mut f: impl FnMut(V) -> W) -> IniEvent<W> {
        match self {
            IniEvent::Section(name) => IniEvent::Section(f(name)),
            IniEvent::Entry(label, value) => IniEvent::Entry(f(label), value.map(f)),
            IniEvent::PopEntry(label, value, offset) => {
                let label = f(label);
                let value = f(value);
                IniEvent::PopEntry(label, value, f(offset))
            }
        }
    }
}

/// Where the grammar runs: what `zend_ini_get_constant` and
/// `zend_ini_get_var` look up, how a double becomes text, and the
/// callback `zend_parse_ini_string` is handed, called as each statement
/// is reduced so a later `${…}` reads what an earlier entry set.
pub(crate) trait Host {
    /// What a lookup may raise: a request's `Thrown`, or nothing.
    type Error;
    /// `zend_get_constant`'s value as a string, `None` where there is no
    /// constant of the name.
    fn constant(&mut self, name: &[u8]) -> Result<Option<Vec<u8>>, Self::Error>;
    /// `zend_get_configuration_directive`.
    fn directive(&mut self, name: &[u8]) -> Option<Vec<u8>>;
    /// `zend_getenv`, then `getenv`.
    fn env(&mut self, name: &[u8]) -> Option<Vec<u8>>;
    /// `convert_to_string` of a double.
    fn double(&mut self, d: f64) -> Result<Vec<u8>, Self::Error>;
    /// The callback, after which the event's strings are the host's to
    /// give up, as the grammar's actions release them.
    fn event(&mut self, event: IniEvent<Val>) -> Result<(), Self::Error>;
    /// `zend_string_init` of `size` bytes holding `bytes`, the string
    /// `zend_ini_copy_value` and the lookups make; `None` where the host
    /// keeps no account of the parse's strings.
    fn hold(&mut self, _bytes: &[u8], _size: usize) -> Result<Option<Held>, Self::Error> {
        Ok(None)
    }
    /// `zend_string_extend` of `held` to hold `bytes`.
    fn extend(&mut self, _held: Held, _bytes: &[u8]) -> Result<Option<Held>, Self::Error> {
        Ok(None)
    }
    /// `zend_string_release` of `held`.
    fn free(&mut self, _held: Held) {}
    /// `zend_stack_push`'s `erealloc` of the scanner's state stack to
    /// `slots` ints.
    fn stack(&mut self, _slots: usize) -> Result<(), Self::Error> {
        Ok(())
    }
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
    !b"=\n\r\t;&|^$~(){}!\"[]\0".contains(&c)
}

/// `TOKENS`: `[:,.[\]"'()&|^+-/*=%$!~<>?@{}]`, `+-/` the range from
/// `+` to `/`.
fn token_char(c: u8) -> bool {
    matches!(c, b'+'..=b'/') || b":[]\"'()&|^*=%$!~<>?@{}".contains(&c)
}

/// The value of a step that answers `Result<Result<T, Syntax>, _>`:
/// the host's error raised with `?`, and a syntax error the caller's
/// answer, as the grammar's `YYERROR` unwinds every rule.
macro_rules! syntax {
    ($step:expr) => {
        match $step? {
            Ok(v) => v,
            Err(e) => return Ok(Err(e)),
        }
    };
}

/// The scanner over one buffer, read past its end as NUL padding.
struct Scanner<'a> {
    buf: &'a [u8],
    at: usize,
    state: State,
    stack: Vec<State>,
    /// The deepest the state stack has been, which its allocation
    /// follows.
    deepest: usize,
    /// The length `zend_ini_escape_string` asked for the last quoted
    /// string, its text before the escapes were read.
    escaped: Option<usize>,
    lineno: u32,
    mode: IniScanner,
}

impl Scanner<'_> {
    /// `yy_push_state`.
    fn push_state(&mut self, state: State) {
        self.stack.push(state);
        self.deepest = self.deepest.max(self.stack.len());
    }

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
                    self.units(p, |c| !b"$= \t\n\r;&|^~()!\"'\0".contains(&c), false),
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
                    self.state = if self.mode == IniScanner::Raw {
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
                    self.push_state(self.state);
                    self.state = S::VarName;
                    return (Tok::DollarCurly, none);
                }
                7 => {
                    self.state = self.stack.pop().unwrap_or(S::Initial);
                    self.push_state(self.state);
                    self.state = S::VarFallback;
                    return (Tok::Fallback, none);
                }
                8 => {
                    // A one-byte name followed by `:-` goes straight to
                    // the fallback, the name never handed on.
                    if self.byte(self.at) == b':' && self.byte(self.at + 1) == b'-' {
                        self.at += 1;
                        self.state = self.stack.pop().unwrap_or(S::Initial);
                        self.push_state(self.state);
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
                    self.state = if self.mode == IniScanner::Raw {
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
                    self.push_state(self.state);
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
        if self.mode == IniScanner::Typed && matches!(self.state, State::Value | State::Raw) {
            let value = match tok {
                Tok::BoolTrue => IniValue::Bool(true),
                Tok::BoolFalse => IniValue::Bool(false),
                Tok::Null => IniValue::Null,
                _ => IniValue::Str(bytes.to_vec()),
            };
            return Val {
                value,
                number: false,
                held: None,
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
        self.escaped = Some(raw.len());
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

/// The parser: the scanner, one token of lookahead, and the host the
/// constants, the variables and the callback go to.
struct Parser<'a, 'h, H: Host> {
    scanner: Scanner<'a>,
    tok: Tok,
    val: Val,
    host: &'h mut H,
    /// The state stack's slots, as `zend_stack` has allocated them.
    stack_slots: usize,
}

/// What a syntax error stops the parse with: bison's message.
struct Syntax(String);

impl<H: Host> Parser<'_, '_, H> {
    /// The next token, its string held as `RETURN_TOKEN`'s
    /// `zend_ini_copy_value` (or `zend_ini_escape_string`'s) makes it,
    /// after the state stack grew where the scanner pushed past it.
    fn advance(&mut self) -> Result<(), H::Error> {
        self.scanner.escaped = None;
        let (tok, mut val) = self.scanner.lex();
        while self.scanner.deepest > self.stack_slots {
            self.stack_slots += STACK_BLOCK;
            self.host.stack(self.stack_slots)?;
        }
        let valued = !matches!(
            tok,
            Tok::End
                | Tok::Section
                | Tok::DollarCurly
                | Tok::Fallback
                | Tok::EndOfLine
                | Tok::Char(_)
        );
        if valued && let IniValue::Str(bytes) = &val.value {
            let size = self.scanner.escaped.unwrap_or(bytes.len());
            val.held = self.host.hold(bytes, size)?;
        }
        self.tok = tok;
        self.val = val;
        Ok(())
    }

    /// The lookahead's value, taken: whoever takes it owns its string.
    fn take(&mut self) -> Val {
        std::mem::replace(&mut self.val, Val::string(b""))
    }

    /// `zval_ini_dtor` and `zend_string_free`: the value's string given up.
    fn free(&mut self, val: &Val) {
        if let Some(held) = val.held {
            self.host.free(held);
        }
    }

    /// A string the grammar makes, `zend_string_init` of `bytes`.
    fn made(&mut self, bytes: &[u8]) -> Result<Val, H::Error> {
        let mut val = Val::string(bytes);
        val.held = self.host.hold(bytes, bytes.len())?;
        Ok(val)
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

    fn expect(&mut self, tok: Tok) -> Result<Result<(), Syntax>, H::Error> {
        if self.tok == tok {
            self.advance()?;
            Ok(Ok(()))
        } else {
            Ok(Err(self.error(&[tok])))
        }
    }

    /// `zval_get_string` of a value, and `convert_to_string` for a
    /// concatenation's right side.
    fn as_bytes(&mut self, val: &Val) -> Result<Vec<u8>, H::Error> {
        Ok(match &val.value {
            IniValue::Str(s) => s.clone(),
            IniValue::Bool(true) => b"1".to_vec(),
            IniValue::Bool(false) | IniValue::Null => Vec::new(),
            IniValue::Long(n) => n.to_string().into_bytes(),
            IniValue::Double(d) => self.host.double(*d)?,
        })
    }

    /// `zend_ini_add_string(result, op1, op2)` and the
    /// `zend_string_free(op2)` every rule that calls it makes: the two as
    /// one string, `op1`'s grown in place (a new one where it was
    /// interned) and `op1`'s number bit kept, then `op2`'s given up.
    fn add(&mut self, a: &Val, b: &Val) -> Result<Val, H::Error> {
        let mut bytes = self.as_bytes(a)?;
        bytes.extend_from_slice(&self.as_bytes(b)?);
        let held = match a.held {
            Some(held) => self.host.extend(held, &bytes)?,
            None => self.host.hold(&bytes, bytes.len())?,
        };
        self.free(b);
        Ok(Val {
            value: IniValue::Str(bytes),
            number: a.number,
            held,
        })
    }

    /// `zend_ini_get_constant(result, name)`: the constant's value as a
    /// string, or the name where there is none or it holds a `:`.
    ///
    /// ```php
    /// var_dump(parse_ini_string("a = \${nope:-TRUE}\nb = \${nope:-null}\nc = \${nope:-E_ALL}\nd[false] = x"));
    /// ```
    /// ```expect
    /// array(4) {
    ///   ["a"]=>
    ///   string(1) "1"
    ///   ["b"]=>
    ///   string(0) ""
    ///   ["c"]=>
    ///   string(5) "30719"
    ///   ["d"]=>
    ///   array(1) {
    ///     [0]=>
    ///     string(1) "x"
    ///   }
    /// }
    /// ```
    fn get_constant(&mut self, name: Val) -> Result<Val, H::Error> {
        let IniValue::Str(bytes) = &name.value else {
            return Ok(name);
        };
        if bytes.contains(&b':') {
            return Ok(name);
        }
        let value = match self.host.constant(bytes)? {
            Some(value) => value,
            // `zend_get_constant` ends at `zend_get_special_const`:
            // `true`, `false` and `null` in any case, as strings.
            None => match bytes.to_ascii_lowercase().as_slice() {
                b"true" => b"1".to_vec(),
                b"false" | b"null" => Vec::new(),
                _ => return Ok(name),
            },
        };
        let found = self.made(&value)?;
        self.free(&name);
        Ok(found)
    }

    /// `zend_ini_get_var(result, name, fallback)`: the configuration
    /// directive, else the environment variable, else the fallback, else
    /// the empty string.
    /// The rules that call it free the name and the fallback after.
    fn get_var(&mut self, name: &Val, fallback: Option<&Val>) -> Result<Val, H::Error> {
        let name = self.as_bytes(name)?;
        if let Some(value) = self.host.directive(&name) {
            return self.made(&value);
        }
        if let Some(value) = self.host.env(&name) {
            return self.made(&value);
        }
        match fallback {
            Some(fallback) => {
                let bytes = self.as_bytes(fallback)?;
                self.made(&bytes)
            }
            None => Ok(Val::string(b"")),
        }
    }

    /// `cfg_var_ref`, the lookahead at `TC_DOLLAR_CURLY`.
    fn cfg_var_ref(&mut self) -> Result<Result<Val, Syntax>, H::Error> {
        self.advance()?;
        if self.tok != Tok::VarName {
            return Ok(Err(self.error(&[Tok::VarName])));
        }
        let name = self.take();
        self.advance()?;
        match self.tok {
            Tok::Char(b'}') => {
                self.advance()?;
                let found = self.get_var(&name, None)?;
                self.free(&name);
                Ok(Ok(found))
            }
            Tok::Fallback => {
                self.advance()?;
                let fallback =
                    syntax!(self.var_string_list(false)).unwrap_or_else(|| Val::string(b""));
                if self.tok != Tok::Char(b'}') {
                    return Ok(Err(self.error(&[Tok::Char(b'}')])));
                }
                self.advance()?;
                let found = self.get_var(&name, Some(&fallback))?;
                self.free(&name);
                self.free(&fallback);
                Ok(Ok(found))
            }
            _ => Ok(Err(self.error(&[Tok::Fallback, Tok::Char(b'}')]))),
        }
    }

    /// `'"' encapsed_list '"'`, the lookahead at the opening quote.
    fn encapsed(&mut self) -> Result<Result<Val, Syntax>, H::Error> {
        self.advance()?;
        let mut acc = Val::string(b"");
        loop {
            match self.tok {
                Tok::DollarCurly => {
                    let v = syntax!(self.cfg_var_ref());
                    acc = self.add(&acc, &v)?;
                }
                Tok::QuotedString => {
                    let v = self.take();
                    self.advance()?;
                    acc = self.add(&acc, &v)?;
                }
                Tok::Char(b'"') => {
                    self.advance()?;
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
    fn var_string_list(&mut self, section: bool) -> Result<Result<Option<Val>, Syntax>, H::Error> {
        let mut acc: Option<Val> = None;
        loop {
            let item = match self.tok {
                Tok::DollarCurly => syntax!(self.cfg_var_ref()),
                Tok::Char(b'"') => syntax!(self.encapsed()),
                Tok::Constant | Tok::Raw | Tok::Number | Tok::Str | Tok::Whitespace => {
                    let tok = self.tok;
                    let mut v = self.take();
                    self.advance()?;
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
            IniValue::Long(n) => {
                i32::from_ne_bytes(u32::try_from(n & 0xFFFF_FFFF).unwrap_or(0).to_ne_bytes())
            }
            #[allow(clippy::cast_possible_truncation)]
            IniValue::Double(d) => *d as i32,
            IniValue::Str(s) => crate::c_number::atoi(s),
            IniValue::Bool(b) => i32::from(*b),
            IniValue::Null => 0,
        }
    }

    /// `zend_ini_do_op(type, result, op1, op2)`: the operation over
    /// C `int`s, each operand's string freed as `get_int_val` reads it,
    /// answered as a new `%d` string or, typed, as a long.
    fn do_op(&mut self, op: u8, lhs: &Val, rhs: Option<&Val>) -> Result<Val, H::Error> {
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
        for operand in std::iter::once(lhs).chain(rhs) {
            self.free(operand);
        }
        let mut val = if self.scanner.mode == IniScanner::Typed {
            Val {
                value: IniValue::Long(i64::from(out)),
                number: false,
                held: None,
            }
        } else {
            self.made(out.to_string().as_bytes())?
        };
        val.number = lhs.number;
        Ok(val)
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
    fn unary(&mut self) -> Result<Result<Val, Syntax>, H::Error> {
        match self.tok {
            Tok::Char(op @ (b'~' | b'!')) => {
                self.advance()?;
                match self.unary()? {
                    Ok(v) => self.do_op(op, &v, None).map(Ok),
                    Err(e) => Ok(Err(e)),
                }
            }
            Tok::Char(b'(') => {
                self.advance()?;
                let inner = syntax!(self.expr());
                if self.tok != Tok::Char(b')') {
                    return Ok(Err(self.error(&[
                        Tok::Char(b'^'),
                        Tok::Char(b'|'),
                        Tok::Char(b'&'),
                        Tok::Char(b')'),
                    ])));
                }
                self.advance()?;
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
    fn expr(&mut self) -> Result<Result<Val, Syntax>, H::Error> {
        let mut acc = syntax!(self.unary());
        while let Tok::Char(op @ (b'|' | b'&' | b'^')) = self.tok {
            self.advance()?;
            let rhs = syntax!(self.unary());
            acc = self.do_op(op, &acc, Some(&rhs))?;
        }
        Ok(Ok(acc))
    }

    /// `normalize_value(zv)`: in the typed scanner, a string that began
    /// with a number and reads whole as one, as that number, the string
    /// given up.
    fn normalize(&mut self, val: Val) -> Val {
        if self.scanner.mode != IniScanner::Typed || !val.number {
            return val;
        }
        let IniValue::Str(s) = &val.value else {
            return val;
        };
        let value = match numeric_string(s).as_ref().map(Value::as_ref) {
            Some(ValueRef::Int(n)) => IniValue::Long(n),
            Some(ValueRef::Float(d)) if !overflowed(s) => IniValue::Double(d),
            _ => return val,
        };
        self.free(&val);
        Val {
            value,
            number: true,
            held: None,
        }
    }

    /// `string_or_value`.
    fn string_or_value(&mut self) -> Result<Result<Val, Syntax>, H::Error> {
        match self.tok {
            Tok::BoolTrue | Tok::BoolFalse | Tok::Null => {
                let v = self.take();
                self.advance()?;
                Ok(Ok(v))
            }
            Tok::EndOfLine => {
                self.advance()?;
                Ok(Ok(Val::string(b"")))
            }
            _ => match self.expr()? {
                Ok(v) => Ok(Ok(self.normalize(v))),
                Err(e) => Ok(Err(e)),
            },
        }
    }

    /// `statement_list`: each statement's callback as it is reduced,
    /// the host giving up its strings after it.
    fn statements(&mut self) -> Result<Result<(), Syntax>, H::Error> {
        loop {
            match self.tok {
                Tok::End => return Ok(Ok(())),
                Tok::EndOfLine => self.advance()?,
                Tok::Section => {
                    self.advance()?;
                    let name =
                        syntax!(self.var_string_list(true)).unwrap_or_else(|| Val::string(b""));
                    if let Err(e) = self.expect(Tok::Char(b']'))? {
                        self.free(&name);
                        return Ok(Err(e));
                    }
                    self.host.event(IniEvent::Section(name))?;
                }
                Tok::Label => {
                    let label = self.take();
                    self.advance()?;
                    if self.tok == Tok::Char(b'=') {
                        self.advance()?;
                        let value = match self.string_or_value()? {
                            Ok(v) => v,
                            Err(e) => {
                                self.free(&label);
                                return Ok(Err(e));
                            }
                        };
                        self.host.event(IniEvent::Entry(label, Some(value)))?;
                    } else {
                        self.host.event(IniEvent::Entry(label, None))?;
                    }
                }
                Tok::Offset => {
                    let label = self.take();
                    self.advance()?;
                    let offset = match self.var_string_list(false)? {
                        Ok(Some(v)) => v,
                        Ok(None) => Val::string(b""),
                        Err(e) => {
                            self.free(&label);
                            return Ok(Err(e));
                        }
                    };
                    let mut closed = self.expect(Tok::Char(b']'))?;
                    if closed.is_ok() {
                        closed = self.expect(Tok::Char(b'='))?;
                    }
                    let value = match closed {
                        Ok(()) => self.string_or_value()?,
                        Err(e) => Err(e),
                    };
                    let value = match value {
                        Ok(v) => v,
                        Err(e) => {
                            self.free(&label);
                            self.free(&offset);
                            return Ok(Err(e));
                        }
                    };
                    self.host.event(IniEvent::PopEntry(label, value, offset))?;
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
        .filter(|c| !crate::c_number::is_whitespace(**c))
        .collect::<Vec<_>>();
    let digits = t
        .strip_prefix(&[&b'-'])
        .or_else(|| t.strip_prefix(&[&b'+']))
        .unwrap_or(&t);
    !digits.is_empty() && digits.iter().all(|c| c.is_ascii_digit())
}

/// `zend_parse_ini_string` and `zend_parse_ini_file`'s common body:
/// each statement's callback on the host as it is reduced, then nothing,
/// or bison's message and the line the scanner stood at, the callbacks
/// before it already made.
pub(crate) fn parse<H: Host>(
    buf: &[u8],
    mode: IniScanner,
    host: &mut H,
) -> Result<Result<(), (String, u32)>, H::Error> {
    let mut parser = Parser {
        scanner: Scanner {
            buf,
            at: 0,
            state: State::Initial,
            stack: Vec::new(),
            deepest: 0,
            escaped: None,
            lineno: 1,
            mode,
        },
        tok: Tok::End,
        val: Val::string(b""),
        host,
        stack_slots: 0,
    };
    parser.advance()?;
    match parser.statements()? {
        Ok(()) => Ok(Ok(())),
        Err(Syntax(message)) => Ok(Err((message, parser.scanner.lineno))),
    }
}

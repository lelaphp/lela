//! `php_json_parser`: `json_scanner.re` and `json_parser.y` at the tag,
//! the scanner's conditions as one walk over the bytes and bison's
//! grammar as a walk over the brackets below it, since the grammar is
//! `start: value EOI` over two bracketed forms and reads the same
//! language either way. The open brackets are a stack on the heap and
//! bison's own state count is kept beside them, so a nest is refused
//! where php's `YYMAXDEPTH` refuses it and never by the machine.
//!
//! What is copied exactly, because it is observable: the token a byte
//! begins, longest match first and the earlier condition where two are
//! the same length; the error code each refusal carries, which
//! `json_last_error` answers and `JSON_THROW_ON_ERROR` throws; the
//! depth counted from one at the opening bracket; and the two
//! `INVALID_UTF8_*` arms inside a string. `php_json_scan` reads the
//! `zend_string`'s NUL terminator as its end of input, so the byte past
//! the last is a zero here, which is what tells an unterminated string
//! (a control character) from the end of a well-formed one.

use php_ext::call;
use php_ext::raising::{Thrown, fatal};
use php_ext::request::Request;
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpString, Value};

use crate::constants::{
    JSON_BIGINT_AS_STRING, JSON_ERROR_CTRL_CHAR, JSON_ERROR_DEPTH,
    JSON_ERROR_INVALID_PROPERTY_NAME, JSON_ERROR_NONE, JSON_ERROR_STATE_MISMATCH,
    JSON_ERROR_SYNTAX, JSON_ERROR_UTF8, JSON_ERROR_UTF16, JSON_INVALID_UTF8_IGNORE,
    JSON_INVALID_UTF8_SUBSTITUTE, JSON_OBJECT_AS_ARRAY,
};

/// `LONG_MIN_DIGITS`: the digits of `-9223372036854775808`, what the
/// scanner compares a nineteen-digit number against before it decides
/// the number does not fit a `zend_long`.
const LONG_MIN_DIGITS: &[u8] = b"9223372036854775808";

/// `PHP_JSON_INT_MAX_LENGTH`, `MAX_LENGTH_OF_LONG - 1` on a 64-bit
/// build: the digit count at which a number is compared rather than
/// read.
const INT_MAX_LENGTH: usize = 19;

/// What one `php_json_scan` answered: a bison terminal with the value
/// the scanner put in `s->value` beside it.
enum Token {
    ObjectOpen,
    ObjectClose,
    ArrayOpen,
    ArrayClose,
    Colon,
    Comma,
    Null,
    True,
    False,
    Int(i64),
    Double(f64),
    /// `PHP_JSON_T_STRING` and `PHP_JSON_T_ESTRING`, which the grammar
    /// accepts in the same two places, with the string the scanner made.
    Str(Text),
    Eoi,
    /// `PHP_JSON_T_ERROR`: the scanner has written its code, or none
    /// where the memory limit refused a string, which the grammar's
    /// `yyerror` makes the syntax error.
    Error,
}

/// A string token's `s->value` as `php_json_yylex` hands it to the
/// grammar.
enum Text {
    /// The string the scanner made.
    Made(Owner<PhpString>),
    /// `UNDEF`: the string given up as it was scanned, as the validating
    /// methods' `yylex` gives each up.
    GivenUp,
}

impl Text {
    /// The string given up, where one was made.
    fn give_up(self, req: &mut Request) {
        if let Text::Made(string) = self {
            string.give_up(req);
        }
    }
}

impl Token {
    /// The token discarded, bison's `%destructor` over its value: the
    /// string a string token holds given up.
    fn give_up(self, req: &mut Request) {
        if let Token::Str(text) = self {
            text.give_up(req);
        }
    }
}

/// `php_json_scanner`: the cursor, the options and the code the last
/// refusal left.
struct Scanner<'a> {
    input: &'a [u8],
    cursor: usize,
    options: i64,
    errcode: i64,
}

impl Scanner<'_> {
    /// The byte at `at`, and zero past the last, which is the
    /// `zend_string`'s NUL terminator the scanner runs into.
    fn byte(&self, at: usize) -> u8 {
        self.input.get(at).copied().unwrap_or(0)
    }

    /// Set the code and answer the error token.
    fn refuse(&mut self, code: i64) -> Token {
        self.errcode = code;
        Token::Error
    }

    /// Whether `word` stands at the cursor, and past it if it does.
    fn keyword(&mut self, word: &[u8]) -> bool {
        if self
            .input
            .get(self.cursor..)
            .is_some_and(|r| r.starts_with(word))
        {
            self.cursor += word.len();
            return true;
        }
        false
    }

    /// `php_json_scan` in condition `JS`: the whitespace of `NL|WS`
    /// skipped, then the longest rule that matches at the cursor. A
    /// string token's string is made here, as php's scanner makes it
    /// into `s->value`; the `Err` is the memory limit's fatal where the
    /// string is made infallibly.
    fn scan(&mut self, req: &mut Request) -> Result<Token, Thrown> {
        while matches!(self.byte(self.cursor), b' ' | b'\t' | b'\r' | b'\n') {
            self.cursor += 1;
        }
        let start = self.cursor;
        let c = self.byte(start);
        let punctuation = match c {
            b'{' => Some(Token::ObjectOpen),
            b'}' => Some(Token::ObjectClose),
            b'[' => Some(Token::ArrayOpen),
            b']' => Some(Token::ArrayClose),
            b':' => Some(Token::Colon),
            b',' => Some(Token::Comma),
            _ => None,
        };
        if let Some(token) = punctuation {
            self.cursor += 1;
            return Ok(token);
        }
        if c == b'"' {
            self.cursor += 1;
            return self.scan_string(req);
        }
        if c == 0 {
            // `<JS>EOI`: the terminator past the last byte is the end of
            // input, and a zero byte within it a control character.
            self.cursor += 1;
            return Ok(if self.cursor > self.input.len() {
                Token::Eoi
            } else {
                self.refuse(JSON_ERROR_CTRL_CHAR)
            });
        }
        if self.keyword(b"null") {
            return Ok(Token::Null);
        }
        if self.keyword(b"true") {
            return Ok(Token::True);
        }
        if self.keyword(b"false") {
            return Ok(Token::False);
        }
        if let Some(token) = self.scan_number(req)? {
            return Ok(token);
        }
        if c <= 0x1f {
            return Ok(self.refuse(JSON_ERROR_CTRL_CHAR));
        }
        // `<JS>UTF8` is a syntax error and `<JS>ANY`, the byte no UTF-8
        // sequence begins, is the malformed one.
        if utf8_length(self.input, start).is_some() {
            self.cursor += 1;
            return Ok(self.refuse(JSON_ERROR_SYNTAX));
        }
        self.cursor += 1;
        Ok(self.refuse(JSON_ERROR_UTF8))
    }

    /// `<JS>INT` and `<JS>FLOAT|EXP`, longest match: the integer alone,
    /// the integer with a fraction, or either with an exponent. `None`
    /// where no number stands at the cursor, which leaves the byte to
    /// the conditions below it.
    fn scan_number(&mut self, req: &mut Request) -> Result<Option<Token>, Thrown> {
        let start = self.cursor;
        let mut at = start;
        let negative = self.byte(at) == b'-';
        if negative {
            at += 1;
        }
        let digits_at = at;
        match self.byte(at) {
            b'0' => at += 1,
            b'1'..=b'9' => {
                while self.byte(at).is_ascii_digit() {
                    at += 1;
                }
            }
            _ => return Ok(None),
        }
        let digits = at - digits_at;
        let mut end = at;
        let mut double = false;
        if self.byte(end) == b'.' && self.byte(end + 1).is_ascii_digit() {
            end += 1;
            while self.byte(end).is_ascii_digit() {
                end += 1;
            }
            double = true;
        }
        if matches!(self.byte(end), b'e' | b'E') {
            let mut exponent = end + 1;
            if matches!(self.byte(exponent), b'+' | b'-') {
                exponent += 1;
            }
            if self.byte(exponent).is_ascii_digit() {
                while self.byte(exponent).is_ascii_digit() {
                    exponent += 1;
                }
                end = exponent;
                double = true;
            }
        }
        self.cursor = end;
        let text = self.input.get(start..end).unwrap_or_default();
        if double {
            return Ok(Some(Token::Double(strtod(text))));
        }
        let digits_of = self.input.get(digits_at..at).unwrap_or_default();
        self.integer(req, text, digits_of, digits, negative)
            .map(Some)
    }

    /// `<JS>INT`'s body: the number as a `zend_long` while it fits,
    /// and as the string itself or a double once it does not, which is
    /// what `JSON_BIGINT_AS_STRING` chooses between. The string is
    /// `ZVAL_STRINGL`, `zend_string_init` asked for infallibly
    /// (`Z/ext/json/json_scanner.re:187`), where a string token's is
    /// fallible ([`Scanner::string`]): one past the hard memory limit is
    /// its `_exit` where it is asked for, where a fallible one would be a
    /// refused parse and the soft limit's fatal as the call returns.
    ///
    /// ```php
    /// ini_set('memory_limit', '16M');
    /// ini_set('memory_hard_limit_percent', '100');
    /// $n = 10 * 1024 * 1024; $json = str_repeat('9', $n); $json[0] = '['; $json[$n - 1] = ']';
    /// try { var_dump(json_decode($json, false, 512, JSON_BIGINT_AS_STRING | JSON_THROW_ON_ERROR)); } catch (JsonException $e) { echo "caught\n"; }
    /// echo "unreached\n";
    /// ```
    /// ```expect capture exit=137
    /// ```
    /// ```stderr
    /// Hard memory limit of 16777216 bytes exhausted (tried to allocate 10502144 bytes) in %SCRATCH%/witness.php on line 5
    /// ```
    fn integer(
        &self,
        req: &mut Request,
        text: &[u8],
        digits_of: &[u8],
        digits: usize,
        negative: bool,
    ) -> Result<Token, Thrown> {
        let bigint = digits > INT_MAX_LENGTH
            || (digits == INT_MAX_LENGTH && {
                let cmp = digits_of.cmp(LONG_MIN_DIGITS);
                !(cmp.is_lt() || (cmp.is_eq() && negative))
            });
        if !bigint {
            return Ok(Token::Int(
                std::str::from_utf8(text)
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(0),
            ));
        }
        if self.options & JSON_BIGINT_AS_STRING != 0 {
            return Ok(Token::Str(Text::Made(PhpString::new(req, text)?)));
        }
        Ok(Token::Double(strtod(text)))
    }

    /// Conditions `STR_P1` and `STR_P2`: php sizes the string in the
    /// first and writes it in the second, over the same bytes in the same
    /// order, so the first walk meets every refusal and the second, into
    /// the string itself, meets none. The cursor stands past the opening
    /// quote.
    fn scan_string(&mut self, req: &mut Request) -> Result<Token, Thrown> {
        let start = self.cursor;
        let mut sized = Written(None, 0);
        if let Some(code) = self.walk_string(&mut sized) {
            return Ok(self.refuse(code));
        }
        let (end, len) = (self.cursor, sized.1);
        let made = Scanner::string(req, len, |to| {
            self.cursor = start;
            let mut written = Written(Some(to), 0);
            let refused = self.walk_string(&mut written);
            debug_assert!(refused.is_none() && written.1 == len);
        });
        self.cursor = end;
        made
    }

    /// One walk of a string's bytes to its closing quote, past which it
    /// leaves the cursor, each byte it makes put to `out`; the code of
    /// the refusal it met, if any.
    fn walk_string(&mut self, out: &mut Written<'_>) -> Option<i64> {
        loop {
            let c = self.byte(self.cursor);
            if c == b'"' {
                self.cursor += 1;
                return None;
            }
            if c <= 0x1f {
                // `<STR_P1>CTRL`, the NUL terminator of an unterminated
                // string among them.
                return Some(JSON_ERROR_CTRL_CHAR);
            }
            if c == b'\\' {
                if let Some(code) = self.scan_escape(out) {
                    return Some(code);
                }
                continue;
            }
            let Some(len) = utf8_length(self.input, self.cursor) else {
                // `<STR_P1>ANY`: one byte, ignored or replaced by U+FFFD
                // where the options say so.
                if self.options & JSON_INVALID_UTF8_SUBSTITUTE != 0 {
                    out.put("\u{fffd}".as_bytes());
                } else if self.options & JSON_INVALID_UTF8_IGNORE == 0 {
                    return Some(JSON_ERROR_UTF8);
                }
                self.cursor += 1;
                continue;
            };
            out.put(
                self.input
                    .get(self.cursor..self.cursor + len)
                    .unwrap_or_default(),
            );
            self.cursor += len;
        }
    }

    /// `<STR_P1>["]`: the string token's string, made as the scanner
    /// meets the closing quote and before the grammar sees the token.
    /// The empty string is `ZVAL_EMPTY_STRING`, php's interned one, and
    /// any other `zend_string_falloc` (`Z/ext/json/json_scanner.re:267`),
    /// a string of the request's own even at one byte, and asked for
    /// fallibly since 8.5.12: `PHP_JSON_T_ERROR`, with no code of its
    /// own, where the memory limit refused it, the parse refused with it
    /// and the limit's fatal raised as the call returns rather than the
    /// hard limit met:
    ///
    /// ```php
    /// ini_set('memory_limit', '16M');
    /// $n = 10 * 1024 * 1024; $json = str_repeat("\x80", $n); $json[0] = '"'; $json[$n - 1] = '"';
    /// var_dump(json_decode($json, false, 512, JSON_INVALID_UTF8_SUBSTITUTE), "unreached");
    /// ```
    /// ```expect exit=255
    ///
    /// Fatal error: Allowed memory size of 16777216 bytes exhausted (tried to allocate 31473664 bytes) in %s on line 4
    /// ```
    ///
    /// A string of one byte takes its bin where a native's would be the
    /// interned character, and a key keeps its string in the table it is
    /// written to:
    ///
    /// ```php
    /// foreach (['"a"', '"ab"', '""', '["a","b","c"]', '{"a":1}', '{"ab":1}'] as $j) {
    ///     $m = memory_get_usage(); $x = json_decode($j, true); echo $j, ' ', memory_get_usage() - $m, "\n"; $x = null;
    /// }
    /// ```
    /// ```expect
    /// "a" 64
    /// "ab" 32
    /// "" 0
    /// ["a","b","c"] 312
    /// {"a":1} 408
    /// {"ab":1} 408
    /// ```
    fn string(
        req: &mut Request,
        len: usize,
        fill: impl FnOnce(&mut [u8]),
    ) -> Result<Token, Thrown> {
        if len == 0 {
            return Ok(Token::Str(Text::Made(PhpString::new(req, b"")?)));
        }
        let made = req
            .memory_mut()
            .fallible(|req| PhpString::filled(req, len, fill))?;
        Ok(made.map_or(Token::Error, |string| Token::Str(Text::Made(string))))
    }

    /// One escape at the cursor, which stands on the backslash: the
    /// surrogate pair of `UTF16_4` first, since it is the longest, then
    /// the four hex digits of `UTF16_1` through `UCS2`, then the
    /// one-character escapes of `ESC`. The code of the refusal, or
    /// `None` where the escape was written.
    fn scan_escape(&mut self, out: &mut Written<'_>) -> Option<i64> {
        if let Some(code) = self.surrogate_pair() {
            push_utf8(out, code);
            self.cursor += 12;
            return None;
        }
        if let Some(code) = self.ucs2_at(self.cursor) {
            if (0xd800..=0xdfff).contains(&code) {
                // `<STR_P1>UCS2`, everything `UTF16_1..3` left: one half
                // of a surrogate pair and nothing to pair it with.
                return Some(JSON_ERROR_UTF16);
            }
            push_utf8(out, code);
            self.cursor += 6;
            return None;
        }
        let escaped = match self.byte(self.cursor + 1) {
            b'b' => 0x08,
            b'f' => 0x0c,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            c @ (b'\\' | b'/' | b'"') => c,
            // `<STR_P1>ESCPREF`: a backslash before anything else.
            _ => return Some(JSON_ERROR_SYNTAX),
        };
        out.put(&[escaped]);
        self.cursor += 2;
        None
    }

    /// `UTF16_4`: a high surrogate escape and a low one beside it, as
    /// the code point `php_json_ucs2_to_int_ex` reads them back to.
    fn surrogate_pair(&self) -> Option<u32> {
        let high = self.ucs2_at(self.cursor)?;
        if !(0xd800..=0xdbff).contains(&high) {
            return None;
        }
        let low = self.ucs2_at(self.cursor + 6)?;
        if !(0xdc00..=0xdfff).contains(&low) {
            return None;
        }
        Some(((high & 0x3ff) << 10) + (low & 0x3ff) + 0x10000)
    }

    /// `UCS2`: `\u` and four hex digits at `at`, as the number they spell.
    fn ucs2_at(&self, at: usize) -> Option<u32> {
        if self.byte(at) != b'\\' || self.byte(at + 1) != b'u' {
            return None;
        }
        let mut code = 0;
        for i in 0..4 {
            code = code * 16 + char::from(self.byte(at + 2 + i)).to_digit(16)?;
        }
        Some(code)
    }
}

/// `zend_strtod` over the text the scanner matched, which is decimal
/// and well formed by construction.
fn strtod(text: &[u8]) -> f64 {
    std::str::from_utf8(text)
        .ok()
        .and_then(|t| t.parse().ok())
        .unwrap_or(0.0)
}

/// `php_utf32_utf8`: the code point as the bytes php writes for it.
fn push_utf8(out: &mut Written<'_>, code: u32) {
    let mut buffer = [0_u8; 4];
    if let Some(c) = char::from_u32(code) {
        out.put(c.encode_utf8(&mut buffer).as_bytes());
    }
}

/// Where a string's walk puts its bytes, and how many it has put: into
/// the string `STR_P1`'s count sized, `STR_P2`'s write, or, with no
/// string yet, nowhere, `STR_P1`'s count itself.
struct Written<'b>(Option<&'b mut [u8]>, usize);

impl Written<'_> {
    fn put(&mut self, bytes: &[u8]) {
        let end = self.1 + bytes.len();
        if let Some(room) = self.0.as_mut().and_then(|to| to.get_mut(self.1..end)) {
            room.copy_from_slice(bytes);
        }
        self.1 = end;
    }
}

/// The scanner's `UTF8` rule: the length of the sequence beginning at
/// `at`, or `None` where the byte begins none. The ranges are
/// `UTF8_1..UTF8_4`, which refuse an overlong form and a surrogate as
/// php's do; a byte past the end reads as zero and matches no trail.
fn utf8_length(input: &[u8], at: usize) -> Option<usize> {
    let byte = |i: usize| input.get(i).copied().unwrap_or(0);
    let trail = |i: usize| (0x80..=0xbf).contains(&byte(i));
    match byte(at) {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf if trail(at + 1) => Some(2),
        0xe0 if (0xa0..=0xbf).contains(&byte(at + 1)) && trail(at + 2) => Some(3),
        0xe1..=0xec | 0xee..=0xef if trail(at + 1) && trail(at + 2) => Some(3),
        0xed if (0x80..=0x9f).contains(&byte(at + 1)) && trail(at + 2) => Some(3),
        0xf0 if (0x90..=0xbf).contains(&byte(at + 1)) && trail(at + 2) && trail(at + 3) => Some(4),
        0xf1..=0xf3 if trail(at + 1) && trail(at + 2) && trail(at + 3) => Some(4),
        0xf4 if (0x80..=0x8f).contains(&byte(at + 1)) && trail(at + 2) && trail(at + 3) => Some(4),
        _ => None,
    }
}

/// What a `{ ... }` is built into: `php_json_parser_object_create`'s
/// `stdClass`, its array under `JSON_OBJECT_AS_ARRAY`, and nothing
/// under the validating methods.
enum Holder {
    Object(Owner<PhpObject>),
    Array(Owner<PhpArray>),
    Nothing,
}

/// A bracket the walk is inside, its rule half reduced on bison's
/// stack: the count of states the stack held before its bracket, what
/// it is building, and for an object the key its next value goes under,
/// none where no key is held: before the first is read, and from the
/// member a key named to the next key's read.
enum Open {
    Array {
        at: usize,
        array: Option<Owner<PhpArray>>,
    },
    Object {
        at: usize,
        holder: Holder,
        key: Option<Text>,
    },
}

/// What [`Parser::begin`] made of the lookahead.
enum Begun {
    /// A scalar, or a pair with nothing inside, whole, one owner.
    Value(Owner<Value>),
    /// A bracket left open on the walk's stack, its first value next.
    Opened,
    /// The parse refused, the code the scanner's.
    Refused,
}

/// What [`Parser::attach`] left the bracket at.
enum Attached {
    /// A comma: its next value.
    Next,
    /// No comma: its closing token.
    Close,
    /// The parse refused, the code the scanner's.
    Refused,
}

impl Open {
    /// What the bracket became, `null` where nothing was built, and a
    /// key an object still holds given up.
    fn into_value(self, req: &mut Request) -> Owner<Value> {
        match self {
            Open::Array { array, .. } => match array {
                Some(array) => array.into_value(),
                None => Owner::null(),
            },
            Open::Object { holder, key, .. } => {
                if let Some(key) = key {
                    key.give_up(req);
                }
                holder.into_value()
            }
        }
    }

    /// The owner given up where the parse was refused inside it.
    fn give_up(self, req: &mut Request) {
        match self {
            Open::Array { array, .. } => {
                if let Some(array) = array {
                    array.give_up(req);
                }
            }
            Open::Object { holder, key, .. } => {
                if let Some(key) = key {
                    key.give_up(req);
                }
                holder.give_up(req);
            }
        }
    }
}

/// `php_json_parser`: the scanner, one token of lookahead, and the
/// depth bison's `PHP_JSON_DEPTH_INC` counts.
struct Parser<'a> {
    scanner: Scanner<'a>,
    token: Token,
    depth: i64,
    max_depth: i64,
    /// How many states bison's stack holds, state 0 counted: what the
    /// grammar's shifts and gotos push and its reductions pop, kept
    /// beside the descent so it stops where php's parser does
    /// ([`Parser::push`]).
    stack: usize,
    /// `php_json_get_validate_methods`: nothing is built and each string
    /// the scanner makes is given up as it is scanned, which is what
    /// `json_validate` runs.
    validating: bool,
}

/// Bison's `YYMAXDEPTH`, which `json_parser.y` leaves at the skeleton's
/// 10000: the states the parser's stack may grow to before `yyparse`
/// gives up with "memory exhausted", which `php_json_yyerror` records
/// as `JSON_ERROR_SYNTAX`.
const YYMAXDEPTH: usize = 10000;

impl Parser<'_> {
    /// `php_json_yylex`: the next token, its string given up at once
    /// under the validating methods, and the lookahead it replaces
    /// discarded.
    fn advance(&mut self, req: &mut Request) -> Result<(), Thrown> {
        let mut token = self.scanner.scan(req)?;
        if self.validating && matches!(token, Token::Str(_)) {
            std::mem::replace(&mut token, Token::Str(Text::GivenUp)).give_up(req);
        }
        std::mem::replace(&mut self.token, token).give_up(req);
        Ok(())
    }

    /// One state pushed on bison's stack, a shift or a goto, and
    /// `yyexhaustedlab` where the stack has reached [`YYMAXDEPTH`]:
    /// php's parser keeps its states on the heap, so a nest is refused
    /// by this count and not by the machine, at 4998 arrays one inside
    /// the next, 2499 objects each under a key, and fewer where every
    /// level holds a member or an element before the nested one.
    ///
    /// ```php
    /// foreach ([4998, 4999] as $n) { var_dump(json_decode(str_repeat('[', $n) . str_repeat(']', $n), true, 100000) !== null, json_last_error_msg()); }
    /// foreach ([2499, 2500] as $n) { var_dump(json_validate(str_repeat('{"a":', $n) . '1' . str_repeat('}', $n), 100000), json_last_error_msg()); }
    /// var_dump(json_decode(str_repeat('{"x":0,"a":', 1667) . '1' . str_repeat('}', 1667), false, 100000), json_last_error_msg());
    /// ```
    /// ```expect
    /// bool(true)
    /// string(8) "No error"
    /// bool(false)
    /// string(12) "Syntax error"
    /// bool(true)
    /// string(8) "No error"
    /// bool(false)
    /// string(12) "Syntax error"
    /// NULL
    /// string(12) "Syntax error"
    /// ```
    fn push(&mut self) -> bool {
        self.stack += 1;
        if self.stack >= YYMAXDEPTH {
            self.syntax();
            return false;
        }
        true
    }

    /// `php_json_yyerror`: the code a syntax error leaves, which stands
    /// only where the scanner left none.
    fn syntax(&mut self) {
        if self.scanner.errcode == JSON_ERROR_NONE {
            self.scanner.errcode = JSON_ERROR_SYNTAX;
        }
    }

    /// One `value` of the grammar; `None` where the parse was refused
    /// and the code is the scanner's. The brackets the walk is inside
    /// are a stack of its own, as bison keeps its states on the heap, so
    /// a nest costs memory and never the machine's stack; where the
    /// parse is refused, each is given up innermost first, bison's
    /// `%destructor` over what it had built.
    fn value(&mut self, req: &mut Request) -> Result<Option<Owner<Value>>, Thrown> {
        let mut open = Vec::new();
        let parsed = self.walk(req, &mut open);
        while let Some(frame) = open.pop() {
            frame.give_up(req);
        }
        parsed
    }

    /// [`Parser::value`]'s loop: a value begun, then handed to the
    /// bracket it is inside, which asks for the next or closes and is
    /// itself the value handed on.
    fn walk(
        &mut self,
        req: &mut Request,
        open: &mut Vec<Open>,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        'value: loop {
            let mut done = match self.begin(req, open)? {
                Begun::Value(value) => value,
                Begun::Opened => continue 'value,
                Begun::Refused => return Ok(None),
            };
            loop {
                let Some(frame) = open.last_mut() else {
                    return Ok(Some(done));
                };
                match self.attach(req, frame, done)? {
                    Attached::Next => continue 'value,
                    Attached::Close => {}
                    Attached::Refused => return Ok(None),
                }
                let Some(frame) = open.pop() else {
                    return Ok(None);
                };
                let Some(closed) = self.close(frame, req)? else {
                    return Ok(None);
                };
                done = closed;
            }
        }
    }

    /// A value at the lookahead: a scalar shifted and built, or a bracket
    /// opened, `'[' $@2` and `'{' $@1` with the midrule's depth test
    /// between the shift and the goto. An empty pair is the value
    /// whole; one with something inside is left open on `open`, its
    /// first value next. A string token's string was made as it was
    /// scanned ([`Scanner::string`]), and is the value as it stands.
    fn begin(&mut self, req: &mut Request, open: &mut Vec<Open>) -> Result<Begun, Thrown> {
        let token = std::mem::replace(&mut self.token, Token::Eoi);
        let bracket = matches!(token, Token::ArrayOpen | Token::ObjectOpen);
        if matches!(token, Token::Error) {
            return Ok(Begun::Refused);
        }
        let known = bracket
            || matches!(
                token,
                Token::Null
                    | Token::True
                    | Token::False
                    | Token::Int(_)
                    | Token::Double(_)
                    | Token::Str(_)
            );
        if !known {
            self.syntax();
            return Ok(Begun::Refused);
        }
        let at = self.stack;
        if !self.push() || bracket && (!self.deeper() || !self.push()) {
            token.give_up(req);
            return Ok(Begun::Refused);
        }
        let built = match token {
            Token::True => Owner::bool(true),
            Token::False => Owner::bool(false),
            Token::Int(n) => Owner::int(n),
            Token::Double(d) => Owner::float(d),
            Token::Str(Text::Made(string)) => string.into_value(),
            Token::ArrayOpen => {
                self.advance(req)?;
                // The array is made where its rule reduces, at its first
                // element ([`Parser::attach`]) or as the empty pair.
                let frame = Open::Array { at, array: None };
                return self.opened(req, frame, open);
            }
            Token::ObjectOpen => {
                self.advance(req)?;
                let frame = Open::Object {
                    at,
                    holder: Holder::Nothing,
                    key: None,
                };
                return self.opened(req, frame, open);
            }
            _ => Owner::null(),
        };
        if let Err(thrown) = self.advance(req) {
            built.give_up(req);
            return Err(thrown);
        }
        Ok(Begun::Value(built))
    }

    /// A bracket just opened: `elements: %empty` or `members: %empty`
    /// where the next token closes it, and the pair closed; else the
    /// first member's key read and the bracket left open. Under the
    /// default methods an empty pair that decodes to an array is
    /// `ZVAL_EMPTY_ARRAY`, the immutable empty array, which costs
    /// nothing, and `{}` decoding to an object is `object_create`'s
    /// `stdClass`; the validating methods build `null` for either.
    /// An array with elements is made at the first, `element: value`'s
    /// `array_create`, after that value is built:
    ///
    /// ```php
    /// foreach (['[]', '[[]]', '{}', '[{}]', '{"a":[]}', '[[1]]'] as $j) {
    ///     foreach ([true, false] as $assoc) {
    ///         $m = memory_get_usage(); $x = json_decode($j, $assoc); echo $j, ' ', (int) $assoc, ' ', memory_get_usage() - $m, "\n"; $x = null;
    ///     }
    /// }
    /// ```
    /// ```expect
    /// [] 1 32
    /// [] 0 0
    /// [[]] 1 216
    /// [[]] 0 216
    /// {} 1 0
    /// {} 0 40
    /// [{}] 1 216
    /// [{}] 0 256
    /// {"a":[]} 1 408
    /// {"a":[]} 0 448
    /// [[1]] 1 432
    /// [[1]] 0 432
    /// ```
    fn opened(
        &mut self,
        req: &mut Request,
        mut frame: Open,
        open: &mut Vec<Open>,
    ) -> Result<Begun, Thrown> {
        if matches!(self.token, Token::ArrayClose | Token::ObjectClose) {
            if !self.push() {
                frame.give_up(req);
                return Ok(Begun::Refused);
            }
            let as_array = !self.validating
                && match &frame {
                    Open::Array { .. } => true,
                    Open::Object { .. } => self.scanner.options & JSON_OBJECT_AS_ARRAY != 0,
                };
            match &mut frame {
                Open::Array { array, .. } if as_array => {
                    *array = Some(Owner::take(req, PhpArray::empty()));
                }
                Open::Object { holder, .. } if as_array => {
                    *holder = Holder::Array(Owner::take(req, PhpArray::empty()));
                }
                Open::Object { holder, .. } => match self.holder(req) {
                    Ok(Some(made)) => *holder = made,
                    made => {
                        frame.give_up(req);
                        return made.map(|_| Begun::Refused);
                    }
                },
                Open::Array { .. } => {}
            }
            return Ok(self.close(frame, req)?.map_or(Begun::Refused, Begun::Value));
        }
        if let Open::Object { key, .. } = &mut frame {
            match self.key(req) {
                Ok(Some(read)) => *key = Some(read),
                Ok(None) => {
                    frame.give_up(req);
                    return Ok(Begun::Refused);
                }
                Err(thrown) => {
                    frame.give_up(req);
                    return Err(thrown);
                }
            }
        }
        open.push(frame);
        Ok(Begun::Opened)
    }

    /// `key ':'` of a `member`, both shifted, the key answered as the
    /// scanner made it; `None` where the parse was refused, the key given
    /// up.
    fn key(&mut self, req: &mut Request) -> Result<Option<Text>, Thrown> {
        let key = match std::mem::replace(&mut self.token, Token::Eoi) {
            Token::Str(key) => key,
            other => {
                self.token = other;
                self.syntax();
                return Ok(None);
            }
        };
        match self.colon(req) {
            Ok(true) => Ok(Some(key)),
            shifted => {
                key.give_up(req);
                shifted.map(|_| None)
            }
        }
    }

    /// The key shifted and then `':'`, and the token past it read;
    /// `false` where the parse was refused.
    fn colon(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        if !self.push() {
            return Ok(false);
        }
        self.advance(req)?;
        if !matches!(self.token, Token::Colon) {
            self.syntax();
            return Ok(false);
        }
        if !self.push() {
            return Ok(false);
        }
        self.advance(req)?;
        Ok(true)
    }

    /// `value` handed to the bracket it closes: appended as an `element`,
    /// or written under its key by `php_json_parser_object_update` as a
    /// `member`, the holder built at the first; then the next after a
    /// comma, or the bracket to close. A member's key was made as it was
    /// scanned, before its value ([`Scanner::string`]), so the string
    /// made second is the one the memory limit refuses, the value's here:
    ///
    /// ```php
    /// $json = '{"' . str_repeat('k', 6 << 20) . '":"' . str_repeat('v', 10 << 20) . '"}';
    /// ini_set('memory_limit', '30M');
    /// var_dump(json_decode($json, true));
    /// ```
    /// ```expect exit=255
    /// Fatal error: Allowed memory size of 31457280 bytes exhausted (tried to allocate 10502144 bytes) in %s on line 4
    /// ```
    fn attach(
        &mut self,
        req: &mut Request,
        frame: &mut Open,
        value: Owner<Value>,
    ) -> Result<Attached, Thrown> {
        let at = match frame {
            Open::Array { at, array } => {
                if array.is_none() && !self.validating {
                    // `element: value`'s `array_create`, `array_finit`
                    // since 8.5.12: `YYERROR` where the memory limit
                    // refused it.
                    match req.memory_mut().fallible(PhpArray::new) {
                        Ok(Some(made)) => *array = Some(made),
                        Ok(None) => {
                            value.give_up(req);
                            self.syntax();
                            return Ok(Attached::Refused);
                        }
                        Err(thrown) => {
                            value.give_up(req);
                            return Err(thrown);
                        }
                    }
                }
                match array.as_mut() {
                    // `zend_hash_next_index_finsert` since 8.5.12:
                    // `YYERROR` where the memory limit refused the table's
                    // growth.
                    Some(array) => {
                        if req
                            .memory_mut()
                            .fallible(|req| array.append(req, value))?
                            .is_none()
                        {
                            self.syntax();
                            return Ok(Attached::Refused);
                        }
                    }
                    None => value.give_up(req),
                }
                *at
            }
            Open::Object { at, holder, key } => {
                if matches!(holder, Holder::Nothing) {
                    *holder = match self.holder(req) {
                        Ok(Some(holder)) => holder,
                        Ok(None) => {
                            value.give_up(req);
                            return Ok(Attached::Refused);
                        }
                        Err(thrown) => {
                            value.give_up(req);
                            return Err(thrown);
                        }
                    };
                }
                match (holder, key.take()) {
                    (Holder::Array(array), Some(Text::Made(name))) => {
                        let name = Key::from_string(req, name);
                        // `zend_symtable_fupdate` since 8.5.12.
                        if req
                            .memory_mut()
                            .fallible(|req| array.set(req, name, value))?
                            .is_none()
                        {
                            self.syntax();
                            return Ok(Attached::Refused);
                        }
                    }
                    (Holder::Object(object), Some(Text::Made(name))) => {
                        if name.as_bytes().first() == Some(&0) {
                            // A name php mangles a private or protected
                            // property into; `zend_std_write_property`
                            // may not be handed one.
                            self.scanner.errcode = JSON_ERROR_INVALID_PROPERTY_NAME;
                            name.give_up(req);
                            value.give_up(req);
                            return Ok(Attached::Refused);
                        }
                        object.set_prop(req, name, value)?;
                        // `FAILURE` once the memory limit was met, since
                        // 8.5.12.
                        if req.memory().has_oom() {
                            self.syntax();
                            return Ok(Attached::Refused);
                        }
                    }
                    // Nothing built: under the validating methods, or with
                    // no `stdClass`.
                    (Holder::Nothing, Some(name)) | (_, Some(name @ Text::GivenUp)) => {
                        name.give_up(req);
                        value.give_up(req);
                    }
                    (_, None) => {
                        value.give_up(req);
                        return Err(fatal(
                            req,
                            "InternalError: a JSON member's value came with no key",
                        ));
                    }
                }
                *at
            }
        };
        // `element` or `member`, reduced over the one before the comma.
        self.stack = at + 3;
        if !matches!(self.token, Token::Comma) {
            return Ok(Attached::Close);
        }
        if !self.push() {
            return Ok(Attached::Refused);
        }
        self.advance(req)?;
        if let Open::Object { key, .. } = frame {
            let Some(read) = self.key(req)? else {
                return Ok(Attached::Refused);
            };
            *key = Some(read);
        }
        Ok(Attached::Next)
    }

    /// `array_end` or `object_end` shifted and the rule reduced to the
    /// one `value` it is: the other bracket is the state mismatch and
    /// anything else a syntax error, the bracket given up on either.
    fn close(&mut self, frame: Open, req: &mut Request) -> Result<Option<Owner<Value>>, Thrown> {
        let (at, closes, other) = match &frame {
            Open::Array { at, .. } => (*at, Token::ArrayClose, Token::ObjectClose),
            Open::Object { at, .. } => (*at, Token::ObjectClose, Token::ArrayClose),
        };
        let token = std::mem::discriminant(&self.token);
        let refused = if token == std::mem::discriminant(&closes) {
            !self.push()
        } else if token == std::mem::discriminant(&other) {
            if self.push() {
                self.scanner.errcode = JSON_ERROR_STATE_MISMATCH;
            }
            true
        } else {
            self.syntax();
            true
        };
        if refused {
            frame.give_up(req);
            return Ok(None);
        }
        let value = frame.into_value(req);
        if let Err(thrown) = self.advance(req) {
            value.give_up(req);
            return Err(thrown);
        }
        self.depth -= 1;
        self.stack = at + 1;
        Ok(Some(value))
    }

    /// `PHP_JSON_DEPTH_INC`: one level deeper, or the depth refusal.
    fn deeper(&mut self) -> bool {
        if self.max_depth != 0 && self.depth >= self.max_depth {
            self.scanner.errcode = JSON_ERROR_DEPTH;
            return false;
        }
        self.depth += 1;
        true
    }

    /// `php_json_parser_object_create`: the `stdClass` an object decodes
    /// to, its array under `JSON_OBJECT_AS_ARRAY`, and nothing while
    /// validating; both asked for fallibly since 8.5.12 (`array_finit`,
    /// `object_finit`), `None` with the syntax error where the memory
    /// limit refused it, php's `YYERROR`.
    fn holder(&mut self, req: &mut Request) -> Result<Option<Holder>, Thrown> {
        if self.validating {
            return Ok(Some(Holder::Nothing));
        }
        let made = if self.scanner.options & JSON_OBJECT_AS_ARRAY != 0 {
            req.memory_mut().fallible(PhpArray::new)?.map(Holder::Array)
        } else {
            let Some(class) = call::lookup_class(req, b"stdClass")? else {
                return Ok(Some(Holder::Nothing));
            };
            req.memory_mut()
                .fallible(|req| call::new_object(req, class))?
                .map(Holder::Object)
        };
        if made.is_none() {
            self.syntax();
        }
        Ok(made)
    }
}

impl Holder {
    /// What the holder became, `null` where nothing was built.
    fn into_value(self) -> Owner<Value> {
        match self {
            Holder::Object(object) => object.into_value(),
            Holder::Array(array) => array.into_value(),
            Holder::Nothing => Owner::null(),
        }
    }

    /// The owner given up where the parse was refused past the create,
    /// which is `%destructor` over the rule's value.
    fn give_up(self, req: &mut Request) {
        match self {
            Holder::Object(object) => object.give_up(req),
            Holder::Array(array) => array.give_up(req),
            Holder::Nothing => {}
        }
    }
}

/// `php_json_yyparse` over `start: value PHP_JSON_T_EOI`: the value, or
/// the error code the refusal left.
fn parse(
    req: &mut Request,
    text: &[u8],
    options: i64,
    depth: i64,
    validating: bool,
) -> Result<Result<Owner<Value>, i64>, Thrown> {
    let mut parser = Parser {
        scanner: Scanner {
            input: text,
            cursor: 0,
            options,
            errcode: JSON_ERROR_NONE,
        },
        token: Token::Eoi,
        depth: 1,
        max_depth: depth,
        stack: 1,
        validating,
    };
    let parsed = parser.advance(req).and_then(|()| parser.value(req));
    // The lookahead the parse stopped at, discarded as bison's cleanup
    // discards it.
    let lookahead = std::mem::replace(&mut parser.token, Token::Eoi);
    let ended = matches!(lookahead, Token::Eoi);
    lookahead.give_up(req);
    match parsed? {
        Some(value) if ended => Ok(Ok(value)),
        Some(value) => {
            parser.syntax();
            value.give_up(req);
            Ok(Err(parser.scanner.errcode))
        }
        None => {
            parser.syntax();
            Ok(Err(parser.scanner.errcode))
        }
    }
}

/// `php_json_decode_ex`: the value the text denotes, or the error code
/// `json_last_error` answers and `JSON_THROW_ON_ERROR` throws.
pub(super) fn decode(
    req: &mut Request,
    text: &[u8],
    options: i64,
    depth: i64,
) -> Result<Result<Owner<Value>, i64>, Thrown> {
    parse(req, text, options, depth, false)
}

/// `php_json_validate_ex`: the scanner and the grammar with the
/// validating methods under them, which build nothing. The scanner
/// still makes each string token's string, fallibly, and `yylex` gives
/// it up before the next token is scanned, so a string past the memory
/// limit is refused and the limit's fatal raised as the call returns,
/// while two strings that fit only one at a time validate:
///
/// ```php
/// ini_set('memory_limit', '18M');
/// $n = 10 * 1024 * 1024; $h = $n >> 1; $json = str_repeat('a', $n);
/// $json[0] = '['; $json[1] = '"'; $json[$h - 1] = '"'; $json[$h] = ','; $json[$h + 1] = '"'; $json[$n - 2] = '"'; $json[$n - 1] = ']';
/// var_dump(json_validate($json), json_last_error_msg());
/// $json = null; $json = str_repeat('a', $n); $json[0] = '"'; $json[$n - 1] = '"';
/// var_dump(json_validate($json), "unreached");
/// ```
/// ```expect exit=255
/// bool(true)
/// string(8) "No error"
///
/// Fatal error: Allowed memory size of 18874368 bytes exhausted (tried to allocate 10502144 bytes) in %s on line 7
/// ```
pub(super) fn validate(
    req: &mut Request,
    text: &[u8],
    options: i64,
    depth: i64,
) -> Result<Result<Owner<Value>, i64>, Thrown> {
    parse(req, text, options, depth, true)
}

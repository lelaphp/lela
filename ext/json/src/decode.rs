//! `php_json_parser`: `json_scanner.re` and `json_parser.y` at the tag,
//! the scanner's conditions as one walk over the bytes and bison's
//! grammar as the descent below it, since the grammar is `start: value
//! EOI` over two bracketed forms and reads the same language either way.
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
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpObject, PhpString, Value};

use crate::{
    BIGINT_AS_STRING, ERROR_CTRL_CHAR, ERROR_DEPTH, ERROR_INVALID_PROPERTY_NAME, ERROR_NONE,
    ERROR_STATE_MISMATCH, ERROR_SYNTAX, ERROR_UTF8, ERROR_UTF16, INVALID_UTF8_IGNORE,
    INVALID_UTF8_SUBSTITUTE, OBJECT_AS_ARRAY,
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
    /// accepts in the same two places.
    Str(Vec<u8>),
    Eoi,
    /// `PHP_JSON_T_ERROR`: the scanner has written its code.
    Error,
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
    /// skipped, then the longest rule that matches at the cursor.
    fn scan(&mut self) -> Token {
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
            return token;
        }
        if c == b'"' {
            self.cursor += 1;
            return self.scan_string();
        }
        if c == 0 {
            // `<JS>EOI`: the terminator past the last byte is the end of
            // input, and a zero byte within it a control character.
            self.cursor += 1;
            return if self.cursor > self.input.len() {
                Token::Eoi
            } else {
                self.refuse(ERROR_CTRL_CHAR)
            };
        }
        if self.keyword(b"null") {
            return Token::Null;
        }
        if self.keyword(b"true") {
            return Token::True;
        }
        if self.keyword(b"false") {
            return Token::False;
        }
        if let Some(token) = self.scan_number() {
            return token;
        }
        if c <= 0x1f {
            return self.refuse(ERROR_CTRL_CHAR);
        }
        // `<JS>UTF8` is a syntax error and `<JS>ANY`, the byte no UTF-8
        // sequence begins, is the malformed one.
        if utf8_length(self.input, start).is_some() {
            self.cursor += 1;
            return self.refuse(ERROR_SYNTAX);
        }
        self.cursor += 1;
        self.refuse(ERROR_UTF8)
    }

    /// `<JS>INT` and `<JS>FLOAT|EXP`, longest match: the integer alone,
    /// the integer with a fraction, or either with an exponent. `None`
    /// where no number stands at the cursor, which leaves the byte to
    /// the conditions below it.
    fn scan_number(&mut self) -> Option<Token> {
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
            _ => return None,
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
            return Some(Token::Double(strtod(text)));
        }
        Some(self.integer(
            text,
            self.input.get(digits_at..at).unwrap_or_default(),
            digits,
            negative,
        ))
    }

    /// `<JS>INT`'s body: the number as a `zend_long` while it fits,
    /// and as the string itself or a double once it does not, which is
    /// what `JSON_BIGINT_AS_STRING` chooses between.
    fn integer(&self, text: &[u8], digits_of: &[u8], digits: usize, negative: bool) -> Token {
        let bigint = digits > INT_MAX_LENGTH
            || (digits == INT_MAX_LENGTH && {
                let cmp = digits_of.cmp(LONG_MIN_DIGITS);
                !(cmp.is_lt() || (cmp.is_eq() && negative))
            });
        if !bigint {
            return Token::Int(
                std::str::from_utf8(text)
                    .ok()
                    .and_then(|t| t.parse().ok())
                    .unwrap_or(0),
            );
        }
        if self.options & BIGINT_AS_STRING != 0 {
            return Token::Str(text.to_vec());
        }
        Token::Double(strtod(text))
    }

    /// Conditions `STR_P1` and `STR_P2` as one walk: php sizes the
    /// string in the first and writes it in the second, over the same
    /// bytes in the same order, so the first refusal is the same one.
    /// The cursor stands past the opening quote.
    fn scan_string(&mut self) -> Token {
        let mut out: Vec<u8> = Vec::new();
        loop {
            let c = self.byte(self.cursor);
            if c == b'"' {
                self.cursor += 1;
                return Token::Str(out);
            }
            if c <= 0x1f {
                // `<STR_P1>CTRL`, the NUL terminator of an unterminated
                // string among them.
                return self.refuse(ERROR_CTRL_CHAR);
            }
            if c == b'\\' {
                if let Some(code) = self.scan_escape(&mut out) {
                    return self.refuse(code);
                }
                continue;
            }
            if c < 0x80 {
                out.push(c);
                self.cursor += 1;
                continue;
            }
            let Some(len) = utf8_length(self.input, self.cursor) else {
                // `<STR_P1>ANY`: one byte, ignored or replaced by U+FFFD
                // where the options say so.
                if self.options & INVALID_UTF8_SUBSTITUTE != 0 {
                    out.extend_from_slice("\u{fffd}".as_bytes());
                } else if self.options & INVALID_UTF8_IGNORE == 0 {
                    return self.refuse(ERROR_UTF8);
                }
                self.cursor += 1;
                continue;
            };
            let taken = self.input.get(self.cursor..self.cursor + len);
            out.extend_from_slice(taken.unwrap_or_default());
            self.cursor += len;
        }
    }

    /// One escape at the cursor, which stands on the backslash: the
    /// surrogate pair of `UTF16_4` first, since it is the longest, then
    /// the four hex digits of `UTF16_1` through `UCS2`, then the
    /// one-character escapes of `ESC`. The code of the refusal, or
    /// `None` where the escape was written.
    fn scan_escape(&mut self, out: &mut Vec<u8>) -> Option<i64> {
        if let Some(code) = self.surrogate_pair() {
            push_utf8(out, code);
            self.cursor += 12;
            return None;
        }
        if let Some(code) = self.ucs2_at(self.cursor) {
            if (0xd800..=0xdfff).contains(&code) {
                // `<STR_P1>UCS2`, everything `UTF16_1..3` left: one half
                // of a surrogate pair and nothing to pair it with.
                return Some(ERROR_UTF16);
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
            _ => return Some(ERROR_SYNTAX),
        };
        out.push(escaped);
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
fn push_utf8(out: &mut Vec<u8>, code: u32) {
    let mut buffer = [0_u8; 4];
    if let Some(c) = char::from_u32(code) {
        out.extend_from_slice(c.encode_utf8(&mut buffer).as_bytes());
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
    Object(PhpObject),
    Array(PhpArray),
    Nothing,
}

/// `php_json_parser`: the scanner, one token of lookahead, and the
/// depth bison's `PHP_JSON_DEPTH_INC` counts.
struct Parser<'a> {
    scanner: Scanner<'a>,
    token: Token,
    depth: i64,
    max_depth: i64,
    /// `php_json_get_validate_methods`: nothing is built and the
    /// scanner's strings are dropped, which is what `json_validate` runs.
    validating: bool,
}

impl Parser<'_> {
    /// `php_json_yylex`.
    fn advance(&mut self) {
        self.token = self.scanner.scan();
    }

    /// `php_json_yyerror`: the code a syntax error leaves, which stands
    /// only where the scanner left none.
    fn syntax(&mut self) {
        if self.scanner.errcode == ERROR_NONE {
            self.scanner.errcode = ERROR_SYNTAX;
        }
    }

    /// One `value` of the grammar; `None` where the parse was refused
    /// and the code is the scanner's.
    fn value(&mut self, req: &mut Request) -> Result<Option<Value>, Thrown> {
        let token = std::mem::replace(&mut self.token, Token::Eoi);
        let built = match token {
            Token::Null => Value::null(),
            Token::True => Value::bool(true),
            Token::False => Value::bool(false),
            Token::Int(n) => Value::int(n),
            Token::Double(d) => Value::float(d),
            Token::Str(bytes) => {
                if self.validating {
                    Value::null()
                } else {
                    Value::string(PhpString::new(req, &bytes)?)
                }
            }
            Token::ObjectOpen => return self.object(req),
            Token::ArrayOpen => return self.array(req),
            Token::Error => return Ok(None),
            _ => {
                self.syntax();
                return Ok(None);
            }
        };
        self.advance();
        Ok(Some(built))
    }

    /// `PHP_JSON_DEPTH_INC`: one level deeper, or the depth refusal.
    fn deeper(&mut self) -> bool {
        if self.max_depth != 0 && self.depth >= self.max_depth {
            self.scanner.errcode = ERROR_DEPTH;
            return false;
        }
        self.depth += 1;
        true
    }

    /// The `array` rule: `elements` between the bracket and `array_end`,
    /// whose `'}'` is the state mismatch.
    fn array(&mut self, req: &mut Request) -> Result<Option<Value>, Thrown> {
        if !self.deeper() {
            return Ok(None);
        }
        self.advance();
        let mut array = if self.validating {
            None
        } else {
            Some(PhpArray::new(req)?)
        };
        let give_up = |req: &mut Request, array: Option<PhpArray>| {
            if let Some(array) = array {
                req.release(array.to_value());
            }
        };
        if !matches!(self.token, Token::ArrayClose | Token::ObjectClose) {
            loop {
                let Some(element) = self.value(req)? else {
                    give_up(req, array);
                    return Ok(None);
                };
                if let Some(array) = array.as_mut() {
                    array.append(req, element)?;
                }
                if matches!(self.token, Token::Comma) {
                    self.advance();
                    continue;
                }
                break;
            }
        }
        match self.token {
            Token::ArrayClose => self.advance(),
            Token::ObjectClose => {
                self.scanner.errcode = ERROR_STATE_MISMATCH;
                give_up(req, array);
                return Ok(None);
            }
            _ => {
                self.syntax();
                give_up(req, array);
                return Ok(None);
            }
        }
        self.depth -= 1;
        Ok(Some(array.map_or_else(Value::null, PhpArray::to_value)))
    }

    /// The `object` rule: `members` between the brace and `object_end`,
    /// whose `']'` is the state mismatch. The holder is built where
    /// bison reduces `members` or `member`, which is after the first
    /// value, so a nested object is the earlier store entry and prints
    /// the lower number.
    fn object(&mut self, req: &mut Request) -> Result<Option<Value>, Thrown> {
        if !self.deeper() {
            return Ok(None);
        }
        self.advance();
        let mut holder = Holder::Nothing;
        if matches!(self.token, Token::ObjectClose | Token::ArrayClose) {
            holder = self.holder(req)?;
        } else {
            loop {
                if self.member(req, &mut holder)?.is_none() {
                    holder.give_up(req);
                    return Ok(None);
                }
                if matches!(self.token, Token::Comma) {
                    self.advance();
                    continue;
                }
                break;
            }
        }
        match self.token {
            Token::ObjectClose => self.advance(),
            Token::ArrayClose => {
                self.scanner.errcode = ERROR_STATE_MISMATCH;
                holder.give_up(req);
                return Ok(None);
            }
            _ => {
                self.syntax();
                holder.give_up(req);
                return Ok(None);
            }
        }
        self.depth -= 1;
        Ok(Some(holder.into_value()))
    }

    /// `php_json_parser_object_create`: the `stdClass` an object decodes
    /// to, its array under `JSON_OBJECT_AS_ARRAY`, and nothing while
    /// validating.
    fn holder(&mut self, req: &mut Request) -> Result<Holder, Thrown> {
        if self.validating {
            return Ok(Holder::Nothing);
        }
        if self.scanner.options & OBJECT_AS_ARRAY != 0 {
            return Ok(Holder::Array(PhpArray::new(req)?));
        }
        let Some(class) = call::lookup_class(req, b"stdClass")? else {
            return Ok(Holder::Nothing);
        };
        Ok(Holder::Object(call::new_object(req, class)?))
    }

    /// One `key ':' value` of `member`, written into the holder by
    /// `php_json_parser_object_update`.
    fn member(&mut self, req: &mut Request, holder: &mut Holder) -> Result<Option<()>, Thrown> {
        let Token::Str(key) = std::mem::replace(&mut self.token, Token::Eoi) else {
            self.syntax();
            return Ok(None);
        };
        self.advance();
        if !matches!(self.token, Token::Colon) {
            self.syntax();
            return Ok(None);
        }
        self.advance();
        let Some(value) = self.value(req)? else {
            return Ok(None);
        };
        if matches!(holder, Holder::Nothing) {
            *holder = self.holder(req)?;
        }
        match holder {
            Holder::Array(array) => {
                let name = Key::from_string(PhpString::new(req, &key)?);
                array.set(req, name, value)?;
            }
            Holder::Object(object) => {
                if key.first() == Some(&0) {
                    // A name php mangles a private or protected property
                    // into; `zend_std_write_property` may not be handed one.
                    self.scanner.errcode = ERROR_INVALID_PROPERTY_NAME;
                    req.release(value);
                    return Ok(None);
                }
                let name = PhpString::new(req, &key)?;
                object.set_prop(req, name, value)?;
            }
            Holder::Nothing => req.release(value),
        }
        Ok(Some(()))
    }
}

impl Holder {
    /// What the holder became, `null` where nothing was built.
    fn into_value(self) -> Value {
        match self {
            Holder::Object(object) => object.as_value(),
            Holder::Array(array) => array.to_value(),
            Holder::Nothing => Value::null(),
        }
    }

    /// The owner given up where the parse was refused past the create,
    /// which is `%destructor` over the rule's value.
    fn give_up(&self, req: &mut Request) {
        match self {
            Holder::Object(object) => req.release(object.as_value()),
            Holder::Array(array) => req.release(array.to_value()),
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
) -> Result<Result<Value, i64>, Thrown> {
    let mut parser = Parser {
        scanner: Scanner {
            input: text,
            cursor: 0,
            options,
            errcode: ERROR_NONE,
        },
        token: Token::Eoi,
        depth: 1,
        max_depth: depth,
        validating,
    };
    parser.advance();
    let parsed = parser.value(req)?;
    match parsed {
        Some(value) if matches!(parser.token, Token::Eoi) => Ok(Ok(value)),
        Some(value) => {
            parser.syntax();
            req.release(value);
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
) -> Result<Result<Value, i64>, Thrown> {
    parse(req, text, options, depth, false)
}

/// `php_json_validate_ex`: the scanner and the grammar with the
/// validating methods under them, which build nothing.
pub(super) fn validate(
    req: &mut Request,
    text: &[u8],
    options: i64,
    depth: i64,
) -> Result<Result<Value, i64>, Thrown> {
    parse(req, text, options, depth, true)
}

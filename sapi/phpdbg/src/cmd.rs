//! A command line read as phpdbg reads one: its lexer
//! (`Z/sapi/phpdbg/phpdbg_lexer.l`) and its grammar
//! (`phpdbg_parser.y`), which cut the line at each `#` into commands and
//! each command into typed parameters, `ev`, `sh` and `run` taking the
//! rest of their command raw.

use std::fmt::Write as _;

use php_sapi::c_number::atoi;

/// One parameter, `phpdbg_param_t` by its type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parameter {
    /// `STR_PARAM`: a word, a quoted one unescaped.
    Str(Vec<u8>),
    /// `NUMERIC_PARAM`: digits as `atoi` reads them, or a truthy or
    /// falsy word as one or zero.
    Num(i64),
    /// `ADDR_PARAM`: `0x` and hex digits.
    Addr(u64),
    /// `OP_PARAM`: `ZEND_` and a name.
    Op(Vec<u8>),
    /// `FILE_PARAM`: `file:line`.
    File { name: Vec<u8>, line: i64 },
    /// `NUMERIC_FILE_PARAM`: `file:#opline`.
    NumericFile { name: Vec<u8>, line: i64 },
    /// `METHOD_PARAM`: `class::method`.
    Method { class: Vec<u8>, name: Vec<u8> },
    /// `NUMERIC_METHOD_PARAM`: `class::method#opline`.
    NumericMethod {
        class: Vec<u8>,
        name: Vec<u8>,
        num: i64,
    },
    /// `NUMERIC_FUNCTION_PARAM`: `function#opline`.
    NumericFunction { name: Vec<u8>, num: i64 },
    /// `COND_PARAM`: `if` and the raw rest.
    Cond(Vec<u8>),
}

impl Parameter {
    /// `phpdbg_get_param_type`.
    pub fn kind(&self) -> &'static str {
        match self {
            Parameter::Str(_) => "string",
            Parameter::Num(_) => "numeric",
            Parameter::Addr(_) => "address",
            Parameter::Method { .. } => "method",
            Parameter::NumericFunction { .. } => "function opline",
            Parameter::NumericMethod { .. } => "method opline",
            Parameter::File { .. } => "file or file opline",
            Parameter::Op(_) | Parameter::NumericFile { .. } | Parameter::Cond(_) => "unknown",
        }
    }

    /// `phpdbg_param_tostring`.
    pub fn text(&self) -> Vec<u8> {
        let mut text = String::new();
        let lossy = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
        match self {
            Parameter::Str(s) | Parameter::Op(s) | Parameter::Cond(s) => {
                write!(text, "{}", lossy(s))
            }
            Parameter::Addr(addr) => write!(text, "{addr}"),
            Parameter::Num(num) => write!(text, "{num}"),
            Parameter::Method { class, name } => write!(text, "{}::{}", lossy(class), lossy(name)),
            Parameter::File { name, line } | Parameter::NumericFile { name, line } => {
                write!(text, "{}:{line}", lossy(name))
            }
            Parameter::NumericFunction { name, num } => write!(text, "{}#{num}", lossy(name)),
            Parameter::NumericMethod { class, name, num } => {
                write!(text, "{}::{}#{num}", lossy(class), lossy(name))
            }
        }
        .ok();
        text.into_bytes()
    }
}

/// One command of a line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// A command by name and its parameters: the first is the name.
    Params(Vec<Parameter>),
    /// `ev` and the raw code after it.
    Eval(Vec<u8>),
    /// `sh` and the raw line after it.
    Shell(Vec<u8>),
    /// `run` or `r` and what followed, raw, where something did.
    Serve(Option<Vec<u8>>),
}

/// The lexer's tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Separator,
    Colon,
    DColon,
    Pound,
    Proto(Vec<u8>),
    Id(Vec<u8>),
    Digits(i64),
    Truthy(i64),
    Addr(u64),
    Opcode(Vec<u8>),
    If,
    Input(Vec<u8>),
    Eval,
    Shell,
    Run,
    ReqId,
}

/// `phpdbg_lexer.l`'s conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Initial,
    Normal,
    PreRaw,
    Raw,
}

fn is_ws(b: u8) -> bool {
    matches!(b, b' ' | b'\r' | b'\t')
}

fn is_end(b: Option<&u8>) -> bool {
    matches!(b, None | Some(b'\n' | 0))
}

/// The line cut into tokens, each quoted word unquoted in the line
/// where it stands, as `unescape_string` leaves php's input buffer.
fn tokens(line: &mut [u8]) -> Vec<Token> {
    let mut out = Vec::new();
    let mut state = State::Initial;
    let mut at = 0;
    while !is_end(line.get(at)) {
        let rest = &line[at..];
        let Some(&c) = rest.first() else { break };
        if is_ws(c) {
            at += rest.iter().take_while(|b| is_ws(**b)).count();
            continue;
        }
        if c == b'#' && !(state == State::Normal && digits_len(&rest[1..]) > 0) {
            out.push(Token::Separator);
            state = State::Initial;
            at += 1;
            continue;
        }
        if matches!(state, State::PreRaw | State::Normal)
            && let Some(len) = req_id_len(rest)
        {
            out.push(Token::ReqId);
            at += len;
            continue;
        }
        match state {
            State::Initial => {
                if let Some((token, len)) = keyword(rest) {
                    out.push(token);
                    at += len;
                    state = State::PreRaw;
                } else {
                    state = State::Normal;
                }
            }
            State::PreRaw => state = State::Raw,
            State::Raw => {
                let len = input_len(rest);
                if len == 0 {
                    break;
                }
                out.push(Token::Input(rest[..len].to_vec()));
                at += len;
            }
            State::Normal => {
                let (token, len, raw) = normal(rest);
                if len == 0 {
                    break;
                }
                if len > 1 && matches!(c, b'"' | b'\'') {
                    unescape_string(&mut line[at..at + len]);
                }
                out.push(token);
                at += len;
                if raw {
                    state = State::Raw;
                }
            }
        }
    }
    out
}

/// `ev`, `sh`, `run` or `r` and blanks at the start of a command.
fn keyword(rest: &[u8]) -> Option<(Token, usize)> {
    for (word, token) in [
        (&b"ev"[..], Token::Eval),
        (b"sh", Token::Shell),
        (b"run", Token::Run),
        (b"r", Token::Run),
    ] {
        if rest.starts_with(word) && rest.get(word.len()).copied().is_some_and(is_ws) {
            let blanks = rest[word.len()..].iter().take_while(|b| is_ws(**b)).count();
            return Some((token, word.len() + blanks));
        }
    }
    None
}

/// `"-r"{WS}?{DIGITS}`.
fn req_id_len(rest: &[u8]) -> Option<usize> {
    let after = rest.strip_prefix(b"-r")?;
    let blanks = after.iter().take_while(|b| is_ws(**b)).count();
    let digits = digits_len(&after[blanks..]);
    (digits > 0).then_some(2 + blanks + digits)
}

/// `{DIGITS}`, `[-]?[0-9.]+`.
fn digits_len(rest: &[u8]) -> usize {
    let sign = usize::from(rest.first() == Some(&b'-'));
    let body = rest[sign..]
        .iter()
        .take_while(|b| b.is_ascii_digit() || **b == b'.')
        .count();
    if body == 0 { 0 } else { sign + body }
}

/// `{GENERIC_ID}`'s first alternative, `([^ \r\n\t:#\000"']|":\\")+`.
fn plain_id_len(rest: &[u8]) -> usize {
    let mut at = 0;
    loop {
        match rest.get(at) {
            Some(b':') if rest.get(at + 1) == Some(&b'\\') => at += 2,
            Some(b)
                if !matches!(
                    b,
                    b' ' | b'\r' | b'\n' | b'\t' | b':' | b'#' | 0 | b'"' | b'\''
                ) =>
            {
                at += 1;
            }
            _ => return at,
        }
    }
}

/// `unescape_string`: a quoted word's bytes moved down over its opening
/// quote in place, escapes taken, up to its closing quote; the bytes
/// past what moved are left as they were, so `"cmd"` reads `cmdd"`.
fn unescape_string(word: &mut [u8]) {
    let Some(&start) = word.first() else {
        return;
    };
    let (mut at, mut off) = (0, 1);
    loop {
        if word.get(at + off) == Some(&b'\\') {
            off += 1;
        }
        let Some(&b) = word.get(at + off) else {
            return;
        };
        word[at] = b;
        at += 1;
        if word.get(at + off).is_none_or(|b| *b == start) {
            return;
        }
    }
}

/// `{GENERIC_ID}`'s quoted alternatives: the length, and the text
/// unescaped as `unescape_string` does.
fn quoted(rest: &[u8]) -> Option<(usize, Vec<u8>)> {
    let quote = *rest.first()?;
    if quote != b'"' && quote != b'\'' {
        return None;
    }
    let mut text = Vec::new();
    let mut at = 1;
    loop {
        match rest.get(at) {
            Some(b'\\')
                if matches!(rest.get(at + 1), Some(b'\\')) || rest.get(at + 1) == Some(&quote) =>
            {
                text.push(rest[at + 1]);
                at += 2;
            }
            None | Some(b'\n' | 0 | b'\\') => return None,
            Some(b) if *b == quote => {
                return (at > 1).then_some((at + 1, text));
            }
            Some(b) => {
                text.push(*b);
                at += 1;
            }
        }
    }
}

/// One token under `NORMAL`, the longest rule winning and the first of
/// equals: the token, its length, and whether it opens `RAW`.
fn normal(rest: &[u8]) -> (Token, usize, bool) {
    if rest.starts_with(b"if") && rest.get(2).copied().is_some_and(is_ws) {
        let blanks = rest[2..].iter().take_while(|b| is_ws(**b)).count();
        return (Token::If, 2 + blanks, true);
    }
    if rest.first() == Some(&b'#') {
        return (Token::Pound, 1, false);
    }
    if rest.starts_with(b"::") {
        return (Token::DColon, 2, false);
    }
    if rest.first() == Some(&b':') && rest.get(1) != Some(&b'\\') {
        return (Token::Colon, 1, false);
    }
    let plain = plain_id_len(rest);
    let id = rest
        .iter()
        .take_while(|b| !matches!(b, b' ' | b'\r' | b'\n' | b'\t' | b':' | b'#' | 0))
        .count();
    if id > 0 && rest[id..].starts_with(b"://") {
        return (Token::Proto(rest[..id + 3].to_vec()), id + 3, false);
    }
    let word_end = |len: usize| {
        rest.get(len)
            .is_none_or(|b| matches!(b, b' ' | b'\r' | b'\t' | b'\n' | 0))
    };
    for (word, value) in [
        (&b"yes"[..], 1),
        (b"on", 1),
        (b"enabled", 1),
        (b"true", 1),
        (b"no", 0),
        (b"off", 0),
        (b"disabled", 0),
        (b"false", 0),
    ] {
        if rest.starts_with(word) && word_end(word.len()) && plain <= word.len() {
            return (Token::Truthy(value), word.len(), false);
        }
    }
    let quoted = quoted(rest);
    let generic = quoted.as_ref().map_or(plain, |(len, _)| (*len).max(plain));
    let digits = digits_len(rest);
    let addr = if rest.starts_with(b"0x") {
        let hex = rest[2..]
            .iter()
            .take_while(|b| b.is_ascii_hexdigit())
            .count();
        if hex > 0 { 2 + hex } else { 0 }
    } else {
        0
    };
    let opcode = if rest.starts_with(b"ZEND_") || rest.starts_with(b"zend_") {
        let name = rest[5..]
            .iter()
            .take_while(|b| b.is_ascii_alphabetic() || **b == b'_')
            .count();
        if name > 0 { 5 + name } else { 0 }
    } else {
        0
    };
    let longest = digits.max(addr).max(opcode).max(generic);
    if longest == 0 {
        // `["']`: a quote that closes nothing is a word of its own.
        return match rest.first() {
            Some(q @ (b'"' | b'\'')) => (Token::Id(vec![*q]), 1, false),
            _ => (Token::Id(Vec::new()), 0, false),
        };
    }
    if digits == longest {
        // `yylval->num = atoi(yytext)`: the `int` C's `atoi` reads.
        return (
            Token::Digits(i64::from(atoi(&rest[..digits]))),
            digits,
            false,
        );
    }
    if addr == longest {
        let value = u64::from_str_radix(&String::from_utf8_lossy(&rest[2..addr]), 16).unwrap_or(0);
        return (Token::Addr(value), addr, false);
    }
    if opcode == longest {
        return (Token::Opcode(rest[..opcode].to_vec()), opcode, false);
    }
    match quoted {
        Some((len, text)) if len >= plain => (Token::Id(text), len, false),
        _ => (Token::Id(rest[..plain].to_vec()), plain, false),
    }
}

/// `{INPUT}`: escapes, quoted runs and anything but a `#`.
fn input_len(rest: &[u8]) -> usize {
    let mut at = 0;
    loop {
        match rest.get(at) {
            None | Some(b'\n' | 0 | b'#') => return at,
            Some(b'\\') if matches!(rest.get(at + 1), Some(b'#' | b'"' | b'\'')) => at += 2,
            Some(q @ (b'"' | b'\'')) => {
                let quote = *q;
                let mut end = at + 1;
                let closed = loop {
                    match rest.get(end) {
                        None | Some(b'\n' | 0) => break false,
                        Some(b'\\')
                            if matches!(rest.get(end + 1), Some(b'\\'))
                                || rest.get(end + 1) == Some(&quote) =>
                        {
                            end += 2;
                        }
                        Some(b) if *b == quote => break true,
                        Some(_) => end += 1,
                    }
                };
                if !closed {
                    return at;
                }
                at = end + 1;
            }
            Some(_) => at += 1,
        }
    }
}

/// Why a line did not parse: bison's message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Malformed(pub String);

/// The line as its commands, in order; the line is left as php's lexer
/// leaves its input, each quoted word unquoted where it stands.
pub fn parse(line: &mut [u8]) -> Result<Vec<Command>, Malformed> {
    let tokens = tokens(line);
    let mut commands = Vec::new();
    for part in tokens.split(|token| *token == Token::Separator) {
        if part.is_empty() {
            continue;
        }
        commands.push(command(part)?);
    }
    Ok(commands)
}

fn unexpected(token: &Token) -> Malformed {
    let name = match token {
        Token::Separator => "# (command separator)",
        Token::Colon => ": (colon)",
        Token::DColon => ":: (double colon)",
        Token::Pound => "# (pound sign followed by digits)",
        Token::Proto(_) => "protocol (file://)",
        Token::Id(_) => "identifier (command or function name)",
        Token::Digits(_) => "digits (numbers)",
        Token::Truthy(_) => {
            "truthy or falsy (true, on, yes or enabled / false, off, no or disabled)"
        }
        Token::Addr(_) => "address",
        Token::Opcode(_) => "opcode",
        Token::If => "if (condition)",
        Token::Input(_) => "input (input string or data)",
        Token::Eval => "eval",
        Token::Shell => "shell",
        Token::Run => "run",
        Token::ReqId => "request id (-r %d)",
    };
    Malformed(format!("syntax error, unexpected {name}"))
}

/// One command's tokens: a `full_expression`, `ev`, `sh` or `run` with
/// its raw rest, or `parameters`.
fn command(tokens: &[Token]) -> Result<Command, Malformed> {
    let tokens: Vec<&Token> = tokens.iter().filter(|t| **t != Token::ReqId).collect();
    match tokens.first() {
        Some(Token::Eval | Token::Shell) => {
            let raw = match tokens.get(1) {
                Some(Token::Input(raw)) => raw.clone(),
                Some(other) => return Err(unexpected(other)),
                None => {
                    return Err(Malformed(String::from(
                        "syntax error, unexpected end of input",
                    )));
                }
            };
            if let Some(extra) = tokens.get(2) {
                return Err(unexpected(extra));
            }
            Ok(if tokens[0] == &Token::Eval {
                Command::Eval(raw)
            } else {
                Command::Shell(raw)
            })
        }
        Some(Token::Run) => match tokens.get(1) {
            None => Ok(Command::Serve(None)),
            Some(Token::Input(raw)) if tokens.len() == 2 => Ok(Command::Serve(Some(raw.clone()))),
            Some(other) => Err(unexpected(other)),
        },
        _ => parameters(&tokens).map(Command::Params),
    }
}

/// `parameters`: each `parameter` of the grammar in turn, a `T_PROTO`
/// read as the start of the file name after it, as the grammar joins
/// the two.
fn parameters(tokens: &[&Token]) -> Result<Vec<Parameter>, Malformed> {
    let mut joined: Vec<Token> = Vec::with_capacity(tokens.len());
    let mut rest = tokens.iter();
    while let Some(token) = rest.next() {
        match token {
            Token::Proto(proto) => match rest.next() {
                Some(Token::Id(name)) => joined.push(Token::Id([proto.as_slice(), name].concat())),
                Some(other) => return Err(unexpected(other)),
                None => return Err(unexpected(token)),
            },
            other => joined.push((*other).clone()),
        }
    }
    let tokens: Vec<&Token> = joined.iter().collect();
    let mut params = Vec::new();
    let mut at = 0;
    while let Some(token) = tokens.get(at) {
        let next = |n: usize| tokens.get(at + n).copied();
        let param = match (token, next(1), next(2), next(3), next(4)) {
            (Token::Id(name), Some(Token::Colon), Some(Token::Digits(line)), _, _) => {
                at += 3;
                Parameter::File {
                    name: name.clone(),
                    line: *line,
                }
            }
            (
                Token::Id(name),
                Some(Token::Colon),
                Some(Token::Pound),
                Some(Token::Digits(line)),
                _,
            ) => {
                at += 4;
                Parameter::NumericFile {
                    name: name.clone(),
                    line: *line,
                }
            }
            (
                Token::Id(class),
                Some(Token::DColon),
                Some(Token::Id(name)),
                Some(Token::Pound),
                Some(Token::Digits(num)),
            ) => {
                at += 5;
                Parameter::NumericMethod {
                    class: class.clone(),
                    name: name.clone(),
                    num: *num,
                }
            }
            (Token::Id(class), Some(Token::DColon), Some(Token::Id(name)), _, _) => {
                at += 3;
                Parameter::Method {
                    class: class.clone(),
                    name: name.clone(),
                }
            }
            (Token::Id(name), Some(Token::Pound), Some(Token::Digits(num)), _, _) => {
                at += 3;
                Parameter::NumericFunction {
                    name: name.clone(),
                    num: *num,
                }
            }
            (Token::If, Some(Token::Input(raw)), _, _, _) => {
                at += 2;
                Parameter::Cond(raw.clone())
            }
            (Token::Opcode(op), ..) => {
                at += 1;
                Parameter::Op(op.clone())
            }
            (Token::Addr(addr), ..) => {
                at += 1;
                Parameter::Addr(*addr)
            }
            (Token::Truthy(n) | Token::Digits(n), ..) => {
                at += 1;
                Parameter::Num(*n)
            }
            (Token::Id(word), ..) => {
                at += 1;
                Parameter::Str(word.clone())
            }
            (other, ..) => return Err(unexpected(other)),
        };
        params.push(param);
    }
    Ok(params)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(line: &str) -> Vec<Parameter> {
        match parse(&mut line.as_bytes().to_vec()).as_deref() {
            Ok([Command::Params(params), ..]) => params.clone(),
            _ => Vec::new(),
        }
    }

    #[test]
    fn a_line_is_cut_at_each_pound_and_ev_takes_its_rest_raw() {
        assert_eq!(
            parse(&mut b"ev 1 + 3 # ev 2 ** 3#q\n".to_vec()),
            Ok(vec![
                Command::Eval(b"1 + 3 ".to_vec()),
                Command::Eval(b"2 ** 3".to_vec()),
                Command::Params(vec![Parameter::Str(b"q".to_vec())]),
            ])
        );
        assert_eq!(
            parse(&mut b"r\n".to_vec()),
            Ok(vec![Command::Params(vec![Parameter::Str(b"r".to_vec())])])
        );
        assert_eq!(
            parse(&mut b"r echo # quit\n".to_vec()),
            Ok(vec![
                Command::Serve(Some(b"echo ".to_vec())),
                Command::Params(vec![Parameter::Str(b"quit".to_vec())]),
            ])
        );
    }

    #[test]
    fn the_parameters_are_typed_as_phpdbg_types_them() {
        assert_eq!(
            params("b 3"),
            [Parameter::Str(b"b".to_vec()), Parameter::Num(3)]
        );
        assert_eq!(
            params("b breakpoints_002.php:4")[1],
            Parameter::File {
                name: b"breakpoints_002.php".to_vec(),
                line: 4
            }
        );
        assert_eq!(
            params("b \\Foo\\Bar::Foo")[1],
            Parameter::Method {
                class: b"\\Foo\\Bar".to_vec(),
                name: b"Foo".to_vec()
            }
        );
        assert_eq!(
            params("b ZEND_ECHO")[1],
            Parameter::Op(b"ZEND_ECHO".to_vec())
        );
        assert_eq!(
            params("b Foo::bar#0")[1],
            Parameter::NumericMethod {
                class: b"Foo".to_vec(),
                name: b"bar".to_vec(),
                num: 0
            }
        );
        assert_eq!(
            params("b baz#0")[1],
            Parameter::NumericFunction {
                name: b"baz".to_vec(),
                num: 0
            }
        );
        assert_eq!(
            params("w r $b")[1..],
            [
                Parameter::Str(b"r".to_vec()),
                Parameter::Str(b"$b".to_vec())
            ]
        );
        assert_eq!(params("a';")[0], Parameter::Str(b"a".to_vec()));
        assert_eq!(
            params("var_dump \"a\" \"b\"")[1..],
            [Parameter::Str(b"a".to_vec()), Parameter::Str(b"b".to_vec())]
        );
    }
}

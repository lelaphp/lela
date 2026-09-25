//! One native's declaration read off the line `basic_functions.stub.php`
//! writes for it, `strpos(string $haystack, string $needle, int $offset
//! = 0): int|false`, so the declaration model is php's own text and not
//! a second spelling of it. What the line carries is the stub's grammar
//! for a function: parameter types with `?` and `|`, `&` for a
//! by-reference slot, `...` for a variadic one, and a default that is a
//! number, a string, `null`, `true`, `false`, `[]`, a constant, or constants joined by `|`. The native
//! attributes that derive a declaration from the Rust signature replace
//! this once their expansion lands.

use php_ext::decl::{
    Attribute, AttributeArgument, ConstExpr, FunctionDecl, Name, Parameter, ParameterDefault, Type,
};

/// The declaration a stub line writes, with no frameless entries.
///
/// A line the grammar cannot read declares what it read up to there,
/// since a declaration table is built where nothing may raise; each
/// file's `ReflectionFunction` witness is what holds the lines to
/// php's.
pub(super) fn stub(line: &str) -> FunctionDecl {
    let mut reader = Reader {
        bytes: line.as_bytes(),
        at: 0,
    };
    let name = reader.word();
    let mut parameters = Vec::new();
    if reader.eat(b'(') {
        while !reader.eat(b')') && reader.at < reader.bytes.len() {
            if let Some(parameter) = reader.parameter() {
                parameters.push(parameter);
            }
            reader.eat(b',');
        }
    }
    let return_type = if reader.eat(b':') {
        Some(reader.ty())
    } else {
        None
    };
    FunctionDecl {
        name: Name(name.into()),
        parameters,
        return_type,
        returns_by_ref: false,
        is_generator: false,
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
        frameless: Vec::new(),
    }
}

/// The declaration under php's `#[\Deprecated(since: ...)]`, as the
/// stub marks `utf8_encode` and its kin.
pub(super) fn deprecated_since(decl: FunctionDecl, since: &str) -> FunctionDecl {
    FunctionDecl {
        attributes: vec![Attribute {
            name: Name(b"Deprecated".as_slice().into()),
            arguments: vec![
                AttributeArgument {
                    name: Some(Name(b"since".as_slice().into())),
                    value: ConstExpr::Str(since.as_bytes().into()),
                },
                AttributeArgument {
                    name: Some(Name(b"message".as_slice().into())),
                    value: ConstExpr::Str(
                        b"visit the php.net documentation for various alternatives"
                            .as_slice()
                            .into(),
                    ),
                },
            ],
        }],
        ..decl
    }
}

/// The cursor over one line.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn skip_space(&mut self) {
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_space();
        self.bytes.get(self.at).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        if self.peek() == Some(byte) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn eat_str(&mut self, text: &[u8]) -> bool {
        self.skip_space();
        if self
            .bytes
            .get(self.at..)
            .is_some_and(|rest| rest.starts_with(text))
        {
            self.at += text.len();
            true
        } else {
            false
        }
    }

    /// A name: letters, digits, `_` and `\`.
    fn word(&mut self) -> Vec<u8> {
        self.skip_space();
        let start = self.at;
        while self
            .bytes
            .get(self.at)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'\\')
        {
            self.at += 1;
        }
        self.bytes[start..self.at].to_vec()
    }

    /// `?T` or `A|B|...`, each a scalar keyword or a class name.
    fn ty(&mut self) -> Type {
        if self.eat(b'?') {
            return Type::Nullable(Box::new(self.one_type()));
        }
        let mut members = vec![self.one_type()];
        while self.eat(b'|') {
            members.push(self.one_type());
        }
        if members.len() == 1 {
            members.pop().unwrap_or(Type::Mixed)
        } else {
            Type::Union(members)
        }
    }

    fn one_type(&mut self) -> Type {
        let word = self.word();
        match word.as_slice() {
            b"mixed" => Type::Mixed,
            b"void" => Type::Void,
            b"never" => Type::Never,
            b"null" => Type::Null,
            b"bool" => Type::Bool,
            b"false" => Type::False,
            b"true" => Type::True,
            b"int" => Type::Int,
            b"float" => Type::Float,
            b"string" => Type::String,
            b"array" => Type::Array,
            b"object" => Type::Object,
            b"iterable" => Type::Iterable,
            b"callable" => Type::Callable,
            _ => Type::Named(Name(word.into())),
        }
    }

    /// `[T] [&] [...]$name [= default]`.
    fn parameter(&mut self) -> Option<Parameter> {
        let ty = match self.peek() {
            Some(b'$' | b'&' | b'.') => None,
            _ => Some(self.ty()),
        };
        let by_ref = self.eat(b'&');
        let variadic = self.eat_str(b"...");
        if !self.eat(b'$') {
            // Nothing this grammar reads: step past the byte so the
            // caller's loop ends.
            self.at += 1;
            return None;
        }
        let name = self.word();
        let default = if self.eat(b'=') {
            match self.default() {
                // `= UNKNOWN`: optional, with no default to show.
                ConstExpr::Constant(named) if named.0.as_ref() == b"UNKNOWN" => {
                    ParameterDefault::Unshown
                }
                expr => ParameterDefault::Expr(expr),
            }
        } else {
            ParameterDefault::None
        };
        Some(Parameter {
            name: Name(name.into()),
            ty,
            by_ref,
            prefer_ref: false,
            promoted: false,
            variadic,
            default,
            attributes: Vec::new(),
        })
    }

    /// A default: one operand, or operands joined by `|`.
    fn default(&mut self) -> ConstExpr {
        let mut expr = self.operand();
        while self.peek() == Some(b'|') {
            self.at += 1;
            let rhs = self.operand();
            expr = ConstExpr::Binary {
                op: Name(b"|".as_slice().into()),
                lhs: Box::new(expr),
                rhs: Box::new(rhs),
            };
        }
        expr
    }

    fn operand(&mut self) -> ConstExpr {
        match self.peek() {
            Some(b'"') => {
                self.at += 1;
                ConstExpr::Str(self.double_quoted().into())
            }
            Some(b'\'') => {
                self.at += 1;
                ConstExpr::Str(self.single_quoted().into())
            }
            Some(b'[') => {
                self.at += 1;
                self.eat(b']');
                ConstExpr::Array(Vec::new())
            }
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => {
                let word = self.word();
                match word.as_slice() {
                    b"null" => ConstExpr::Null,
                    b"true" => ConstExpr::Bool(true),
                    b"false" => ConstExpr::Bool(false),
                    _ => ConstExpr::Constant(Name(word.into())),
                }
            }
        }
    }

    fn number(&mut self) -> ConstExpr {
        let start = self.at;
        if self.bytes.get(self.at) == Some(&b'-') {
            self.at += 1;
        }
        while self
            .bytes
            .get(self.at)
            .is_some_and(|b| b.is_ascii_digit() || *b == b'.' || *b == b'e' || *b == b'E')
        {
            self.at += 1;
        }
        let text = std::str::from_utf8(&self.bytes[start..self.at]).unwrap_or("0");
        if text.contains(['.', 'e', 'E']) {
            ConstExpr::Double(text.parse().unwrap_or(0.0))
        } else {
            ConstExpr::Long(text.parse().unwrap_or(0))
        }
    }

    /// The body of a `"..."`, php's escapes read.
    fn double_quoted(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(&byte) = self.bytes.get(self.at) {
            self.at += 1;
            match byte {
                b'"' => break,
                b'\\' => {
                    let Some(&next) = self.bytes.get(self.at) else {
                        break;
                    };
                    self.at += 1;
                    match next {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'v' => out.push(0x0b),
                        b'f' => out.push(0x0c),
                        b'0' => out.push(0),
                        b'x' => {
                            let hex = self.bytes.get(self.at..self.at + 2).unwrap_or(b"00");
                            let text = std::str::from_utf8(hex).unwrap_or("00");
                            out.push(u8::from_str_radix(text, 16).unwrap_or(0));
                            self.at += 2;
                        }
                        other => out.push(other),
                    }
                }
                other => out.push(other),
            }
        }
        out
    }

    /// The body of a `'...'`: `\\` and `\'` alone are escapes.
    fn single_quoted(&mut self) -> Vec<u8> {
        let mut out = Vec::new();
        while let Some(&byte) = self.bytes.get(self.at) {
            self.at += 1;
            match byte {
                b'\'' => break,
                b'\\' if matches!(self.bytes.get(self.at), Some(b'\\' | b'\'')) => {
                    out.push(self.bytes[self.at]);
                    self.at += 1;
                }
                other => out.push(other),
            }
        }
        out
    }
}

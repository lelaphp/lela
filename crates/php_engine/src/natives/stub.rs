//! One native's declaration read off the line
//! `zend_builtin_functions.stub.php` writes for it, so the declaration
//! model of a Zend native is php's own text: parameter types with `?`
//! and `|`, `&` for a by-reference slot, `...` for a variadic one, and a
//! default that is a number, `null`, `true`, `false`, `[]` or a constant,
//! `UNKNOWN` among them, which is the stub's optional parameter with no
//! default. The grammar is the one `ext/standard` reads its own stub
//! with, cut to what this stub writes.

use php_unit::decl::{ConstExpr, FunctionDecl, Name, Parameter, ParameterDefault, Type};

/// The declaration a stub line writes. A line the grammar cannot read
/// declares what it read up to there, since a declaration table is
/// built where nothing may raise; the witnesses beside the natives are
/// what hold the lines to php's.
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
    let return_type = reader.eat(b':').then(|| reader.ty());
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

/// The cursor over one line.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Reader<'_> {
    fn peek(&mut self) -> Option<u8> {
        while self.bytes.get(self.at).is_some_and(u8::is_ascii_whitespace) {
            self.at += 1;
        }
        self.bytes.get(self.at).copied()
    }

    fn eat(&mut self, byte: u8) -> bool {
        let found = self.peek() == Some(byte);
        if found {
            self.at += 1;
        }
        found
    }

    /// A name: letters, digits, `_` and `\`.
    fn word(&mut self) -> Vec<u8> {
        self.peek();
        let start = self.at;
        while self
            .bytes
            .get(self.at)
            .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'\\')
        {
            self.at += 1;
        }
        self.bytes.get(start..self.at).unwrap_or_default().to_vec()
    }

    /// `?T` or `A|B|...`.
    fn ty(&mut self) -> Type {
        if self.eat(b'?') {
            return Type::Nullable(Box::new(self.one_type()));
        }
        let mut members = vec![self.one_type()];
        while self.eat(b'|') {
            members.push(self.one_type());
        }
        match members.len() {
            1 => members.pop().unwrap_or(Type::Mixed),
            _ => Type::Union(members),
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
        let variadic = self.eat(b'.') && self.eat(b'.') && self.eat(b'.');
        if !self.eat(b'$') {
            // Nothing this grammar reads: step past the byte so the
            // caller's loop ends.
            self.at += 1;
            return None;
        }
        let name = self.word();
        let default = if self.eat(b'=') {
            match self.operand() {
                ConstExpr::Constant(named) if named.0.as_ref() == b"UNKNOWN" => {
                    ParameterDefault::Unshown
                }
                operand => ParameterDefault::Expr(operand),
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

    fn operand(&mut self) -> ConstExpr {
        match self.peek() {
            Some(b'[') => {
                self.at += 1;
                self.eat(b']');
                ConstExpr::Array(Vec::new())
            }
            Some(b'-' | b'0'..=b'9') => {
                let start = self.at;
                self.at += 1;
                while self.bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
                    self.at += 1;
                }
                let text = self.bytes.get(start..self.at).unwrap_or_default();
                ConstExpr::long(
                    std::str::from_utf8(text)
                        .ok()
                        .and_then(|text| text.parse().ok())
                        .unwrap_or(0),
                )
            }
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
}

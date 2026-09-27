//! The natives of `php_spl.stub.php` as declarations, each read off its
//! stub line: the parameter grammar `ext/standard` read its natives'
//! stub lines with, cut to what these lines write.

use php_ext::decl::{ConstExpr, FunctionDecl, Name, Parameter, ParameterDefault, Type};

/// A function's declaration read off one stub line, for the natives of
/// `php_spl.stub.php`.
///
/// ```php
/// $f = new ReflectionFunction("spl_autoload_register");
/// foreach ($f->getParameters() as $p) echo $p, "\n";
/// echo $f->getReturnType(), " ", (new ReflectionFunction("iterator_to_array"))->getNumberOfRequiredParameters(), "\n";
/// ```
/// ```expect
/// Parameter #0 [ <optional> ?callable $callback = null ]
/// Parameter #1 [ <optional> bool $throw = true ]
/// Parameter #2 [ <optional> bool $prepend = false ]
/// bool 1
/// ```
pub(crate) fn function(line: &str) -> FunctionDecl {
    let mut reader = Reader {
        bytes: line.as_bytes(),
        at: 0,
    };
    let name = reader.word();
    reader.signature(name)
}

/// The cursor over a stub's text.
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
        let found = self.peek() == Some(byte);
        if found {
            self.at += 1;
        }
        found
    }

    fn eat_str(&mut self, text: &[u8]) -> bool {
        self.skip_space();
        let found = self
            .bytes
            .get(self.at..)
            .is_some_and(|rest| rest.starts_with(text));
        if found {
            self.at += text.len();
        }
        found
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
        self.bytes.get(start..self.at).unwrap_or_default().to_vec()
    }

    /// `(parameters)[: type]` after a function's name.
    fn signature(&mut self, name: Vec<u8>) -> FunctionDecl {
        let mut parameters = Vec::new();
        if self.eat(b'(') {
            while !self.eat(b')') && self.at < self.bytes.len() {
                if let Some(parameter) = self.parameter() {
                    parameters.push(parameter);
                }
                self.eat(b',');
            }
        }
        let return_type = self.eat(b':').then(|| self.ty());
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
            b"static" => Type::CalledClass,
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
            // No SPL stub marks a parameter `@prefer-ref`.
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
            Some(quote @ (b'"' | b'\'')) => {
                self.at += 1;
                let start = self.at;
                while self.bytes.get(self.at).is_some_and(|b| *b != quote) {
                    self.at += 1;
                }
                let text = self.bytes.get(start..self.at).unwrap_or_default().to_vec();
                self.at += 1;
                ConstExpr::Str(text.into())
            }
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
                let text = std::str::from_utf8(self.bytes.get(start..self.at).unwrap_or_default())
                    .unwrap_or("0");
                ConstExpr::long(text.parse().unwrap_or(0))
            }
            _ => {
                let word = self.word();
                if self.eat_str(b"::") {
                    let member = self.word();
                    return ConstExpr::ClassConstant {
                        class: Name(word.into()),
                        name: Name(member.into()),
                    };
                }
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

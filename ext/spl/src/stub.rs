//! The classes of `ext/spl/*.stub.php` as declarations, read off the
//! stub's own text, so the declaration model is php's and not a second
//! spelling of it: `class`, `abstract class` and `interface` headers
//! with `extends` and `implements`; `public const int NAME = 1;` with
//! the value the `@cvalue` names written where the stub writes
//! `UNKNOWN`; `public ?string $name = null;`; and
//! `public function name(parameters): type {}` with its
//! modifiers, `#[\Deprecated(...)]` before it, and the
//! `/** @tentative-return-type */` docblock that sets the tentative bit.
//! The parameter grammar is the one `ext/standard` reads its natives'
//! stub lines with, cut to what these stubs write.

use php_ext::decl::{
    Attribute, AttributeArgument, ClassConstDecl, ClassDecl, ClassFlags, ClassKind, ConstExpr,
    EnumBacking, FunctionDecl, MethodDecl, Name, Parameter, ParameterDefault, PropertyDecl, Type,
    Visibility,
};

use crate::Entry;

/// One class with its methods' entries, the shape
/// `ExtensionBuilder::classes` takes.
pub(crate) type Declared = (ClassDecl, Vec<(MethodDecl, Entry)>);

/// The classes `text` declares, each method paired with the entry
/// `entries` names for `Class::method`; an abstract method takes none,
/// and a concrete one `entries` does not name is left off, which the
/// crate's own test holds to none.
///
/// ```php
/// $m = new ReflectionMethod("SeekableIterator", "seek");
/// var_dump($m->isAbstract(), $m->hasTentativeReturnType(), (string) $m->getTentativeReturnType(), $m->getParameters()[0]->getName());
/// var_dump((new ReflectionClass("OutOfRangeException"))->getParentClass()->getName(), (new ReflectionClass("RecursiveIterator"))->getInterfaceNames());
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// string(4) "void"
/// string(6) "offset"
/// string(14) "LogicException"
/// array(2) {
///   [0]=>
///   string(8) "Iterator"
///   [1]=>
///   string(11) "Traversable"
/// }
/// ```
pub(crate) fn classes(text: &str, entries: &[(&str, Entry)]) -> Vec<Declared> {
    let mut reader = Reader {
        bytes: text.as_bytes(),
        at: 0,
    };
    let mut out = Vec::new();
    while let Some(class) = reader.class(entries) {
        out.push(class);
    }
    out
}

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

/// What stands before a member: its modifiers, its attributes and
/// whether its docblock marks the return type tentative.
// php's modifier bits and the docblock's mark, one each.
#[allow(clippy::struct_excessive_bools)]
#[derive(Default)]
struct Modifiers {
    visibility: Option<Visibility>,
    is_static: bool,
    is_abstract: bool,
    is_final: bool,
    tentative: bool,
    attributes: Vec<Attribute>,
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

    fn names(&mut self) -> Vec<Name> {
        let mut names = vec![Name(self.word().into())];
        while self.eat(b',') {
            names.push(Name(self.word().into()));
        }
        names
    }

    /// The `extends` and `implements` of a class header: a class's
    /// parent and interfaces, or the interfaces an interface extends.
    fn heritage(&mut self, kind: ClassKind) -> (Option<Name>, Vec<Name>) {
        let mut parent = None;
        let mut interfaces = Vec::new();
        loop {
            if self.eat_str(b"extends") {
                if kind == ClassKind::Interface {
                    interfaces = self.names();
                } else {
                    parent = Some(Name(self.word().into()));
                }
            } else if self.eat_str(b"implements") {
                interfaces = self.names();
            } else {
                break;
            }
        }
        (parent, interfaces)
    }

    /// One class, from its header to its closing brace.
    fn class(&mut self, entries: &[(&str, Entry)]) -> Option<Declared> {
        let mut flags = ClassFlags::default();
        let kind = loop {
            match self.word().as_slice() {
                b"abstract" => flags.is_abstract = true,
                b"final" => flags.is_final = true,
                b"class" => break ClassKind::Class,
                b"interface" => break ClassKind::Interface,
                _ => return None,
            }
        };
        let name = self.word();
        let (parent, interfaces) = self.heritage(kind);
        self.eat(b'{');
        let mut decl = ClassDecl {
            name: Name(name.clone().into()),
            kind,
            flags,
            parent,
            interfaces,
            traits: Vec::new(),
            adaptations: Vec::new(),
            backing: EnumBacking::Pure,
            constants: Vec::new(),
            properties: Vec::new(),
            methods: Vec::new(),
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            end_line: 0,
        };
        let mut methods = Vec::new();
        while !self.eat(b'}') && self.at < self.bytes.len() {
            let modifiers = self.modifiers();
            if self.eat_str(b"const") {
                let ty = self.ty();
                let constant = self.word();
                self.eat(b'=');
                let value = self.default();
                self.eat(b';');
                decl.constants.push(ClassConstDecl {
                    name: Name(constant.into()),
                    visibility: modifiers.visibility.unwrap_or(Visibility::Public),
                    is_final: modifiers.is_final,
                    is_case: false,
                    ty: Some(ty),
                    value,
                    attributes: Vec::new(),
                    doc_comment: None,
                });
                continue;
            }
            if !self.eat_str(b"function") {
                match modifiers.visibility {
                    Some(visibility) => decl.properties.push(self.property(visibility, &modifiers)),
                    None => self.at += 1,
                }
                continue;
            }
            let method = self.word();
            let function = FunctionDecl {
                attributes: modifiers.attributes,
                ..self.signature(method.clone())
            };
            let is_abstract = modifiers.is_abstract || kind == ClassKind::Interface;
            if !self.eat(b';') {
                self.eat(b'{');
                self.eat(b'}');
            }
            let method_decl = MethodDecl {
                function,
                visibility: modifiers.visibility.unwrap_or(Visibility::Public),
                is_instance: !modifiers.is_static,
                is_abstract,
                is_final: modifiers.is_final,
                tentative_return: modifiers.tentative,
            };
            if is_abstract {
                decl.methods.push(method_decl);
                continue;
            }
            let key = [name.as_slice(), b"::", method.as_slice()].concat();
            if let Some((_, entry)) = entries.iter().find(|(k, _)| k.as_bytes() == key) {
                methods.push((method_decl, *entry));
            }
        }
        Some((decl, methods))
    }

    /// The modifiers, docblocks and attributes before a member.
    fn modifiers(&mut self) -> Modifiers {
        let mut modifiers = Modifiers::default();
        loop {
            if self.eat_str(b"/**") {
                let start = self.at;
                while self.at < self.bytes.len() && !self.eat_str(b"*/") {
                    self.at += 1;
                }
                let doc = self.bytes.get(start..self.at).unwrap_or_default();
                if doc
                    .windows(b"@tentative-return-type".len())
                    .any(|w| w == b"@tentative-return-type")
                {
                    modifiers.tentative = true;
                }
                continue;
            }
            if self.eat_str(b"#[") {
                modifiers.attributes.push(self.attribute());
                continue;
            }
            let save = self.at;
            match self.word().as_slice() {
                b"public" => modifiers.visibility = Some(Visibility::Public),
                b"protected" => modifiers.visibility = Some(Visibility::Protected),
                b"private" => modifiers.visibility = Some(Visibility::Private),
                b"static" => modifiers.is_static = true,
                b"abstract" => modifiers.is_abstract = true,
                b"final" => modifiers.is_final = true,
                _ => {
                    self.at = save;
                    return modifiers;
                }
            }
        }
    }

    /// `\Name(label: 'value', ...)]`, after the `#[`.
    fn attribute(&mut self) -> Attribute {
        let name = self.word();
        let name = name.strip_prefix(b"\\").unwrap_or(&name).to_vec();
        let mut arguments = Vec::new();
        if self.eat(b'(') {
            while !self.eat(b')') && self.at < self.bytes.len() {
                let label = self.word();
                self.eat(b':');
                arguments.push(AttributeArgument {
                    name: Some(Name(label.into())),
                    value: self.operand(),
                });
                self.eat(b',');
            }
        }
        self.eat(b']');
        Attribute {
            name: Name(name.into()),
            arguments,
        }
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

    /// `[T] $name [= default];` after a property's modifiers.
    fn property(&mut self, visibility: Visibility, modifiers: &Modifiers) -> PropertyDecl {
        let ty = (self.peek() != Some(b'$')).then(|| self.ty());
        self.eat(b'$');
        let name = self.word();
        let default = self.eat(b'=').then(|| self.default());
        self.eat(b';');
        PropertyDecl {
            name: Name(name.into()),
            visibility,
            set_visibility: None,
            is_instance: !modifiers.is_static,
            is_readonly: false,
            is_final: modifiers.is_final,
            is_promoted: false,
            is_virtual: false,
            ty,
            default,
            attributes: Vec::new(),
            doc_comment: None,
            hooks: None,
        }
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

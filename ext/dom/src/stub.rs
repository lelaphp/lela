//! The classes of `php_dom.stub.php` as declarations, read off the stub's
//! own text, so the declaration model is php's and not a second spelling
//! of it: `class`, `final class`, `abstract class`, `readonly final
//! class`, `interface` and `enum Name: string` headers with `extends` and
//! `implements`, inside a `namespace Name { ... }` block that qualifies
//! every name not written with a leading `\`; `public const int NAME =
//! 0x01;` and `case Name = "value";`; a property with its type and
//! default; and `public function name(parameters): type {}` with its
//! modifiers. Three docblock tags are read: `@tentative-return-type` sets
//! the tentative bit, `@implementation-alias Class::method` names the
//! entry a method shares, and `@strict-properties` on a class forbids
//! dynamic properties. The grammar is `ext/pdo`'s reader's, with
//! hexadecimal constants beside it.

use php_ext::decl::{
    Attribute, AttributeArgument, ClassConstDecl, ClassDecl, ClassFlags, ClassKind, ConstExpr,
    DynamicProperties, EnumBacking, FunctionDecl, MethodDecl, Name, Parameter, PropertyDecl, Type,
    Visibility,
};

use crate::Entry;

/// One class with its methods' entries, the shape
/// `ExtensionBuilder::classes` takes.
pub(crate) type Declared = (ClassDecl, Vec<(MethodDecl, Entry)>);

/// The classes `text` declares, each method paired with the entry
/// `entries` names for `Class::method`; a method `entries` does not name
/// is left off.
///
/// ```php
/// var_dump(DOMNode::DOCUMENT_POSITION_CONTAINED_BY, (new ReflectionMethod("DOMNode", "getLineNo"))->hasTentativeReturnType());
/// echo implode(",", (new ReflectionClass("DOMElement"))->getInterfaceNames()), "\n";
/// echo (new ReflectionMethod("DOMDocument", "__construct"))->getParameters()[0], "\n";
/// ```
/// ```expect
/// int(16)
/// bool(true)
/// DOMParentNode,DOMChildNode
/// Parameter #0 [ <optional> string $version = "1.0" ]
/// ```
pub(super) fn classes(text: &str, entries: &[(&str, Entry)]) -> Vec<Declared> {
    let mut reader = Reader {
        bytes: text.as_bytes(),
        at: 0,
        namespace: Vec::new(),
    };
    let mut out = Vec::new();
    while let Some(class) = reader.class(entries) {
        out.push(class);
    }
    out
}

/// A function's declaration read off one stub line, for the one native
/// of `php_dom.stub.php`'s legacy namespace.
///
/// ```php
/// echo (new ReflectionFunction("dom_import_simplexml"))->getReturnType(), "\n";
/// ```
/// ```expect
/// DOMAttr|DOMElement
/// ```
pub(super) fn function(line: &str) -> FunctionDecl {
    let mut reader = Reader {
        bytes: line.as_bytes(),
        at: 0,
        namespace: Vec::new(),
    };
    let name = reader.word();
    reader.signature(name)
}

/// The cursor over a stub's text, and the namespace block it is in.
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
    namespace: Vec<u8>,
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
    /// `@implementation-alias Class::method`.
    alias: Option<Vec<u8>>,
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

    /// A name: letters, digits, `_` and `\`, a leading `\` dropped.
    fn word(&mut self) -> Vec<u8> {
        self.skip_space();
        if self.bytes.get(self.at) == Some(&b'\\') {
            self.at += 1;
        }
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

    /// A class name as the stub writes it, qualified by the namespace
    /// block unless written with a leading `\`.
    fn class_name(&mut self) -> Vec<u8> {
        let absolute = self.peek() == Some(b'\\');
        let word = self.word();
        self.qualify(word, absolute)
    }

    fn qualify(&self, word: Vec<u8>, absolute: bool) -> Vec<u8> {
        if absolute || self.namespace.is_empty() || word.is_empty() {
            return word;
        }
        [self.namespace.as_slice(), b"\\", word.as_slice()].concat()
    }

    fn names(&mut self) -> Vec<Name> {
        let mut names = vec![Name(self.class_name().into())];
        while self.eat(b',') {
            names.push(Name(self.class_name().into()));
        }
        names
    }

    /// The tags of the docblock at the cursor, where one stands there.
    fn docblock(&mut self) -> Option<Vec<u8>> {
        if !self.eat_str(b"/**") {
            return None;
        }
        let start = self.at;
        while self.at < self.bytes.len() && !self.eat_str(b"*/") {
            self.at += 1;
        }
        Some(self.bytes.get(start..self.at).unwrap_or_default().to_vec())
    }

    /// A class's header up to its opening brace, with the namespace block
    /// it opens or closes on the way: the declaration, its body empty.
    fn header(&mut self) -> Option<ClassDecl> {
        let mut flags = ClassFlags::default();
        let kind = loop {
            if let Some(doc) = self.docblock() {
                if contains(&doc, b"@strict-properties") {
                    flags.dynamic_properties = DynamicProperties::Forbidden;
                }
                continue;
            }
            if self.eat(b'}') {
                self.namespace.clear();
                continue;
            }
            match self.word().as_slice() {
                b"namespace" => {
                    self.namespace = self.word();
                    self.eat(b'{');
                }
                b"abstract" => flags.is_abstract = true,
                b"final" => flags.is_final = true,
                b"readonly" => flags.is_readonly = true,
                b"class" => break ClassKind::Class,
                b"interface" => break ClassKind::Interface,
                b"enum" => break ClassKind::Enum,
                _ => return None,
            }
        };
        let name = self.class_name();
        let mut parent = None;
        let mut interfaces = Vec::new();
        let mut backing = EnumBacking::Pure;
        if kind == ClassKind::Enum {
            // `zend_register_internal_enum`: no dynamic property, and
            // `UnitEnum`, or `BackedEnum`, which brings `UnitEnum` behind
            // it, for a backed one.
            flags.dynamic_properties = DynamicProperties::Forbidden;
            if self.eat(b':') {
                backing = match self.word().as_slice() {
                    b"int" => EnumBacking::Int,
                    _ => EnumBacking::String,
                };
                interfaces.push(Name(b"BackedEnum".as_slice().into()));
            } else {
                interfaces.push(Name(b"UnitEnum".as_slice().into()));
            }
        }
        loop {
            if self.eat_str(b"extends") {
                if kind == ClassKind::Interface {
                    interfaces = self.names();
                } else {
                    parent = Some(Name(self.class_name().into()));
                }
            } else if self.eat_str(b"implements") {
                interfaces.extend(self.names());
            } else {
                break;
            }
        }
        self.eat(b'{');
        Some(ClassDecl {
            name: Name(name.clone().into()),
            kind,
            flags,
            parent,
            interfaces,
            traits: Vec::new(),
            backing,
            constants: Vec::new(),
            properties: enum_properties(kind, backing),
            methods: Vec::new(),
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            end_line: 0,
        })
    }

    /// One class, from its header to its closing brace.
    fn class(&mut self, entries: &[(&str, Entry)]) -> Option<Declared> {
        let mut decl = self.header()?;
        let name = decl.name.0.to_vec();
        let kind = decl.kind;
        let mut methods = Vec::new();
        while !self.eat(b'}') && self.at < self.bytes.len() {
            let modifiers = self.modifiers();
            if self.eat_str(b"const") {
                decl.constants.push(self.constant(&modifiers));
                continue;
            }
            if self.eat_str(b"case ") {
                decl.constants.push(self.case(&decl.name));
                continue;
            }
            if !self.eat_str(b"function") {
                if let Some(mut property) = self.property(&modifiers) {
                    property.is_readonly |= decl.flags.is_readonly;
                    decl.properties.push(property);
                } else {
                    self.at += 1;
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
            let found = entries
                .iter()
                .find(|(k, _)| k.as_bytes() == key)
                .or_else(|| {
                    let alias = modifiers.alias.as_deref()?;
                    entries.iter().find(|(k, _)| k.as_bytes() == alias)
                });
            if let Some((_, entry)) = found {
                methods.push((method_decl, *entry));
            }
        }
        Some((decl, methods))
    }

    /// The modifiers, docblocks and attributes before a member.
    fn modifiers(&mut self) -> Modifiers {
        let mut modifiers = Modifiers::default();
        loop {
            if let Some(doc) = self.docblock() {
                if contains(&doc, b"@tentative-return-type") {
                    modifiers.tentative = true;
                }
                modifiers.alias = alias_of(&doc).or(modifiers.alias);
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
        let absolute = self.peek() == Some(b'\\');
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
            _ => Type::Named(Name(self.qualify(word, absolute).into())),
        }
    }

    /// `Name = "value";` after `case`, a backed enum's case.
    fn case(&mut self, class: &Name) -> ClassConstDecl {
        let case = self.word();
        let value = if self.eat(b'=') {
            self.operand()
        } else {
            ConstExpr::EnumCase {
                class: class.clone(),
                case: Name(case.clone().into()),
            }
        };
        self.eat(b';');
        ClassConstDecl {
            name: Name(case.into()),
            visibility: Visibility::Public,
            is_final: false,
            is_case: true,
            ty: None,
            value,
            attributes: Vec::new(),
            doc_comment: None,
        }
    }

    /// `T NAME = value;` after a constant's modifiers and `const`.
    fn constant(&mut self, modifiers: &Modifiers) -> ClassConstDecl {
        let ty = self.ty();
        let constant = self.word();
        self.eat(b'=');
        let value = self.default();
        self.eat(b';');
        ClassConstDecl {
            name: Name(constant.into()),
            visibility: modifiers.visibility.unwrap_or(Visibility::Public),
            is_final: modifiers.is_final,
            is_case: false,
            ty: Some(ty),
            value,
            attributes: Vec::new(),
            doc_comment: None,
        }
    }

    /// `[T] $name [= default];` after a property's modifiers, `None`
    /// where what stands there is no property.
    fn property(&mut self, modifiers: &Modifiers) -> Option<PropertyDecl> {
        let save = self.at;
        let ty = (self.peek() != Some(b'$')).then(|| self.ty());
        if !self.eat(b'$') {
            self.at = save;
            return None;
        }
        let name = self.word();
        let default = self.eat(b'=').then(|| self.default());
        self.eat(b';');
        Some(PropertyDecl {
            name: Name(name.into()),
            visibility: modifiers.visibility.unwrap_or(Visibility::Public),
            set_visibility: None,
            is_instance: !modifiers.is_static,
            is_readonly: false,
            is_final: false,
            default: default.or_else(|| ty.is_none().then_some(ConstExpr::Null)),
            ty,
            attributes: Vec::new(),
            doc_comment: None,
        })
    }

    /// `[#[\Attribute]] [T] [&] [...]$name [= default]`.
    fn parameter(&mut self) -> Option<Parameter> {
        let mut attributes = Vec::new();
        while self.eat_str(b"#[") {
            attributes.push(self.attribute());
        }
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
        let default = self.eat(b'=').then(|| self.default());
        Some(Parameter {
            name: Name(name.into()),
            ty,
            by_ref,
            // No dom stub marks a parameter `@prefer-ref`.
            prefer_ref: false,
            variadic,
            default,
            attributes,
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
            Some(b'0') if self.bytes.get(self.at + 1) == Some(&b'x') => {
                self.at += 2;
                let start = self.at;
                while self.bytes.get(self.at).is_some_and(u8::is_ascii_hexdigit) {
                    self.at += 1;
                }
                let text = std::str::from_utf8(self.bytes.get(start..self.at).unwrap_or_default())
                    .unwrap_or("0");
                ConstExpr::Long(i64::from_str_radix(text, 16).unwrap_or(0))
            }
            Some(b'-' | b'0'..=b'9') => {
                let start = self.at;
                self.at += 1;
                while self.bytes.get(self.at).is_some_and(u8::is_ascii_digit) {
                    self.at += 1;
                }
                let text = std::str::from_utf8(self.bytes.get(start..self.at).unwrap_or_default())
                    .unwrap_or("0");
                ConstExpr::Long(text.parse().unwrap_or(0))
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

/// Whether `doc` holds `tag`.
fn contains(doc: &[u8], tag: &[u8]) -> bool {
    doc.windows(tag.len()).any(|w| w == tag)
}

/// The `Class::method` of an `@implementation-alias` tag.
fn alias_of(doc: &[u8]) -> Option<Vec<u8>> {
    const TAG: &[u8] = b"@implementation-alias";
    let at = doc.windows(TAG.len()).position(|w| w == TAG)? + TAG.len();
    let rest = doc.get(at..)?;
    let start = rest.iter().position(|b| !b.is_ascii_whitespace())?;
    let rest = rest.get(start..)?;
    let end = rest
        .iter()
        .position(|b| b.is_ascii_whitespace() || *b == b'*')
        .unwrap_or(rest.len());
    rest.get(..end).map(<[u8]>::to_vec)
}

/// `zend_enum_register_props`: an enum's `name` and, backed, its `value`,
/// both public and readonly.
fn enum_properties(kind: ClassKind, backing: EnumBacking) -> Vec<PropertyDecl> {
    if kind != ClassKind::Enum {
        return Vec::new();
    }
    let property = |name: &[u8], ty: Type| PropertyDecl {
        name: Name(name.into()),
        visibility: Visibility::Public,
        set_visibility: None,
        is_instance: true,
        is_readonly: true,
        is_final: false,
        ty: Some(ty),
        default: None,
        attributes: Vec::new(),
        doc_comment: None,
    };
    let mut out = vec![property(b"name", Type::String)];
    match backing {
        EnumBacking::Int => out.push(property(b"value", Type::Int)),
        EnumBacking::String => out.push(property(b"value", Type::String)),
        EnumBacking::Pure => {}
    }
    out
}

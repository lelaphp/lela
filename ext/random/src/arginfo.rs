//! `random_arginfo.h`: the declarations `random.stub.php` generates, the
//! constants, the nine natives and the eleven classes in `MINIT`'s order
//! (`Z/ext/random/random.c:720-790`), each method beside its entry. What
//! the stub says with a docblock is said here with a field:
//! `@strict-properties` is `DynamicProperties::Forbidden`, a
//! `#[\Deprecated]` is the attribute the engine raises the deprecation
//! from, and `= UNKNOWN` is the optional parameter with no default.

use php_ext::decl::{
    Attribute, AttributeArgument, ClassConstDecl, ClassDecl, ClassFlags, ClassKind, ConstDecl,
    ConstExpr, DynamicProperties, EnumBacking, FunctionDecl, MethodDecl, Name, Parameter,
    ParameterDefault, PropertyDecl, Type, Visibility,
};

use crate::Entry;

/// A name as the stub spells it.
pub(super) fn name(text: &str) -> Name {
    Name(text.as_bytes().into())
}

/// A class type, `Random\Engine`.
pub(super) fn class_type(text: &str) -> Type {
    Type::Named(name(text))
}

/// One parameter by value, with its default where it is optional.
pub(super) fn param(text: &str, ty: Type, default: Option<ConstExpr>) -> Parameter {
    Parameter {
        name: name(text),
        ty: Some(ty),
        by_ref: false,
        prefer_ref: false,
        promoted: false,
        variadic: false,
        default: default.map_or(ParameterDefault::None, ParameterDefault::Expr),
        attributes: Vec::new(),
    }
}

/// `= UNKNOWN`: optional, with no default a reader can show.
fn unknown() -> ConstExpr {
    ConstExpr::Constant(name("UNKNOWN"))
}

/// `?int $seed = null, int $mode = MT_RAND_MT19937`, what `mt_srand`,
/// `srand` and `Mt19937::__construct` take.
pub(super) fn seed_and_mode() -> Vec<Parameter> {
    vec![
        param(
            "seed",
            Type::Nullable(Box::new(Type::Int)),
            Some(ConstExpr::Null),
        ),
        param(
            "mode",
            Type::Int,
            Some(ConstExpr::Constant(name("MT_RAND_MT19937"))),
        ),
    ]
}

/// `string|int|null $seed = null`, the 128- and 256-bit engines' seed.
pub(super) fn wide_seed() -> Vec<Parameter> {
    vec![param(
        "seed",
        Type::Union(vec![Type::String, Type::Int, Type::Null]),
        Some(ConstExpr::Null),
    )]
}

/// A native's declaration.
pub(super) fn function(text: &str, parameters: Vec<Parameter>, ret: Type) -> FunctionDecl {
    FunctionDecl {
        name: name(text),
        parameters,
        return_type: Some(ret),
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

/// A public instance method; a constructor has no return type.
pub(super) fn method(text: &str, parameters: Vec<Parameter>, ret: Option<Type>) -> MethodDecl {
    MethodDecl {
        function: FunctionDecl {
            return_type: ret,
            ..function(text, parameters, Type::Void)
        },
        visibility: Visibility::Public,
        is_instance: true,
        is_abstract: false,
        is_final: false,
        tentative_return: false,
    }
}

/// `#[\Deprecated(since: ..., message: ...)]`.
fn deprecated(since: &str, message: &str) -> Attribute {
    let argument = |key: &str, value: &str| AttributeArgument {
        name: Some(name(key)),
        value: ConstExpr::Str(value.as_bytes().into()),
    };
    Attribute {
        name: name("Deprecated"),
        arguments: vec![argument("since", since), argument("message", message)],
    }
}

/// `MT_RAND_MT19937` and the deprecated `MT_RAND_PHP`.
pub(super) fn constants() -> Vec<ConstDecl> {
    vec![
        ConstDecl {
            name: name("MT_RAND_MT19937"),
            value: ConstExpr::long(0),
            attributes: Vec::new(),
        },
        ConstDecl {
            name: name("MT_RAND_PHP"),
            value: ConstExpr::long(1),
            attributes: vec![deprecated(
                "8.3",
                "as it uses a biased non-standard variant of Mt19937",
            )],
        },
    ]
}

/// The entries of the seven natives `random.c` defines.
pub(crate) struct Natives {
    pub(crate) lcg_value: Entry,
    pub(crate) mt_srand: Entry,
    pub(crate) rand: Entry,
    pub(crate) mt_rand: Entry,
    pub(crate) mt_getrandmax: Entry,
    pub(crate) random_bytes: Entry,
    pub(crate) random_int: Entry,
}

/// The natives in the stub's order, `srand` and `getrandmax` the
/// aliases of `mt_srand` and `mt_getrandmax` (`ZEND_FALIAS`).
pub(super) fn functions(natives: &Natives) -> Vec<(FunctionDecl, Entry)> {
    let range = || {
        vec![
            param("min", Type::Int, Some(unknown())),
            param("max", Type::Int, Some(unknown())),
        ]
    };
    let mut lcg = function("lcg_value", Vec::new(), Type::Float);
    lcg.attributes = vec![deprecated(
        "8.4",
        "use \\Random\\Randomizer::getFloat() instead",
    )];
    vec![
        (lcg, natives.lcg_value),
        (
            function("mt_srand", seed_and_mode(), Type::Void),
            natives.mt_srand,
        ),
        (
            function("srand", seed_and_mode(), Type::Void),
            natives.mt_srand,
        ),
        (function("rand", range(), Type::Int), natives.rand),
        (function("mt_rand", range(), Type::Int), natives.mt_rand),
        (
            function("mt_getrandmax", Vec::new(), Type::Int),
            natives.mt_getrandmax,
        ),
        (
            function("getrandmax", Vec::new(), Type::Int),
            natives.mt_getrandmax,
        ),
        (
            function(
                "random_bytes",
                vec![param("length", Type::Int, None)],
                Type::String,
            ),
            natives.random_bytes,
        ),
        (
            function(
                "random_int",
                vec![param("min", Type::Int, None), param("max", Type::Int, None)],
                Type::Int,
            ),
            natives.random_int,
        ),
    ]
}

/// A class of the stub: `final` unless it is one of the three
/// throwables, every one `@strict-properties`.
pub(super) fn class(
    text: &str,
    kind: ClassKind,
    parent: Option<&str>,
    interfaces: &[&str],
) -> ClassDecl {
    ClassDecl {
        name: name(text),
        kind,
        flags: ClassFlags {
            is_final: kind == ClassKind::Class && parent.is_none(),
            dynamic_properties: DynamicProperties::Forbidden,
            ..ClassFlags::default()
        },
        parent: parent.map(name),
        interfaces: interfaces.iter().copied().map(name).collect(),
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
    }
}

/// `interface Random\Engine { public function generate(): string; }`
/// and `interface Random\CryptoSafeEngine extends Engine {}`.
pub(super) fn interfaces() -> Vec<ClassDecl> {
    let mut engine = class("Random\\Engine", ClassKind::Interface, None, &[]);
    engine.flags.dynamic_properties = DynamicProperties::Deprecated;
    let mut generate = method("generate", Vec::new(), Some(Type::String));
    generate.is_abstract = true;
    engine.methods.push(generate);
    let mut crypto = class(
        "Random\\CryptoSafeEngine",
        ClassKind::Interface,
        None,
        &["Random\\Engine"],
    );
    crypto.flags.dynamic_properties = DynamicProperties::Deprecated;
    vec![engine, crypto]
}

/// The three throwables: `RandomError extends \Error`,
/// `BrokenRandomEngineError extends RandomError` and
/// `RandomException extends \Exception`.
pub(super) fn throwables() -> Vec<ClassDecl> {
    vec![
        class("Random\\RandomError", ClassKind::Class, Some("Error"), &[]),
        class(
            "Random\\BrokenRandomEngineError",
            ClassKind::Class,
            Some("Random\\RandomError"),
            &[],
        ),
        class(
            "Random\\RandomException",
            ClassKind::Class,
            Some("Exception"),
            &[],
        ),
    ]
}

/// `public readonly Engine $engine;`, the Randomizer's one property.
pub(super) fn engine_property() -> PropertyDecl {
    PropertyDecl {
        name: name("engine"),
        visibility: Visibility::Public,
        set_visibility: None,
        is_instance: true,
        is_readonly: true,
        is_final: false,
        is_promoted: false,
        is_virtual: false,
        ty: Some(class_type("Random\\Engine")),
        default: None,
        attributes: Vec::new(),
        doc_comment: None,
        hooks: None,
    }
}

/// `enum Random\IntervalBoundary`, its four cases in the stub's order;
/// `cases()` is the engine's, as `zend_enum_register_funcs` gives it.
pub(super) fn interval_boundary() -> ClassDecl {
    let enum_name = "Random\\IntervalBoundary";
    let mut decl = class(enum_name, ClassKind::Enum, None, &["UnitEnum"]);
    decl.constants = ["ClosedOpen", "ClosedClosed", "OpenClosed", "OpenOpen"]
        .into_iter()
        .map(|case| ClassConstDecl {
            name: name(case),
            visibility: Visibility::Public,
            is_final: false,
            is_case: true,
            ty: None,
            value: ConstExpr::EnumCase {
                class: name(enum_name),
                case: name(case),
            },
            attributes: Vec::new(),
            doc_comment: None,
        })
        .collect();
    decl.flags.is_final = true;
    decl.properties = vec![PropertyDecl {
        name: name("name"),
        ty: Some(Type::String),
        ..engine_property()
    }];
    decl
}

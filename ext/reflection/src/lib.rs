//! The `reflection` extension: `php-src/ext/reflection` as one crate under `ext/`
//! (ADR-0004). One dependency, the facade; one entry, `extension()`; its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/reflection/tests`, through `xtask phpt --subset`.
//!
//! What is here reads the linked tables through `api` and nothing else
//! (design §12): `ReflectionClass`, `ReflectionObject`,
//! `ReflectionProperty` and `ReflectionConstant` with the accessors
//! `php_reflection.c` answers off the class entry, the property info
//! and the constant (the section below `ReflectionClass`), and
//! `ReflectionExtension` and `ReflectionFunction`, each
//! with its constructor and its `__toString`, rendering as
//! `_extension_string` and `_function_string` in `php_reflection.c` at
//! the tag render, in php's wording and order, since `xtask crosscheck`
//! holds the bytes to `php --re` (design §7.1, §8), beside the
//! accessors `php_reflection.c` answers off the `zend_module_entry`,
//! `getName` to `isTemporary`, `info()` writing the extension's `MINFO`
//! table as `php_info_print_module` does, and `isDeprecated`. A reflection object
//! keeps what php keeps on it, the `name` property its stub declares,
//! and looks the extension or function up again when it renders, which
//! for the linked tables is the same entry php's `intern->ptr` points
//! at. `ReflectionException` is declared here as `php_reflection.stub.php`
//! declares it, under `Exception`.
//!
//! Rendered so far: the header, and the `Functions` block with each
//! function's parameters and return. An extension's `INI`, `Constants`
//! and `Classes` blocks, `_extension_ini_string`, `_const_string` and
//! `_class_string`, are not rendered yet; `ctype` has none of the three.
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::fmt::Write as _;

use php_ext::abi::{DependencyKind, Extension, ExtensionBuilder};
use php_ext::call::{
    Function, constant, eval_const_expr, lookup_class, lookup_function, new_object,
};
use php_ext::decl::{
    ClassConstDecl, ClassDecl, ClassFlags, ClassKind, ConstExpr, EnumBacking, FunctionDecl,
    MethodDecl, Name, Parameter, PropertyDecl, Type, Visibility,
};
use php_ext::raising::{Thrown, arg_count_error, arg_type_error, deprecated, throw};
use php_ext::request::{ExtensionId, PHP_VERSION, Request};
use php_ext::values::{ClassId, Key, PhpArray, PhpObject, PhpString, Value, ValueRef};

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or synchronised.
#[derive(Default)]
struct State {}

/// The extension value (design §7.1): declarations paired with entries,
/// and the state declared on it.
///
/// ```php
/// var_dump(extension_loaded('reflection'));
/// ```
/// ```expect
/// bool(true)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .classes(vec![
            (
                class(
                    b"ReflectionException",
                    ClassKind::Class,
                    Some(b"Exception"),
                    &[],
                ),
                Vec::new(),
            ),
            (
                class(b"Reflector", ClassKind::Interface, None, &[b"Stringable"]),
                Vec::new(),
            ),
            reflection_function_abstract(),
            reflection_function(),
            reflection_class(),
            (
                class(
                    b"ReflectionObject",
                    ClassKind::Class,
                    Some(b"ReflectionClass"),
                    &[],
                ),
                vec![(
                    method(b"__construct", &[parameter(b"object", Type::Object)], None),
                    reflection_object_construct as Entry,
                )],
            ),
            reflection_property(),
            reflection_constant(),
            reflection_extension(),
        ])
        .request_state::<State>()
        .info(reflection_info)
        // `zend_module_entry.name` is `Reflection`, the spelling
        // `ReflectionClass::getExtensionName` and `php --re` print;
        // `extension_loaded('reflection')` lowers both sides.
        .build("Reflection")
}

/// `PHP_MINFO_FUNCTION(reflection)`: the one-row table `Reflection =>
/// enabled`. The witness is `ReflectionExtension::info`'s.
fn reflection_info(req: &mut Request) {
    req.info_table(&[&["Reflection", "enabled"]]);
}

/// A reflector with `public string $name` and the `Reflector` interface,
/// the shape `ReflectionExtension` and `ReflectionConstant` share.
fn named_reflector(name: &[u8]) -> ClassDecl {
    ClassDecl {
        properties: vec![name_property()],
        ..class(name, ClassKind::Class, None, &[b"Reflector"])
    }
}

/// `abstract class ReflectionFunctionAbstract implements Reflector`,
/// with `isDeprecated`, which the stub declares here and
/// `ReflectionFunction` inherits.
fn reflection_function_abstract() -> (ClassDecl, Vec<(MethodDecl, Entry)>) {
    (
        ClassDecl {
            flags: ClassFlags {
                is_abstract: true,
                ..ClassFlags::default()
            },
            ..named_reflector(b"ReflectionFunctionAbstract")
        },
        vec![(
            tentative(method(b"isDeprecated", &[], Some(Type::Bool))),
            reflection_function_is_deprecated as Entry,
        )],
    )
}

/// `ReflectionFunction`: its constructor and `__toString`.
fn reflection_function() -> (ClassDecl, Vec<(MethodDecl, Entry)>) {
    (
        class(
            b"ReflectionFunction",
            ClassKind::Class,
            Some(b"ReflectionFunctionAbstract"),
            &[],
        ),
        vec![
            (
                method(
                    b"__construct",
                    &[parameter(
                        b"function",
                        Type::Union(vec![Type::Named(Name::from("Closure")), Type::String]),
                    )],
                    None,
                ),
                reflection_function_construct as Entry,
            ),
            (
                method(b"__toString", &[], Some(Type::String)),
                reflection_function_to_string as Entry,
            ),
        ],
    )
}

/// `ReflectionConstant`: its constructor and `getAttributes`.
fn reflection_constant() -> (ClassDecl, Vec<(MethodDecl, Entry)>) {
    (
        named_reflector(b"ReflectionConstant"),
        vec![
            (
                method(b"__construct", &[parameter(b"name", Type::String)], None),
                reflection_constant_construct as Entry,
            ),
            (
                method(
                    b"getAttributes",
                    &attributes_parameters(),
                    Some(Type::Array),
                ),
                reflection_constant_get_attributes as Entry,
            ),
        ],
    )
}

/// `ReflectionExtension`: its constructor, `__toString`, and the
/// accessors that take no argument, each tentative as the stub marks
/// it, in the stub's order; `getVersion`, `getFunctions`,
/// `getConstants` and `getINIEntries` arrive with the tests that call
/// them.
fn reflection_extension() -> (ClassDecl, Vec<(MethodDecl, Entry)>) {
    let accessors: [(&[u8], Type, Entry); 7] = [
        (b"getName", Type::String, reflection_extension_get_name),
        (b"getClasses", Type::Array, reflection_extension_get_classes),
        (
            b"getClassNames",
            Type::Array,
            reflection_extension_get_class_names,
        ),
        (
            b"getDependencies",
            Type::Array,
            reflection_extension_get_dependencies,
        ),
        (b"info", Type::Void, reflection_extension_info),
        (
            b"isPersistent",
            Type::Bool,
            reflection_extension_is_persistent,
        ),
        (
            b"isTemporary",
            Type::Bool,
            reflection_extension_is_temporary,
        ),
    ];
    let mut methods: Vec<(MethodDecl, Entry)> = vec![
        (
            method(b"__construct", &[parameter(b"name", Type::String)], None),
            reflection_extension_construct as Entry,
        ),
        (
            method(b"__toString", &[], Some(Type::String)),
            reflection_extension_to_string as Entry,
        ),
    ];
    methods.extend(
        accessors
            .into_iter()
            .map(|(name, returns, entry)| (tentative(method(name, &[], Some(returns))), entry)),
    );
    (named_reflector(b"ReflectionExtension"), methods)
}

/// `ReflectionClass` as `php_reflection.stub.php` declares it: its six
/// constants, `public string $name`, and the methods the corpus's tests
/// call so far; the rest of the stub's sixty-four arrive with the
/// tests that call them. A return the stub marks tentative is
/// tentative here.
fn reflection_class() -> (ClassDecl, Vec<(MethodDecl, Entry)>) {
    (
        ClassDecl {
            constants: [
                (&b"IS_IMPLICIT_ABSTRACT"[..], 16),
                (b"IS_EXPLICIT_ABSTRACT", 64),
                (b"IS_FINAL", 32),
                (b"IS_READONLY", 65536),
                (b"SKIP_INITIALIZATION_ON_SERIALIZE", 8),
                (b"SKIP_DESTRUCTOR", 16),
            ]
            .into_iter()
            .map(|(name, value)| int_constant(name, value))
            .collect(),
            ..named_reflector(b"ReflectionClass")
        },
        reflection_class_methods(),
    )
}

/// `ReflectionClass`'s methods, each beside its entry: the constructor,
/// the accessors that take no argument, the ones that take a name, and
/// `getAttributes`.
fn reflection_class_methods() -> Vec<(MethodDecl, Entry)> {
    let mut methods: Vec<(MethodDecl, Entry)> = vec![(
        method(
            b"__construct",
            &[parameter(
                b"objectOrClass",
                Type::Union(vec![Type::Object, Type::String]),
            )],
            None,
        ),
        reflection_class_construct as Entry,
    )];
    methods.extend(
        reflection_class_plain_accessors()
            .into_iter()
            .map(|(name, returns, entry)| (tentative(method(name, &[], Some(returns))), entry)),
    );
    methods.extend(reflection_class_named_accessors().into_iter().map(
        |(name, parameter, returns, entry)| {
            (tentative(method(name, &[parameter], Some(returns))), entry)
        },
    ));
    methods.push((
        method(
            b"getAttributes",
            &attributes_parameters(),
            Some(Type::Array),
        ),
        reflection_class_get_attributes as Entry,
    ));
    methods
}

/// The `ReflectionClass` accessors that take no argument: name, return
/// type and entry, in the stub's order.
fn reflection_class_plain_accessors() -> Vec<(&'static [u8], Type, Entry)> {
    let string_or_false = || Type::Union(vec![Type::String, Type::False]);
    let int_or_false = || Type::Union(vec![Type::Int, Type::False]);
    let reflection_extension =
        Type::Nullable(Box::new(Type::Named(Name::from("ReflectionExtension"))));
    vec![
        (b"getName", Type::String, reflection_class_get_name),
        (b"isInternal", Type::Bool, reflection_class_is_internal),
        (
            b"isUserDefined",
            Type::Bool,
            reflection_class_is_user_defined,
        ),
        (
            b"getFileName",
            string_or_false(),
            reflection_class_get_file_name,
        ),
        (
            b"getStartLine",
            int_or_false(),
            reflection_class_get_start_line,
        ),
        (b"getEndLine", int_or_false(), reflection_class_get_end_line),
        (
            b"getDefaultProperties",
            Type::Array,
            reflection_class_get_default_properties,
        ),
        (
            b"getExtension",
            reflection_extension,
            reflection_class_get_extension,
        ),
        (
            b"getExtensionName",
            string_or_false(),
            reflection_class_get_extension_name,
        ),
        (b"inNamespace", Type::Bool, reflection_class_in_namespace),
        (
            b"getNamespaceName",
            Type::String,
            reflection_class_get_namespace_name,
        ),
        (
            b"getShortName",
            Type::String,
            reflection_class_get_short_name,
        ),
    ]
}

/// The `ReflectionClass` accessors that take one argument: name, the
/// parameter, return type and entry.
fn reflection_class_named_accessors() -> Vec<(&'static [u8], Parameter, Type, Entry)> {
    let name = || parameter(b"name", Type::String);
    let class_or_string = Type::Union(vec![
        Type::Named(Name::from("ReflectionClass")),
        Type::String,
    ]);
    vec![
        (
            b"hasProperty",
            name(),
            Type::Bool,
            reflection_class_has_property,
        ),
        (
            b"hasConstant",
            name(),
            Type::Bool,
            reflection_class_has_constant,
        ),
        (
            b"getConstant",
            name(),
            Type::Mixed,
            reflection_class_get_constant,
        ),
        (
            b"isSubclassOf",
            parameter(b"class", class_or_string),
            Type::Bool,
            reflection_class_is_subclass_of,
        ),
    ]
}

/// `ReflectionProperty` as the stub declares it: its ten constants,
/// `$name` and `$class`, the constructor, `getValue` and `setValue`.
fn reflection_property() -> (ClassDecl, Vec<(MethodDecl, Entry)>) {
    (
        ClassDecl {
            constants: [
                (&b"IS_STATIC"[..], 16),
                (b"IS_READONLY", 128),
                (b"IS_PUBLIC", 1),
                (b"IS_PROTECTED", 2),
                (b"IS_PRIVATE", 4),
                (b"IS_ABSTRACT", 64),
                (b"IS_PROTECTED_SET", 2048),
                (b"IS_PRIVATE_SET", 4096),
                (b"IS_VIRTUAL", 512),
                (b"IS_FINAL", 32),
            ]
            .into_iter()
            .map(|(name, value)| int_constant(name, value))
            .collect(),
            properties: vec![
                name_property(),
                PropertyDecl {
                    name: Name::from("class"),
                    ..name_property()
                },
            ],
            ..class(
                b"ReflectionProperty",
                ClassKind::Class,
                None,
                &[b"Reflector"],
            )
        },
        vec![
            (
                method(
                    b"__construct",
                    &[
                        parameter(b"class", Type::Union(vec![Type::Object, Type::String])),
                        parameter(b"property", Type::String),
                    ],
                    None,
                ),
                reflection_property_construct as Entry,
            ),
            (
                tentative(method(
                    b"getValue",
                    &[parameter_with_default(
                        b"object",
                        Type::Nullable(Box::new(Type::Object)),
                        ConstExpr::Null,
                    )],
                    Some(Type::Mixed),
                )),
                reflection_property_get_value as Entry,
            ),
            (
                tentative(method(
                    b"setValue",
                    &[
                        parameter(b"objectOrValue", Type::Mixed),
                        parameter(b"value", Type::Mixed),
                    ],
                    Some(Type::Void),
                )),
                reflection_property_set_value as Entry,
            ),
        ],
    )
}

/// `?string $name = null, int $flags = 0`, `getAttributes`'s parameters
/// on every reflector.
fn attributes_parameters() -> Vec<Parameter> {
    vec![
        parameter_with_default(
            b"name",
            Type::Nullable(Box::new(Type::String)),
            ConstExpr::Null,
        ),
        parameter_with_default(b"flags", Type::Int, ConstExpr::Long(0)),
    ]
}

/// `public const int NAME = value;`, as the stub declares a reflector's
/// `IS_*` constants.
fn int_constant(name: &[u8], value: i64) -> ClassConstDecl {
    ClassConstDecl {
        name: Name(name.into()),
        visibility: Visibility::Public,
        is_final: false,
        ty: Some(Type::Int),
        value: ConstExpr::Long(value),
        attributes: Vec::new(),
        doc_comment: None,
    }
}

/// A method whose return type the stub marks `@tentative-return-type`.
fn tentative(method: MethodDecl) -> MethodDecl {
    MethodDecl {
        tentative_return: true,
        ..method
    }
}

/// A native's entry, the shape `ExtensionBuilder::classes` takes beside
/// each method.
type Entry = fn(&mut Request) -> Result<Value, Thrown>;

fn class(name: &[u8], kind: ClassKind, parent: Option<&[u8]>, interfaces: &[&[u8]]) -> ClassDecl {
    ClassDecl {
        name: Name(name.into()),
        kind,
        flags: ClassFlags::default(),
        parent: parent.map(|p| Name(p.into())),
        interfaces: interfaces.iter().map(|i| Name((*i).into())).collect(),
        traits: Vec::new(),
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

/// `public string $name;`, as the stub declares it on each reflector.
fn name_property() -> PropertyDecl {
    PropertyDecl {
        name: Name::from("name"),
        visibility: Visibility::Public,
        is_instance: true,
        is_readonly: false,
        ty: Some(Type::String),
        default: None,
        attributes: Vec::new(),
        doc_comment: None,
    }
}

fn parameter(name: &[u8], ty: Type) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty: Some(ty),
        by_ref: false,
        variadic: false,
        default: None,
        attributes: Vec::new(),
    }
}

fn parameter_with_default(name: &[u8], ty: Type, default: ConstExpr) -> Parameter {
    Parameter {
        default: Some(default),
        ..parameter(name, ty)
    }
}

fn method(name: &[u8], parameters: &[Parameter], returns: Option<Type>) -> MethodDecl {
    MethodDecl {
        function: FunctionDecl {
            name: Name(name.into()),
            parameters: parameters.to_vec(),
            return_type: returns,
            returns_by_ref: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            frameless: Vec::new(),
        },
        visibility: Visibility::Public,
        is_instance: true,
        is_abstract: false,
        is_final: false,
        tentative_return: false,
    }
}

/// `ZEND_PARSE_PARAMETERS_START(1, 1)` with one `string`: the count
/// held to exactly one and the argument read as [`string_argument`].
fn one_string_argument(req: &mut Request) -> Result<Vec<u8>, Thrown> {
    exactly_arguments(req, 1)?;
    string_argument(req, 1)
}

/// `ZEND_PARSE_PARAMETERS_START(n, n)`'s count check: php's
/// `ArgumentCountError`, `expects exactly N argument(s), M given`.
fn exactly_arguments(req: &mut Request, expected: usize) -> Result<(), Thrown> {
    let count = req.argument_count();
    if count == expected {
        return Ok(());
    }
    let plural = if expected == 1 { "" } else { "s" };
    Err(arg_count_error(
        req,
        &format!("exactly {expected} argument{plural}, {count} given"),
    ))
}

/// `ZEND_PARSE_PARAMETERS_START(0, n)`'s count check: `expects at most
/// N arguments, M given`.
fn at_most_arguments(req: &mut Request, most: usize) -> Result<(), Thrown> {
    let count = req.argument_count();
    if count <= most {
        return Ok(());
    }
    Err(arg_count_error(
        req,
        &format!("at most {most} arguments, {count} given"),
    ))
}

/// `Z_PARAM_STR` at `position`: a scalar is coerced as a string
/// parameter is, `null` the 8.1 deprecation naming the declared type
/// and the empty string written back as the argument, both
/// `coerce_param`'s; the argument's bytes are copied out of the frame.
fn string_argument(req: &mut Request, position: u32) -> Result<Vec<u8>, Thrown> {
    let Some(given) = req.argument(position) else {
        return Ok(Vec::new());
    };
    let coerced = php_ext::coerce::coerce_param(req, &given, &Type::String, position)?;
    Ok(coerced.as_str_bytes().unwrap_or_default().to_vec())
}

/// `Z_PARAM_OBJ_OR_STR` at `position` and the lookup both reflection
/// constructors do with it: an object is its class, kept beside it; a
/// string is `zend_lookup_class`, a leading `\` dropped, or php's
/// `ReflectionException` `Class "X" does not exist` with the name as
/// given; anything else is the `TypeError` naming `object|string`.
fn class_argument(
    req: &mut Request,
    position: u32,
) -> Result<(ClassId, Option<PhpObject>), Thrown> {
    if let Some(given) = req.argument(position) {
        match given.deref().as_ref() {
            ValueRef::Object(object) => return Ok((object.class(), Some(*object))),
            ValueRef::Array(_) | ValueRef::Resource(_) => {
                return Err(arg_type_error(
                    req,
                    position,
                    &format!(
                        "be of type object|string, {} given",
                        given.deref().type_name()
                    ),
                ));
            }
            _ => {}
        }
    }
    let name = string_argument(req, position)?;
    Ok((class_named(req, &name)?, None))
}

/// `zend_lookup_class` or the `ReflectionException` every reflector
/// throws for a miss, `Class "X" does not exist`, the name as given.
fn class_named(req: &mut Request, name: &[u8]) -> Result<ClassId, Thrown> {
    let bare = name.strip_prefix(b"\\").unwrap_or(name);
    match lookup_class(req, bare)? {
        Some(class) => Ok(class),
        None => Err(throw(
            req,
            "ReflectionException",
            &format!("Class \"{}\" does not exist", String::from_utf8_lossy(name)),
        )),
    }
}

/// `ZEND_THIS` and its `name` property, or php's `Error` for a
/// reflector read before its constructor ran.
fn this_name(req: &mut Request, class: &str) -> Result<(PhpObject, Vec<u8>), Thrown> {
    let Some(this) = req.this() else {
        return Err(throw(
            req,
            "Error",
            &format!("Internal error: Failed to retrieve the reflection object of {class}"),
        ));
    };
    match this
        .prop(req, b"name")
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
    {
        Some(name) => Ok((this, name)),
        None => Err(throw(
            req,
            "Error",
            &format!("Typed property {class}::$name must not be accessed before initialization"),
        )),
    }
}

fn set_name(req: &mut Request, this: PhpObject, name: &[u8]) -> Result<(), Thrown> {
    set_string_prop(req, this, b"name", name)
}

fn set_string_prop(
    req: &mut Request,
    this: PhpObject,
    prop: &[u8],
    value: &[u8],
) -> Result<(), Thrown> {
    let key = PhpString::new(req, prop)?;
    let value = PhpString::new(req, value)?;
    this.set_prop(req, key, Value::string(value))
}

/// A string of the request's as a native's return.
fn string_value(req: &mut Request, bytes: &[u8]) -> Result<Value, Thrown> {
    Ok(Value::string(PhpString::new(req, bytes)?))
}

/// `ReflectionExtension::__construct(string $name)`: the extension
/// under the lowered name in `module_registry`, its declared name kept
/// as `$name`; a name the bundle has not is php's `ReflectionException`,
/// the name as given.
///
/// ```php
/// $e = new ReflectionExtension('CTYPE'); $f = new ReflectionFunction('CTYPE_ALPHA'); echo $e->name, '|', $f->name, '|'; var_dump($e instanceof Reflector);
/// ```
/// ```expect
/// ctype|ctype_alpha|bool(true)
/// ```
///
/// ```php
/// new ReflectionExtension('nope');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Extension "nope" does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionExtension->__construct('nope')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new ReflectionExtension();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionExtension::__construct() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionExtension->__construct()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_extension_construct(req: &mut Request) -> Result<Value, Thrown> {
    let name = one_string_argument(req)?;
    let Some(extension) = req.extension_named(&name) else {
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Extension \"{}\" does not exist",
                String::from_utf8_lossy(&name)
            ),
        ));
    };
    let declared = req
        .extension_name(extension)
        .map(str::to_owned)
        .unwrap_or_default();
    if let Some(this) = req.this() {
        set_name(req, this, declared.as_bytes())?;
    }
    Ok(Value::null())
}

/// `ReflectionExtension::__toString()`: `_extension_string` over the
/// extension `$name` names, indent empty. The load number in the first
/// line is the oracle's build order there and the bundle's here, `#10`
/// against `#15` for ctype, a difference that is nobody's defect; the
/// pin's `[recipe.normalise] extension_number_token` (#317) replaces
/// the digits after `extension #` in both streams, so the fence below
/// holds every byte the oracle prints and the token stands where the
/// number was.
///
/// ```php
/// echo new ReflectionExtension('ctype');
/// ```
/// ```expect
/// Extension [ <persistent> extension #%N% ctype version 8.5.9 ] {
///
///   - Functions {
///     Function [ <internal:ctype> function ctype_alnum ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_alpha ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_cntrl ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_digit ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_lower ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_graph ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_print ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_punct ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_space ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_upper ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///     Function [ <internal:ctype> function ctype_xdigit ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> mixed $text ]
///       }
///       - Return [ bool ]
///     }
///   }
/// }
/// ```
fn reflection_extension_to_string(req: &mut Request) -> Result<Value, Thrown> {
    let extension = this_extension(req)?;
    let text = extension_string(req, extension, "");
    Ok(Value::string(PhpString::new(req, text.as_bytes())?))
}

/// `GET_REFLECTION_OBJECT_PTR(module)` for a `ReflectionExtension`: the
/// extension `$name` names, looked up again as the constructor found
/// it; a name the bundle has not is the constructor's
/// `ReflectionException`.
fn this_extension(req: &mut Request) -> Result<ExtensionId, Thrown> {
    let (_, name) = this_name(req, "ReflectionExtension")?;
    match req.extension_named(&name) {
        Some(extension) => Ok(extension),
        None => Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Extension \"{}\" does not exist",
                String::from_utf8_lossy(&name)
            ),
        )),
    }
}

/// `ReflectionExtension::getName()`: `module->name`, the spelling the
/// `zend_module_entry` carries whatever the constructor was given.
///
/// ```php
/// var_dump((new ReflectionExtension('CTYPE'))->getName());
/// ```
/// ```expect
/// string(5) "ctype"
/// ```
fn reflection_extension_get_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let extension = this_extension(req)?;
    let name = req
        .extension_name(extension)
        .map(str::to_owned)
        .unwrap_or_default();
    string_value(req, name.as_bytes())
}

/// `ReflectionExtension::getClasses()`: over `EG(class_table)` in its
/// order, every class whose `ce->info.internal.module` is this
/// extension, keyed by its name and holding the `ReflectionClass`
/// `zend_reflection_class_factory` makes, `object_init_ex` and the name
/// written with no constructor served, a `ReflectionEnum` for an enum.
/// The alias key php takes for an aliased class has no case here until
/// `class_alias` links one. `ctype` declares no class, so the fence
/// pins the empty walk; the walk over `Reflection`'s classes is
/// `ReflectionExtension_getClasses_basic.phpt`'s, which names php's
/// twenty-six where this crate declares a subset, and a fence over one
/// of them waits on the compiler reading a dimension fetch.
///
/// ```php
/// $e = new ReflectionExtension('ctype'); var_dump($e->getClasses(), $e->getClassNames());
/// ```
/// ```expect
/// array(0) {
/// }
/// array(0) {
/// }
/// ```
fn reflection_extension_get_classes(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let extension = this_extension(req)?;
    let mut classes = PhpArray::new(req)?;
    for (class, name) in extension_classes(req, extension) {
        let reflector = reflection_class_factory(req, class)?;
        let key = Key::Str(PhpString::new(req, &name)?);
        classes.set(req, key, Value::object(reflector))?;
    }
    Ok(Value::array(classes))
}

/// `ReflectionExtension::getClassNames()`: the same walk as
/// `getClasses`, the names appended in order. The witness is
/// `getClasses`'s.
fn reflection_extension_get_class_names(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let extension = this_extension(req)?;
    let mut names = PhpArray::new(req)?;
    for (_, name) in extension_classes(req, extension) {
        let name = Value::string(PhpString::new(req, &name)?);
        names.append(req, name)?;
    }
    Ok(Value::array(names))
}

/// `add_extension_class`'s test over the linked table: each class the
/// extension declared, with its name, in table order.
fn extension_classes(req: &Request, extension: ExtensionId) -> Vec<(ClassId, Vec<u8>)> {
    req.declared_classes()
        .into_iter()
        .filter(|&class| req.class_extension(class) == Some(extension))
        .map(|class| (class, class_name_of(req, class)))
        .collect()
}

/// `zend_reflection_class_factory`: a `ReflectionClass`, or a
/// `ReflectionEnum` for an enum, with `$name` written and no
/// constructor served.
fn reflection_class_factory(req: &mut Request, class: ClassId) -> Result<PhpObject, Thrown> {
    let is_enum = req
        .class_decl(class)
        .is_some_and(|decl| decl.kind == ClassKind::Enum);
    let reflector = if is_enum {
        lookup_class(req, b"ReflectionEnum")?
    } else {
        None
    };
    let reflector = match reflector {
        Some(reflector) => reflector,
        None => class_named(req, b"ReflectionClass")?,
    };
    let object = new_object(req, reflector)?;
    let name = class_name_of(req, class);
    set_name(req, object, &name)?;
    Ok(object)
}

/// `ReflectionExtension::getDependencies()`: `module->deps` walked into
/// an array keyed by the other extension's name, each value the kind,
/// `Required`, `Conflicts` or `Optional`, then the row's `rel` and
/// `version` each after one space where set; the empty array where the
/// pointer is null, as it is for `ctype`.
///
/// ```php
/// var_dump((new ReflectionExtension('ctype'))->getDependencies());
/// ```
/// ```expect
/// array(0) {
/// }
/// ```
fn reflection_extension_get_dependencies(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let extension = this_extension(req)?;
    let mut dependencies = PhpArray::new(req)?;
    for dependency in req.extension_dependencies(extension) {
        let mut relation = String::from(match dependency.kind {
            DependencyKind::Required => "Required",
            DependencyKind::Conflicts => "Conflicts",
            DependencyKind::Optional => "Optional",
        });
        for part in [&dependency.rel, &dependency.version].into_iter().flatten() {
            relation.push(' ');
            relation.push_str(part);
        }
        let key = Key::Str(PhpString::new(req, dependency.name.as_bytes())?);
        let value = Value::string(PhpString::new(req, relation.as_bytes())?);
        dependencies.set(req, key, value)?;
    }
    Ok(Value::array(dependencies))
}

/// `ReflectionExtension::info()`: `php_info_print_module` over the
/// extension, the name then its `MINFO` table, written to the body as
/// the CLI prints it; `null` returned, as a `void` method is.
///
/// ```php
/// (new ReflectionExtension('ctype'))->info();
/// ```
/// ```expect
/// ctype
///
/// ctype functions => enabled
/// ```
fn reflection_extension_info(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let extension = this_extension(req)?;
    req.extension_info(extension);
    Ok(Value::null())
}

/// `ReflectionExtension::isPersistent()`:
/// `module->type == MODULE_PERSISTENT`, which every extension compiled
/// into the binary is; `MODULE_TEMPORARY` is `dl()`'s, which this
/// engine has none of.
///
/// ```php
/// $e = new ReflectionExtension('ctype'); var_dump($e->isPersistent(), $e->isTemporary());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
fn reflection_extension_is_persistent(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    this_extension(req)?;
    Ok(Value::bool(true))
}

/// `ReflectionExtension::isTemporary()`:
/// `module->type == MODULE_TEMPORARY`, the complement of
/// `isPersistent`, whose witness this shares.
fn reflection_extension_is_temporary(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    this_extension(req)?;
    Ok(Value::bool(false))
}

/// `ReflectionFunctionAbstract::isDeprecated()`:
/// `_function_check_flag(ZEND_ACC_DEPRECATED)`, the bit the
/// `#[\Deprecated]` attribute sets on a native's or a user function's
/// declaration, read from the attribute as `_function_string` reads it
/// for `, deprecated`. `ctype` deprecates none of its eleven.
///
/// ```php
/// var_dump((new ReflectionFunction('ctype_alpha'))->isDeprecated());
/// ```
/// ```expect
/// bool(false)
/// ```
fn reflection_function_is_deprecated(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let (_, name) = this_name(req, "ReflectionFunction")?;
    let Some(function) = lookup_function(req, &name) else {
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Function {}() does not exist",
                String::from_utf8_lossy(&name)
            ),
        ));
    };
    let deprecated = req.function_decl(function).is_some_and(|decl| {
        decl.attributes
            .iter()
            .any(|a| a.name.0.eq_ignore_ascii_case(b"Deprecated"))
    });
    Ok(Value::bool(deprecated))
}

/// `ReflectionFunction::__construct(Closure|string $function)`: the
/// function under the lowered name, a leading `\` dropped, its declared
/// name kept as `$name`; a name the tables have not is php's
/// `ReflectionException`, the name as given. A `Closure` is not a
/// class here yet, so an object is the type error.
///
/// ```php
/// new ReflectionFunction('nope');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Function nope() does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionFunction->__construct('nope')
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_function_construct(req: &mut Request) -> Result<Value, Thrown> {
    if let Some(given) = req.argument(1)
        && let ValueRef::Object(object) = given.deref().as_ref()
    {
        let class = req.class_decl(object.class()).map_or_else(
            || String::from("object"),
            |decl| String::from_utf8_lossy(&decl.name.0).into_owned(),
        );
        return Err(arg_type_error(
            req,
            1,
            &format!("be of type Closure|string, {class} given"),
        ));
    }
    let name = one_string_argument(req)?;
    let bare = name.strip_prefix(b"\\").unwrap_or(&name);
    let Some(function) = lookup_function(req, bare) else {
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Function {}() does not exist",
                String::from_utf8_lossy(&name)
            ),
        ));
    };
    let declared = req
        .function_decl(function)
        .map(|decl| decl.name.0.to_vec())
        .unwrap_or_default();
    if let Some(this) = req.this() {
        set_name(req, this, &declared)?;
    }
    Ok(Value::null())
}

/// `ReflectionFunction::__toString()`: `_function_string` over the
/// function `$name` names, no scope, indent empty.
///
/// ```php
/// echo new ReflectionFunction('ctype_alpha');
/// ```
/// ```expect
/// Function [ <internal:ctype> function ctype_alpha ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> mixed $text ]
///   }
///   - Return [ bool ]
/// }
/// ```
fn reflection_function_to_string(req: &mut Request) -> Result<Value, Thrown> {
    let (_, name) = this_name(req, "ReflectionFunction")?;
    let Some(function) = lookup_function(req, &name) else {
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Function {}() does not exist",
                String::from_utf8_lossy(&name)
            ),
        ));
    };
    let text = function_string(req, function, "");
    Ok(Value::string(PhpString::new(req, text.as_bytes())?))
}

// -------------------------------------------------- ReflectionClass

/// `GET_REFLECTION_OBJECT_PTR(ce)` for a class reflector: the linked
/// class `$name` names on `ZEND_THIS`, looked up again since the
/// reflector keeps the name and not php's `intern->ptr`.
fn this_class(req: &mut Request) -> Result<ClassId, Thrown> {
    let (_, name) = this_name(req, "ReflectionClass")?;
    class_named(req, &name)
}

/// `ce->name` as bytes, empty for an identity the tables have no
/// declaration for.
fn class_name_of(req: &Request, class: ClassId) -> Vec<u8> {
    req.class_decl(class)
        .map(|decl| decl.name.0.to_vec())
        .unwrap_or_default()
}

/// The class and its parents, nearest first: what php has in one
/// class entry after `zend_do_inheritance` copied the parent's tables
/// into the child's, and what every walk below reads instead.
fn class_chain(req: &mut Request, class: ClassId) -> Vec<ClassId> {
    let mut chain = vec![class];
    let mut at = class;
    while chain.len() <= 64 {
        let Some(parent) = req
            .class_decl(at)
            .and_then(|decl| decl.parent.clone())
            .and_then(|name| lookup_class(req, &name.0).ok().flatten())
        else {
            break;
        };
        chain.push(parent);
        at = parent;
    }
    chain
}

/// `instanceof_function(ce, of)` over identities: `ce` is `of`,
/// extends it, or implements it, each class of the chain's interfaces
/// walked with it.
fn class_is(req: &mut Request, class: ClassId, of: ClassId, depth: u32) -> bool {
    if class == of {
        return true;
    }
    if depth > 64 {
        return false;
    }
    let Some(decl) = req.class_decl(class) else {
        return false;
    };
    let names: Vec<Name> = decl
        .interfaces
        .iter()
        .chain(decl.parent.iter())
        .cloned()
        .collect();
    names.iter().any(|name| {
        lookup_class(req, &name.0)
            .ok()
            .flatten()
            .is_some_and(|linked| class_is(req, linked, of, depth + 1))
    })
}

/// `zend_memrchr(name, '\\')`: where the namespace ends, if the name
/// has one.
fn namespace_end(name: &[u8]) -> Option<usize> {
    name.iter().rposition(|&b| b == b'\\')
}

/// `reflection_class_object_ctor` with `is_object` off,
/// `ReflectionClass::__construct(object|string $objectOrClass)`: an
/// object's class or the class named, its declared name kept as
/// `$name`.
///
/// ```php
/// $r = new ReflectionClass('reflectionexception'); var_dump($r, (new ReflectionClass(new ReflectionException))->name);
/// ```
/// ```expect
/// object(ReflectionClass)#1 (1) {
///   ["name"]=>
///   string(19) "ReflectionException"
/// }
/// string(19) "ReflectionException"
/// ```
///
/// ```php
/// new ReflectionClass('Nope');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Class "Nope" does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->__construct('Nope')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// The `TypeError` for an array, `must be of type object|string, array
/// given`, has no fence until the compiler reads an array expression.
///
/// ```php
/// new ReflectionClass(null);
/// ```
/// ```expect exit=255
/// Deprecated: ReflectionClass::__construct(): Passing null to parameter #1 ($objectOrClass) of type object|string is deprecated in %s on line 2
///
/// Fatal error: Uncaught ReflectionException: Class "" does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->__construct('')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new ReflectionClass();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionClass::__construct() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->__construct()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_class_construct(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 1)?;
    let (class, _) = class_argument(req, 1)?;
    let name = class_name_of(req, class);
    if let Some(this) = req.this() {
        set_name(req, this, &name)?;
    }
    Ok(Value::null())
}

/// `reflection_class_object_ctor` with `is_object` on,
/// `ReflectionObject::__construct(object $object)`: the object's class
/// as `$name`. php keeps the object too, for `hasProperty`'s dynamic
/// names and the `Dynamic properties` block of `__toString`; the
/// reflector here keeps the name alone until a native payload can
/// hold it, so those two read the class and not the instance.
///
/// ```php
/// var_dump(new ReflectionObject(new ReflectionException));
/// ```
/// ```expect
/// object(ReflectionObject)#1 (1) {
///   ["name"]=>
///   string(19) "ReflectionException"
/// }
/// ```
///
/// ```php
/// new ReflectionObject('x');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: ReflectionObject::__construct(): Argument #1 ($object) must be of type object, string given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionObject->__construct('x')
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_object_construct(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 1)?;
    let given = req.argument(1).unwrap_or_default().deref();
    let ValueRef::Object(object) = given.as_ref() else {
        return Err(arg_type_error(
            req,
            1,
            &format!("be of type object, {} given", given.type_name()),
        ));
    };
    let name = class_name_of(req, object.class());
    if let Some(this) = req.this() {
        set_name(req, this, &name)?;
    }
    Ok(Value::null())
}

/// `ReflectionClass::getName()`: `ce->name`. `ReflectionObject` has no
/// `getName` of its own and answers with this one, as php's inherited
/// function table does.
///
/// ```php
/// class C {} var_dump((new ReflectionClass('c'))->getName(), (new ReflectionObject(new C))->getName());
/// ```
/// ```expect
/// string(1) "C"
/// string(1) "C"
/// ```
///
/// ```php
/// ReflectionObject::getName();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Non-static method ReflectionClass::getName() cannot be called statically in %s:2
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// $r = new ReflectionClass('ReflectionException'); var_dump($r->getName(1));
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionClass::getName() expects exactly 0 arguments, 1 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->getName(1)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_class_get_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let name = class_name_of(req, class);
    string_value(req, &name)
}

/// `ReflectionClass::isInternal()`: `ce->type == ZEND_INTERNAL_CLASS`,
/// which for the declaration model is a class with no file.
///
/// ```php
/// class C {} var_dump((new ReflectionClass('C'))->isInternal(), (new ReflectionClass('ReflectionClass'))->isInternal(), (new ReflectionClass('C'))->isUserDefined(), (new ReflectionObject(new ReflectionProperty('Exception', 'message')))->isUserDefined());
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
fn reflection_class_is_internal(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    Ok(Value::bool(is_internal(req, class)))
}

/// `ReflectionClass::isUserDefined()`: `ce->type == ZEND_USER_CLASS`.
/// The witness is `isInternal`'s above.
fn reflection_class_is_user_defined(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    Ok(Value::bool(!is_internal(req, class)))
}

fn is_internal(req: &Request, class: ClassId) -> bool {
    req.class_decl(class)
        .is_none_or(|decl| decl.file.0.is_empty())
}

/// `ReflectionClass::getFileName()`: `ce->info.user.filename` for a
/// user class, `false` for an internal one. The three site accessors
/// share this witness; the name is the file's as the runner names
/// it, a path under the oracle and `witness.php` here.
///
/// ```php
/// $r = new ReflectionClass('C'); var_dump($r->getFileName(), $r->getStartLine(), $r->getEndLine()); $r = new ReflectionClass('Exception'); var_dump($r->getFileName(), $r->getStartLine(), $r->getEndLine()); class C { public $a; }
/// ```
/// ```expect
/// string(%d) "%s.php"
/// int(2)
/// int(2)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
fn reflection_class_get_file_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let file = req
        .class_decl(class)
        .filter(|decl| !decl.file.0.is_empty())
        .map(|decl| decl.file.0.to_vec());
    match file {
        Some(file) => string_value(req, &file),
        None => Ok(Value::bool(false)),
    }
}

/// `ReflectionClass::getStartLine()`: `ce->info.user.line_start`, or
/// `false` for an internal class. The witness is `getFileName`'s.
fn reflection_class_get_start_line(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    Ok(user_line(req, class, |decl| decl.line))
}

/// `ReflectionClass::getEndLine()`: `ce->info.user.line_end`, the line
/// of the closing brace, or `false` for an internal class. The witness
/// is `getFileName`'s.
fn reflection_class_get_end_line(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    Ok(user_line(req, class, |decl| decl.end_line))
}

fn user_line(req: &Request, class: ClassId, line: fn(&ClassDecl) -> u32) -> Value {
    req.class_decl(class)
        .filter(|decl| !decl.file.0.is_empty())
        .map_or(Value::bool(false), |decl| Value::int(i64::from(line(decl))))
}

/// `ReflectionClass::getExtension()`: `ce->info.internal.module` as a
/// `ReflectionExtension`, made as `reflection_extension_factory` makes
/// it, `object_init_ex` and the name written, no constructor served;
/// `null` for a user class. A class Zend declares itself is php's
/// `Core` `zend_module_entry`, which this engine has no `Extension` value for yet
/// (#320), so `Exception` answers `null` here and `Core` there; the
/// fence stays off that class until the bundle names it.
///
/// ```php
/// class C {} var_dump((new ReflectionClass('ReflectionException'))->getExtension(), (new ReflectionClass('ReflectionException'))->getExtensionName(), (new ReflectionClass('C'))->getExtension(), (new ReflectionClass('C'))->getExtensionName());
/// ```
/// ```expect
/// object(ReflectionExtension)#2 (1) {
///   ["name"]=>
///   string(10) "Reflection"
/// }
/// string(10) "Reflection"
/// NULL
/// bool(false)
/// ```
fn reflection_class_get_extension(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let Some(name) = class_extension_name(req, class) else {
        return Ok(Value::null());
    };
    let Some(reflector) = lookup_class(req, b"ReflectionExtension")? else {
        return Ok(Value::null());
    };
    let object = new_object(req, reflector)?;
    set_name(req, object, name.as_bytes())?;
    Ok(Value::object(object))
}

/// `ReflectionClass::getExtensionName()`: the extension's name, or `false`
/// for a user class. The witness is `getExtension`'s.
fn reflection_class_get_extension_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    match class_extension_name(req, class) {
        Some(name) => string_value(req, name.as_bytes()),
        None => Ok(Value::bool(false)),
    }
}

fn class_extension_name(req: &Request, class: ClassId) -> Option<String> {
    let extension = req.class_extension(class)?;
    req.extension_name(extension).map(str::to_owned)
}

/// `ReflectionClass::hasConstant(string $name)`:
/// `zend_hash_exists(&ce->constants_table, name)`, the table php fills
/// with the class's own constants and every parent's but the private
/// ones (`do_inherit_class_constant`). A scalar is the string it
/// coerces to, so `hasConstant(1)` asks for `"1"`.
///
/// ```php
/// class Foo { const c1 = 1; } $r = new ReflectionClass('Foo'); var_dump($r->hasConstant('c1'), $r->hasConstant('c2'), $r->hasConstant(1), (new ReflectionClass('ReflectionClass'))->hasConstant('IS_FINAL'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// ```
fn reflection_class_has_constant(req: &mut Request) -> Result<Value, Thrown> {
    let name = one_string_argument(req)?;
    let class = this_class(req)?;
    Ok(Value::bool(
        class_constant_named(req, class, &name).is_some(),
    ))
}

/// The constant `name` names in `ce->constants_table`: the class's own or
/// a parent's non-private one, with the class that declares it.
fn class_constant_named(
    req: &mut Request,
    class: ClassId,
    name: &[u8],
) -> Option<(ClassId, ClassConstDecl)> {
    for (depth, at) in class_chain(req, class).into_iter().enumerate() {
        let found =
            req.class_decl(at)?.constants.iter().find(|c| {
                &*c.name.0 == name && (depth == 0 || c.visibility != Visibility::Private)
            });
        if let Some(constant) = found {
            return Some((at, constant.clone()));
        }
    }
    None
}

/// `ReflectionClass::getConstant(string $name)`: the constant's value,
/// its expression evaluated in the declaring class
/// (`zend_update_class_constant`); a name the table has not is the 8.4
/// deprecation and `false`.
///
/// ```php
/// class Foo { const c1 = 1; } $r = new ReflectionClass('Foo'); var_dump($r->getConstant('c1'), (new ReflectionClass('ReflectionClass'))->getConstant('IS_FINAL')); var_dump($r->getConstant('c2'));
/// ```
/// ```expect
/// int(1)
/// int(32)
///
/// Deprecated: ReflectionClass::getConstant() for a non-existent constant is deprecated, use ReflectionClass::hasConstant() to check if the constant exists in %s on line 2
/// bool(false)
/// ```
fn reflection_class_get_constant(req: &mut Request) -> Result<Value, Thrown> {
    let name = one_string_argument(req)?;
    let class = this_class(req)?;
    if let Some((declaring, constant)) = class_constant_named(req, class, &name) {
        return eval_const_expr(req, &constant.value, Some(declaring));
    }
    deprecated(
        req,
        "ReflectionClass::getConstant() for a non-existent constant is deprecated, use ReflectionClass::hasConstant() to check if the constant exists",
    );
    Ok(Value::bool(false))
}

/// `ReflectionClass::hasProperty(string $name)`: the name in
/// `ce->properties_info`, the class's own properties and every
/// parent's, a parent's private one excluded as php's
/// `property_info->ce == ce` test excludes it. php then asks a
/// `ReflectionObject`'s instance for a dynamic property of that name;
/// the reflector here keeps no instance, so that branch is `false`
/// until it does.
///
/// ```php
/// class C { public $a; protected $b; private $c; public static $d; } $r = new ReflectionClass('C'); $e = new ReflectionClass('ReflectionException'); var_dump($r->hasProperty('a'), $r->hasProperty('b'), $r->hasProperty('c'), $r->hasProperty('d'), $r->hasProperty('e'), $r->hasProperty(1.5), $e->hasProperty('message'), $e->hasProperty('string'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
fn reflection_class_has_property(req: &mut Request) -> Result<Value, Thrown> {
    let name = one_string_argument(req)?;
    let class = this_class(req)?;
    Ok(Value::bool(property_named(req, class, &name).is_some()))
}

/// The property `name` names in the class's info table: the class's
/// own or a parent's non-private one, with the class that declares it.
fn property_named(
    req: &mut Request,
    class: ClassId,
    name: &[u8],
) -> Option<(ClassId, PropertyDecl)> {
    for (depth, at) in class_chain(req, class).into_iter().enumerate() {
        let found =
            req.class_decl(at)?.properties.iter().find(|p| {
                &*p.name.0 == name && (depth == 0 || p.visibility != Visibility::Private)
            });
        if let Some(property) = found {
            return Some((at, property.clone()));
        }
    }
    None
}

/// `ReflectionClass::getDefaultProperties()`: `add_class_vars` twice,
/// the `static` properties then the instance ones, each in
/// `properties_info` order, the class's own before the inherited, a
/// parent's private ones skipped, and a typed property with no
/// default skipped as its `UNDEF` slot is; each default is the
/// expression it was written as, evaluated in the declaring class.
///
/// ```php
/// class Foo { protected static $s = 'foo'; protected $i = 'foo'; public $n; } var_dump((new ReflectionClass('Foo'))->getDefaultProperties(), (new ReflectionClass('ReflectionException'))->getDefaultProperties());
/// ```
/// ```expect
/// array(3) {
///   ["s"]=>
///   string(3) "foo"
///   ["i"]=>
///   string(3) "foo"
///   ["n"]=>
///   NULL
/// }
/// array(4) {
///   ["message"]=>
///   string(0) ""
///   ["code"]=>
///   int(0)
///   ["file"]=>
///   string(0) ""
///   ["line"]=>
///   int(0)
/// }
/// ```
fn reflection_class_get_default_properties(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let mut defaults = PhpArray::new(req)?;
    let chain = class_chain(req, class);
    for statics in [true, false] {
        for (depth, &at) in chain.iter().enumerate() {
            let properties: Vec<PropertyDecl> = req
                .class_decl(at)
                .map(|decl| decl.properties.clone())
                .unwrap_or_default();
            for property in properties {
                if property.is_instance == statics
                    || (depth > 0 && property.visibility == Visibility::Private)
                {
                    continue;
                }
                let value = match &property.default {
                    Some(expr) => eval_const_expr(req, expr, Some(at))?,
                    None if property.ty.is_some() => continue,
                    None => Value::null(),
                };
                let key = Key::Str(PhpString::new(req, &property.name.0)?);
                if defaults.contains(&key) {
                    continue;
                }
                defaults.set(req, key, value)?;
            }
        }
    }
    Ok(Value::array(defaults))
}

/// `ReflectionClass::isSubclassOf(ReflectionClass|string $class)`:
/// `ce != class_ce && instanceof_function(ce, class_ce)`, so a class
/// is not a subclass of itself and is one of every parent and every
/// interface up the chain. A `ReflectionClass` argument is the class
/// it reflects; a string is looked up, a miss php's `ReflectionException`.
///
/// ```php
/// class A {} $a = new ReflectionClass('A'); $e = new ReflectionClass('ReflectionException'); var_dump($a->isSubclassOf($a), $e->isSubclassOf('exception'), $e->isSubclassOf(new ReflectionClass('Throwable')), $e->isSubclassOf('Stringable'), $e->isSubclassOf('ReflectionException'), $a->isSubclassOf('ReflectionException'));
/// ```
/// ```expect
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
///
/// ```php
/// class A {} (new ReflectionClass('A'))->isSubclassOf('X');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Class "X" does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->isSubclassOf('X')
/// #1 {main}
///   thrown in %s on line 2
/// ```
/// The `TypeError` for an array has no fence until the compiler reads
/// an array expression.
fn reflection_class_is_subclass_of(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 1)?;
    let class = this_class(req)?;
    let given = req.argument(1).unwrap_or_default().deref();
    let of = match given.as_ref() {
        ValueRef::Object(object) => {
            let reflector = lookup_class(req, b"ReflectionClass")?;
            if !reflector.is_some_and(|reflector| object.instance_of(req, reflector)) {
                let name = class_name_of(req, object.class());
                return Err(arg_type_error(
                    req,
                    1,
                    &format!(
                        "be of type ReflectionClass|string, {} given",
                        String::from_utf8_lossy(&name)
                    ),
                ));
            }
            let Some(name) = object
                .prop(req, b"name")
                .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
            else {
                return Err(throw(
                    req,
                    "Error",
                    "Internal error: Failed to retrieve the argument's reflection object",
                ));
            };
            class_named(req, &name)?
        }
        ValueRef::Array(_) | ValueRef::Resource(_) => {
            return Err(arg_type_error(
                req,
                1,
                &format!(
                    "be of type ReflectionClass|string, {} given",
                    given.type_name()
                ),
            ));
        }
        _ => {
            let name = string_argument(req, 1)?;
            class_named(req, &name)?
        }
    };
    Ok(Value::bool(class != of && class_is(req, class, of, 0)))
}

/// `ReflectionClass::inNamespace()`: whether `ce->name` has a `\`.
///
/// ```php
/// $r = new ReflectionClass('ReflectionException'); var_dump($r->inNamespace(), $r->getNamespaceName(), $r->getShortName());
/// ```
/// ```expect
/// bool(false)
/// string(0) ""
/// string(19) "ReflectionException"
/// ```
fn reflection_class_in_namespace(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let name = class_name_of(req, class);
    Ok(Value::bool(namespace_end(&name).is_some()))
}

/// `ReflectionClass::getNamespaceName()`: the name up to its last `\`,
/// empty without one. The witness is `inNamespace`'s.
fn reflection_class_get_namespace_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let name = class_name_of(req, class);
    let end = namespace_end(&name).unwrap_or(0);
    string_value(req, &name[..end])
}

/// `ReflectionClass::getShortName()`: the name after its last `\`, the
/// whole name without one. The witness is `inNamespace`'s.
fn reflection_class_get_short_name(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 0)?;
    let class = this_class(req)?;
    let name = class_name_of(req, class);
    let start = namespace_end(&name).map_or(0, |end| end + 1);
    string_value(req, &name[start..])
}

/// `ReflectionClass::getAttributes(?string $name = null, int $flags =
/// 0)`: `reflect_attributes` over `ce->attributes`, an empty array
/// for a class with none. A class that has some needs
/// `ReflectionAttribute`, which no crate declares yet, so the answer
/// is php's `ReflectionException` for that class rather than a wrong list.
///
/// ```php
/// class C {} var_dump((new ReflectionClass('C'))->getAttributes(), (new ReflectionClass('ReflectionClass'))->getAttributes());
/// ```
/// ```expect
/// array(0) {
/// }
/// array(0) {
/// }
/// ```
///
/// ```php
/// (new ReflectionClass('ReflectionException'))->getAttributes(1, 2, 3);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionClass::getAttributes() expects at most 2 arguments, 3 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionClass->getAttributes(1, 2, 3)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_class_get_attributes(req: &mut Request) -> Result<Value, Thrown> {
    at_most_arguments(req, 2)?;
    let class = this_class(req)?;
    let attributes = req
        .class_decl(class)
        .map_or(0, |decl| decl.attributes.len());
    if attributes > 0 {
        return Err(throw(
            req,
            "ReflectionException",
            "Class \"ReflectionAttribute\" does not exist",
        ));
    }
    Ok(Value::array(PhpArray::new(req)?))
}

// -------------------------------------------------- ReflectionProperty

/// `ReflectionProperty::__construct(object|string $class, string
/// $property)`: the property in the class's info table, or a dynamic
/// one on the object given, else php's `ReflectionException`; `$name` is the name
/// and `$class` the declaring class, `property_info->ce->name`.
///
/// ```php
/// var_dump(new ReflectionProperty('ReflectionException', 'message'));
/// ```
/// ```expect
/// object(ReflectionProperty)#1 (2) {
///   ["name"]=>
///   string(7) "message"
///   ["class"]=>
///   string(9) "Exception"
/// }
/// ```
///
/// ```php
/// new ReflectionProperty('ReflectionException', 'string');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Property ReflectionException::$string does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionProperty->__construct('ReflectionExcep...', 'string')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new ReflectionProperty('Nope', 'x');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Class "Nope" does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionProperty->__construct('Nope', 'x')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new ReflectionProperty('ReflectionException');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionProperty::__construct() expects exactly 2 arguments, 1 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionProperty->__construct('ReflectionExcep...')
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_property_construct(req: &mut Request) -> Result<Value, Thrown> {
    exactly_arguments(req, 2)?;
    let (class, object) = class_argument(req, 1)?;
    let name = string_argument(req, 2)?;
    let declaring = match property_named(req, class, &name) {
        Some((declaring, _)) => declaring,
        None if object.is_some_and(|o| o.prop(req, &name).is_some()) => class,
        None => {
            let class = class_name_of(req, class);
            return Err(throw(
                req,
                "ReflectionException",
                &format!(
                    "Property {}::${} does not exist",
                    String::from_utf8_lossy(&class),
                    String::from_utf8_lossy(&name)
                ),
            ));
        }
    };
    let declaring = class_name_of(req, declaring);
    if let Some(this) = req.this() {
        set_name(req, this, &name)?;
        set_string_prop(req, this, b"class", &declaring)?;
    }
    Ok(Value::null())
}

/// The property a `ReflectionProperty` reflects: its declaring class
/// (`$class`), its name, and its declaration where the class has one,
/// none for a dynamic property.
fn this_property(req: &mut Request) -> Result<(ClassId, Vec<u8>, Option<PropertyDecl>), Thrown> {
    let (this, name) = this_name(req, "ReflectionProperty")?;
    let Some(class) = this
        .prop(req, b"class")
        .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
    else {
        return Err(throw(
            req,
            "Error",
            "Typed property ReflectionProperty::$class must not be accessed before initialization",
        ));
    };
    let class = class_named(req, &class)?;
    let declared = property_named(req, class, &name).map(|(_, decl)| decl);
    Ok((class, name, declared))
}

/// `ReflectionProperty::getValue(?object $object = null)`: an instance
/// property read off the object given, which must be of the declaring
/// class; the read is the slot or the dynamic table by name, php's
/// `read_property` under `fake_scope`, so visibility is no bar. A
/// slot nothing wrote holds the declared default in php, placed there
/// by `object_properties_init`; an object of a class that inherits the
/// property has no such slot here until the linker places inherited
/// properties, so the default is evaluated in its stead, and a typed
/// property with none is php's `Error` for a read before
/// initialization. A `static` property is
/// `zend_read_static_property_ex`, which needs the `static` table
/// `api::call::read_static_prop` does not link yet, so the answer is a
/// `ReflectionException` naming that and not a value.
///
/// ```php
/// class T { protected $p = 2; } $o = new T; var_dump((new ReflectionProperty($o, 'p'))->getValue($o), (new ReflectionProperty('Exception', 'message'))->getValue(new ReflectionException), (new ReflectionProperty('Exception', 'code'))->getValue(new Exception));
/// ```
/// ```expect
/// int(2)
/// string(0) ""
/// int(0)
/// ```
///
/// ```php
/// (new ReflectionProperty('Exception', 'message'))->getValue();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: ReflectionProperty::getValue(): Argument #1 ($object) must be provided for instance properties in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionProperty->getValue()
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// class T { protected $p = 2; } (new ReflectionProperty('Exception', 'message'))->getValue(new T);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Given object is not an instance of the class this property was declared in in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionProperty->getValue(Object(T))
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// (new ReflectionProperty('Exception', 'message'))->getValue(1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: ReflectionProperty::getValue(): Argument #1 ($object) must be of type ?object, int given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionProperty->getValue(1)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_property_get_value(req: &mut Request) -> Result<Value, Thrown> {
    at_most_arguments(req, 1)?;
    let (class, name, declared) = this_property(req)?;
    if declared.as_ref().is_some_and(|decl| !decl.is_instance) {
        return Err(static_property_unlinked(req, class, &name));
    }
    let given = req.argument(1).unwrap_or_default().deref();
    let object = match given.as_ref() {
        ValueRef::Object(object) => object,
        ValueRef::Null | ValueRef::Undef => {
            return Err(arg_type_error(
                req,
                1,
                "be provided for instance properties",
            ));
        }
        _ => {
            return Err(arg_type_error(
                req,
                1,
                &format!("be of type ?object, {} given", given.type_name()),
            ));
        }
    };
    if !object.instance_of(req, class) {
        return Err(throw(
            req,
            "ReflectionException",
            "Given object is not an instance of the class this property was declared in",
        ));
    }
    match object.prop(req, &name) {
        Some(value) => {
            let value = value.deref();
            req.addref(&value);
            Ok(value)
        }
        None if declared.as_ref().is_some_and(|decl| decl.default.is_some()) => {
            let default = declared
                .and_then(|decl| decl.default)
                .unwrap_or(ConstExpr::Null);
            eval_const_expr(req, &default, Some(class))
        }
        None if declared.as_ref().is_some_and(|decl| decl.ty.is_some()) => {
            let class = class_name_of(req, class);
            Err(throw(
                req,
                "Error",
                &format!(
                    "Typed property {}::${} must not be accessed before initialization",
                    String::from_utf8_lossy(&class),
                    String::from_utf8_lossy(&name)
                ),
            ))
        }
        None => Ok(Value::null()),
    }
}

/// The `ReflectionException` a `static` property's value is until the
/// `static` table is linked: php reads `ce->default_static_members_table`
/// here and this engine has none to read (`bug30146.phpt` waits on it).
fn static_property_unlinked(req: &mut Request, class: ClassId, name: &[u8]) -> Thrown {
    let class = class_name_of(req, class);
    throw(
        req,
        "ReflectionException",
        &format!(
            "Static property {}::${} has no value to read: static properties are not linked yet",
            String::from_utf8_lossy(&class),
            String::from_utf8_lossy(name)
        ),
    )
}

/// `ReflectionProperty::setValue(mixed $objectOrValue, mixed $value)`:
/// an instance property written on the object given through
/// `write_property` under `fake_scope`, one more owner of the value
/// taken for the slot. A `static` property takes php's two 8.3
/// deprecations for the one-argument and the non-object forms and
/// then, as `getValue` does, the `ReflectionException` for the table not linked.
///
/// ```php
/// class T { public $p = 1; } $o = new T; $r = new ReflectionProperty($o, 'p'); $r->setValue($o, 'two'); var_dump($r->getValue($o), $o->p);
/// ```
/// ```expect
/// string(3) "two"
/// string(3) "two"
/// ```
///
/// ```php
/// class T { public $p = 1; } (new ReflectionProperty('T', 'p'))->setValue(new T);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionProperty::setValue() expects exactly 2 arguments, 1 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionProperty->setValue(Object(T))
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// (new ReflectionProperty('Exception', 'message'))->setValue(1, 2);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: ReflectionProperty::setValue(): Argument #1 ($objectOrValue) must be of type object, int given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionProperty->setValue(1, 2)
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_property_set_value(req: &mut Request) -> Result<Value, Thrown> {
    let (class, name, declared) = this_property(req)?;
    if declared.as_ref().is_some_and(|decl| !decl.is_instance) {
        let count = req.argument_count();
        if count == 1 {
            deprecated(
                req,
                "Calling ReflectionProperty::setValue() with a single argument is deprecated",
            );
        } else {
            exactly_arguments(req, 2)?;
            let first = req.argument(1).unwrap_or_default().deref();
            if !matches!(first.as_ref(), ValueRef::Null | ValueRef::Object(_)) {
                deprecated(
                    req,
                    "Calling ReflectionProperty::setValue() with a 1st argument which is not null or an object is deprecated",
                );
            }
        }
        return Err(static_property_unlinked(req, class, &name));
    }
    exactly_arguments(req, 2)?;
    let given = req.argument(1).unwrap_or_default().deref();
    let ValueRef::Object(object) = given.as_ref() else {
        return Err(arg_type_error(
            req,
            1,
            &format!("be of type object, {} given", given.type_name()),
        ));
    };
    let value = req.argument(2).unwrap_or_default().deref();
    req.addref(&value);
    let key = PhpString::new(req, &name)?;
    object.set_prop(req, key, value)?;
    Ok(Value::null())
}

// -------------------------------------------------- ReflectionConstant

/// `ReflectionConstant::__construct(string $name)`: the constant under
/// the name with its namespace lowered and a leading `\` dropped,
/// `zend_get_constant_ptr`; a miss is php's `ReflectionException` with the name as
/// given, and `$name` keeps the spelling given too.
///
/// ```php
/// var_dump(new ReflectionConstant('\PHP_VERSION'), (new ReflectionConstant('PHP_EOL'))->name);
/// ```
/// ```expect
/// object(ReflectionConstant)#1 (1) {
///   ["name"]=>
///   string(12) "\PHP_VERSION"
/// }
/// string(7) "PHP_EOL"
/// ```
///
/// ```php
/// new ReflectionConstant('NOPE');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ReflectionException: Constant "NOPE" does not exist in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionConstant->__construct('NOPE')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// new ReflectionConstant();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: ReflectionConstant::__construct() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): ReflectionConstant->__construct()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn reflection_constant_construct(req: &mut Request) -> Result<Value, Thrown> {
    let name = one_string_argument(req)?;
    let bare = name.strip_prefix(b"\\").unwrap_or(&name);
    let mut lowered = bare.to_vec();
    if let Some(end) = namespace_end(bare) {
        lowered[..end].make_ascii_lowercase();
    }
    if constant(req, &lowered)?.is_none() {
        return Err(throw(
            req,
            "ReflectionException",
            &format!(
                "Constant \"{}\" does not exist",
                String::from_utf8_lossy(&name)
            ),
        ));
    }
    if let Some(this) = req.this() {
        set_name(req, this, &name)?;
    }
    Ok(Value::null())
}

/// `ReflectionConstant::getAttributes(?string $name = null, int $flags
/// = 0)`: `reflect_attributes` over `const_->attributes`. A constant
/// declaration (`ConstDecl`) carries no attributes, and no constant
/// this engine declares has one in php either (`E_STRICT`, the one
/// with `#[\Deprecated]`, is not declared), so the answer is the empty
/// array; `ReflectionConstant_getAttributes_internal.phpt` waits on
/// both.
///
/// ```php
/// var_dump((new ReflectionConstant('PHP_EOL'))->getAttributes());
/// ```
/// ```expect
/// array(0) {
/// }
/// ```
fn reflection_constant_get_attributes(req: &mut Request) -> Result<Value, Thrown> {
    at_most_arguments(req, 2)?;
    this_name(req, "ReflectionConstant")?;
    Ok(Value::array(PhpArray::new(req)?))
}

/// `_extension_string`: the header with the extension's number and
/// version, then the blocks it has something for. Every bundled
/// extension is `<persistent>`, as one compiled in is, and its version
/// is `PHP_VERSION`, as `PHP_CTYPE_VERSION` and its kin are.
fn extension_string(req: &Request, extension: ExtensionId, indent: &str) -> String {
    let mut out = String::new();
    let name = req.extension_name(extension).unwrap_or_default();
    let _ = writeln!(
        out,
        "{indent}Extension [ <persistent> extension #{} {name} version {PHP_VERSION} ] {{",
        req.extension_number(extension)
    );
    let functions = req.extension_functions(extension);
    if !functions.is_empty() {
        out.push_str("\n  - Functions {\n");
        for function in functions {
            out.push_str(&function_string(req, function, "    "));
        }
        let _ = writeln!(out, "{indent}  }}");
    }
    let _ = writeln!(out, "{indent}}}");
    out
}

/// `_function_string` with no scope: `Function [ <internal:ext> function
/// name ] {`, its parameters and its return row, each line under
/// `indent`. A function with the `Deprecated` attribute carries
/// `, deprecated` after `<internal`, as `ZEND_ACC_DEPRECATED` does.
fn function_string(req: &Request, function: Function, indent: &str) -> String {
    let mut out = String::new();
    let Some(decl) = req.function_decl(function) else {
        return out;
    };
    let _ = write!(out, "{indent}Function [ <internal");
    if decl
        .attributes
        .iter()
        .any(|a| a.name.0.eq_ignore_ascii_case(b"Deprecated"))
    {
        out.push_str(", deprecated");
    }
    if let Some(extension) = req
        .function_extension(function)
        .and_then(|e| req.extension_name(e))
    {
        let _ = write!(out, ":{extension}");
    }
    out.push_str("> function ");
    if decl.returns_by_ref {
        out.push('&');
    }
    let _ = writeln!(out, "{} ] {{", String::from_utf8_lossy(&decl.name.0));
    let param_indent = format!("{indent}  ");
    function_parameter_string(&mut out, decl, &param_indent);
    if let Some(returns) = &decl.return_type {
        let _ = writeln!(out, "  {indent}- Return [ {} ]", type_to_string(returns));
    }
    let _ = writeln!(out, "{indent}}}");
    out
}

/// `_function_parameter_string`: `- Parameters [N] {` and one
/// `_parameter_string` line per parameter, a required one being every
/// parameter before the first with a default or the variadic one.
fn function_parameter_string(out: &mut String, decl: &FunctionDecl, indent: &str) {
    let required = decl
        .parameters
        .iter()
        .position(|p| p.default.is_some() || p.variadic)
        .unwrap_or(decl.parameters.len());
    out.push('\n');
    let _ = writeln!(out, "{indent}- Parameters [{}] {{", decl.parameters.len());
    for (offset, parameter) in decl.parameters.iter().enumerate() {
        let _ = write!(out, "{indent}  Parameter #{offset} [ ");
        out.push_str(if offset < required {
            "<required> "
        } else {
            "<optional> "
        });
        if let Some(ty) = &parameter.ty {
            let _ = write!(out, "{} ", type_to_string(ty));
        }
        if parameter.by_ref {
            out.push('&');
        }
        if parameter.variadic {
            out.push_str("...");
        }
        let _ = write!(out, "${}", String::from_utf8_lossy(&parameter.name.0));
        if offset >= required && !parameter.variadic {
            out.push_str(" = ");
            out.push_str(
                &parameter
                    .default
                    .as_ref()
                    .map_or_else(|| String::from("<default>"), const_expr_string),
            );
        }
        out.push_str(" ]\n");
    }
    let _ = writeln!(out, "{indent}}}");
}

/// A native parameter's default as `gen_stub.php` writes it into
/// `zend_internal_arg_info.default_value`: the expression's source
/// text, which for a scalar is the scalar as written.
fn const_expr_string(expr: &php_ext::decl::ConstExpr) -> String {
    use php_ext::decl::ConstExpr;
    match expr {
        ConstExpr::Null => String::from("null"),
        ConstExpr::Bool(b) => String::from(if *b { "true" } else { "false" }),
        ConstExpr::Long(n) => n.to_string(),
        ConstExpr::Double(f) => f.to_string(),
        ConstExpr::Str(s) => format!("'{}'", String::from_utf8_lossy(s)),
        ConstExpr::Constant(name) => String::from_utf8_lossy(&name.0).into_owned(),
        ConstExpr::Array(elements) if elements.is_empty() => String::from("[]"),
        _ => String::from("<default>"),
    }
}

/// `zend_type_to_string`: class names first, then the scalar and
/// special types in `zend_type_to_string_resolved`'s order, `|` between; `mixed` alone
/// for the any-mask; a single type with `null` as `?T`, a union with it
/// as `|null` last; an intersection with `&`, bracketed inside a union.
fn type_to_string(ty: &Type) -> String {
    match ty {
        Type::Mixed => return String::from("mixed"),
        Type::Intersection(members) => {
            return members
                .iter()
                .map(type_to_string)
                .collect::<Vec<_>>()
                .join("&");
        }
        _ => {}
    }
    let members: Vec<&Type> = match ty {
        Type::Union(members) => members.iter().collect(),
        Type::Nullable(inner) => vec![inner.as_ref(), &Type::Null],
        other => vec![other],
    };
    let mut parts: Vec<String> = Vec::new();
    for member in &members {
        match member {
            Type::Named(name) => parts.push(String::from_utf8_lossy(&name.0).into_owned()),
            Type::SelfClass => parts.push(String::from("self")),
            Type::ParentClass => parts.push(String::from("parent")),
            Type::Intersection(inner) => parts.push(format!(
                "({})",
                inner
                    .iter()
                    .map(type_to_string)
                    .collect::<Vec<_>>()
                    .join("&")
            )),
            _ => {}
        }
    }
    let has = |wanted: &Type| members.contains(&wanted);
    if has(&Type::CalledClass) {
        parts.push(String::from("static"));
    }
    for (member, word) in [
        (Type::Callable, "callable"),
        (Type::Object, "object"),
        (Type::Array, "array"),
        (Type::String, "string"),
        (Type::Int, "int"),
        (Type::Float, "float"),
    ] {
        if has(&member) {
            parts.push(String::from(word));
        }
    }
    if has(&Type::Iterable) {
        parts.push(String::from("iterable"));
    }
    if has(&Type::Bool) || (has(&Type::False) && has(&Type::True)) {
        parts.push(String::from("bool"));
    } else if has(&Type::False) {
        parts.push(String::from("false"));
    } else if has(&Type::True) {
        parts.push(String::from("true"));
    }
    for (member, word) in [(Type::Void, "void"), (Type::Never, "never")] {
        if has(&member) {
            parts.push(String::from(word));
        }
    }
    if has(&Type::Null) {
        let joined = parts.join("|");
        if !parts.is_empty() && !joined.contains('|') && !joined.contains('&') {
            return format!("?{joined}");
        }
        parts.push(String::from("null"));
    }
    parts.join("|")
}

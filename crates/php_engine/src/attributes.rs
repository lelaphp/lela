//! The classes `zend_register_attribute_ce` registers
//! (`Z/Zend/zend_attributes.c:575-607` at the tag), each as
//! `zend_attributes.stub.php` declares it: `Attribute` with its
//! `TARGET_*` and `IS_REPEATABLE` constants, the seven attributes php
//! marks internal, and `SensitiveParameterValue`, which is no attribute
//! and the value a trace shows in place of a parameter
//! `#[\SensitiveParameter]` marks. Every attribute carries its own
//! `#[Attribute(...)]` marker, the one `zend_mark_internal_attribute`
//! reads its targets from and `ReflectionAttribute::newInstance` reads
//! them from too.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_unit::decl::{
    Attribute, AttributeArgument, ClassConstDecl, ClassDecl, ClassFlags, ClassKind, ConstExpr,
    DynamicProperties, EnumBacking, FunctionDecl, MethodDecl, Name, Parameter, ParameterDefault,
    PropertyDecl, Type, Visibility,
};

use php_unit::Level;

use crate::api::abi::{Entry, NativeClassDecl, ObjectHandlers, PropertyPurpose};
use crate::api::call::{Function, eval_const_expr, instantiate_named};
use crate::api::coerce::argument;
use crate::api::raising::{Thrown, arg_count_error, raise, throw};
use crate::api::request::Request;
use crate::api::values::{ClassId, PhpArray, PhpObject, PhpString, Value};

/// `fn_flags & (ZEND_ACC_DEPRECATED | ZEND_ACC_NODISCARD)` of a
/// function, and `CONST_DEPRECATED` of a constant: what the
/// `#[\Deprecated]` and `#[\NoDiscard]` attributes among a declaration's
/// attributes raise, decoded once where the declaration is linked or
/// registered, so a call and a fetch read a bit and never the
/// attributes. php's compiler raises `ZEND_ACC_DEPRECATED` from the
/// attribute's presence (`zend_compile_func_decl`) and
/// `validate_nodiscard` raises `ZEND_ACC_NODISCARD`; `gen_stub.php`
/// writes both from a native's stub.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Flags {
    pub(crate) deprecated: bool,
    pub(crate) no_discard: bool,
}

impl Flags {
    /// The flags `attributes` raise, each attribute by its resolved name
    /// as `zend_get_attribute_str` finds it.
    pub(crate) fn of(attributes: &[Attribute]) -> Flags {
        Flags {
            deprecated: find(attributes, b"Deprecated").is_some(),
            no_discard: find(attributes, b"NoDiscard").is_some(),
        }
    }
}

/// `zend_get_attribute_str`: the attribute of a lowered name among a
/// declaration's, its name as resolved, a leading `\` dropped.
fn find<'a>(attributes: &'a [Attribute], name: &[u8]) -> Option<&'a Attribute> {
    attributes.iter().find(|one| {
        let written = one.name.0.strip_prefix(b"\\").unwrap_or(&one.name.0);
        written.eq_ignore_ascii_case(name)
    })
}

/// `zend_get_attribute_object` with no call frame of its own, over the
/// internal attribute `class` names among `attributes`, and the string
/// each of its `properties` holds: `None` where the list
/// carries no such attribute or one with no argument, which is the
/// empty suffix php reads the two as; the object's constructor is the
/// one that holds each argument to its `?string` under the caller's
/// `strict_types`, and what it threw is the `Err`.
fn internal_strings(
    req: &mut Request,
    attributes: &[Attribute],
    class: &[u8],
    properties: &[&[u8]],
    scope: Option<ClassId>,
) -> Result<Option<Vec<Vec<u8>>>, Thrown> {
    let Some(attribute) = find(attributes, class).filter(|one| !one.arguments.is_empty()) else {
        return Ok(None);
    };
    let Some(class) = req.class_named(class) else {
        return Ok(None);
    };
    let mut positional = Vec::new();
    let mut named = Vec::new();
    for argument in &attribute.arguments {
        let value = eval_const_expr(req, &argument.value, scope)?;
        match &argument.name {
            Some(name) => named.push((PhpString::new(req, &name.0)?, value)),
            None => positional.push(value),
        }
    }
    let made = instantiate_named(req, class, &positional, &named);
    for value in positional {
        req.release(value);
    }
    for (name, value) in named {
        req.release(Value::string(name));
        req.release(value);
    }
    let object = made?;
    let read = properties
        .iter()
        .map(|name| {
            object
                .prop(req, name)
                .and_then(|value| value.as_str_bytes().map(<[u8]>::to_vec))
                .unwrap_or_default()
        })
        .collect();
    req.release(object.as_value());
    Ok(Some(read))
}

/// `Method C::m()` or `Function f()`, as `zend_deprecated_function` and
/// `zend_nodiscard_function` name the callee: the class that declares
/// it, `common.scope`, and the name it was declared under.
fn callee_named(req: &Request, function: Function, method: &str, function_word: &str) -> String {
    let name = req.declared_name(function);
    match req.method_class(function) {
        Some(class) => format!("{method} {}::{name}()", req.class_name(class)),
        None => format!("{function_word} {name}()"),
    }
}

/// `zend_deprecated_function` (`Z/Zend/zend_execute.c:1866`), raised
/// where `fn_flags` carries `ZEND_ACC_DEPRECATED`: by `DO_FCALL` and
/// `DO_FCALL_BY_NAME` before the callee runs and by
/// `zend_call_function` before its arguments are placed. The suffix is
/// the `Deprecated` object's `since` and `message`, the object made
/// from the attribute's arguments in the callee's scope
/// (`get_deprecation_suffix_from_attribute`), and the level
/// `E_DEPRECATED` for a native and `E_USER_DEPRECATED` for a program's.
/// What the raise left pending, or the object's constructor threw, is
/// the `Err`.
///
/// ```php
/// #[\Deprecated]
/// function test() {}
/// #[\Deprecated("use test() instead", since: "1.0")]
/// function test2() {}
/// class Clazz {
///     #[\Deprecated] function m() {}
///     #[\Deprecated(since: "2.0")] static function s() {}
/// }
/// test();
/// test2();
/// (new Clazz)->m();
/// Clazz::s();
/// call_user_func('test');
/// set_error_handler(function ($errno, $errstr) { var_dump($errno === E_USER_DEPRECATED); });
/// test();
/// ```
/// ```expect
/// Deprecated: Function test() is deprecated in %s on line 10
///
/// Deprecated: Function test2() is deprecated since 1.0, use test() instead in %s on line 11
///
/// Deprecated: Method Clazz::m() is deprecated in %s on line 12
///
/// Deprecated: Method Clazz::s() is deprecated since 2.0 in %s on line 13
///
/// Deprecated: Function test() is deprecated in %s on line 14
/// bool(true)
/// ```
///
/// The message is the object's, so an argument is held to `?string`
/// under the file's `strict_types`, and a handler that throws stops the
/// call before the callee runs:
///
/// ```php
/// #[\Deprecated(1234)]
/// function test() { echo "not run\n"; }
/// test();
/// set_error_handler(function ($errno, $errstr) { throw new Exception($errstr); });
/// try { test(); } catch (Exception $e) { echo "Caught: ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Deprecated: Function test() is deprecated, 1234 in %s on line 4
/// not run
/// Caught: Function test() is deprecated, 1234
/// ```
pub(crate) fn deprecated_function(req: &mut Request, function: Function) -> Result<(), Thrown> {
    let attributes = req
        .function_decl(function)
        .map(|decl| decl.attributes.clone())
        .unwrap_or_default();
    let scope = req.method_class(function);
    let suffix = match internal_strings(
        req,
        &attributes,
        b"Deprecated",
        &[b"message", b"since"],
        scope,
    )? {
        Some(read) => joined_suffix(&read[0], &read[1]),
        None => String::new(),
    };
    let level = if req.user_function(function).is_some() {
        Level::UserDeprecated
    } else {
        Level::Deprecated
    };
    let named = callee_named(req, function, "Method", "Function");
    raise(req, level, &format!("{named} is deprecated{suffix}"));
    pending(req)
}

/// `zend_nodiscard_function` (`Z/Zend/zend_execute.c:1947`), raised by
/// `DO_FCALL` and `DO_FCALL_BY_NAME` where `fn_flags` carries
/// `ZEND_ACC_NODISCARD` and the opline's `result` operand is `UNUSED`: php's
/// warning, `E_USER_WARNING` for a program's function, with the
/// `NoDiscard` object's `message` after it.
///
/// ```php
/// #[\NoDiscard]
/// function test(): int { return 0; }
/// #[\NoDiscard("this is important")]
/// function test2(): int { return 0; }
/// class Clazz { #[\NoDiscard] static function m(): int { return 0; } }
/// test();
/// test2();
/// Clazz::m();
/// $x = test();
/// (void) test();
/// call_user_func('test');
/// ```
/// ```expect
/// Warning: The return value of function test() should either be used or intentionally ignored by casting it as (void) in %s on line 7
///
/// Warning: The return value of function test2() should either be used or intentionally ignored by casting it as (void), this is important in %s on line 8
///
/// Warning: The return value of method Clazz::m() should either be used or intentionally ignored by casting it as (void) in %s on line 9
///
/// Warning: The return value of function test() should either be used or intentionally ignored by casting it as (void) in %s on line 12
/// ```
pub(crate) fn no_discard_function(req: &mut Request, function: Function) -> Result<(), Thrown> {
    let attributes = req
        .function_decl(function)
        .map(|decl| decl.attributes.clone())
        .unwrap_or_default();
    let scope = req.method_class(function);
    let suffix = match internal_strings(req, &attributes, b"NoDiscard", &[b"message"], scope)? {
        Some(read) if !read[0].is_empty() => {
            format!(", {}", String::from_utf8_lossy(&read[0]))
        }
        _ => String::new(),
    };
    let level = if req.user_function(function).is_some() {
        Level::UserWarning
    } else {
        Level::Warning
    };
    let named = callee_named(req, function, "method", "function");
    raise(
        req,
        level,
        &format!(
            "The return value of {named} should either be used or intentionally ignored by casting it as (void){suffix}"
        ),
    );
    pending(req)
}

/// `zend_deprecated_class_constant` (`Z/Zend/zend_execute.c:1966`):
/// `Constant C::K is deprecated`, `Enum case` for a case, with the
/// `Deprecated` object's suffix read in the declaring class's scope,
/// `E_USER_DEPRECATED` for a program's class.
///
/// ```php
/// class Clazz {
///     #[\Deprecated] const A = 1;
///     #[\Deprecated("use B", since: "1.0")] const B = 2;
///     #[\Deprecated(self::C)] const C = "from itself";
/// }
/// enum E { #[\Deprecated] case Test; }
/// echo Clazz::A, Clazz::B, Clazz::C, "\n";
/// var_dump(E::Test, constant('Clazz::A'));
/// ```
/// ```expect
/// Deprecated: Constant Clazz::A is deprecated in %s on line 8
/// 1
/// Deprecated: Constant Clazz::B is deprecated since 1.0, use B in %s on line 8
/// 2
/// Deprecated: Constant Clazz::C is deprecated, from itself in %s on line 8
/// from itself
///
/// Deprecated: Enum case E::Test is deprecated in %s on line 9
///
/// Deprecated: Constant Clazz::A is deprecated in %s on line 9
/// enum(E::Test)
/// int(1)
/// ```
pub(crate) fn deprecated_class_constant(
    req: &mut Request,
    declaring: ClassId,
    name: &[u8],
    constant: &php_unit::decl::ClassConstDecl,
    native: bool,
) -> Result<(), Thrown> {
    let suffix = match internal_strings(
        req,
        &constant.attributes,
        b"Deprecated",
        &[b"message", b"since"],
        Some(declaring),
    )? {
        Some(read) => joined_suffix(&read[0], &read[1]),
        None => String::new(),
    };
    let level = if native {
        Level::Deprecated
    } else {
        Level::UserDeprecated
    };
    let what = if constant.is_case {
        "Enum case"
    } else {
        "Constant"
    };
    let message = format!(
        "{what} {}::{} is deprecated{suffix}",
        req.class_name(declaring),
        String::from_utf8_lossy(name)
    );
    raise(req, level, &message);
    pending(req)
}

/// `get_deprecation_suffix_from_attribute` (`Z/Zend/zend_execute.c:1801`):
/// what php prints after `is deprecated` for a declaration carrying
/// `attributes`, read off the `Deprecated` object their `#[\Deprecated]`
/// makes in `scope`; empty where the attribute takes no argument or
/// is not there. What the object's constructor threw is the `Err`.
pub(crate) fn deprecation_suffix(
    req: &mut Request,
    attributes: &[Attribute],
    scope: Option<ClassId>,
) -> Result<String, Thrown> {
    Ok(
        match internal_strings(
            req,
            attributes,
            b"Deprecated",
            &[b"message", b"since"],
            scope,
        )? {
            Some(read) => joined_suffix(&read[0], &read[1]),
            None => String::new(),
        },
    )
}

/// What php prints after `is deprecated`: ` since <since>` and
/// `, <message>`, each only where it is no empty string.
fn joined_suffix(message: &[u8], since: &[u8]) -> String {
    let mut out = String::new();
    if !since.is_empty() {
        out.push_str(" since ");
        out.push_str(&String::from_utf8_lossy(since));
    }
    if !message.is_empty() {
        out.push_str(", ");
        out.push_str(&String::from_utf8_lossy(message));
    }
    out
}

/// `if (UNEXPECTED(EG(exception)))` after a raise: a handler that
/// threw is the `Err`, the throwable left pending.
fn pending(req: &Request) -> Result<(), Thrown> {
    if req.is_pending() {
        return Err(Thrown::of(None));
    }
    Ok(())
}

/// `ZEND_ATTRIBUTE_TARGET_*` and `ZEND_ATTRIBUTE_IS_REPEATABLE`
/// (`Z/Zend/zend_attributes.h:26-35`), under the names the stub's
/// constants give them.
const TARGETS: [(&[u8], i64); 9] = [
    (b"TARGET_CLASS", 1),
    (b"TARGET_FUNCTION", 1 << 1),
    (b"TARGET_METHOD", 1 << 2),
    (b"TARGET_PROPERTY", 1 << 3),
    (b"TARGET_CLASS_CONSTANT", 1 << 4),
    (b"TARGET_PARAMETER", 1 << 5),
    (b"TARGET_CONSTANT", 1 << 6),
    (b"TARGET_ALL", (1 << 7) - 1),
    (b"IS_REPEATABLE", 1 << 7),
];

/// `attributes_object_handlers_sensitive_parameter_value`:
/// `std_object_handlers` with `get_properties_for` answering nothing
/// for every purpose, so no dumper, cast or encoder shows the value.
struct SensitiveValueHandlers;

impl ObjectHandlers for SensitiveValueHandlers {
    /// `attributes_sensitive_parameter_value_get_properties_for`.
    ///
    /// ```php
    /// $v = new SensitiveParameterValue('secret');
    /// var_dump($v, (array) $v);
    /// print_r($v);
    /// echo "\n", json_encode($v), "\n";
    /// var_export($v);
    /// echo "\n";
    /// var_dump($v->getValue(), $v->__debugInfo());
    /// ```
    /// ```expect
    /// object(SensitiveParameterValue)#1 (0) {
    /// }
    /// array(0) {
    /// }
    /// SensitiveParameterValue Object
    /// (
    /// )
    ///
    /// {}
    /// \SensitiveParameterValue::__set_state(array(
    /// ))
    /// string(6) "secret"
    /// array(0) {
    /// }
    /// ```
    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<Option<Vec<(PhpString, Value)>>, Thrown> {
        let _ = (req, object, purpose);
        Ok(Some(Vec::new()))
    }
}

static SENSITIVE_VALUE: SensitiveValueHandlers = SensitiveValueHandlers;

/// The nine classes, in the order `zend_register_attribute_ce`
/// registers them.
///
/// ```php
/// foreach (['Attribute', 'ReturnTypeWillChange', 'AllowDynamicProperties',
///     'SensitiveParameter', 'SensitiveParameterValue', 'Override', 'Deprecated',
///     'NoDiscard', 'DelayedTargetValidation'] as $name) {
///     $c = new ReflectionClass($name);
///     echo $name, ' ', (int) $c->isFinal(), ' ', count($c->getAttributes()), "\n";
/// }
/// $all = Attribute::TARGET_CLASS | Attribute::TARGET_FUNCTION | Attribute::TARGET_METHOD
///     | Attribute::TARGET_PROPERTY | Attribute::TARGET_CLASS_CONSTANT
///     | Attribute::TARGET_PARAMETER | Attribute::TARGET_CONSTANT;
/// var_dump($all === Attribute::TARGET_ALL, Attribute::IS_REPEATABLE);
/// ```
/// ```expect
/// Attribute 1 1
/// ReturnTypeWillChange 1 1
/// AllowDynamicProperties 1 1
/// SensitiveParameter 1 1
/// SensitiveParameterValue 1 0
/// Override 1 1
/// Deprecated 1 1
/// NoDiscard 1 1
/// DelayedTargetValidation 1 1
/// bool(true)
/// int(128)
/// ```
///
/// `Attribute` renders as `php --rc Attribute` renders it:
///
/// ```php
/// echo new ReflectionClass('Attribute');
/// ```
/// ```expect
/// Class [ <internal:Core> final class Attribute ] {
///
///   - Constants [9] {
///     Constant [ public int TARGET_CLASS ] { 1 }
///     Constant [ public int TARGET_FUNCTION ] { 2 }
///     Constant [ public int TARGET_METHOD ] { 4 }
///     Constant [ public int TARGET_PROPERTY ] { 8 }
///     Constant [ public int TARGET_CLASS_CONSTANT ] { 16 }
///     Constant [ public int TARGET_PARAMETER ] { 32 }
///     Constant [ public int TARGET_CONSTANT ] { 64 }
///     Constant [ public int TARGET_ALL ] { 127 }
///     Constant [ public int IS_REPEATABLE ] { 128 }
///   }
///
///   - Static properties [0] {
///   }
///
///   - Static methods [0] {
///   }
///
///   - Properties [1] {
///     Property [ public int $flags ]
///   }
///
///   - Methods [1] {
///     Method [ <internal:Core, ctor> public method __construct ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <optional> int $flags = Attribute::TARGET_ALL ]
///       }
///     }
///   }
/// }
/// ```
///
/// `@strict-properties` is what refuses a dynamic property on five of
/// them; the other three take one with 8.2's deprecation:
///
/// ```php
/// foreach ([new Override, new Deprecated, new NoDiscard, new DelayedTargetValidation,
///     new SensitiveParameter, new SensitiveParameterValue(1)] as $o) {
///     try { $o->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// }
/// $a = new AllowDynamicProperties;
/// $a->x = 1;
/// ```
/// ```expect
/// Cannot create dynamic property Override::$x
/// Cannot create dynamic property Deprecated::$x
/// Cannot create dynamic property NoDiscard::$x
/// Cannot create dynamic property DelayedTargetValidation::$x
/// Cannot create dynamic property SensitiveParameter::$x
/// Cannot create dynamic property SensitiveParameterValue::$x
///
/// Deprecated: Creation of dynamic property AllowDynamicProperties::$x is deprecated in %s on line 7
/// ```
pub(crate) fn classes() -> Vec<NativeClassDecl> {
    vec![
        attribute(),
        marker(b"ReturnTypeWillChange", b"TARGET_METHOD", false),
        marker(b"AllowDynamicProperties", b"TARGET_CLASS", false),
        marker(b"SensitiveParameter", b"TARGET_PARAMETER", true),
        sensitive_value(),
        marker(b"Override", b"TARGET_METHOD|TARGET_PROPERTY", true),
        deprecated(),
        no_discard(),
        (
            attribute_class(b"DelayedTargetValidation", b"TARGET_ALL", true),
            Vec::new(),
            None,
            None,
        ),
    ]
}

/// `final class Attribute`, its constants, `$flags` and constructor.
fn attribute() -> NativeClassDecl {
    let constants = TARGETS
        .iter()
        .map(|(name, value)| ClassConstDecl {
            name: Name((*name).into()),
            visibility: Visibility::Public,
            is_final: false,
            is_case: false,
            ty: Some(Type::Int),
            value: ConstExpr::Long(*value),
            attributes: Vec::new(),
            doc_comment: None,
        })
        .collect();
    let flags = Parameter {
        default: ParameterDefault::Expr(ConstExpr::ClassConstant {
            class: Name::from("Attribute"),
            name: Name::from("TARGET_ALL"),
        }),
        ..parameter(b"flags", Type::Int)
    };
    (
        ClassDecl {
            constants,
            properties: vec![property(b"flags", Visibility::Public, false, Type::Int)],
            ..attribute_class(b"Attribute", b"TARGET_CLASS", false)
        },
        vec![(
            method(b"__construct", vec![flags], None),
            attribute_construct as Entry,
        )],
        None,
        None,
    )
}

/// `final class SensitiveParameterValue`, no attribute.
fn sensitive_value() -> NativeClassDecl {
    (
        ClassDecl {
            properties: vec![property(b"value", Visibility::Private, true, Type::Mixed)],
            ..class(b"SensitiveParameterValue", true)
        },
        vec![
            (
                method(b"__construct", vec![parameter(b"value", Type::Mixed)], None),
                sensitive_value_construct as Entry,
            ),
            (
                method(b"getValue", Vec::new(), Some(Type::Mixed)),
                sensitive_value_get_value as Entry,
            ),
            (
                method(b"__debugInfo", Vec::new(), Some(Type::Array)),
                sensitive_value_debug_info as Entry,
            ),
        ],
        Some(&SENSITIVE_VALUE),
        None,
    )
}

/// `final class Deprecated`, its two readonly properties.
fn deprecated() -> NativeClassDecl {
    (
        ClassDecl {
            properties: vec![
                property(b"message", Visibility::Public, true, nullable_string()),
                property(b"since", Visibility::Public, true, nullable_string()),
            ],
            ..attribute_class(
                b"Deprecated",
                b"TARGET_METHOD|TARGET_FUNCTION|TARGET_CLASS_CONSTANT|TARGET_CONSTANT|TARGET_CLASS",
                true,
            )
        },
        vec![(
            method(
                b"__construct",
                vec![optional_string(b"message"), optional_string(b"since")],
                None,
            ),
            deprecated_construct as Entry,
        )],
        None,
        None,
    )
}

/// `final class NoDiscard`, its one readonly property.
fn no_discard() -> NativeClassDecl {
    (
        ClassDecl {
            properties: vec![property(
                b"message",
                Visibility::Public,
                true,
                nullable_string(),
            )],
            ..attribute_class(b"NoDiscard", b"TARGET_METHOD|TARGET_FUNCTION", true)
        },
        vec![(
            method(b"__construct", vec![optional_string(b"message")], None),
            no_discard_construct as Entry,
        )],
        None,
        None,
    )
}

/// One of the four attributes whose constructor takes nothing.
fn marker(name: &[u8], targets: &[u8], strict: bool) -> NativeClassDecl {
    (
        attribute_class(name, targets, strict),
        vec![(
            method(b"__construct", Vec::new(), None),
            no_arguments_construct as Entry,
        )],
        None,
        None,
    )
}

/// A final class of the stub, `strict` its `@strict-properties`.
fn class(name: &[u8], strict: bool) -> ClassDecl {
    ClassDecl {
        name: Name(name.into()),
        kind: ClassKind::Class,
        flags: ClassFlags {
            is_final: true,
            dynamic_properties: if strict {
                DynamicProperties::Forbidden
            } else {
                DynamicProperties::Deprecated
            },
            ..ClassFlags::default()
        },
        parent: None,
        interfaces: Vec::new(),
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

/// A class the stub marks `#[Attribute(Attribute::A|Attribute::B)]`,
/// `targets` the constants' names joined by `|` as the stub writes
/// them.
fn attribute_class(name: &[u8], targets: &[u8], strict: bool) -> ClassDecl {
    let mut flags = targets
        .split(|b| *b == b'|')
        .map(|target| ConstExpr::ClassConstant {
            class: Name::from("Attribute"),
            name: Name(target.into()),
        });
    let first = flags.next().unwrap_or(ConstExpr::Long(0));
    let value = flags.fold(first, |lhs, rhs| ConstExpr::Binary {
        op: Name::from("|"),
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    });
    ClassDecl {
        attributes: vec![Attribute {
            name: Name::from("Attribute"),
            arguments: vec![AttributeArgument { name: None, value }],
        }],
        ..class(name, strict)
    }
}

fn nullable_string() -> Type {
    Type::Nullable(Box::new(Type::String))
}

/// A declared instance property, a public readonly one `protected(set)`
/// as `zend_declare_typed_property` makes it.
fn property(name: &[u8], visibility: Visibility, is_readonly: bool, ty: Type) -> PropertyDecl {
    PropertyDecl {
        name: Name(name.into()),
        visibility,
        set_visibility: (is_readonly && visibility == Visibility::Public)
            .then_some(Visibility::Protected),
        is_instance: true,
        is_readonly,
        is_final: false,
        is_promoted: false,
        ty: Some(ty),
        default: None,
        attributes: Vec::new(),
        doc_comment: None,
    }
}

fn method(name: &[u8], parameters: Vec<Parameter>, returns: Option<Type>) -> MethodDecl {
    MethodDecl {
        function: FunctionDecl {
            name: Name(name.into()),
            parameters,
            return_type: returns,
            returns_by_ref: false,
            is_generator: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            end_line: 0,
            frameless: Vec::new(),
        },
        visibility: Visibility::Public,
        is_instance: true,
        is_abstract: false,
        is_final: false,
        tentative_return: false,
    }
}

fn parameter(name: &[u8], ty: Type) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty: Some(ty),
        by_ref: false,
        prefer_ref: false,
        promoted: false,
        variadic: false,
        default: ParameterDefault::None,
        attributes: Vec::new(),
    }
}

/// `?string $name = null`.
fn optional_string(name: &[u8]) -> Parameter {
    Parameter {
        default: ParameterDefault::Expr(ConstExpr::Null),
        ..parameter(name, nullable_string())
    }
}

/// `ZEND_PARSE_PARAMETERS_START(0, most)`: php's `ArgumentCountError`
/// past `most`.
fn at_most(req: &mut Request, most: usize) -> Result<(), Thrown> {
    let given = req.argument_count();
    if given <= most {
        return Ok(());
    }
    let what = match most {
        1 => format!("at most 1 argument, {given} given"),
        n => format!("at most {n} arguments, {given} given"),
    };
    Err(arg_count_error(req, &what))
}

/// `ZEND_PARSE_PARAMETERS_NONE` and `ZEND_PARSE_PARAMETERS_START(n, n)`.
fn exactly(req: &mut Request, count: usize) -> Result<(), Thrown> {
    let given = req.argument_count();
    if given == count {
        return Ok(());
    }
    let what = match count {
        1 => format!("exactly 1 argument, {given} given"),
        n => format!("exactly {n} arguments, {given} given"),
    };
    Err(arg_count_error(req, &what))
}

fn receiver(req: &mut Request) -> Result<PhpObject, Thrown> {
    match req.this() {
        Some(this) => Ok(this),
        None => Err(throw(req, "Error", "Non-static method called statically")),
    }
}

/// `zend_update_property_ex` over a readonly property from its class's
/// own constructor: the first write initialises it, and a second is
/// php's `Cannot modify readonly property`.
fn initialise(
    req: &mut Request,
    this: PhpObject,
    class: &str,
    name: &[u8],
    value: Value,
) -> Result<(), Thrown> {
    if this.prop(req, name).is_some() {
        req.release(value);
        return Err(throw(
            req,
            "Error",
            &format!(
                "Cannot modify readonly property {class}::${}",
                String::from_utf8_lossy(name)
            ),
        ));
    }
    let key = PhpString::new(req, name)?;
    this.set_prop(req, key, value)
}

/// `Attribute::__construct(int $flags = Attribute::TARGET_ALL)`: the
/// flags into the one property, unchecked, as php writes
/// `OBJ_PROP_NUM(obj, 0)`; `ReflectionAttribute::newInstance` is what
/// holds them to the targets.
///
/// ```php
/// var_dump(new Attribute, new Attribute(Attribute::TARGET_CLASS | Attribute::IS_REPEATABLE));
/// try { new Attribute("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { new Attribute(1, 2); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(Attribute)#1 (1) {
///   ["flags"]=>
///   int(127)
/// }
/// object(Attribute)#2 (1) {
///   ["flags"]=>
///   int(129)
/// }
/// Attribute::__construct(): Argument #1 ($flags) must be of type int, string given
/// Attribute::__construct() expects at most 1 argument, 2 given
/// ```
fn attribute_construct(req: &mut Request) -> Result<Value, Thrown> {
    at_most(req, 1)?;
    let flags = argument(req, 1)?.unwrap_or(Value::int(TARGETS[7].1));
    let this = receiver(req)?;
    this.set_slot(req, 0, flags)?;
    Ok(Value::null())
}

/// The constructor of `ReturnTypeWillChange`, `AllowDynamicProperties`,
/// `SensitiveParameter` and `Override`, `ZEND_PARSE_PARAMETERS_NONE`.
///
/// ```php
/// var_dump(new Override);
/// try { new Override(1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(Override)#1 (0) {
/// }
/// Override::__construct() expects exactly 0 arguments, 1 given
/// ```
fn no_arguments_construct(req: &mut Request) -> Result<Value, Thrown> {
    exactly(req, 0)?;
    Ok(Value::null())
}

/// `SensitiveParameterValue::__construct(mixed $value)`.
///
/// ```php
/// $v = new SensitiveParameterValue([1]);
/// try { $v->__construct(2); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { new SensitiveParameterValue; } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// var_dump($v->getValue());
/// ```
/// ```expect
/// Cannot modify readonly property SensitiveParameterValue::$value
/// SensitiveParameterValue::__construct() expects exactly 1 argument, 0 given
/// array(1) {
///   [0]=>
///   int(1)
/// }
/// ```
fn sensitive_value_construct(req: &mut Request) -> Result<Value, Thrown> {
    exactly(req, 1)?;
    let value = argument(req, 1)?.unwrap_or_default();
    req.addref(&value);
    let this = receiver(req)?;
    initialise(req, this, "SensitiveParameterValue", b"value", value)?;
    Ok(Value::null())
}

/// `SensitiveParameterValue::getValue(): mixed`, the value the
/// constructor kept.
fn sensitive_value_get_value(req: &mut Request) -> Result<Value, Thrown> {
    exactly(req, 0)?;
    let this = receiver(req)?;
    let value = this.slot(0).unwrap_or_default();
    req.addref(&value);
    Ok(value)
}

/// `SensitiveParameterValue::__debugInfo(): array`, empty.
fn sensitive_value_debug_info(req: &mut Request) -> Result<Value, Thrown> {
    exactly(req, 0)?;
    Ok(Value::array(PhpArray::new(req)?))
}

/// `Deprecated::__construct(?string $message = null, ?string $since =
/// null)`: each argument held to `?string` under the caller's
/// `strict_types`, then written into its readonly property.
///
/// ```php
/// var_dump(new Deprecated, new Deprecated(since: "8.0"), new Deprecated(null, 5));
/// try { new Deprecated([]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// $d = new Deprecated("foo");
/// try { $d->message = 'bar'; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $d->__construct("bar"); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(Deprecated)#1 (2) {
///   ["message"]=>
///   NULL
///   ["since"]=>
///   NULL
/// }
/// object(Deprecated)#2 (2) {
///   ["message"]=>
///   NULL
///   ["since"]=>
///   string(3) "8.0"
/// }
/// object(Deprecated)#3 (2) {
///   ["message"]=>
///   NULL
///   ["since"]=>
///   string(1) "5"
/// }
/// Deprecated::__construct(): Argument #1 ($message) must be of type ?string, array given
/// Cannot modify readonly property Deprecated::$message
/// Cannot modify readonly property Deprecated::$message
/// ```
fn deprecated_construct(req: &mut Request) -> Result<Value, Thrown> {
    at_most(req, 2)?;
    let message = argument(req, 1)?.unwrap_or_default();
    let since = argument(req, 2)?.unwrap_or_default();
    let this = receiver(req)?;
    req.addref(&message);
    initialise(req, this, "Deprecated", b"message", message)?;
    req.addref(&since);
    initialise(req, this, "Deprecated", b"since", since)?;
    Ok(Value::null())
}

/// `NoDiscard::__construct(?string $message = null)`.
///
/// ```php
/// var_dump(new NoDiscard, new NoDiscard("kept"));
/// ```
/// ```expect
/// object(NoDiscard)#1 (1) {
///   ["message"]=>
///   NULL
/// }
/// object(NoDiscard)#2 (1) {
///   ["message"]=>
///   string(4) "kept"
/// }
/// ```
fn no_discard_construct(req: &mut Request) -> Result<Value, Thrown> {
    at_most(req, 1)?;
    let message = argument(req, 1)?.unwrap_or_default();
    let this = receiver(req)?;
    req.addref(&message);
    initialise(req, this, "NoDiscard", b"message", message)?;
    Ok(Value::null())
}

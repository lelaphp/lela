//! `php_reflection.stub.php` as declarations: the twenty-six classes
//! `PHP_MINIT_FUNCTION(reflection)` registers, in its order, each with
//! every method the stub declares, so `php --re reflection` and the
//! linked table agree on the extension's `Classes [26]` block (design
//! §7.1, §8). What `gen_stub.php` derives from the stub, this file
//! writes by hand in the same shape: the modifiers, the parameters with
//! their defaults as the stub spells them, the return type with its
//! `@tentative-return-type` bit, the `#[\Deprecated]` attribute, the
//! `IS_*` constants with `ZEND_ACC_*`'s numbers and the `$name` and
//! `$class` properties.
//!
//! A method the engine serves names its entry in `super`; every other
//! method is declared beside [`unbuilt`], which throws, so a call
//! reaches php's `Error` and not php's "Call to undefined method",
//! which would be wrong about a method the class has. The bodies
//! arrive with the tests that call them.
//!
//! Two spellings of the stub the declaration model has no word for
//! are carried as the stub writes them: `= UNKNOWN`, an optional
//! parameter with no default to show, is [`unknown_default`], the
//! constant named `UNKNOWN`, which the renderer prints as php prints it,
//! `<default>`; and an enum's cases are its constants, since the model
//! has no case bit and `PropertyHookType` declares no other constant.
// A class's function is one row per method of the stub, and the rows
// are what the linter counts.
#![allow(clippy::too_many_lines)]

use php_ext::decl::{
    Attribute, AttributeArgument, ClassConstDecl, ClassDecl, ClassFlags, ClassKind, ConstExpr,
    EnumBacking, FunctionDecl, MethodDecl, Name, Parameter, PropertyDecl, Type, Visibility,
};
use php_ext::raising::{Thrown, throw};
use php_ext::request::Request;
use php_ext::values::Value;

use super::Entry;

/// One class with its methods' entries, the shape
/// `ExtensionBuilder::classes` takes.
pub(super) type Declared = (ClassDecl, Vec<(MethodDecl, Entry)>);

/// The spelling of an optional parameter the stub gives no default,
/// `mixed $default = UNKNOWN`.
const UNKNOWN: &str = "UNKNOWN";

/// Whether a parameter's default is the stub's `UNKNOWN`, which
/// `_parameter_string` prints as `<default>`.
pub(super) fn is_unknown_default(expr: &ConstExpr) -> bool {
    matches!(expr, ConstExpr::Constant(name) if name.0.as_ref() == UNKNOWN.as_bytes())
}

/// The entry of a method declared and not yet served: php's `Error`,
/// as a native that cannot do what it was asked raises one.
fn unbuilt(req: &mut Request) -> Result<Value, Thrown> {
    Err(throw(
        req,
        "Error",
        "This Reflection method is declared and not yet served by this engine",
    ))
}

/// `case NAME = 'value';` of a string-backed enum, carried as a
/// constant with the backing value.
fn case(name: &[u8], value: &[u8]) -> ClassConstDecl {
    ClassConstDecl {
        name: Name(name.into()),
        visibility: Visibility::Public,
        is_final: false,
        ty: None,
        value: ConstExpr::Str(value.into()),
        attributes: Vec::new(),
        doc_comment: None,
    }
}

/// `public readonly string $name;`, the two properties
/// `zend_enum_register_props` declares on a backed enum.
fn readonly_string(name: &[u8]) -> PropertyDecl {
    PropertyDecl {
        name: Name(name.into()),
        is_readonly: true,
        ..name_property()
    }
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

/// `public string $name;`, as the stub declares it on each reflector.
fn name_property() -> PropertyDecl {
    PropertyDecl {
        name: Name::from("name"),
        visibility: Visibility::Public,
        set_visibility: None,
        is_instance: true,
        is_readonly: false,
        is_final: false,
        ty: Some(Type::String),
        default: None,
        attributes: Vec::new(),
        doc_comment: None,
    }
}

/// A class with no modifier, no member and no site: the rest is
/// written over it.
fn class(name: &[u8], parent: Option<&[u8]>, interfaces: &[&[u8]]) -> ClassDecl {
    ClassDecl {
        name: Name(name.into()),
        kind: ClassKind::Class,
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

fn named(name: &[u8]) -> Type {
    Type::Named(Name(name.into()))
}

fn nullable(ty: Type) -> Type {
    Type::Nullable(Box::new(ty))
}

fn union(members: &[Type]) -> Type {
    Type::Union(members.to_vec())
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

/// `$function` with a `@param` comment for its type and none declared.
fn untyped(name: &[u8]) -> Parameter {
    Parameter {
        ty: None,
        ..parameter(name, Type::Mixed)
    }
}

fn with_default(name: &[u8], ty: Type, default: ConstExpr) -> Parameter {
    Parameter {
        default: Some(default),
        ..parameter(name, ty)
    }
}

/// `type $name = UNKNOWN`: optional, with no default to show.
fn unknown_default(name: &[u8], ty: Type) -> Parameter {
    with_default(name, ty, ConstExpr::Constant(Name::from(UNKNOWN)))
}

fn variadic(name: &[u8], ty: Type) -> Parameter {
    Parameter {
        variadic: true,
        ..parameter(name, ty)
    }
}

/// `public function name(parameters): returns {}`.
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

/// A method whose return the stub marks `@tentative-return-type`.
fn tentative(method: MethodDecl) -> MethodDecl {
    MethodDecl {
        tentative_return: true,
        ..method
    }
}

/// A `static` method.
fn is_static(method: MethodDecl) -> MethodDecl {
    MethodDecl {
        is_instance: false,
        ..method
    }
}

/// A `private` method.
fn private(method: MethodDecl) -> MethodDecl {
    MethodDecl {
        visibility: Visibility::Private,
        ..method
    }
}

/// A method under `#[\Deprecated(since: ..., message: ...)]`, which
/// `ZEND_ACC_DEPRECATED` marks and `_function_string` prints as
/// `, deprecated`.
fn deprecated(method: MethodDecl, since: &str, message: &str) -> MethodDecl {
    let argument = |name: &str, value: &str| AttributeArgument {
        name: Some(Name::from(name)),
        value: ConstExpr::Str(value.as_bytes().into()),
    };
    let mut method = method;
    method.function.attributes.push(Attribute {
        name: Name::from("Deprecated"),
        arguments: vec![argument("since", since), argument("message", message)],
    });
    method
}

/// The twenty-six, in the order `PHP_MINIT_FUNCTION(reflection)`
/// registers them, which is `EG(class_table)`'s and what `php --re`
/// walks; the stub writes `PropertyHookType` earlier and registers it
/// last.
///
/// ```php
/// echo new ReflectionExtension('reflection');
/// ```
/// ```expect
/// Extension [ <persistent> extension #%N% Reflection version 8.5.9 ] {
///
///   - Classes [26] {
///     Class [ <internal:Reflection> class ReflectionException extends Exception implements Throwable, Stringable ] {
///
///       - Constants [0] {
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [4] {
///         Property [ protected $message = '' ]
///         Property [ protected $code = 0 ]
///         Property [ protected string $file = '' ]
///         Property [ protected int $line = 0 ]
///       }
///
///       - Methods [10] {
///         Method [ <internal:Core, inherits Exception, ctor> public method __construct ] {
///
///           - Parameters [3] {
///             Parameter #0 [ <optional> string $message = "" ]
///             Parameter #1 [ <optional> int $code = 0 ]
///             Parameter #2 [ <optional> ?Throwable $previous = null ]
///           }
///         }
///
///         Method [ <internal:Core, inherits Exception> public method __wakeup ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ void ]
///         }
///
///         Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getMessage ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getCode ] {
///
///           - Parameters [0] {
///           }
///         }
///
///         Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getFile ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getLine ] {
///
///           - Parameters [0] {
///           }
///           - Return [ int ]
///         }
///
///         Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getTrace ] {
///
///           - Parameters [0] {
///           }
///           - Return [ array ]
///         }
///
///         Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getPrevious ] {
///
///           - Parameters [0] {
///           }
///           - Return [ ?Throwable ]
///         }
///
///         Method [ <internal:Core, inherits Exception, prototype Throwable> final public method getTraceAsString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Core, inherits Exception, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> class Reflection ] {
///
///       - Constants [0] {
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [1] {
///         Method [ <internal:Reflection> static public method getModifierNames ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> int $modifiers ]
///           }
///           - Tentative return [ array ]
///         }
///       }
///
///       - Properties [0] {
///       }
///
///       - Methods [0] {
///       }
///     }
///
///     Interface [ <internal:Reflection> interface Reflector extends Stringable ] {
///
///       - Constants [0] {
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [0] {
///       }
///
///       - Methods [1] {
///         Method [ <internal:Core, inherits Stringable> abstract public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> abstract class ReflectionFunctionAbstract implements Reflector, Stringable ] {
///
///       - Constants [0] {
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [1] {
///         Property [ public string $name ]
///       }
///
///       - Methods [33] {
///         Method [ <internal:Reflection> private method __clone ] {
///
///           - Parameters [0] {
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection> public method inNamespace ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isClosure ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isDeprecated ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isInternal ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isUserDefined ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isGenerator ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isVariadic ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isStatic ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getClosureThis ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?object ]
///         }
///
///         Method [ <internal:Reflection> public method getClosureScopeClass ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionClass ]
///         }
///
///         Method [ <internal:Reflection> public method getClosureCalledClass ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionClass ]
///         }
///
///         Method [ <internal:Reflection> public method getClosureUsedVariables ] {
///
///           - Parameters [0] {
///           }
///           - Return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method getDocComment ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection> public method getEndLine ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int|false ]
///         }
///
///         Method [ <internal:Reflection> public method getExtension ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionExtension ]
///         }
///
///         Method [ <internal:Reflection> public method getExtensionName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection> public method getFileName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection> public method getName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getNamespaceName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getNumberOfParameters ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int ]
///         }
///
///         Method [ <internal:Reflection> public method getNumberOfRequiredParameters ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int ]
///         }
///
///         Method [ <internal:Reflection> public method getParameters ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method getShortName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getStartLine ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int|false ]
///         }
///
///         Method [ <internal:Reflection> public method getStaticVariables ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method returnsReference ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method hasReturnType ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getReturnType ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionType ]
///         }
///
///         Method [ <internal:Reflection> public method hasTentativeReturnType ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getTentativeReturnType ] {
///
///           - Parameters [0] {
///           }
///           - Return [ ?ReflectionType ]
///         }
///
///         Method [ <internal:Reflection> public method getAttributes ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <optional> ?string $name = null ]
///             Parameter #1 [ <optional> int $flags = 0 ]
///           }
///           - Return [ array ]
///         }
///
///         Method [ <internal:Core, inherits Stringable> abstract public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> class ReflectionFunction extends ReflectionFunctionAbstract implements Stringable, Reflector ] {
///
///       - Constants [1] {
///         Constant [ public int IS_DEPRECATED ] { 2048 }
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [1] {
///         Property [ public string $name ]
///       }
///
///       - Methods [38] {
///         Method [ <internal:Reflection, ctor> public method __construct ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> Closure|string $function ]
///           }
///         }
///
///         Method [ <internal:Reflection, overwrites Stringable, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method isAnonymous ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal, deprecated:Reflection> public method isDisabled ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method invoke ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> mixed ...$args ]
///           }
///           - Tentative return [ mixed ]
///         }
///
///         Method [ <internal:Reflection> public method invokeArgs ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> array $args ]
///           }
///           - Tentative return [ mixed ]
///         }
///
///         Method [ <internal:Reflection> public method getClosure ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ Closure ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method inNamespace ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method isClosure ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method isDeprecated ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method isInternal ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method isUserDefined ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method isGenerator ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method isVariadic ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method isStatic ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getClosureThis ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?object ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getClosureScopeClass ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionClass ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getClosureCalledClass ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionClass ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getClosureUsedVariables ] {
///
///           - Parameters [0] {
///           }
///           - Return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getDocComment ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getEndLine ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getExtension ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionExtension ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getExtensionName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getFileName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getNamespaceName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getNumberOfParameters ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getNumberOfRequiredParameters ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getParameters ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getShortName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getStartLine ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getStaticVariables ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method returnsReference ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method hasReturnType ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getReturnType ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionType ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method hasTentativeReturnType ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getTentativeReturnType ] {
///
///           - Parameters [0] {
///           }
///           - Return [ ?ReflectionType ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getAttributes ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <optional> ?string $name = null ]
///             Parameter #1 [ <optional> int $flags = 0 ]
///           }
///           - Return [ array ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> final class ReflectionGenerator ] {
///
///       - Constants [0] {
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [0] {
///       }
///
///       - Methods [8] {
///         Method [ <internal:Reflection, ctor> public method __construct ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> Generator $generator ]
///           }
///         }
///
///         Method [ <internal:Reflection> public method getExecutingLine ] {
///
///           - Parameters [0] {
///           }
///           - Return [ int ]
///         }
///
///         Method [ <internal:Reflection> public method getExecutingFile ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getTrace ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> int $options = DEBUG_BACKTRACE_PROVIDE_OBJECT ]
///           }
///           - Return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method getFunction ] {
///
///           - Parameters [0] {
///           }
///           - Return [ ReflectionFunctionAbstract ]
///         }
///
///         Method [ <internal:Reflection> public method getThis ] {
///
///           - Parameters [0] {
///           }
///           - Return [ ?object ]
///         }
///
///         Method [ <internal:Reflection> public method getExecutingGenerator ] {
///
///           - Parameters [0] {
///           }
///           - Return [ Generator ]
///         }
///
///         Method [ <internal:Reflection> public method isClosed ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> class ReflectionParameter implements Stringable, Reflector ] {
///
///       - Constants [0] {
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [1] {
///         Property [ public string $name ]
///       }
///
///       - Methods [23] {
///         Method [ <internal:Reflection> private method __clone ] {
///
///           - Parameters [0] {
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection, ctor> public method __construct ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> $function ]
///             Parameter #1 [ <required> string|int $param ]
///           }
///         }
///
///         Method [ <internal:Reflection, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method isPassedByReference ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method canBePassedByValue ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getDeclaringFunction ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ReflectionFunctionAbstract ]
///         }
///
///         Method [ <internal:Reflection> public method getDeclaringClass ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionClass ]
///         }
///
///         Method [ <internal, deprecated:Reflection> public method getClass ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionClass ]
///         }
///
///         Method [ <internal:Reflection> public method hasType ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getType ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionType ]
///         }
///
///         Method [ <internal, deprecated:Reflection> public method isArray ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal, deprecated:Reflection> public method isCallable ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method allowsNull ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getPosition ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int ]
///         }
///
///         Method [ <internal:Reflection> public method isOptional ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isDefaultValueAvailable ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getDefaultValue ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ mixed ]
///         }
///
///         Method [ <internal:Reflection> public method isDefaultValueConstant ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getDefaultValueConstantName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?string ]
///         }
///
///         Method [ <internal:Reflection> public method isVariadic ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isPromoted ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getAttributes ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <optional> ?string $name = null ]
///             Parameter #1 [ <optional> int $flags = 0 ]
///           }
///           - Return [ array ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> abstract class ReflectionType implements Stringable ] {
///
///       - Constants [0] {
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [0] {
///       }
///
///       - Methods [3] {
///         Method [ <internal:Reflection> private method __clone ] {
///
///           - Parameters [0] {
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection> public method allowsNull ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> class ReflectionNamedType extends ReflectionType implements Stringable ] {
///
///       - Constants [0] {
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [0] {
///       }
///
///       - Methods [4] {
///         Method [ <internal:Reflection> public method getName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method isBuiltin ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionType> public method allowsNull ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionType, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> class ReflectionUnionType extends ReflectionType implements Stringable ] {
///
///       - Constants [0] {
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [0] {
///       }
///
///       - Methods [3] {
///         Method [ <internal:Reflection> public method getTypes ] {
///
///           - Parameters [0] {
///           }
///           - Return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionType> public method allowsNull ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionType, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> class ReflectionIntersectionType extends ReflectionType implements Stringable ] {
///
///       - Constants [0] {
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [0] {
///       }
///
///       - Methods [3] {
///         Method [ <internal:Reflection> public method getTypes ] {
///
///           - Parameters [0] {
///           }
///           - Return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionType> public method allowsNull ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionType, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> class ReflectionMethod extends ReflectionFunctionAbstract implements Stringable, Reflector ] {
///
///       - Constants [6] {
///         Constant [ public int IS_STATIC ] { 16 }
///         Constant [ public int IS_PUBLIC ] { 1 }
///         Constant [ public int IS_PROTECTED ] { 2 }
///         Constant [ public int IS_PRIVATE ] { 4 }
///         Constant [ public int IS_ABSTRACT ] { 64 }
///         Constant [ public int IS_FINAL ] { 32 }
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [1] {
///         Method [ <internal:Reflection> static public method createFromMethodName ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $method ]
///           }
///           - Return [ static ]
///         }
///       }
///
///       - Properties [2] {
///         Property [ public string $name ]
///         Property [ public string $class ]
///       }
///
///       - Methods [48] {
///         Method [ <internal:Reflection, ctor> public method __construct ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> object|string $objectOrMethod ]
///             Parameter #1 [ <optional> ?string $method = null ]
///           }
///         }
///
///         Method [ <internal:Reflection, overwrites Stringable, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method isPublic ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isPrivate ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isProtected ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isAbstract ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isFinal ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isConstructor ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isDestructor ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getClosure ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> ?object $object = null ]
///           }
///           - Tentative return [ Closure ]
///         }
///
///         Method [ <internal:Reflection> public method getModifiers ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int ]
///         }
///
///         Method [ <internal:Reflection> public method invoke ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> ?object $object ]
///             Parameter #1 [ <optional> mixed ...$args ]
///           }
///           - Tentative return [ mixed ]
///         }
///
///         Method [ <internal:Reflection> public method invokeArgs ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> ?object $object ]
///             Parameter #1 [ <required> array $args ]
///           }
///           - Tentative return [ mixed ]
///         }
///
///         Method [ <internal:Reflection> public method getDeclaringClass ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ReflectionClass ]
///         }
///
///         Method [ <internal:Reflection> public method getPrototype ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ReflectionMethod ]
///         }
///
///         Method [ <internal:Reflection> public method hasPrototype ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal, deprecated:Reflection> public method setAccessible ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> bool $accessible ]
///           }
///           - Tentative return [ void ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method inNamespace ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method isClosure ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method isDeprecated ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method isInternal ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method isUserDefined ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method isGenerator ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method isVariadic ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method isStatic ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getClosureThis ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?object ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getClosureScopeClass ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionClass ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getClosureCalledClass ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionClass ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getClosureUsedVariables ] {
///
///           - Parameters [0] {
///           }
///           - Return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getDocComment ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getEndLine ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getExtension ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionExtension ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getExtensionName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getFileName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getNamespaceName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getNumberOfParameters ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getNumberOfRequiredParameters ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getParameters ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getShortName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getStartLine ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getStaticVariables ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method returnsReference ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method hasReturnType ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getReturnType ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionType ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method hasTentativeReturnType ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getTentativeReturnType ] {
///
///           - Parameters [0] {
///           }
///           - Return [ ?ReflectionType ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionFunctionAbstract> public method getAttributes ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <optional> ?string $name = null ]
///             Parameter #1 [ <optional> int $flags = 0 ]
///           }
///           - Return [ array ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> class ReflectionClass implements Stringable, Reflector ] {
///
///       - Constants [6] {
///         Constant [ public int IS_IMPLICIT_ABSTRACT ] { 16 }
///         Constant [ public int IS_EXPLICIT_ABSTRACT ] { 64 }
///         Constant [ public int IS_FINAL ] { 32 }
///         Constant [ public int IS_READONLY ] { 65536 }
///         Constant [ public int SKIP_INITIALIZATION_ON_SERIALIZE ] { 8 }
///         Constant [ public int SKIP_DESTRUCTOR ] { 16 }
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [1] {
///         Property [ public string $name ]
///       }
///
///       - Methods [64] {
///         Method [ <internal:Reflection> private method __clone ] {
///
///           - Parameters [0] {
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection, ctor> public method __construct ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object|string $objectOrClass ]
///           }
///         }
///
///         Method [ <internal:Reflection, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method isInternal ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isUserDefined ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isAnonymous ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isInstantiable ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isCloneable ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getFileName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection> public method getStartLine ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int|false ]
///         }
///
///         Method [ <internal:Reflection> public method getEndLine ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int|false ]
///         }
///
///         Method [ <internal:Reflection> public method getDocComment ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection> public method getConstructor ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionMethod ]
///         }
///
///         Method [ <internal:Reflection> public method hasMethod ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getMethod ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ ReflectionMethod ]
///         }
///
///         Method [ <internal:Reflection> public method getMethods ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> ?int $filter = null ]
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method hasProperty ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getProperty ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ ReflectionProperty ]
///         }
///
///         Method [ <internal:Reflection> public method getProperties ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> ?int $filter = null ]
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method hasConstant ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getConstants ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> ?int $filter = null ]
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method getReflectionConstants ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> ?int $filter = null ]
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method getConstant ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ mixed ]
///         }
///
///         Method [ <internal:Reflection> public method getReflectionConstant ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ ReflectionClassConstant|false ]
///         }
///
///         Method [ <internal:Reflection> public method getInterfaces ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method getInterfaceNames ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method isInterface ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getTraits ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method getTraitNames ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method getTraitAliases ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method isTrait ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isEnum ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isAbstract ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isFinal ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isReadOnly ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getModifiers ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int ]
///         }
///
///         Method [ <internal:Reflection> public method isInstance ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method newInstance ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> mixed ...$args ]
///           }
///           - Tentative return [ object ]
///         }
///
///         Method [ <internal:Reflection> public method newInstanceWithoutConstructor ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ object ]
///         }
///
///         Method [ <internal:Reflection> public method newInstanceArgs ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> array $args = [] ]
///           }
///           - Tentative return [ ?object ]
///         }
///
///         Method [ <internal:Reflection> public method newLazyGhost ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> callable $initializer ]
///             Parameter #1 [ <optional> int $options = 0 ]
///           }
///           - Return [ object ]
///         }
///
///         Method [ <internal:Reflection> public method newLazyProxy ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> callable $factory ]
///             Parameter #1 [ <optional> int $options = 0 ]
///           }
///           - Return [ object ]
///         }
///
///         Method [ <internal:Reflection> public method resetAsLazyGhost ] {
///
///           - Parameters [3] {
///             Parameter #0 [ <required> object $object ]
///             Parameter #1 [ <required> callable $initializer ]
///             Parameter #2 [ <optional> int $options = 0 ]
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection> public method resetAsLazyProxy ] {
///
///           - Parameters [3] {
///             Parameter #0 [ <required> object $object ]
///             Parameter #1 [ <required> callable $factory ]
///             Parameter #2 [ <optional> int $options = 0 ]
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection> public method initializeLazyObject ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///           - Return [ object ]
///         }
///
///         Method [ <internal:Reflection> public method isUninitializedLazyObject ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method markLazyObjectAsInitialized ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///           - Return [ object ]
///         }
///
///         Method [ <internal:Reflection> public method getLazyInitializer ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///           - Return [ ?callable ]
///         }
///
///         Method [ <internal:Reflection> public method getParentClass ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ReflectionClass|false ]
///         }
///
///         Method [ <internal:Reflection> public method isSubclassOf ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> ReflectionClass|string $class ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getStaticProperties ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method getStaticPropertyValue ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> string $name ]
///             Parameter #1 [ <optional> mixed $default = <default> ]
///           }
///           - Tentative return [ mixed ]
///         }
///
///         Method [ <internal:Reflection> public method setStaticPropertyValue ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> string $name ]
///             Parameter #1 [ <required> mixed $value ]
///           }
///           - Tentative return [ void ]
///         }
///
///         Method [ <internal:Reflection> public method getDefaultProperties ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method isIterable ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isIterateable ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method implementsInterface ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> ReflectionClass|string $interface ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getExtension ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionExtension ]
///         }
///
///         Method [ <internal:Reflection> public method getExtensionName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection> public method inNamespace ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getNamespaceName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getShortName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getAttributes ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <optional> ?string $name = null ]
///             Parameter #1 [ <optional> int $flags = 0 ]
///           }
///           - Return [ array ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> class ReflectionObject extends ReflectionClass implements Reflector, Stringable ] {
///
///       - Constants [6] {
///         Constant [ public int IS_IMPLICIT_ABSTRACT ] { 16 }
///         Constant [ public int IS_EXPLICIT_ABSTRACT ] { 64 }
///         Constant [ public int IS_FINAL ] { 32 }
///         Constant [ public int IS_READONLY ] { 65536 }
///         Constant [ public int SKIP_INITIALIZATION_ON_SERIALIZE ] { 8 }
///         Constant [ public int SKIP_DESTRUCTOR ] { 16 }
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [1] {
///         Property [ public string $name ]
///       }
///
///       - Methods [63] {
///         Method [ <internal:Reflection, overwrites ReflectionClass, ctor> public method __construct ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isInternal ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isUserDefined ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isAnonymous ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isInstantiable ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isCloneable ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getFileName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getStartLine ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getEndLine ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getDocComment ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getConstructor ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionMethod ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method hasMethod ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getMethod ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ ReflectionMethod ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getMethods ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> ?int $filter = null ]
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method hasProperty ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getProperty ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ ReflectionProperty ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getProperties ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> ?int $filter = null ]
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method hasConstant ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getConstants ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> ?int $filter = null ]
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getReflectionConstants ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> ?int $filter = null ]
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getConstant ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ mixed ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getReflectionConstant ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ ReflectionClassConstant|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getInterfaces ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getInterfaceNames ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isInterface ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getTraits ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getTraitNames ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getTraitAliases ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isTrait ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isEnum ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isAbstract ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isFinal ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isReadOnly ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getModifiers ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isInstance ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method newInstance ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> mixed ...$args ]
///           }
///           - Tentative return [ object ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method newInstanceWithoutConstructor ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ object ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method newInstanceArgs ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> array $args = [] ]
///           }
///           - Tentative return [ ?object ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method newLazyGhost ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> callable $initializer ]
///             Parameter #1 [ <optional> int $options = 0 ]
///           }
///           - Return [ object ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method newLazyProxy ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> callable $factory ]
///             Parameter #1 [ <optional> int $options = 0 ]
///           }
///           - Return [ object ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method resetAsLazyGhost ] {
///
///           - Parameters [3] {
///             Parameter #0 [ <required> object $object ]
///             Parameter #1 [ <required> callable $initializer ]
///             Parameter #2 [ <optional> int $options = 0 ]
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method resetAsLazyProxy ] {
///
///           - Parameters [3] {
///             Parameter #0 [ <required> object $object ]
///             Parameter #1 [ <required> callable $factory ]
///             Parameter #2 [ <optional> int $options = 0 ]
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method initializeLazyObject ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///           - Return [ object ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isUninitializedLazyObject ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method markLazyObjectAsInitialized ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///           - Return [ object ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getLazyInitializer ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///           - Return [ ?callable ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getParentClass ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ReflectionClass|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isSubclassOf ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> ReflectionClass|string $class ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getStaticProperties ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getStaticPropertyValue ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> string $name ]
///             Parameter #1 [ <optional> mixed $default = <default> ]
///           }
///           - Tentative return [ mixed ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method setStaticPropertyValue ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> string $name ]
///             Parameter #1 [ <required> mixed $value ]
///           }
///           - Tentative return [ void ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getDefaultProperties ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isIterable ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isIterateable ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method implementsInterface ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> ReflectionClass|string $interface ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getExtension ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionExtension ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getExtensionName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method inNamespace ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getNamespaceName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getShortName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getAttributes ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <optional> ?string $name = null ]
///             Parameter #1 [ <optional> int $flags = 0 ]
///           }
///           - Return [ array ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> class ReflectionProperty implements Stringable, Reflector ] {
///
///       - Constants [10] {
///         Constant [ public int IS_STATIC ] { 16 }
///         Constant [ public int IS_READONLY ] { 128 }
///         Constant [ public int IS_PUBLIC ] { 1 }
///         Constant [ public int IS_PROTECTED ] { 2 }
///         Constant [ public int IS_PRIVATE ] { 4 }
///         Constant [ public int IS_ABSTRACT ] { 64 }
///         Constant [ public int IS_PROTECTED_SET ] { 2048 }
///         Constant [ public int IS_PRIVATE_SET ] { 4096 }
///         Constant [ public int IS_VIRTUAL ] { 512 }
///         Constant [ public int IS_FINAL ] { 32 }
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [2] {
///         Property [ public string $name ]
///         Property [ public string $class ]
///       }
///
///       - Methods [40] {
///         Method [ <internal:Reflection> private method __clone ] {
///
///           - Parameters [0] {
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection, ctor> public method __construct ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> object|string $class ]
///             Parameter #1 [ <required> string $property ]
///           }
///         }
///
///         Method [ <internal:Reflection, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getMangledName ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getValue ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> ?object $object = null ]
///           }
///           - Tentative return [ mixed ]
///         }
///
///         Method [ <internal:Reflection> public method setValue ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> mixed $objectOrValue ]
///             Parameter #1 [ <optional> mixed $value = <default> ]
///           }
///           - Tentative return [ void ]
///         }
///
///         Method [ <internal:Reflection> public method getRawValue ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///           - Return [ mixed ]
///         }
///
///         Method [ <internal:Reflection> public method setRawValue ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> object $object ]
///             Parameter #1 [ <required> mixed $value ]
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection> public method setRawValueWithoutLazyInitialization ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> object $object ]
///             Parameter #1 [ <required> mixed $value ]
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection> public method skipLazyInitialization ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection> public method isLazy ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isInitialized ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> ?object $object = null ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isPublic ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isPrivate ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isProtected ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isPrivateSet ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isProtectedSet ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isStatic ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isReadOnly ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isDefault ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isDynamic ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isAbstract ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isVirtual ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isPromoted ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getModifiers ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int ]
///         }
///
///         Method [ <internal:Reflection> public method getDeclaringClass ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ReflectionClass ]
///         }
///
///         Method [ <internal:Reflection> public method getDocComment ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal, deprecated:Reflection> public method setAccessible ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> bool $accessible ]
///           }
///           - Tentative return [ void ]
///         }
///
///         Method [ <internal:Reflection> public method getType ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionType ]
///         }
///
///         Method [ <internal:Reflection> public method getSettableType ] {
///
///           - Parameters [0] {
///           }
///           - Return [ ?ReflectionType ]
///         }
///
///         Method [ <internal:Reflection> public method hasType ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method hasDefaultValue ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getDefaultValue ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ mixed ]
///         }
///
///         Method [ <internal:Reflection> public method getAttributes ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <optional> ?string $name = null ]
///             Parameter #1 [ <optional> int $flags = 0 ]
///           }
///           - Return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method hasHooks ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getHooks ] {
///
///           - Parameters [0] {
///           }
///           - Return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method hasHook ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> PropertyHookType $type ]
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getHook ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> PropertyHookType $type ]
///           }
///           - Return [ ?ReflectionMethod ]
///         }
///
///         Method [ <internal:Reflection> public method isFinal ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> class ReflectionClassConstant implements Stringable, Reflector ] {
///
///       - Constants [4] {
///         Constant [ public int IS_PUBLIC ] { 1 }
///         Constant [ public int IS_PROTECTED ] { 2 }
///         Constant [ public int IS_PRIVATE ] { 4 }
///         Constant [ public int IS_FINAL ] { 32 }
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [2] {
///         Property [ public string $name ]
///         Property [ public string $class ]
///       }
///
///       - Methods [17] {
///         Method [ <internal:Reflection> private method __clone ] {
///
///           - Parameters [0] {
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection, ctor> public method __construct ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> object|string $class ]
///             Parameter #1 [ <required> string $constant ]
///           }
///         }
///
///         Method [ <internal:Reflection, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getValue ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ mixed ]
///         }
///
///         Method [ <internal:Reflection> public method isPublic ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isPrivate ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isProtected ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isFinal ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getModifiers ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int ]
///         }
///
///         Method [ <internal:Reflection> public method getDeclaringClass ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ReflectionClass ]
///         }
///
///         Method [ <internal:Reflection> public method getDocComment ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection> public method getAttributes ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <optional> ?string $name = null ]
///             Parameter #1 [ <optional> int $flags = 0 ]
///           }
///           - Return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method isEnumCase ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isDeprecated ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method hasType ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getType ] {
///
///           - Parameters [0] {
///           }
///           - Return [ ?ReflectionType ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> class ReflectionExtension implements Stringable, Reflector ] {
///
///       - Constants [0] {
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [1] {
///         Property [ public string $name ]
///       }
///
///       - Methods [14] {
///         Method [ <internal:Reflection> private method __clone ] {
///
///           - Parameters [0] {
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection, ctor> public method __construct ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///         }
///
///         Method [ <internal:Reflection, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getVersion ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?string ]
///         }
///
///         Method [ <internal:Reflection> public method getFunctions ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method getConstants ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method getINIEntries ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method getClasses ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method getClassNames ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method getDependencies ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method info ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ void ]
///         }
///
///         Method [ <internal:Reflection> public method isPersistent ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method isTemporary ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> class ReflectionZendExtension implements Stringable, Reflector ] {
///
///       - Constants [0] {
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [1] {
///         Property [ public string $name ]
///       }
///
///       - Methods [8] {
///         Method [ <internal:Reflection> private method __clone ] {
///
///           - Parameters [0] {
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection, ctor> public method __construct ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///         }
///
///         Method [ <internal:Reflection, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getVersion ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getAuthor ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getURL ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getCopyright ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> final class ReflectionReference ] {
///
///       - Constants [0] {
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [1] {
///         Method [ <internal:Reflection> static public method fromArrayElement ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> array $array ]
///             Parameter #1 [ <required> string|int $key ]
///           }
///           - Return [ ?ReflectionReference ]
///         }
///       }
///
///       - Properties [0] {
///       }
///
///       - Methods [3] {
///         Method [ <internal:Reflection> public method getId ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection> private method __clone ] {
///
///           - Parameters [0] {
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection, ctor> private method __construct ] {
///
///           - Parameters [0] {
///           }
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> class ReflectionAttribute implements Stringable, Reflector ] {
///
///       - Constants [1] {
///         Constant [ public int IS_INSTANCEOF ] { 2 }
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [1] {
///         Property [ public string $name ]
///       }
///
///       - Methods [8] {
///         Method [ <internal:Reflection> public method getName ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getTarget ] {
///
///           - Parameters [0] {
///           }
///           - Return [ int ]
///         }
///
///         Method [ <internal:Reflection> public method isRepeated ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getArguments ] {
///
///           - Parameters [0] {
///           }
///           - Return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method newInstance ] {
///
///           - Parameters [0] {
///           }
///           - Return [ object ]
///         }
///
///         Method [ <internal:Reflection, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection> private method __clone ] {
///
///           - Parameters [0] {
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection, ctor> private method __construct ] {
///
///           - Parameters [0] {
///           }
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> class ReflectionEnum extends ReflectionClass implements Reflector, Stringable ] {
///
///       - Constants [6] {
///         Constant [ public int IS_IMPLICIT_ABSTRACT ] { 16 }
///         Constant [ public int IS_EXPLICIT_ABSTRACT ] { 64 }
///         Constant [ public int IS_FINAL ] { 32 }
///         Constant [ public int IS_READONLY ] { 65536 }
///         Constant [ public int SKIP_INITIALIZATION_ON_SERIALIZE ] { 8 }
///         Constant [ public int SKIP_DESTRUCTOR ] { 16 }
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [1] {
///         Property [ public string $name ]
///       }
///
///       - Methods [68] {
///         Method [ <internal:Reflection, overwrites ReflectionClass, ctor> public method __construct ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object|string $objectOrClass ]
///           }
///         }
///
///         Method [ <internal:Reflection> public method hasCase ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getCase ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Return [ ReflectionEnumUnitCase ]
///         }
///
///         Method [ <internal:Reflection> public method getCases ] {
///
///           - Parameters [0] {
///           }
///           - Return [ array ]
///         }
///
///         Method [ <internal:Reflection> public method isBacked ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getBackingType ] {
///
///           - Parameters [0] {
///           }
///           - Return [ ?ReflectionNamedType ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isInternal ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isUserDefined ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isAnonymous ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isInstantiable ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isCloneable ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getFileName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getStartLine ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getEndLine ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getDocComment ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getConstructor ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionMethod ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method hasMethod ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getMethod ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ ReflectionMethod ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getMethods ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> ?int $filter = null ]
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method hasProperty ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getProperty ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ ReflectionProperty ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getProperties ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> ?int $filter = null ]
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method hasConstant ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getConstants ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> ?int $filter = null ]
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getReflectionConstants ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> ?int $filter = null ]
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getConstant ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ mixed ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getReflectionConstant ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///           - Tentative return [ ReflectionClassConstant|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getInterfaces ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getInterfaceNames ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isInterface ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getTraits ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getTraitNames ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getTraitAliases ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isTrait ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isEnum ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isAbstract ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isFinal ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isReadOnly ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getModifiers ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isInstance ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method newInstance ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> mixed ...$args ]
///           }
///           - Tentative return [ object ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method newInstanceWithoutConstructor ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ object ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method newInstanceArgs ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> array $args = [] ]
///           }
///           - Tentative return [ ?object ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method newLazyGhost ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> callable $initializer ]
///             Parameter #1 [ <optional> int $options = 0 ]
///           }
///           - Return [ object ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method newLazyProxy ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> callable $factory ]
///             Parameter #1 [ <optional> int $options = 0 ]
///           }
///           - Return [ object ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method resetAsLazyGhost ] {
///
///           - Parameters [3] {
///             Parameter #0 [ <required> object $object ]
///             Parameter #1 [ <required> callable $initializer ]
///             Parameter #2 [ <optional> int $options = 0 ]
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method resetAsLazyProxy ] {
///
///           - Parameters [3] {
///             Parameter #0 [ <required> object $object ]
///             Parameter #1 [ <required> callable $factory ]
///             Parameter #2 [ <optional> int $options = 0 ]
///           }
///           - Return [ void ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method initializeLazyObject ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///           - Return [ object ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isUninitializedLazyObject ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method markLazyObjectAsInitialized ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///           - Return [ object ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getLazyInitializer ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> object $object ]
///           }
///           - Return [ ?callable ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getParentClass ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ReflectionClass|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isSubclassOf ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> ReflectionClass|string $class ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getStaticProperties ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getStaticPropertyValue ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> string $name ]
///             Parameter #1 [ <optional> mixed $default = <default> ]
///           }
///           - Tentative return [ mixed ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method setStaticPropertyValue ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> string $name ]
///             Parameter #1 [ <required> mixed $value ]
///           }
///           - Tentative return [ void ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getDefaultProperties ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isIterable ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method isIterateable ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method implementsInterface ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> ReflectionClass|string $interface ]
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getExtension ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ?ReflectionExtension ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getExtensionName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method inNamespace ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getNamespaceName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getShortName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClass> public method getAttributes ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <optional> ?string $name = null ]
///             Parameter #1 [ <optional> int $flags = 0 ]
///           }
///           - Return [ array ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> class ReflectionEnumUnitCase extends ReflectionClassConstant implements Reflector, Stringable ] {
///
///       - Constants [4] {
///         Constant [ public int IS_PUBLIC ] { 1 }
///         Constant [ public int IS_PROTECTED ] { 2 }
///         Constant [ public int IS_PRIVATE ] { 4 }
///         Constant [ public int IS_FINAL ] { 32 }
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [2] {
///         Property [ public string $name ]
///         Property [ public string $class ]
///       }
///
///       - Methods [17] {
///         Method [ <internal:Reflection, overwrites ReflectionClassConstant, ctor> public method __construct ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> object|string $class ]
///             Parameter #1 [ <required> string $constant ]
///           }
///         }
///
///         Method [ <internal:Reflection> public method getEnum ] {
///
///           - Parameters [0] {
///           }
///           - Return [ ReflectionEnum ]
///         }
///
///         Method [ <internal:Reflection, overwrites ReflectionClassConstant, prototype ReflectionClassConstant> public method getValue ] {
///
///           - Parameters [0] {
///           }
///           - Return [ UnitEnum ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method getName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method isPublic ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method isPrivate ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method isProtected ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method isFinal ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method getModifiers ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method getDeclaringClass ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ReflectionClass ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method getDocComment ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method getAttributes ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <optional> ?string $name = null ]
///             Parameter #1 [ <optional> int $flags = 0 ]
///           }
///           - Return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method isEnumCase ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method isDeprecated ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method hasType ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method getType ] {
///
///           - Parameters [0] {
///           }
///           - Return [ ?ReflectionType ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> class ReflectionEnumBackedCase extends ReflectionEnumUnitCase implements Stringable, Reflector ] {
///
///       - Constants [4] {
///         Constant [ public int IS_PUBLIC ] { 1 }
///         Constant [ public int IS_PROTECTED ] { 2 }
///         Constant [ public int IS_PRIVATE ] { 4 }
///         Constant [ public int IS_FINAL ] { 32 }
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [2] {
///         Property [ public string $name ]
///         Property [ public string $class ]
///       }
///
///       - Methods [18] {
///         Method [ <internal:Reflection, overwrites ReflectionEnumUnitCase, ctor> public method __construct ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <required> object|string $class ]
///             Parameter #1 [ <required> string $constant ]
///           }
///         }
///
///         Method [ <internal:Reflection> public method getBackingValue ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string|int ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionEnumUnitCase> public method getEnum ] {
///
///           - Parameters [0] {
///           }
///           - Return [ ReflectionEnum ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionEnumUnitCase, prototype ReflectionClassConstant> public method getValue ] {
///
///           - Parameters [0] {
///           }
///           - Return [ UnitEnum ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method getName ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method isPublic ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method isPrivate ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method isProtected ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method isFinal ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method getModifiers ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ int ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method getDeclaringClass ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ ReflectionClass ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method getDocComment ] {
///
///           - Parameters [0] {
///           }
///           - Tentative return [ string|false ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method getAttributes ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <optional> ?string $name = null ]
///             Parameter #1 [ <optional> int $flags = 0 ]
///           }
///           - Return [ array ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method isEnumCase ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method isDeprecated ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method hasType ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection, inherits ReflectionClassConstant> public method getType ] {
///
///           - Parameters [0] {
///           }
///           - Return [ ?ReflectionType ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> final class ReflectionFiber ] {
///
///       - Constants [0] {
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [0] {
///       }
///
///       - Methods [6] {
///         Method [ <internal:Reflection, ctor> public method __construct ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> Fiber $fiber ]
///           }
///         }
///
///         Method [ <internal:Reflection> public method getFiber ] {
///
///           - Parameters [0] {
///           }
///           - Return [ Fiber ]
///         }
///
///         Method [ <internal:Reflection> public method getExecutingFile ] {
///
///           - Parameters [0] {
///           }
///           - Return [ ?string ]
///         }
///
///         Method [ <internal:Reflection> public method getExecutingLine ] {
///
///           - Parameters [0] {
///           }
///           - Return [ ?int ]
///         }
///
///         Method [ <internal:Reflection> public method getCallable ] {
///
///           - Parameters [0] {
///           }
///           - Return [ callable ]
///         }
///
///         Method [ <internal:Reflection> public method getTrace ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <optional> int $options = DEBUG_BACKTRACE_PROVIDE_OBJECT ]
///           }
///           - Return [ array ]
///         }
///       }
///     }
///
///     Class [ <internal:Reflection> class ReflectionConstant implements Stringable, Reflector ] {
///
///       - Constants [0] {
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [0] {
///       }
///
///       - Properties [1] {
///         Property [ public string $name ]
///       }
///
///       - Methods [11] {
///         Method [ <internal:Reflection, ctor> public method __construct ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string $name ]
///           }
///         }
///
///         Method [ <internal:Reflection> public method getName ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getNamespaceName ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getShortName ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getValue ] {
///
///           - Parameters [0] {
///           }
///           - Return [ mixed ]
///         }
///
///         Method [ <internal:Reflection> public method isDeprecated ] {
///
///           - Parameters [0] {
///           }
///           - Return [ bool ]
///         }
///
///         Method [ <internal:Reflection> public method getFileName ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string|false ]
///         }
///
///         Method [ <internal:Reflection> public method getExtension ] {
///
///           - Parameters [0] {
///           }
///           - Return [ ?ReflectionExtension ]
///         }
///
///         Method [ <internal:Reflection> public method getExtensionName ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string|false ]
///         }
///
///         Method [ <internal:Reflection, prototype Stringable> public method __toString ] {
///
///           - Parameters [0] {
///           }
///           - Return [ string ]
///         }
///
///         Method [ <internal:Reflection> public method getAttributes ] {
///
///           - Parameters [2] {
///             Parameter #0 [ <optional> ?string $name = null ]
///             Parameter #1 [ <optional> int $flags = 0 ]
///           }
///           - Return [ array ]
///         }
///       }
///     }
///
///     Enum [ <internal:Reflection> enum PropertyHookType: string implements BackedEnum, UnitEnum ] {
///
///       - Enum cases [2] {
///         Case Get = get
///         Case Set = set
///       }
///
///       - Constants [0] {
///       }
///
///       - Static properties [0] {
///       }
///
///       - Static methods [3] {
///         Method [ <internal:Reflection, prototype UnitEnum> static public method cases ] {
///
///           - Parameters [0] {
///           }
///           - Return [ array ]
///         }
///
///         Method [ <internal:Reflection, prototype BackedEnum> static public method from ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string|int $value ]
///           }
///           - Return [ static ]
///         }
///
///         Method [ <internal:Reflection, prototype BackedEnum> static public method tryFrom ] {
///
///           - Parameters [1] {
///             Parameter #0 [ <required> string|int $value ]
///           }
///           - Return [ ?static ]
///         }
///       }
///
///       - Properties [2] {
///         Property [ public protected(set) readonly string $name ]
///         Property [ public protected(set) readonly string $value ]
///       }
///
///       - Methods [0] {
///       }
///     }
///   }
/// }
/// ```
pub(super) fn classes() -> Vec<Declared> {
    vec![
        reflection_exception(),
        reflection(),
        reflector(),
        reflection_function_abstract(),
        reflection_function(),
        reflection_generator(),
        reflection_parameter(),
        reflection_type(),
        reflection_named_type(),
        reflection_union_type(),
        reflection_intersection_type(),
        reflection_method(),
        reflection_class(),
        reflection_object(),
        reflection_property(),
        reflection_class_constant(),
        reflection_extension(),
        reflection_zend_extension(),
        reflection_reference(),
        reflection_attribute(),
        reflection_enum(),
        reflection_enum_unit_case(),
        reflection_enum_backed_case(),
        reflection_fiber(),
        reflection_constant(),
        property_hook_type(),
    ]
}

/// `ReflectionException` as the stub declares it.
fn reflection_exception() -> Declared {
    (
        class(b"ReflectionException", Some(b"Exception"), &[]),
        Vec::new(),
    )
}

/// `Reflection` as the stub declares it.
fn reflection() -> Declared {
    (
        class(b"Reflection", None, &[]),
        vec![(
            is_static(tentative(method(
                b"getModifierNames",
                &[parameter(b"modifiers", Type::Int)],
                Some(Type::Array),
            ))),
            super::member::reflection_get_modifier_names as Entry,
        )],
    )
}

/// `interface Reflector extends Stringable` as the stub declares it.
fn reflector() -> Declared {
    (
        ClassDecl {
            kind: ClassKind::Interface,
            ..class(b"Reflector", None, &[b"Stringable"])
        },
        Vec::new(),
    )
}

/// `ReflectionFunctionAbstract` as the stub declares it.
fn reflection_function_abstract() -> Declared {
    (
        ClassDecl {
            flags: ClassFlags {
                is_abstract: true,
                ..ClassFlags::default()
            },
            properties: vec![name_property()],
            ..class(b"ReflectionFunctionAbstract", None, &[b"Reflector"])
        },
        vec![
            (
                private(method(b"__clone", &[], Some(Type::Void))),
                super::func::reflection_clone as Entry,
            ),
            (
                tentative(method(b"inNamespace", &[], Some(Type::Bool))),
                super::func::reflection_function_in_namespace as Entry,
            ),
            (
                tentative(method(b"isClosure", &[], Some(Type::Bool))),
                super::func::reflection_function_is_closure as Entry,
            ),
            (
                tentative(method(b"isDeprecated", &[], Some(Type::Bool))),
                super::reflection_function_is_deprecated as Entry,
            ),
            (
                tentative(method(b"isInternal", &[], Some(Type::Bool))),
                super::func::reflection_function_is_internal as Entry,
            ),
            (
                tentative(method(b"isUserDefined", &[], Some(Type::Bool))),
                super::func::reflection_function_is_user_defined as Entry,
            ),
            (
                tentative(method(b"isGenerator", &[], Some(Type::Bool))),
                unbuilt as Entry,
            ),
            (
                tentative(method(b"isVariadic", &[], Some(Type::Bool))),
                super::func::reflection_function_is_variadic as Entry,
            ),
            (
                tentative(method(b"isStatic", &[], Some(Type::Bool))),
                super::func::reflection_function_is_static as Entry,
            ),
            (
                tentative(method(b"getClosureThis", &[], Some(nullable(Type::Object)))),
                super::func::reflection_function_get_closure_this as Entry,
            ),
            (
                tentative(method(
                    b"getClosureScopeClass",
                    &[],
                    Some(nullable(named(b"ReflectionClass"))),
                )),
                super::func::reflection_function_get_closure_scope_class as Entry,
            ),
            (
                tentative(method(
                    b"getClosureCalledClass",
                    &[],
                    Some(nullable(named(b"ReflectionClass"))),
                )),
                unbuilt as Entry,
            ),
            (
                method(b"getClosureUsedVariables", &[], Some(Type::Array)),
                super::func::reflection_function_get_closure_used_variables as Entry,
            ),
            (
                tentative(method(
                    b"getDocComment",
                    &[],
                    Some(union(&[Type::String, Type::False])),
                )),
                super::func::reflection_function_get_doc_comment as Entry,
            ),
            (
                tentative(method(
                    b"getEndLine",
                    &[],
                    Some(union(&[Type::Int, Type::False])),
                )),
                unbuilt as Entry,
            ),
            (
                tentative(method(
                    b"getExtension",
                    &[],
                    Some(nullable(named(b"ReflectionExtension"))),
                )),
                super::func::reflection_function_get_extension as Entry,
            ),
            (
                tentative(method(
                    b"getExtensionName",
                    &[],
                    Some(union(&[Type::String, Type::False])),
                )),
                super::func::reflection_function_get_extension_name as Entry,
            ),
            (
                tentative(method(
                    b"getFileName",
                    &[],
                    Some(union(&[Type::String, Type::False])),
                )),
                super::func::reflection_function_get_file_name as Entry,
            ),
            (
                tentative(method(b"getName", &[], Some(Type::String))),
                super::func::reflection_function_get_name as Entry,
            ),
            (
                tentative(method(b"getNamespaceName", &[], Some(Type::String))),
                super::func::reflection_function_get_namespace_name as Entry,
            ),
            (
                tentative(method(b"getNumberOfParameters", &[], Some(Type::Int))),
                super::signature::reflection_function_get_number_of_parameters as Entry,
            ),
            (
                tentative(method(
                    b"getNumberOfRequiredParameters",
                    &[],
                    Some(Type::Int),
                )),
                super::signature::reflection_function_get_number_of_required_parameters as Entry,
            ),
            (
                tentative(method(b"getParameters", &[], Some(Type::Array))),
                super::signature::reflection_function_get_parameters as Entry,
            ),
            (
                tentative(method(b"getShortName", &[], Some(Type::String))),
                super::func::reflection_function_get_short_name as Entry,
            ),
            (
                tentative(method(
                    b"getStartLine",
                    &[],
                    Some(union(&[Type::Int, Type::False])),
                )),
                super::func::reflection_function_get_start_line as Entry,
            ),
            (
                tentative(method(b"getStaticVariables", &[], Some(Type::Array))),
                unbuilt as Entry,
            ),
            (
                tentative(method(b"returnsReference", &[], Some(Type::Bool))),
                super::func::reflection_function_returns_reference as Entry,
            ),
            (
                tentative(method(b"hasReturnType", &[], Some(Type::Bool))),
                super::func::reflection_function_has_return_type as Entry,
            ),
            (
                tentative(method(
                    b"getReturnType",
                    &[],
                    Some(nullable(named(b"ReflectionType"))),
                )),
                super::func::reflection_function_get_return_type as Entry,
            ),
            (
                method(b"hasTentativeReturnType", &[], Some(Type::Bool)),
                super::func::reflection_function_has_tentative_return_type as Entry,
            ),
            (
                method(
                    b"getTentativeReturnType",
                    &[],
                    Some(nullable(named(b"ReflectionType"))),
                ),
                super::func::reflection_function_get_tentative_return_type as Entry,
            ),
            (
                method(
                    b"getAttributes",
                    &[
                        with_default(b"name", nullable(Type::String), ConstExpr::Null),
                        with_default(b"flags", Type::Int, ConstExpr::Long(0)),
                    ],
                    Some(Type::Array),
                ),
                super::func::reflection_function_get_attributes as Entry,
            ),
        ],
    )
}

/// `ReflectionFunction` as the stub declares it.
fn reflection_function() -> Declared {
    (
        ClassDecl {
            constants: vec![int_constant(b"IS_DEPRECATED", 2048)],
            ..class(
                b"ReflectionFunction",
                Some(b"ReflectionFunctionAbstract"),
                &[],
            )
        },
        vec![
            (
                method(
                    b"__construct",
                    &[parameter(
                        b"function",
                        union(&[named(b"Closure"), Type::String]),
                    )],
                    None,
                ),
                super::reflection_function_construct as Entry,
            ),
            (
                method(b"__toString", &[], Some(Type::String)),
                super::reflection_function_to_string as Entry,
            ),
            (
                method(b"isAnonymous", &[], Some(Type::Bool)),
                super::func::reflection_function_is_anonymous as Entry,
            ),
            (
                deprecated(
                    tentative(method(b"isDisabled", &[], Some(Type::Bool))),
                    "8.0",
                    "as ReflectionFunction can no longer be constructed for disabled functions",
                ),
                unbuilt as Entry,
            ),
            (
                tentative(method(
                    b"invoke",
                    &[variadic(b"args", Type::Mixed)],
                    Some(Type::Mixed),
                )),
                super::func::reflection_function_invoke as Entry,
            ),
            (
                tentative(method(
                    b"invokeArgs",
                    &[parameter(b"args", Type::Array)],
                    Some(Type::Mixed),
                )),
                super::func::reflection_function_invoke_args as Entry,
            ),
            (
                tentative(method(b"getClosure", &[], Some(named(b"Closure")))),
                unbuilt as Entry,
            ),
        ],
    )
}

/// `ReflectionGenerator` as the stub declares it.
fn reflection_generator() -> Declared {
    (
        ClassDecl {
            flags: ClassFlags {
                is_final: true,
                ..ClassFlags::default()
            },
            ..class(b"ReflectionGenerator", None, &[])
        },
        vec![
            (
                method(
                    b"__construct",
                    &[parameter(b"generator", named(b"Generator"))],
                    None,
                ),
                unbuilt as Entry,
            ),
            (
                method(b"getExecutingLine", &[], Some(Type::Int)),
                unbuilt as Entry,
            ),
            (
                method(b"getExecutingFile", &[], Some(Type::String)),
                unbuilt as Entry,
            ),
            (
                method(
                    b"getTrace",
                    &[with_default(
                        b"options",
                        Type::Int,
                        ConstExpr::Constant(Name::from("DEBUG_BACKTRACE_PROVIDE_OBJECT")),
                    )],
                    Some(Type::Array),
                ),
                unbuilt as Entry,
            ),
            (
                method(
                    b"getFunction",
                    &[],
                    Some(named(b"ReflectionFunctionAbstract")),
                ),
                unbuilt as Entry,
            ),
            (
                method(b"getThis", &[], Some(nullable(Type::Object))),
                unbuilt as Entry,
            ),
            (
                method(b"getExecutingGenerator", &[], Some(named(b"Generator"))),
                unbuilt as Entry,
            ),
            (method(b"isClosed", &[], Some(Type::Bool)), unbuilt as Entry),
        ],
    )
}

/// `ReflectionMethod` as the stub declares it.
fn reflection_method() -> Declared {
    (
        ClassDecl {
            constants: vec![
                int_constant(b"IS_STATIC", 16),
                int_constant(b"IS_PUBLIC", 1),
                int_constant(b"IS_PROTECTED", 2),
                int_constant(b"IS_PRIVATE", 4),
                int_constant(b"IS_ABSTRACT", 64),
                int_constant(b"IS_FINAL", 32),
            ],
            properties: vec![PropertyDecl {
                name: Name::from("class"),
                ..name_property()
            }],
            ..class(
                b"ReflectionMethod",
                Some(b"ReflectionFunctionAbstract"),
                &[],
            )
        },
        vec![
            (
                method(
                    b"__construct",
                    &[
                        parameter(b"objectOrMethod", union(&[Type::Object, Type::String])),
                        with_default(b"method", nullable(Type::String), ConstExpr::Null),
                    ],
                    None,
                ),
                super::reflection_method_construct as Entry,
            ),
            (
                is_static(method(
                    b"createFromMethodName",
                    &[parameter(b"method", Type::String)],
                    Some(Type::CalledClass),
                )),
                super::func::reflection_method_create_from_method_name as Entry,
            ),
            (
                method(b"__toString", &[], Some(Type::String)),
                super::reflection_method_to_string as Entry,
            ),
            (
                tentative(method(b"isPublic", &[], Some(Type::Bool))),
                super::func::reflection_method_is_public as Entry,
            ),
            (
                tentative(method(b"isPrivate", &[], Some(Type::Bool))),
                super::func::reflection_method_is_private as Entry,
            ),
            (
                tentative(method(b"isProtected", &[], Some(Type::Bool))),
                super::func::reflection_method_is_protected as Entry,
            ),
            (
                tentative(method(b"isAbstract", &[], Some(Type::Bool))),
                super::func::reflection_method_is_abstract as Entry,
            ),
            (
                tentative(method(b"isFinal", &[], Some(Type::Bool))),
                super::func::reflection_method_is_final as Entry,
            ),
            (
                tentative(method(b"isConstructor", &[], Some(Type::Bool))),
                super::func::reflection_method_is_constructor as Entry,
            ),
            (
                tentative(method(b"isDestructor", &[], Some(Type::Bool))),
                super::func::reflection_method_is_destructor as Entry,
            ),
            (
                tentative(method(
                    b"getClosure",
                    &[with_default(
                        b"object",
                        nullable(Type::Object),
                        ConstExpr::Null,
                    )],
                    Some(named(b"Closure")),
                )),
                unbuilt as Entry,
            ),
            (
                tentative(method(b"getModifiers", &[], Some(Type::Int))),
                super::func::reflection_method_get_modifiers as Entry,
            ),
            (
                tentative(method(
                    b"invoke",
                    &[
                        parameter(b"object", nullable(Type::Object)),
                        variadic(b"args", Type::Mixed),
                    ],
                    Some(Type::Mixed),
                )),
                super::func::reflection_method_invoke as Entry,
            ),
            (
                tentative(method(
                    b"invokeArgs",
                    &[
                        parameter(b"object", nullable(Type::Object)),
                        parameter(b"args", Type::Array),
                    ],
                    Some(Type::Mixed),
                )),
                super::func::reflection_method_invoke_args as Entry,
            ),
            (
                tentative(method(
                    b"getDeclaringClass",
                    &[],
                    Some(named(b"ReflectionClass")),
                )),
                super::func::reflection_method_get_declaring_class as Entry,
            ),
            (
                tentative(method(
                    b"getPrototype",
                    &[],
                    Some(named(b"ReflectionMethod")),
                )),
                super::func::reflection_method_get_prototype as Entry,
            ),
            (
                method(b"hasPrototype", &[], Some(Type::Bool)),
                super::func::reflection_method_has_prototype as Entry,
            ),
            (
                deprecated(
                    tentative(method(
                        b"setAccessible",
                        &[parameter(b"accessible", Type::Bool)],
                        Some(Type::Void),
                    )),
                    "8.5",
                    "as it has no effect since PHP 8.1",
                ),
                unbuilt as Entry,
            ),
        ],
    )
}

/// `ReflectionClass` as the stub declares it.
fn reflection_class() -> Declared {
    (
        ClassDecl {
            constants: vec![
                int_constant(b"IS_IMPLICIT_ABSTRACT", 16),
                int_constant(b"IS_EXPLICIT_ABSTRACT", 64),
                int_constant(b"IS_FINAL", 32),
                int_constant(b"IS_READONLY", 65536),
                int_constant(b"SKIP_INITIALIZATION_ON_SERIALIZE", 8),
                int_constant(b"SKIP_DESTRUCTOR", 16),
            ],
            properties: vec![name_property()],
            ..class(b"ReflectionClass", None, &[b"Reflector"])
        },
        vec![
            (
                private(method(b"__clone", &[], Some(Type::Void))),
                super::func::reflection_clone as Entry,
            ),
            (
                method(
                    b"__construct",
                    &[parameter(
                        b"objectOrClass",
                        union(&[Type::Object, Type::String]),
                    )],
                    None,
                ),
                super::reflection_class_construct as Entry,
            ),
            (
                method(b"__toString", &[], Some(Type::String)),
                super::reflection_class_to_string as Entry,
            ),
            (
                tentative(method(b"getName", &[], Some(Type::String))),
                super::reflection_class_get_name as Entry,
            ),
            (
                tentative(method(b"isInternal", &[], Some(Type::Bool))),
                super::reflection_class_is_internal as Entry,
            ),
            (
                tentative(method(b"isUserDefined", &[], Some(Type::Bool))),
                super::reflection_class_is_user_defined as Entry,
            ),
            (
                tentative(method(b"isAnonymous", &[], Some(Type::Bool))),
                super::class::reflection_class_is_anonymous as Entry,
            ),
            (
                tentative(method(b"isInstantiable", &[], Some(Type::Bool))),
                super::class::reflection_class_is_instantiable as Entry,
            ),
            (
                tentative(method(b"isCloneable", &[], Some(Type::Bool))),
                super::class::reflection_class_is_cloneable as Entry,
            ),
            (
                tentative(method(
                    b"getFileName",
                    &[],
                    Some(union(&[Type::String, Type::False])),
                )),
                super::reflection_class_get_file_name as Entry,
            ),
            (
                tentative(method(
                    b"getStartLine",
                    &[],
                    Some(union(&[Type::Int, Type::False])),
                )),
                super::reflection_class_get_start_line as Entry,
            ),
            (
                tentative(method(
                    b"getEndLine",
                    &[],
                    Some(union(&[Type::Int, Type::False])),
                )),
                super::reflection_class_get_end_line as Entry,
            ),
            (
                tentative(method(
                    b"getDocComment",
                    &[],
                    Some(union(&[Type::String, Type::False])),
                )),
                super::class::reflection_class_get_doc_comment as Entry,
            ),
            (
                tentative(method(
                    b"getConstructor",
                    &[],
                    Some(nullable(named(b"ReflectionMethod"))),
                )),
                super::class::reflection_class_get_constructor as Entry,
            ),
            (
                tentative(method(
                    b"hasMethod",
                    &[parameter(b"name", Type::String)],
                    Some(Type::Bool),
                )),
                super::class::reflection_class_has_method as Entry,
            ),
            (
                tentative(method(
                    b"getMethod",
                    &[parameter(b"name", Type::String)],
                    Some(named(b"ReflectionMethod")),
                )),
                super::class::reflection_class_get_method as Entry,
            ),
            (
                tentative(method(
                    b"getMethods",
                    &[with_default(
                        b"filter",
                        nullable(Type::Int),
                        ConstExpr::Null,
                    )],
                    Some(Type::Array),
                )),
                super::class::reflection_class_get_methods as Entry,
            ),
            (
                tentative(method(
                    b"hasProperty",
                    &[parameter(b"name", Type::String)],
                    Some(Type::Bool),
                )),
                super::reflection_class_has_property as Entry,
            ),
            (
                tentative(method(
                    b"getProperty",
                    &[parameter(b"name", Type::String)],
                    Some(named(b"ReflectionProperty")),
                )),
                super::class::reflection_class_get_property as Entry,
            ),
            (
                tentative(method(
                    b"getProperties",
                    &[with_default(
                        b"filter",
                        nullable(Type::Int),
                        ConstExpr::Null,
                    )],
                    Some(Type::Array),
                )),
                super::class::reflection_class_get_properties as Entry,
            ),
            (
                tentative(method(
                    b"hasConstant",
                    &[parameter(b"name", Type::String)],
                    Some(Type::Bool),
                )),
                super::reflection_class_has_constant as Entry,
            ),
            (
                tentative(method(
                    b"getConstants",
                    &[with_default(
                        b"filter",
                        nullable(Type::Int),
                        ConstExpr::Null,
                    )],
                    Some(Type::Array),
                )),
                super::class::reflection_class_get_constants as Entry,
            ),
            (
                tentative(method(
                    b"getReflectionConstants",
                    &[with_default(
                        b"filter",
                        nullable(Type::Int),
                        ConstExpr::Null,
                    )],
                    Some(Type::Array),
                )),
                super::class::reflection_class_get_reflection_constants as Entry,
            ),
            (
                tentative(method(
                    b"getConstant",
                    &[parameter(b"name", Type::String)],
                    Some(Type::Mixed),
                )),
                super::reflection_class_get_constant as Entry,
            ),
            (
                tentative(method(
                    b"getReflectionConstant",
                    &[parameter(b"name", Type::String)],
                    Some(union(&[named(b"ReflectionClassConstant"), Type::False])),
                )),
                super::class::reflection_class_get_reflection_constant as Entry,
            ),
            (
                tentative(method(b"getInterfaces", &[], Some(Type::Array))),
                super::class::reflection_class_get_interfaces as Entry,
            ),
            (
                tentative(method(b"getInterfaceNames", &[], Some(Type::Array))),
                super::class::reflection_class_get_interface_names as Entry,
            ),
            (
                tentative(method(b"isInterface", &[], Some(Type::Bool))),
                super::class::reflection_class_is_interface as Entry,
            ),
            (
                tentative(method(b"getTraits", &[], Some(Type::Array))),
                super::class::reflection_class_get_traits as Entry,
            ),
            (
                tentative(method(b"getTraitNames", &[], Some(Type::Array))),
                super::class::reflection_class_get_trait_names as Entry,
            ),
            (
                tentative(method(b"getTraitAliases", &[], Some(Type::Array))),
                unbuilt as Entry,
            ),
            (
                tentative(method(b"isTrait", &[], Some(Type::Bool))),
                super::class::reflection_class_is_trait as Entry,
            ),
            (
                method(b"isEnum", &[], Some(Type::Bool)),
                super::class::reflection_class_is_enum as Entry,
            ),
            (
                tentative(method(b"isAbstract", &[], Some(Type::Bool))),
                super::member::reflection_class_is_abstract as Entry,
            ),
            (
                tentative(method(b"isFinal", &[], Some(Type::Bool))),
                super::class::reflection_class_is_final as Entry,
            ),
            (
                method(b"isReadOnly", &[], Some(Type::Bool)),
                super::class::reflection_class_is_read_only as Entry,
            ),
            (
                tentative(method(b"getModifiers", &[], Some(Type::Int))),
                super::class::reflection_class_get_modifiers as Entry,
            ),
            (
                tentative(method(
                    b"isInstance",
                    &[parameter(b"object", Type::Object)],
                    Some(Type::Bool),
                )),
                super::class::reflection_class_is_instance as Entry,
            ),
            (
                tentative(method(
                    b"newInstance",
                    &[variadic(b"args", Type::Mixed)],
                    Some(Type::Object),
                )),
                super::class::reflection_class_new_instance as Entry,
            ),
            (
                tentative(method(
                    b"newInstanceWithoutConstructor",
                    &[],
                    Some(Type::Object),
                )),
                super::class::reflection_class_new_instance_without_constructor as Entry,
            ),
            (
                tentative(method(
                    b"newInstanceArgs",
                    &[with_default(
                        b"args",
                        Type::Array,
                        ConstExpr::Array(Vec::new()),
                    )],
                    Some(nullable(Type::Object)),
                )),
                super::class::reflection_class_new_instance_args as Entry,
            ),
            (
                method(
                    b"newLazyGhost",
                    &[
                        parameter(b"initializer", Type::Callable),
                        with_default(b"options", Type::Int, ConstExpr::Long(0)),
                    ],
                    Some(Type::Object),
                ),
                unbuilt as Entry,
            ),
            (
                method(
                    b"newLazyProxy",
                    &[
                        parameter(b"factory", Type::Callable),
                        with_default(b"options", Type::Int, ConstExpr::Long(0)),
                    ],
                    Some(Type::Object),
                ),
                unbuilt as Entry,
            ),
            (
                method(
                    b"resetAsLazyGhost",
                    &[
                        parameter(b"object", Type::Object),
                        parameter(b"initializer", Type::Callable),
                        with_default(b"options", Type::Int, ConstExpr::Long(0)),
                    ],
                    Some(Type::Void),
                ),
                unbuilt as Entry,
            ),
            (
                method(
                    b"resetAsLazyProxy",
                    &[
                        parameter(b"object", Type::Object),
                        parameter(b"factory", Type::Callable),
                        with_default(b"options", Type::Int, ConstExpr::Long(0)),
                    ],
                    Some(Type::Void),
                ),
                unbuilt as Entry,
            ),
            (
                method(
                    b"initializeLazyObject",
                    &[parameter(b"object", Type::Object)],
                    Some(Type::Object),
                ),
                unbuilt as Entry,
            ),
            (
                method(
                    b"isUninitializedLazyObject",
                    &[parameter(b"object", Type::Object)],
                    Some(Type::Bool),
                ),
                unbuilt as Entry,
            ),
            (
                method(
                    b"markLazyObjectAsInitialized",
                    &[parameter(b"object", Type::Object)],
                    Some(Type::Object),
                ),
                unbuilt as Entry,
            ),
            (
                method(
                    b"getLazyInitializer",
                    &[parameter(b"object", Type::Object)],
                    Some(nullable(Type::Callable)),
                ),
                unbuilt as Entry,
            ),
            (
                tentative(method(
                    b"getParentClass",
                    &[],
                    Some(union(&[named(b"ReflectionClass"), Type::False])),
                )),
                super::class::reflection_class_get_parent_class as Entry,
            ),
            (
                tentative(method(
                    b"isSubclassOf",
                    &[parameter(
                        b"class",
                        union(&[named(b"ReflectionClass"), Type::String]),
                    )],
                    Some(Type::Bool),
                )),
                super::reflection_class_is_subclass_of as Entry,
            ),
            (
                tentative(method(b"getStaticProperties", &[], Some(Type::Array))),
                super::class::reflection_class_get_static_properties as Entry,
            ),
            (
                tentative(method(
                    b"getStaticPropertyValue",
                    &[
                        parameter(b"name", Type::String),
                        unknown_default(b"default", Type::Mixed),
                    ],
                    Some(Type::Mixed),
                )),
                super::class::reflection_class_get_static_property_value as Entry,
            ),
            (
                tentative(method(
                    b"setStaticPropertyValue",
                    &[
                        parameter(b"name", Type::String),
                        parameter(b"value", Type::Mixed),
                    ],
                    Some(Type::Void),
                )),
                super::class::reflection_class_set_static_property_value as Entry,
            ),
            (
                tentative(method(b"getDefaultProperties", &[], Some(Type::Array))),
                super::reflection_class_get_default_properties as Entry,
            ),
            (
                tentative(method(b"isIterable", &[], Some(Type::Bool))),
                super::class::reflection_class_is_iterable as Entry,
            ),
            (
                tentative(method(b"isIterateable", &[], Some(Type::Bool))),
                super::class::reflection_class_is_iterable as Entry,
            ),
            (
                tentative(method(
                    b"implementsInterface",
                    &[parameter(
                        b"interface",
                        union(&[named(b"ReflectionClass"), Type::String]),
                    )],
                    Some(Type::Bool),
                )),
                super::class::reflection_class_implements_interface as Entry,
            ),
            (
                tentative(method(
                    b"getExtension",
                    &[],
                    Some(nullable(named(b"ReflectionExtension"))),
                )),
                super::reflection_class_get_extension as Entry,
            ),
            (
                tentative(method(
                    b"getExtensionName",
                    &[],
                    Some(union(&[Type::String, Type::False])),
                )),
                super::reflection_class_get_extension_name as Entry,
            ),
            (
                tentative(method(b"inNamespace", &[], Some(Type::Bool))),
                super::reflection_class_in_namespace as Entry,
            ),
            (
                tentative(method(b"getNamespaceName", &[], Some(Type::String))),
                super::reflection_class_get_namespace_name as Entry,
            ),
            (
                tentative(method(b"getShortName", &[], Some(Type::String))),
                super::reflection_class_get_short_name as Entry,
            ),
            (
                method(
                    b"getAttributes",
                    &[
                        with_default(b"name", nullable(Type::String), ConstExpr::Null),
                        with_default(b"flags", Type::Int, ConstExpr::Long(0)),
                    ],
                    Some(Type::Array),
                ),
                super::reflection_class_get_attributes as Entry,
            ),
        ],
    )
}

/// `ReflectionObject` as the stub declares it.
fn reflection_object() -> Declared {
    (
        class(b"ReflectionObject", Some(b"ReflectionClass"), &[]),
        vec![(
            method(b"__construct", &[parameter(b"object", Type::Object)], None),
            super::reflection_object_construct as Entry,
        )],
    )
}

/// `enum PropertyHookType: string` as the stub declares it and
/// `zend_register_internal_enum` links it: `BackedEnum` implemented,
/// `cases`, `from` and `tryFrom` on its own table as
/// `zend_enum_register_funcs` adds them, and `$name` and `$value` as
/// `zend_enum_register_props` declares them. Registered last, after
/// `ReflectionConstant`, as `PHP_MINIT_FUNCTION(reflection)` has it.
fn property_hook_type() -> Declared {
    (
        ClassDecl {
            kind: ClassKind::Enum,
            backing: EnumBacking::String,
            constants: vec![case(b"Get", b"get"), case(b"Set", b"set")],
            properties: vec![readonly_string(b"name"), readonly_string(b"value")],
            ..class(b"PropertyHookType", None, &[b"BackedEnum"])
        },
        vec![
            (
                is_static(method(b"cases", &[], Some(Type::Array))),
                super::enums::enum_cases as Entry,
            ),
            (
                is_static(method(
                    b"from",
                    &[parameter(b"value", union(&[Type::Int, Type::String]))],
                    Some(Type::CalledClass),
                )),
                super::enums::enum_from as Entry,
            ),
            (
                is_static(method(
                    b"tryFrom",
                    &[parameter(b"value", union(&[Type::Int, Type::String]))],
                    Some(nullable(Type::CalledClass)),
                )),
                super::enums::enum_try_from as Entry,
            ),
        ],
    )
}

/// `ReflectionProperty` as the stub declares it.
fn reflection_property() -> Declared {
    (
        ClassDecl {
            constants: vec![
                int_constant(b"IS_STATIC", 16),
                int_constant(b"IS_READONLY", 128),
                int_constant(b"IS_PUBLIC", 1),
                int_constant(b"IS_PROTECTED", 2),
                int_constant(b"IS_PRIVATE", 4),
                int_constant(b"IS_ABSTRACT", 64),
                int_constant(b"IS_PROTECTED_SET", 2048),
                int_constant(b"IS_PRIVATE_SET", 4096),
                int_constant(b"IS_VIRTUAL", 512),
                int_constant(b"IS_FINAL", 32),
            ],
            properties: vec![
                name_property(),
                PropertyDecl {
                    name: Name::from("class"),
                    ..name_property()
                },
            ],
            ..class(b"ReflectionProperty", None, &[b"Reflector"])
        },
        vec![
            (
                private(method(b"__clone", &[], Some(Type::Void))),
                super::func::reflection_clone as Entry,
            ),
            (
                method(
                    b"__construct",
                    &[
                        parameter(b"class", union(&[Type::Object, Type::String])),
                        parameter(b"property", Type::String),
                    ],
                    None,
                ),
                super::reflection_property_construct as Entry,
            ),
            (
                method(b"__toString", &[], Some(Type::String)),
                super::member::reflection_property_to_string as Entry,
            ),
            (
                tentative(method(b"getName", &[], Some(Type::String))),
                super::member::reflection_property_get_name as Entry,
            ),
            (
                method(b"getMangledName", &[], Some(Type::String)),
                unbuilt as Entry,
            ),
            (
                tentative(method(
                    b"getValue",
                    &[with_default(
                        b"object",
                        nullable(Type::Object),
                        ConstExpr::Null,
                    )],
                    Some(Type::Mixed),
                )),
                super::reflection_property_get_value as Entry,
            ),
            (
                tentative(method(
                    b"setValue",
                    &[
                        parameter(b"objectOrValue", Type::Mixed),
                        unknown_default(b"value", Type::Mixed),
                    ],
                    Some(Type::Void),
                )),
                super::reflection_property_set_value as Entry,
            ),
            (
                method(
                    b"getRawValue",
                    &[parameter(b"object", Type::Object)],
                    Some(Type::Mixed),
                ),
                unbuilt as Entry,
            ),
            (
                method(
                    b"setRawValue",
                    &[
                        parameter(b"object", Type::Object),
                        parameter(b"value", Type::Mixed),
                    ],
                    Some(Type::Void),
                ),
                unbuilt as Entry,
            ),
            (
                method(
                    b"setRawValueWithoutLazyInitialization",
                    &[
                        parameter(b"object", Type::Object),
                        parameter(b"value", Type::Mixed),
                    ],
                    Some(Type::Void),
                ),
                unbuilt as Entry,
            ),
            (
                method(
                    b"skipLazyInitialization",
                    &[parameter(b"object", Type::Object)],
                    Some(Type::Void),
                ),
                unbuilt as Entry,
            ),
            (
                method(
                    b"isLazy",
                    &[parameter(b"object", Type::Object)],
                    Some(Type::Bool),
                ),
                unbuilt as Entry,
            ),
            (
                tentative(method(
                    b"isInitialized",
                    &[with_default(
                        b"object",
                        nullable(Type::Object),
                        ConstExpr::Null,
                    )],
                    Some(Type::Bool),
                )),
                unbuilt as Entry,
            ),
            (
                tentative(method(b"isPublic", &[], Some(Type::Bool))),
                super::member::reflection_property_is_public as Entry,
            ),
            (
                tentative(method(b"isPrivate", &[], Some(Type::Bool))),
                super::member::reflection_property_is_private as Entry,
            ),
            (
                tentative(method(b"isProtected", &[], Some(Type::Bool))),
                super::member::reflection_property_is_protected as Entry,
            ),
            (
                method(b"isPrivateSet", &[], Some(Type::Bool)),
                super::member::reflection_property_is_private_set as Entry,
            ),
            (
                method(b"isProtectedSet", &[], Some(Type::Bool)),
                super::member::reflection_property_is_protected_set as Entry,
            ),
            (
                tentative(method(b"isStatic", &[], Some(Type::Bool))),
                super::member::reflection_property_is_static as Entry,
            ),
            (
                method(b"isReadOnly", &[], Some(Type::Bool)),
                super::member::reflection_property_is_read_only as Entry,
            ),
            (
                tentative(method(b"isDefault", &[], Some(Type::Bool))),
                super::member::reflection_property_is_default as Entry,
            ),
            (
                method(b"isDynamic", &[], Some(Type::Bool)),
                super::member::reflection_property_is_dynamic as Entry,
            ),
            (
                method(b"isAbstract", &[], Some(Type::Bool)),
                unbuilt as Entry,
            ),
            (
                method(b"isVirtual", &[], Some(Type::Bool)),
                unbuilt as Entry,
            ),
            (
                method(b"isPromoted", &[], Some(Type::Bool)),
                super::member::reflection_property_is_promoted as Entry,
            ),
            (
                tentative(method(b"getModifiers", &[], Some(Type::Int))),
                super::member::reflection_property_get_modifiers as Entry,
            ),
            (
                tentative(method(
                    b"getDeclaringClass",
                    &[],
                    Some(named(b"ReflectionClass")),
                )),
                super::member::reflection_property_get_declaring_class as Entry,
            ),
            (
                tentative(method(
                    b"getDocComment",
                    &[],
                    Some(union(&[Type::String, Type::False])),
                )),
                super::reflection_property_get_doc_comment as Entry,
            ),
            (
                deprecated(
                    tentative(method(
                        b"setAccessible",
                        &[parameter(b"accessible", Type::Bool)],
                        Some(Type::Void),
                    )),
                    "8.5",
                    "as it has no effect since PHP 8.1",
                ),
                unbuilt as Entry,
            ),
            (
                tentative(method(
                    b"getType",
                    &[],
                    Some(nullable(named(b"ReflectionType"))),
                )),
                super::member::reflection_property_get_type as Entry,
            ),
            (
                method(
                    b"getSettableType",
                    &[],
                    Some(nullable(named(b"ReflectionType"))),
                ),
                super::member::reflection_property_get_settable_type as Entry,
            ),
            (
                tentative(method(b"hasType", &[], Some(Type::Bool))),
                super::member::reflection_property_has_type as Entry,
            ),
            (
                method(b"hasDefaultValue", &[], Some(Type::Bool)),
                super::member::reflection_property_has_default_value as Entry,
            ),
            (
                tentative(method(b"getDefaultValue", &[], Some(Type::Mixed))),
                super::member::reflection_property_get_default_value as Entry,
            ),
            (
                method(
                    b"getAttributes",
                    &[
                        with_default(b"name", nullable(Type::String), ConstExpr::Null),
                        with_default(b"flags", Type::Int, ConstExpr::Long(0)),
                    ],
                    Some(Type::Array),
                ),
                super::member::reflection_property_get_attributes as Entry,
            ),
            (method(b"hasHooks", &[], Some(Type::Bool)), unbuilt as Entry),
            (
                method(b"getHooks", &[], Some(Type::Array)),
                unbuilt as Entry,
            ),
            (
                method(
                    b"hasHook",
                    &[parameter(b"type", named(b"PropertyHookType"))],
                    Some(Type::Bool),
                ),
                unbuilt as Entry,
            ),
            (
                method(
                    b"getHook",
                    &[parameter(b"type", named(b"PropertyHookType"))],
                    Some(nullable(named(b"ReflectionMethod"))),
                ),
                unbuilt as Entry,
            ),
            (method(b"isFinal", &[], Some(Type::Bool)), unbuilt as Entry),
        ],
    )
}

/// `ReflectionClassConstant` as the stub declares it.
fn reflection_class_constant() -> Declared {
    (
        ClassDecl {
            constants: vec![
                int_constant(b"IS_PUBLIC", 1),
                int_constant(b"IS_PROTECTED", 2),
                int_constant(b"IS_PRIVATE", 4),
                int_constant(b"IS_FINAL", 32),
            ],
            properties: vec![
                name_property(),
                PropertyDecl {
                    name: Name::from("class"),
                    ..name_property()
                },
            ],
            ..class(b"ReflectionClassConstant", None, &[b"Reflector"])
        },
        vec![
            (
                private(method(b"__clone", &[], Some(Type::Void))),
                super::func::reflection_clone as Entry,
            ),
            (
                method(
                    b"__construct",
                    &[
                        parameter(b"class", union(&[Type::Object, Type::String])),
                        parameter(b"constant", Type::String),
                    ],
                    None,
                ),
                super::reflection_class_constant_construct as Entry,
            ),
            (
                method(b"__toString", &[], Some(Type::String)),
                super::member::reflection_class_constant_to_string as Entry,
            ),
            (
                tentative(method(b"getName", &[], Some(Type::String))),
                super::member::reflection_class_constant_get_name as Entry,
            ),
            (
                tentative(method(b"getValue", &[], Some(Type::Mixed))),
                super::member::reflection_class_constant_get_value as Entry,
            ),
            (
                tentative(method(b"isPublic", &[], Some(Type::Bool))),
                super::member::reflection_class_constant_is_public as Entry,
            ),
            (
                tentative(method(b"isPrivate", &[], Some(Type::Bool))),
                super::member::reflection_class_constant_is_private as Entry,
            ),
            (
                tentative(method(b"isProtected", &[], Some(Type::Bool))),
                super::member::reflection_class_constant_is_protected as Entry,
            ),
            (
                method(b"isFinal", &[], Some(Type::Bool)),
                super::member::reflection_class_constant_is_final as Entry,
            ),
            (
                tentative(method(b"getModifiers", &[], Some(Type::Int))),
                super::member::reflection_class_constant_get_modifiers as Entry,
            ),
            (
                tentative(method(
                    b"getDeclaringClass",
                    &[],
                    Some(named(b"ReflectionClass")),
                )),
                super::member::reflection_class_constant_get_declaring_class as Entry,
            ),
            (
                tentative(method(
                    b"getDocComment",
                    &[],
                    Some(union(&[Type::String, Type::False])),
                )),
                super::reflection_class_constant_get_doc_comment as Entry,
            ),
            (
                method(
                    b"getAttributes",
                    &[
                        with_default(b"name", nullable(Type::String), ConstExpr::Null),
                        with_default(b"flags", Type::Int, ConstExpr::Long(0)),
                    ],
                    Some(Type::Array),
                ),
                super::member::reflection_class_constant_get_attributes as Entry,
            ),
            (
                method(b"isEnumCase", &[], Some(Type::Bool)),
                super::member::reflection_class_constant_is_enum_case as Entry,
            ),
            (
                method(b"isDeprecated", &[], Some(Type::Bool)),
                super::member::reflection_class_constant_is_deprecated as Entry,
            ),
            (
                method(b"hasType", &[], Some(Type::Bool)),
                super::member::reflection_class_constant_has_type as Entry,
            ),
            (
                method(b"getType", &[], Some(nullable(named(b"ReflectionType")))),
                super::member::reflection_class_constant_get_type as Entry,
            ),
        ],
    )
}

/// `ReflectionParameter` as the stub declares it.
fn reflection_parameter() -> Declared {
    (
        ClassDecl {
            properties: vec![name_property()],
            ..class(b"ReflectionParameter", None, &[b"Reflector"])
        },
        vec![
            (
                private(method(b"__clone", &[], Some(Type::Void))),
                super::func::reflection_clone as Entry,
            ),
            (
                method(
                    b"__construct",
                    &[
                        untyped(b"function"),
                        parameter(b"param", union(&[Type::Int, Type::String])),
                    ],
                    None,
                ),
                super::signature::reflection_parameter_construct as Entry,
            ),
            (
                method(b"__toString", &[], Some(Type::String)),
                super::signature::reflection_parameter_to_string as Entry,
            ),
            (
                tentative(method(b"getName", &[], Some(Type::String))),
                super::signature::reflection_parameter_get_name as Entry,
            ),
            (
                tentative(method(b"isPassedByReference", &[], Some(Type::Bool))),
                super::signature::reflection_parameter_is_passed_by_reference as Entry,
            ),
            (
                tentative(method(b"canBePassedByValue", &[], Some(Type::Bool))),
                super::signature::reflection_parameter_can_be_passed_by_value as Entry,
            ),
            (
                tentative(method(
                    b"getDeclaringFunction",
                    &[],
                    Some(named(b"ReflectionFunctionAbstract")),
                )),
                super::signature::reflection_parameter_get_declaring_function as Entry,
            ),
            (
                tentative(method(
                    b"getDeclaringClass",
                    &[],
                    Some(nullable(named(b"ReflectionClass"))),
                )),
                super::signature::reflection_parameter_get_declaring_class as Entry,
            ),
            (
                deprecated(
                    tentative(method(
                        b"getClass",
                        &[],
                        Some(nullable(named(b"ReflectionClass"))),
                    )),
                    "8.0",
                    "use ReflectionParameter::getType() instead",
                ),
                unbuilt as Entry,
            ),
            (
                tentative(method(b"hasType", &[], Some(Type::Bool))),
                super::signature::reflection_parameter_has_type as Entry,
            ),
            (
                tentative(method(
                    b"getType",
                    &[],
                    Some(nullable(named(b"ReflectionType"))),
                )),
                super::signature::reflection_parameter_get_type as Entry,
            ),
            (
                deprecated(
                    tentative(method(b"isArray", &[], Some(Type::Bool))),
                    "8.0",
                    "use ReflectionParameter::getType() instead",
                ),
                unbuilt as Entry,
            ),
            (
                deprecated(
                    tentative(method(b"isCallable", &[], Some(Type::Bool))),
                    "8.0",
                    "use ReflectionParameter::getType() instead",
                ),
                unbuilt as Entry,
            ),
            (
                tentative(method(b"allowsNull", &[], Some(Type::Bool))),
                super::signature::reflection_parameter_allows_null as Entry,
            ),
            (
                tentative(method(b"getPosition", &[], Some(Type::Int))),
                super::signature::reflection_parameter_get_position as Entry,
            ),
            (
                tentative(method(b"isOptional", &[], Some(Type::Bool))),
                super::signature::reflection_parameter_is_optional as Entry,
            ),
            (
                tentative(method(b"isDefaultValueAvailable", &[], Some(Type::Bool))),
                super::signature::reflection_parameter_is_default_value_available as Entry,
            ),
            (
                tentative(method(b"getDefaultValue", &[], Some(Type::Mixed))),
                super::signature::reflection_parameter_get_default_value as Entry,
            ),
            (
                tentative(method(b"isDefaultValueConstant", &[], Some(Type::Bool))),
                super::signature::reflection_parameter_is_default_value_constant as Entry,
            ),
            (
                tentative(method(
                    b"getDefaultValueConstantName",
                    &[],
                    Some(nullable(Type::String)),
                )),
                super::signature::reflection_parameter_get_default_value_constant_name as Entry,
            ),
            (
                tentative(method(b"isVariadic", &[], Some(Type::Bool))),
                super::signature::reflection_parameter_is_variadic as Entry,
            ),
            (
                method(b"isPromoted", &[], Some(Type::Bool)),
                super::signature::reflection_parameter_is_promoted as Entry,
            ),
            (
                method(
                    b"getAttributes",
                    &[
                        with_default(b"name", nullable(Type::String), ConstExpr::Null),
                        with_default(b"flags", Type::Int, ConstExpr::Long(0)),
                    ],
                    Some(Type::Array),
                ),
                super::signature::reflection_parameter_get_attributes as Entry,
            ),
        ],
    )
}

/// `ReflectionType` as the stub declares it.
fn reflection_type() -> Declared {
    (
        ClassDecl {
            flags: ClassFlags {
                is_abstract: true,
                ..ClassFlags::default()
            },
            ..class(b"ReflectionType", None, &[b"Stringable"])
        },
        vec![
            (
                private(method(b"__clone", &[], Some(Type::Void))),
                super::func::reflection_clone as Entry,
            ),
            (
                tentative(method(b"allowsNull", &[], Some(Type::Bool))),
                super::signature::reflection_type_allows_null as Entry,
            ),
            (
                method(b"__toString", &[], Some(Type::String)),
                super::signature::reflection_type_to_string as Entry,
            ),
        ],
    )
}

/// `ReflectionNamedType` as the stub declares it.
fn reflection_named_type() -> Declared {
    (
        class(b"ReflectionNamedType", Some(b"ReflectionType"), &[]),
        vec![
            (
                tentative(method(b"getName", &[], Some(Type::String))),
                super::signature::reflection_named_type_get_name as Entry,
            ),
            (
                tentative(method(b"isBuiltin", &[], Some(Type::Bool))),
                super::signature::reflection_named_type_is_builtin as Entry,
            ),
        ],
    )
}

/// `ReflectionUnionType` as the stub declares it.
fn reflection_union_type() -> Declared {
    (
        class(b"ReflectionUnionType", Some(b"ReflectionType"), &[]),
        vec![(
            method(b"getTypes", &[], Some(Type::Array)),
            super::signature::reflection_union_type_get_types as Entry,
        )],
    )
}

/// `ReflectionIntersectionType` as the stub declares it.
fn reflection_intersection_type() -> Declared {
    (
        class(b"ReflectionIntersectionType", Some(b"ReflectionType"), &[]),
        vec![(
            method(b"getTypes", &[], Some(Type::Array)),
            super::signature::reflection_union_type_get_types as Entry,
        )],
    )
}

/// `ReflectionExtension` as the stub declares it.
fn reflection_extension() -> Declared {
    (
        ClassDecl {
            properties: vec![name_property()],
            ..class(b"ReflectionExtension", None, &[b"Reflector"])
        },
        vec![
            (
                private(method(b"__clone", &[], Some(Type::Void))),
                super::func::reflection_clone as Entry,
            ),
            (
                method(b"__construct", &[parameter(b"name", Type::String)], None),
                super::reflection_extension_construct as Entry,
            ),
            (
                method(b"__toString", &[], Some(Type::String)),
                super::reflection_extension_to_string as Entry,
            ),
            (
                tentative(method(b"getName", &[], Some(Type::String))),
                super::reflection_extension_get_name as Entry,
            ),
            (
                tentative(method(b"getVersion", &[], Some(nullable(Type::String)))),
                super::reflection_extension_get_version as Entry,
            ),
            (
                tentative(method(b"getFunctions", &[], Some(Type::Array))),
                super::reflection_extension_get_functions as Entry,
            ),
            (
                tentative(method(b"getConstants", &[], Some(Type::Array))),
                super::reflection_extension_get_constants as Entry,
            ),
            (
                tentative(method(b"getINIEntries", &[], Some(Type::Array))),
                unbuilt as Entry,
            ),
            (
                tentative(method(b"getClasses", &[], Some(Type::Array))),
                super::reflection_extension_get_classes as Entry,
            ),
            (
                tentative(method(b"getClassNames", &[], Some(Type::Array))),
                super::reflection_extension_get_class_names as Entry,
            ),
            (
                tentative(method(b"getDependencies", &[], Some(Type::Array))),
                super::reflection_extension_get_dependencies as Entry,
            ),
            (
                tentative(method(b"info", &[], Some(Type::Void))),
                super::reflection_extension_info as Entry,
            ),
            (
                tentative(method(b"isPersistent", &[], Some(Type::Bool))),
                super::reflection_extension_is_persistent as Entry,
            ),
            (
                tentative(method(b"isTemporary", &[], Some(Type::Bool))),
                super::reflection_extension_is_temporary as Entry,
            ),
        ],
    )
}

/// `ReflectionZendExtension` as the stub declares it.
fn reflection_zend_extension() -> Declared {
    (
        ClassDecl {
            properties: vec![name_property()],
            ..class(b"ReflectionZendExtension", None, &[b"Reflector"])
        },
        vec![
            (
                private(method(b"__clone", &[], Some(Type::Void))),
                super::func::reflection_clone as Entry,
            ),
            (
                method(b"__construct", &[parameter(b"name", Type::String)], None),
                unbuilt as Entry,
            ),
            (
                method(b"__toString", &[], Some(Type::String)),
                unbuilt as Entry,
            ),
            (
                tentative(method(b"getName", &[], Some(Type::String))),
                unbuilt as Entry,
            ),
            (
                tentative(method(b"getVersion", &[], Some(Type::String))),
                unbuilt as Entry,
            ),
            (
                tentative(method(b"getAuthor", &[], Some(Type::String))),
                unbuilt as Entry,
            ),
            (
                tentative(method(b"getURL", &[], Some(Type::String))),
                unbuilt as Entry,
            ),
            (
                tentative(method(b"getCopyright", &[], Some(Type::String))),
                unbuilt as Entry,
            ),
        ],
    )
}

/// `ReflectionReference` as the stub declares it.
fn reflection_reference() -> Declared {
    (
        ClassDecl {
            flags: ClassFlags {
                is_final: true,
                ..ClassFlags::default()
            },
            ..class(b"ReflectionReference", None, &[])
        },
        vec![
            (
                is_static(method(
                    b"fromArrayElement",
                    &[
                        parameter(b"array", Type::Array),
                        parameter(b"key", union(&[Type::Int, Type::String])),
                    ],
                    Some(nullable(named(b"ReflectionReference"))),
                )),
                unbuilt as Entry,
            ),
            (method(b"getId", &[], Some(Type::String)), unbuilt as Entry),
            (
                private(method(b"__clone", &[], Some(Type::Void))),
                unbuilt as Entry,
            ),
            (private(method(b"__construct", &[], None)), unbuilt as Entry),
        ],
    )
}

/// `ReflectionAttribute` as the stub declares it.
fn reflection_attribute() -> Declared {
    (
        ClassDecl {
            constants: vec![int_constant(b"IS_INSTANCEOF", 2)],
            properties: vec![name_property()],
            ..class(b"ReflectionAttribute", None, &[b"Reflector"])
        },
        vec![
            (
                method(b"getName", &[], Some(Type::String)),
                super::attr::reflection_attribute_get_name as Entry,
            ),
            (
                method(b"getTarget", &[], Some(Type::Int)),
                super::attr::reflection_attribute_get_target as Entry,
            ),
            (
                method(b"isRepeated", &[], Some(Type::Bool)),
                super::attr::reflection_attribute_is_repeated as Entry,
            ),
            (
                method(b"getArguments", &[], Some(Type::Array)),
                super::attr::reflection_attribute_get_arguments as Entry,
            ),
            (
                method(b"newInstance", &[], Some(Type::Object)),
                super::attr::reflection_attribute_new_instance as Entry,
            ),
            (
                method(b"__toString", &[], Some(Type::String)),
                super::attr::reflection_attribute_to_string as Entry,
            ),
            (
                private(method(b"__clone", &[], Some(Type::Void))),
                super::func::reflection_clone as Entry,
            ),
            (
                private(method(b"__construct", &[], None)),
                super::attr::reflection_attribute_construct as Entry,
            ),
        ],
    )
}

/// `ReflectionEnum` as the stub declares it.
fn reflection_enum() -> Declared {
    (
        class(b"ReflectionEnum", Some(b"ReflectionClass"), &[]),
        vec![
            (
                method(
                    b"__construct",
                    &[parameter(
                        b"objectOrClass",
                        union(&[Type::Object, Type::String]),
                    )],
                    None,
                ),
                super::enums::reflection_enum_construct as Entry,
            ),
            (
                method(
                    b"hasCase",
                    &[parameter(b"name", Type::String)],
                    Some(Type::Bool),
                ),
                super::enums::reflection_enum_has_case as Entry,
            ),
            (
                method(
                    b"getCase",
                    &[parameter(b"name", Type::String)],
                    Some(named(b"ReflectionEnumUnitCase")),
                ),
                super::enums::reflection_enum_get_case as Entry,
            ),
            (
                method(b"getCases", &[], Some(Type::Array)),
                super::enums::reflection_enum_get_cases as Entry,
            ),
            (
                method(b"isBacked", &[], Some(Type::Bool)),
                super::enums::reflection_enum_is_backed as Entry,
            ),
            (
                method(
                    b"getBackingType",
                    &[],
                    Some(nullable(named(b"ReflectionNamedType"))),
                ),
                super::enums::reflection_enum_get_backing_type as Entry,
            ),
        ],
    )
}

/// `ReflectionEnumUnitCase` as the stub declares it.
fn reflection_enum_unit_case() -> Declared {
    (
        class(
            b"ReflectionEnumUnitCase",
            Some(b"ReflectionClassConstant"),
            &[],
        ),
        vec![
            (
                method(
                    b"__construct",
                    &[
                        parameter(b"class", union(&[Type::Object, Type::String])),
                        parameter(b"constant", Type::String),
                    ],
                    None,
                ),
                super::enums::reflection_enum_case_construct as Entry,
            ),
            (
                method(b"getEnum", &[], Some(named(b"ReflectionEnum"))),
                super::enums::reflection_enum_case_get_enum as Entry,
            ),
            (
                method(b"getValue", &[], Some(named(b"UnitEnum"))),
                super::enums::reflection_enum_case_get_value as Entry,
            ),
        ],
    )
}

/// `ReflectionEnumBackedCase` as the stub declares it.
fn reflection_enum_backed_case() -> Declared {
    (
        class(
            b"ReflectionEnumBackedCase",
            Some(b"ReflectionEnumUnitCase"),
            &[],
        ),
        vec![
            (
                method(
                    b"__construct",
                    &[
                        parameter(b"class", union(&[Type::Object, Type::String])),
                        parameter(b"constant", Type::String),
                    ],
                    None,
                ),
                super::enums::reflection_enum_case_construct as Entry,
            ),
            (
                method(
                    b"getBackingValue",
                    &[],
                    Some(union(&[Type::Int, Type::String])),
                ),
                super::enums::reflection_enum_case_get_backing_value as Entry,
            ),
        ],
    )
}

/// `ReflectionFiber` as the stub declares it.
fn reflection_fiber() -> Declared {
    (
        ClassDecl {
            flags: ClassFlags {
                is_final: true,
                ..ClassFlags::default()
            },
            ..class(b"ReflectionFiber", None, &[])
        },
        vec![
            (
                method(
                    b"__construct",
                    &[parameter(b"fiber", named(b"Fiber"))],
                    None,
                ),
                unbuilt as Entry,
            ),
            (
                method(b"getFiber", &[], Some(named(b"Fiber"))),
                unbuilt as Entry,
            ),
            (
                method(b"getExecutingFile", &[], Some(nullable(Type::String))),
                unbuilt as Entry,
            ),
            (
                method(b"getExecutingLine", &[], Some(nullable(Type::Int))),
                unbuilt as Entry,
            ),
            (
                method(b"getCallable", &[], Some(Type::Callable)),
                unbuilt as Entry,
            ),
            (
                method(
                    b"getTrace",
                    &[with_default(
                        b"options",
                        Type::Int,
                        ConstExpr::Constant(Name::from("DEBUG_BACKTRACE_PROVIDE_OBJECT")),
                    )],
                    Some(Type::Array),
                ),
                unbuilt as Entry,
            ),
        ],
    )
}

/// `ReflectionConstant` as the stub declares it.
fn reflection_constant() -> Declared {
    (
        ClassDecl {
            properties: vec![name_property()],
            ..class(b"ReflectionConstant", None, &[b"Reflector"])
        },
        vec![
            (
                method(b"__construct", &[parameter(b"name", Type::String)], None),
                super::reflection_constant_construct as Entry,
            ),
            (
                method(b"getName", &[], Some(Type::String)),
                super::member::reflection_constant_get_name as Entry,
            ),
            (
                method(b"getNamespaceName", &[], Some(Type::String)),
                super::member::reflection_constant_get_namespace_name as Entry,
            ),
            (
                method(b"getShortName", &[], Some(Type::String)),
                super::member::reflection_constant_get_short_name as Entry,
            ),
            (
                method(b"getValue", &[], Some(Type::Mixed)),
                super::member::reflection_constant_get_value as Entry,
            ),
            (
                method(b"isDeprecated", &[], Some(Type::Bool)),
                super::member::reflection_constant_is_deprecated as Entry,
            ),
            (
                method(
                    b"getFileName",
                    &[],
                    Some(union(&[Type::String, Type::False])),
                ),
                unbuilt as Entry,
            ),
            (
                method(
                    b"getExtension",
                    &[],
                    Some(nullable(named(b"ReflectionExtension"))),
                ),
                super::member::reflection_constant_get_extension as Entry,
            ),
            (
                method(
                    b"getExtensionName",
                    &[],
                    Some(union(&[Type::String, Type::False])),
                ),
                super::member::reflection_constant_get_extension_name as Entry,
            ),
            (
                method(b"__toString", &[], Some(Type::String)),
                super::member::reflection_constant_to_string as Entry,
            ),
            (
                method(
                    b"getAttributes",
                    &[
                        with_default(b"name", nullable(Type::String), ConstExpr::Null),
                        with_default(b"flags", Type::Int, ConstExpr::Long(0)),
                    ],
                    Some(Type::Array),
                ),
                super::reflection_constant_get_attributes as Entry,
            ),
        ],
    )
}
